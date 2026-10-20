//! Real Windows virtual-output activation and cleanup through GDI.
//! A separately installed IddCx driver must expose a display adapter. Mode 1+
//! reports missing/unsupported hardware instead of substituting a physical monitor.

use crate::{PalError, VirtualDisplayBackend};
use core::mem::size_of;
use duplex_proto::DisplayDescriptor;
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsExW, GetMonitorInfoW,
    MonitorFromPoint, DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP,
    DISPLAY_DEVICE_PRIMARY_DEVICE, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed, SetWindowPos,
    ShowWindow, GWL_EXSTYLE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOZORDER, SW_RESTORE,
    WS_EX_TOOLWINDOW,
};

struct WindowRescueContext {
    primary_work_rect: RECT,
    owned_output_rects: Vec<RECT>,
    rescued_count: usize,
}

/// Evacuate windows from Duplex-owned outputs without moving windows belonging
/// to another application's virtual displays.
unsafe extern "system" fn rescue_window_enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut WindowRescueContext);

    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return BOOL(1);
    }

    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if (ex_style & WS_EX_TOOLWINDOW.0) != 0 {
        return BOOL(1);
    }

    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }

    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 16 || h <= 16 {
        return BOOL(1);
    }

    let center_x = rect.left + w / 2;
    let center_y = rect.top + h / 2;

    let on_owned_output = ctx.owned_output_rects.iter().any(|rect| {
        center_x >= rect.left
            && center_x < rect.right
            && center_y >= rect.top
            && center_y < rect.bottom
    });

    if on_owned_output {
        if IsZoomed(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }

        let work_w = (ctx.primary_work_rect.right - ctx.primary_work_rect.left).max(640);
        let work_h = (ctx.primary_work_rect.bottom - ctx.primary_work_rect.top).max(480);

        let clamped_w = w.min(work_w - 64);
        let clamped_h = h.min(work_h - 64);
        let offset = ((ctx.rescued_count as i32) * 32) % 256;
        let target_x = ctx.primary_work_rect.left + 48 + offset;
        let target_y = ctx.primary_work_rect.top + 48 + offset;

        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            target_x,
            target_y,
            clamped_w,
            clamped_h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        );
        ctx.rescued_count += 1;
    }

    BOOL(1)
}

pub struct WindowsVirtualDisplayBackend {
    active_descriptors: Vec<DisplayDescriptor>,
    claimed_targets: Vec<Vec<u16>>,
}

impl Default for WindowsVirtualDisplayBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsVirtualDisplayBackend {
    pub const fn new() -> Self {
        Self {
            active_descriptors: Vec::new(),
            claimed_targets: Vec::new(),
        }
    }

    fn utf16_null_trimmed(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    fn is_virtual_display_adapter(device_string: &str) -> bool {
        let lower = device_string.to_lowercase();
        lower.contains("idd")
            || lower.contains("duplex")
            || lower.contains("virtual")
            || lower.contains("mttvdd")
    }

    fn is_supported_virtual_adapter(device_string: &str) -> bool {
        let lower = device_string.to_lowercase();
        // Meta and Virtual Desktop adapters belong to other applications. They
        // must be excluded from physical outputs, but never claimed by Duplex.
        if lower.contains("meta") || lower.contains("virtual desktop") {
            return false;
        }
        lower.contains("duplex")
            || lower.contains("mttvdd")
            || lower.contains("virtual display driver")
            || lower.contains("iddsample")
    }

    /// Queries the bounding rectangle of all attached physical monitors and the primary monitor's `rcWork`.
    fn query_physical_bounds() -> (RECT, RECT) {
        let primary_work = unsafe {
            let hmon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut mi = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                mi.rcWork
            } else {
                RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                }
            }
        };

        let mut union_rect = RECT {
            left: 0,
            top: 0,
            right: primary_work.right.max(1920),
            bottom: primary_work.bottom.max(1080),
        };

        unsafe {
            let mut dev_idx = 0u32;
            loop {
                let mut dd = DISPLAY_DEVICEW {
                    cb: size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(PCWSTR::null(), dev_idx, &mut dd, 0).as_bool() {
                    break;
                }
                dev_idx += 1;

                if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP) == 0 {
                    continue;
                }
                let adapter_str = Self::utf16_null_trimmed(&dd.DeviceString);
                if Self::is_virtual_display_adapter(&adapter_str) {
                    continue;
                }

                let mut dm = DEVMODEW {
                    dmSize: size_of::<DEVMODEW>() as u16,
                    ..Default::default()
                };
                if EnumDisplaySettingsExW(
                    PCWSTR(dd.DeviceName.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut dm,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
                .as_bool()
                {
                    let pos_x = dm.Anonymous1.Anonymous2.dmPosition.x;
                    let pos_y = dm.Anonymous1.Anonymous2.dmPosition.y;
                    let right = pos_x + (dm.dmPelsWidth as i32);
                    let bottom = pos_y + (dm.dmPelsHeight as i32);
                    union_rect.left = union_rect.left.min(pos_x);
                    union_rect.top = union_rect.top.min(pos_y);
                    union_rect.right = union_rect.right.max(right);
                    union_rect.bottom = union_rect.bottom.max(bottom);
                }
            }
        }

        (primary_work, union_rect)
    }

    fn load_owned_targets(&mut self) {
        if !self.claimed_targets.is_empty() {
            return;
        }
        if let Some(path) = ownership_path() {
            if let Ok(saved) = fs::read_to_string(path) {
                for name in saved.lines().take(8) {
                    if is_virtual_target(name) {
                        self.claimed_targets
                            .push(name.encode_utf16().chain(Some(0)).collect());
                    }
                }
            }
        }
    }

    fn persist_targets(&self) -> Result<(), PalError> {
        if let Some(path) = ownership_path() {
            if self.claimed_targets.is_empty() {
                let _ = fs::remove_file(path);
            } else {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        PalError::VirtualDisplay(format!(
                            "Cannot preserve display cleanup state: {e}"
                        ))
                    })?;
                }
                let text = self
                    .claimed_targets
                    .iter()
                    .map(|name| Self::utf16_null_trimmed(name))
                    .collect::<Vec<_>>()
                    .join("\n");
                fs::write(path, text).map_err(|e| {
                    PalError::VirtualDisplay(format!("Cannot preserve display cleanup state: {e}"))
                })?;
            }
        }
        Ok(())
    }

    fn configure_virtual_targets(&mut self, specs: &[DisplayDescriptor]) -> Result<(), PalError> {
        let (_, bounds) = Self::query_physical_bounds();
        let mut x = bounds.right;
        unsafe {
            let mut index = 0;
            for spec in specs {
                let target = loop {
                    let mut device = DISPLAY_DEVICEW {
                        cb: size_of::<DISPLAY_DEVICEW>() as u32,
                        ..Default::default()
                    };
                    if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() {
                        return Err(PalError::VirtualDisplay(
                            "No usable Duplex-compatible virtual display adapter is available. Verify that Virtual Display Driver reports OK in Windows Device Manager, or install it from https://github.com/VirtualDrivers/Virtual-Display-Driver, then retry Mode 1+.".into()));
                    }
                    index += 1;
                    if Self::is_supported_virtual_adapter(&Self::utf16_null_trimmed(
                        &device.DeviceString,
                    )) {
                        break device;
                    }
                };
                let mut mode = DEVMODEW {
                    dmSize: size_of::<DEVMODEW>() as u16,
                    dmPelsWidth: spec.width_px,
                    dmPelsHeight: spec.height_px,
                    dmDisplayFrequency: spec.refresh_hz,
                    dmFields: DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
                    ..Default::default()
                };
                mode.Anonymous1.Anonymous2.dmPosition.x = x;
                mode.Anonymous1.Anonymous2.dmPosition.y = bounds.top;
                let result = ChangeDisplaySettingsExW(
                    PCWSTR(target.DeviceName.as_ptr()),
                    Some(&mode),
                    HWND::default(),
                    Default::default(),
                    None,
                );
                if result.0 != 0 {
                    return Err(PalError::VirtualDisplay(format!(
                        "Virtual adapter refused {}x{}@{} (display status {}). Add this resolution to the installed virtual driver's supported modes.", spec.width_px, spec.height_px, spec.refresh_hz, result.0)));
                }
                self.claimed_targets.push(target.DeviceName.to_vec());
                self.persist_targets()?;
                self.active_descriptors.push(spec.clone());
                x += spec.width_px as i32;
            }
        }
        Ok(())
    }
}

impl VirtualDisplayBackend for WindowsVirtualDisplayBackend {
    fn enumerate_physical_displays(&self) -> Result<Vec<DisplayDescriptor>, PalError> {
        Ok(physical_display_entries()
            .into_iter()
            .map(|(_, descriptor)| descriptor)
            .collect())
    }

    fn sweep_orphaned_displays(&mut self) -> Result<usize, PalError> {
        self.load_owned_targets();
        let count = self.claimed_targets.len();
        self.destroy_all_virtual_displays()?;
        info!(
            swept = count,
            "Completed Windows virtual-output crash sweep"
        );
        Ok(count)
    }

    fn sync_virtual_displays(&mut self, specs: &[DisplayDescriptor]) -> Result<(), PalError> {
        if specs.is_empty() {
            return self.destroy_all_virtual_displays();
        }

        self.destroy_all_virtual_displays()?;

        // Successful driver commands alone are not proof of a display.
        // Configure real adapters and verify the resulting capture targets.
        let result = self.configure_virtual_targets(specs).and_then(|()| {
            let attached = attached_virtual_rects();
            if attached.len() < specs.len()
                || attached.iter().zip(specs).any(|(rect, spec)| {
                    rect.right - rect.left != spec.width_px as i32
                        || rect.bottom - rect.top != spec.height_px as i32
                })
            {
                return Err(PalError::VirtualDisplay(
                    "The virtual driver did not attach the requested output geometry.".into(),
                ));
            }
            Ok(())
        });
        if result.is_err() {
            let _ = self.destroy_all_virtual_displays();
        }
        result
    }

    fn rescue_windows_to_primary(&mut self) -> Result<(), PalError> {
        let owned_output_rects = attached_virtual_rects();
        if owned_output_rects.is_empty() {
            return Ok(());
        }
        let (primary_work_rect, _) = Self::query_physical_bounds();
        let mut ctx = WindowRescueContext {
            primary_work_rect,
            owned_output_rects,
            rescued_count: 0,
        };

        unsafe {
            let _ = EnumWindows(
                Some(rescue_window_enum_proc),
                LPARAM(&mut ctx as *mut WindowRescueContext as isize),
            );
        }

        debug!(
            rescued_count = ctx.rescued_count,
            "Completed Win32 top-level window rescue to primary physical monitor"
        );
        Ok(())
    }

    fn destroy_all_virtual_displays(&mut self) -> Result<(), PalError> {
        self.load_owned_targets();
        let _ = self.rescue_windows_to_primary();
        let mut failed = Vec::new();

        for name in self.claimed_targets.drain(..) {
            let mode = DEVMODEW {
                dmSize: size_of::<DEVMODEW>() as u16,
                dmFields: DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT,
                dmPelsWidth: 0,
                dmPelsHeight: 0,
                ..Default::default()
            };
            unsafe {
                let result = ChangeDisplaySettingsExW(
                    PCWSTR(name.as_ptr()),
                    Some(&mode),
                    HWND::default(),
                    Default::default(),
                    None,
                );
                if result.0 != 0 {
                    failed.push(name);
                }
            }
        }
        self.claimed_targets = failed;
        self.active_descriptors.clear();
        self.persist_targets()?;
        if !self.claimed_targets.is_empty() {
            return Err(PalError::VirtualDisplay(
                "The virtual driver refused detachment; cleanup state was preserved for retry."
                    .into(),
            ));
        }
        info!("Detached Duplex-owned Windows virtual displays");
        Ok(())
    }

    fn active_virtual_displays(&self) -> &[DisplayDescriptor] {
        &self.active_descriptors
    }
}

/// Capture targets must come from attached virtual adapters, never monitor order.
pub(super) fn attached_virtual_rects() -> Vec<RECT> {
    attached_virtual_outputs()
        .into_iter()
        .map(|(_, rect)| rect)
        .collect()
}

/// Device names (`\\.\DISPLAYn`, as DXGI reports them) and desktop rectangles
/// of the attached virtual outputs claimed by Duplex.
pub(super) fn attached_virtual_outputs() -> Vec<(String, RECT)> {
    let mut result = Vec::new();
    let owned = ownership_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    unsafe {
        let mut index = 0;
        loop {
            let mut device = DISPLAY_DEVICEW {
                cb: size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() {
                break;
            }
            index += 1;
            if device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == 0
                || !WindowsVirtualDisplayBackend::is_supported_virtual_adapter(
                    &WindowsVirtualDisplayBackend::utf16_null_trimmed(&device.DeviceString),
                )
                || !owned.lines().any(|name| {
                    name == WindowsVirtualDisplayBackend::utf16_null_trimmed(&device.DeviceName)
                })
            {
                continue;
            }
            let mut mode = DEVMODEW {
                dmSize: size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if EnumDisplaySettingsExW(
                PCWSTR(device.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
            .as_bool()
            {
                let position = mode.Anonymous1.Anonymous2.dmPosition;
                result.push((
                    WindowsVirtualDisplayBackend::utf16_null_trimmed(&device.DeviceName),
                    RECT {
                        left: position.x,
                        top: position.y,
                        right: position.x + mode.dmPelsWidth as i32,
                        bottom: position.y + mode.dmPelsHeight as i32,
                    },
                ));
            }
        }
    }
    result
}

fn ownership_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|directory| {
        PathBuf::from(directory)
            .join("Duplex-KVM")
            .join("virtual-displays.txt")
    })
}

fn is_virtual_target(name: &str) -> bool {
    unsafe {
        let mut index = 0;
        loop {
            let mut device = DISPLAY_DEVICEW {
                cb: size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() {
                return false;
            }
            index += 1;
            if WindowsVirtualDisplayBackend::utf16_null_trimmed(&device.DeviceName) == name {
                return WindowsVirtualDisplayBackend::is_supported_virtual_adapter(
                    &WindowsVirtualDisplayBackend::utf16_null_trimmed(&device.DeviceString),
                );
            }
        }
    }
}

/// One enumeration/order supplies both advertised resolutions and viewer targets.
/// This prevents a 1080p stream being stretched onto the 4K primary monitor, and
/// excludes virtual adapters from the physical Windows viewer destinations.
fn physical_display_entries() -> Vec<(RECT, DisplayDescriptor)> {
    let mut entries = Vec::new();
    unsafe {
        let mut index = 0;
        loop {
            let mut device = DISPLAY_DEVICEW {
                cb: size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() {
                break;
            }
            index += 1;
            if device.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == 0
                || WindowsVirtualDisplayBackend::is_virtual_display_adapter(
                    &WindowsVirtualDisplayBackend::utf16_null_trimmed(&device.DeviceString),
                )
            {
                continue;
            }
            let mut mode = DEVMODEW {
                dmSize: size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if !EnumDisplaySettingsExW(
                PCWSTR(device.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
            .as_bool()
            {
                continue;
            }
            let position = mode.Anonymous1.Anonymous2.dmPosition;
            let width = mode.dmPelsWidth.max(1);
            let height = mode.dmPelsHeight.max(1);
            let hz = mode.dmDisplayFrequency.max(1);
            entries.push((
                device.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0,
                RECT {
                    left: position.x,
                    top: position.y,
                    right: position.x + width as i32,
                    bottom: position.y + height as i32,
                },
                DisplayDescriptor {
                    index: 0,
                    width_px: width,
                    height_px: height,
                    refresh_hz: hz,
                    scale_percent: 100,
                    physical_width_mm: 600,
                    physical_height_mm: 340,
                    edid_block: DisplayDescriptor::synthesize_edid(width, height, hz, 0),
                },
            ));
        }
    }
    entries.sort_by_key(|(primary, rect, _)| (!primary, rect.left));
    entries
        .into_iter()
        .enumerate()
        .map(|(index, (_, rect, mut descriptor))| {
            descriptor.index = index as u8;
            descriptor.edid_block = DisplayDescriptor::synthesize_edid(
                descriptor.width_px,
                descriptor.height_px,
                descriptor.refresh_hz,
                descriptor.index,
            );
            (rect, descriptor)
        })
        .collect()
}

pub(super) fn physical_monitor_rects() -> Vec<RECT> {
    physical_display_entries()
        .into_iter()
        .map(|(rect, _)| rect)
        .collect()
}
