//! Windows 11 `IddCx` Virtual Display Controller & Win32 Window Rescue (`duplex-pal::windows::display`).
//!
//! Implements:
//! 1. **Physical Monitor Enumeration (`enumerate_physical_displays`):** Queries Win32 `EnumDisplayDevicesW`
//!    and `EnumDisplaySettingsExW` (`ENUM_CURRENT_SETTINGS`) for all physical monitors (filtering out
//!    `IddCx` virtual adapters) so the Debian Laptop can mirror their exact resolution and refresh rate in Mode 2.
//! 2. **Startup Crash Sweeper (`sweep_orphaned_displays`):** Connects to the `IddCx` UMDF2 control pipe
//!    (`\\.\pipe\DuplexIddCxPipe` / `\\.\pipe\MttVirtualDisplayPipe`) on startup and tears down any
//!    orphaned virtual monitors left behind by a prior crash.
//! 3. **Dynamic `IddCx` Monitor Provisioning (`sync_virtual_displays`):** Pushes 128-byte VESA EDID 1.4
//!    blocks and mode timings to the `IddCx` driver and commits desktop layout via `ChangeDisplaySettingsExW`.
//! 4. **Non-Blocking Win32 Window Rescue (`rescue_windows_to_primary`):** Enumerates all top-level windows
//!    via `EnumWindows` and evacuates any window stranded on virtual display coordinates back into the
//!    primary physical monitor's `rcWork` work area using `SWP_ASYNCWINDOWPOS`.

use crate::{PalError, VirtualDisplayBackend};
use core::mem::size_of;
use duplex_proto::DisplayDescriptor;
use std::fs::OpenOptions;
use std::io::Write;
use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsExW, GetMonitorInfoW,
    MonitorFromPoint, CDS_RESET, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW,
    DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowRect, IsIconic, IsWindowVisible, IsZoomed, SetWindowPos,
    ShowWindow, GWL_EXSTYLE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOZORDER, SW_RESTORE,
    WS_EX_TOOLWINDOW,
};

/// Primary Named Pipe exposed by the Duplex `IddCx` UMDF2 Virtual Display Driver.
pub const DUPLEX_IDDCX_PIPE_PRIMARY: &str = r"\\.\pipe\DuplexIddCxPipe";
/// Fallback Named Pipe compatible with standard MTT / Parsec-style `IddCx` virtual display drivers.
pub const DUPLEX_IDDCX_PIPE_FALLBACK: &str = r"\\.\pipe\MttVirtualDisplayPipe";

/// Binary wire packet sent over `\\.\pipe\DuplexIddCxPipe` to the UMDF2 `IddCx` driver.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IddCxPipeCommand {
    magic: [u8; 4], // "IDDX"
    opcode: u8,     // 0 = DetachAll, 1 = AttachMonitor
    monitor_index: u8,
    width_px: u32,
    height_px: u32,
    refresh_hz: u32,
    scale_percent: u16,
    edid: [u8; 128],
}

struct WindowRescueContext {
    primary_work_rect: RECT,
    physical_union_rect: RECT,
    rescued_count: usize,
}

/// Win32 `EnumWindows` callback that evacuates visible top-level windows positioned outside
/// the physical monitor bounding rectangle back into the primary monitor's work area.
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

    let inside_physical = center_x >= ctx.physical_union_rect.left
        && center_x < ctx.physical_union_rect.right
        && center_y >= ctx.physical_union_rect.top
        && center_y < ctx.physical_union_rect.bottom;

    if !inside_physical {
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

    fn send_iddcx_pipe_command(cmd: &IddCxPipeCommand) -> bool {
        let raw_bytes = unsafe {
            core::slice::from_raw_parts(
                cmd as *const IddCxPipeCommand as *const u8,
                size_of::<IddCxPipeCommand>(),
            )
        };

        for pipe_path in [DUPLEX_IDDCX_PIPE_PRIMARY, DUPLEX_IDDCX_PIPE_FALLBACK] {
            if let Ok(mut pipe) = OpenOptions::new().write(true).open(pipe_path) {
                if pipe.write_all(raw_bytes).is_ok() {
                    let _ = pipe.flush();
                    return true;
                }
            }
        }
        false
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

    /// Applies exact resolution and refresh rate to any attached `IddCx` virtual display target via Win32 GDI.
    fn apply_gdi_mode_to_virtual_targets(specs: &[DisplayDescriptor]) {
        unsafe {
            let mut dev_idx = 0u32;
            let mut spec_idx = 0usize;
            while spec_idx < specs.len() {
                let mut dd = DISPLAY_DEVICEW {
                    cb: size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                if !EnumDisplayDevicesW(PCWSTR::null(), dev_idx, &mut dd, 0).as_bool() {
                    break;
                }
                dev_idx += 1;

                let adapter_str = Self::utf16_null_trimmed(&dd.DeviceString);
                if !Self::is_virtual_display_adapter(&adapter_str) {
                    continue;
                }

                let spec = &specs[spec_idx];
                let dm = DEVMODEW {
                    dmSize: size_of::<DEVMODEW>() as u16,
                    dmPelsWidth: spec.width_px,
                    dmPelsHeight: spec.height_px,
                    dmDisplayFrequency: spec.refresh_hz,
                    dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
                    ..Default::default()
                };

                let _ = ChangeDisplaySettingsExW(
                    PCWSTR(dd.DeviceName.as_ptr()),
                    Some(&dm),
                    HWND::default(),
                    CDS_UPDATEREGISTRY,
                    None,
                );
                spec_idx += 1;
            }
            let _ = ChangeDisplaySettingsExW(
                PCWSTR::null(),
                None,
                HWND::default(),
                CDS_RESET,
                None,
            );
        }
    }
}

impl VirtualDisplayBackend for WindowsVirtualDisplayBackend {
    fn enumerate_physical_displays(&self) -> Result<Vec<DisplayDescriptor>, PalError> {
        let mut displays = Vec::new();

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
                    let index = displays.len() as u8;
                    let width_px = dm.dmPelsWidth.max(640);
                    let height_px = dm.dmPelsHeight.max(480);
                    let refresh_hz = dm.dmDisplayFrequency.max(60);

                    displays.push(DisplayDescriptor {
                        index,
                        width_px,
                        height_px,
                        refresh_hz,
                        scale_percent: 100,
                        physical_width_mm: 600,
                        physical_height_mm: 340,
                        edid_block: DisplayDescriptor::synthesize_edid(
                            width_px, height_px, refresh_hz, index,
                        ),
                    });
                }
            }
        }

        if displays.is_empty() {
            // Fallback dual 1920x1080@60Hz descriptors if called in a headless session
            for idx in 0..2u8 {
                displays.push(DisplayDescriptor {
                    index: idx,
                    width_px: 1920,
                    height_px: 1080,
                    refresh_hz: 60,
                    scale_percent: 100,
                    physical_width_mm: 600,
                    physical_height_mm: 340,
                    edid_block: DisplayDescriptor::synthesize_edid(1920, 1080, 60, idx),
                });
            }
        }

        Ok(displays)
    }

    fn sweep_orphaned_displays(&mut self) -> Result<usize, PalError> {
        let detach_cmd = IddCxPipeCommand {
            magic: *b"IDDX",
            opcode: 0, // DetachAll
            monitor_index: 0,
            width_px: 0,
            height_px: 0,
            refresh_hz: 0,
            scale_percent: 100,
            edid: [0u8; 128],
        };
        let contacted = Self::send_iddcx_pipe_command(&detach_cmd);
        self.active_descriptors.clear();
        info!(
            iddcx_pipe_contacted = contacted,
            "Completed Windows IddCx startup virtual display crash sweep"
        );
        Ok(usize::from(contacted))
    }

    fn sync_virtual_displays(&mut self, specs: &[DisplayDescriptor]) -> Result<(), PalError> {
        if specs.is_empty() {
            return self.destroy_all_virtual_displays();
        }

        self.destroy_all_virtual_displays()?;

        for spec in specs {
            let mut edid = [0u8; 128];
            if spec.edid_block.len() >= 128 {
                edid.copy_from_slice(&spec.edid_block[..128]);
            } else {
                let synth = DisplayDescriptor::synthesize_edid(
                    spec.width_px,
                    spec.height_px,
                    spec.refresh_hz,
                    spec.index,
                );
                edid.copy_from_slice(&synth[..128]);
            }

            let cmd = IddCxPipeCommand {
                magic: *b"IDDX",
                opcode: 1, // AttachMonitor
                monitor_index: spec.index,
                width_px: spec.width_px,
                height_px: spec.height_px,
                refresh_hz: spec.refresh_hz,
                scale_percent: spec.scale_percent,
                edid,
            };

            if !Self::send_iddcx_pipe_command(&cmd) {
                warn!(
                    index = spec.index,
                    width = spec.width_px,
                    height = spec.height_px,
                    hz = spec.refresh_hz,
                    "IddCx pipe not yet listening; virtual display descriptor queued in logical state"
                );
            }
            self.active_descriptors.push(spec.clone());
        }

        Self::apply_gdi_mode_to_virtual_targets(specs);
        info!(
            count = self.active_descriptors.len(),
            "Configured Windows IddCx virtual display targets"
        );
        Ok(())
    }

    fn rescue_windows_to_primary(&mut self) -> Result<(), PalError> {
        let (primary_work_rect, physical_union_rect) = Self::query_physical_bounds();
        let mut ctx = WindowRescueContext {
            primary_work_rect,
            physical_union_rect,
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
        let _ = self.rescue_windows_to_primary();

        let detach_cmd = IddCxPipeCommand {
            magic: *b"IDDX",
            opcode: 0, // DetachAll
            monitor_index: 0,
            width_px: 0,
            height_px: 0,
            refresh_hz: 0,
            scale_percent: 100,
            edid: [0u8; 128],
        };
        let _ = Self::send_iddcx_pipe_command(&detach_cmd);
        self.active_descriptors.clear();
        info!("Detached all Windows IddCx virtual displays");
        Ok(())
    }

    fn active_virtual_displays(&self) -> &[DisplayDescriptor] {
        &self.active_descriptors
    }
}
