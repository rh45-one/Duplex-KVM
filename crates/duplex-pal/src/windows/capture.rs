//! Windows desktop capture for streaming.
//!
//! DXGI Desktop Duplication copies the virtual output on the GPU and wakes only
//! when the desktop or pointer changes. If it is unavailable (or lost, e.g.
//! during a secure-desktop prompt) capture falls back to GDI and retries DXGI
//! periodically. Neither source reliably includes the mouse cursor, so the
//! real cursor image is composited into every frame.

use crate::stream::{capture_fps_limit, FrameSlot};
use core::ffi::c_void;
use core::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC,
    ReleaseDC, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HALFTONE, HBITMAP, HBRUSH, HDC, HGDIOBJ,
    ROP_CODE, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HICON, ICONINFO,
};

/// Largest cursor image composited (accessibility sizes reach 256 px).
const CURSOR_AREA: i32 = 256;
const DXGI_RETRY: Duration = Duration::from_secs(5);

fn utf16_trimmed(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// A top-down 32-bit DIB section selected into its own memory DC.
struct DibSurface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl DibSurface {
    fn new(width: i32, height: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(HWND::default());
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(HWND::default(), screen);
            if dc.is_invalid() {
                return None;
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = core::ptr::null_mut();
            let bitmap = match CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bitmap) if !bits.is_null() => bitmap,
                _ => {
                    let _ = DeleteDC(dc);
                    return None;
                }
            };
            let previous = SelectObject(dc, HGDIOBJ(bitmap.0));
            Some(Self {
                dc,
                bitmap,
                previous,
                bits: bits.cast(),
                width,
                height,
            })
        }
    }

    fn pixels(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.bits, (self.width * self.height * 4) as usize) }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self.bits, (self.width * self.height * 4) as usize)
        }
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Draws the system cursor into captured frames.
struct CursorOverlay {
    scratch: Option<DibSurface>,
}

impl CursorOverlay {
    fn new() -> Self {
        Self {
            scratch: DibSurface::new(CURSOR_AREA, CURSOR_AREA),
        }
    }

    /// Composites the cursor into `frame` (BGRx, `width`×`height`, whose
    /// top-left is at desktop position `origin`). Returns a key that changes
    /// whenever the cursor's position or image changes.
    fn draw(
        &mut self,
        frame: &mut [u8],
        width: i32,
        height: i32,
        origin: POINT,
    ) -> Option<(i32, i32, usize)> {
        let scratch = self.scratch.as_mut()?;
        unsafe {
            let mut cursor = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            if GetCursorInfo(&mut cursor).is_err()
                || cursor.flags.0 & CURSOR_SHOWING.0 == 0
                || cursor.hCursor.is_invalid()
            {
                return None;
            }
            let icon = HICON(cursor.hCursor.0);
            let mut details = ICONINFO::default();
            if GetIconInfo(icon, &mut details).is_err() {
                return None;
            }
            // GetIconInfo returns copies of the cursor bitmaps; free them.
            for bitmap in [details.hbmMask, details.hbmColor] {
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                }
            }
            let key = (
                cursor.ptScreenPos.x,
                cursor.ptScreenPos.y,
                cursor.hCursor.0 as usize,
            );
            let left = cursor.ptScreenPos.x - origin.x - details.xHotspot as i32;
            let top = cursor.ptScreenPos.y - origin.y - details.yHotspot as i32;
            let (x0, y0) = (left.max(0), top.max(0));
            let (x1, y1) = (
                (left + CURSOR_AREA).min(width),
                (top + CURSOR_AREA).min(height),
            );
            if x0 >= x1 || y0 >= y1 {
                return Some(key);
            }
            let stride = (width * 4) as usize;
            let area = (CURSOR_AREA * 4) as usize;
            let span = ((x1 - x0) * 4) as usize;
            // Scratch coordinates are frame coordinates offset by (left, top).
            {
                let pixels = scratch.pixels_mut();
                for y in y0..y1 {
                    let source = y as usize * stride + x0 as usize * 4;
                    let target = (y - top) as usize * area + (x0 - left) as usize * 4;
                    pixels[target..target + span].copy_from_slice(&frame[source..source + span]);
                }
            }
            let _ = DrawIconEx(
                scratch.dc,
                0,
                0,
                icon,
                0,
                0,
                0,
                HBRUSH::default(),
                DI_NORMAL,
            );
            let _ = GdiFlush();
            let pixels = scratch.pixels();
            for y in y0..y1 {
                let target = y as usize * stride + x0 as usize * 4;
                let source = (y - top) as usize * area + (x0 - left) as usize * 4;
                frame[target..target + span].copy_from_slice(&pixels[source..source + span]);
            }
            Some(key)
        }
    }
}

/// GDI screen capture of a desktop rectangle (scaled if sizes differ).
struct GdiCapture {
    surface: DibSurface,
    source: RECT,
}

impl GdiCapture {
    fn new(source: RECT, width: i32, height: i32) -> Option<Self> {
        Some(Self {
            surface: DibSurface::new(width, height)?,
            source,
        })
    }

    fn grab(&mut self, desktop: &mut [u8]) -> bool {
        let (width, height) = (self.surface.width, self.surface.height);
        let (sw, sh) = (
            self.source.right - self.source.left,
            self.source.bottom - self.source.top,
        );
        let ok = unsafe {
            let screen = GetDC(HWND::default());
            let ok = if sw == width && sh == height {
                BitBlt(
                    self.surface.dc,
                    0,
                    0,
                    width,
                    height,
                    screen,
                    self.source.left,
                    self.source.top,
                    ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
                )
                .is_ok()
            } else {
                SetStretchBltMode(self.surface.dc, HALFTONE);
                let _ = SetBrushOrgEx(self.surface.dc, 0, 0, None);
                StretchBlt(
                    self.surface.dc,
                    0,
                    0,
                    width,
                    height,
                    screen,
                    self.source.left,
                    self.source.top,
                    sw,
                    sh,
                    ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
                )
                .as_bool()
            };
            ReleaseDC(HWND::default(), screen);
            let _ = GdiFlush();
            ok
        };
        if ok {
            desktop.copy_from_slice(self.surface.pixels());
        }
        ok
    }
}

/// DXGI Desktop Duplication of one output.
struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

impl DxgiCapture {
    fn open(device_name: &str, width: u32, height: u32) -> Result<Self, String> {
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| format!("DXGI factory: {e}"))?;
            let mut adapter_index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
                adapter_index += 1;
                let mut output_index = 0;
                while let Ok(output) = adapter.EnumOutputs(output_index) {
                    output_index += 1;
                    let Ok(description) = output.GetDesc() else {
                        continue;
                    };
                    if !utf16_trimmed(&description.DeviceName).eq_ignore_ascii_case(device_name) {
                        continue;
                    }
                    let mut device = None;
                    let mut context = None;
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        Some(&[
                            D3D_FEATURE_LEVEL_11_1,
                            D3D_FEATURE_LEVEL_11_0,
                            D3D_FEATURE_LEVEL_10_1,
                            D3D_FEATURE_LEVEL_10_0,
                        ]),
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )
                    .map_err(|e| format!("Direct3D 11 device: {e}"))?;
                    let (Some(device), Some(context)) = (device, context) else {
                        return Err("Direct3D 11 device unavailable".into());
                    };
                    let output: IDXGIOutput1 =
                        output.cast().map_err(|e| format!("IDXGIOutput1: {e}"))?;
                    let duplication = output
                        .DuplicateOutput(&device)
                        .map_err(|e| format!("DuplicateOutput: {e}"))?;
                    let mode = duplication.GetDesc();
                    if mode.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                        return Err(format!(
                            "unsupported desktop format {:?}",
                            mode.ModeDesc.Format
                        ));
                    }
                    if mode.Rotation != DXGI_MODE_ROTATION_IDENTITY
                        && mode.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
                    {
                        return Err("rotated output".into());
                    }
                    if (mode.ModeDesc.Width, mode.ModeDesc.Height) != (width, height) {
                        return Err(format!(
                            "output is {}x{}, stream expects {width}x{height}",
                            mode.ModeDesc.Width, mode.ModeDesc.Height
                        ));
                    }
                    return Ok(Self {
                        device,
                        context,
                        duplication,
                        staging: None,
                        width,
                        height,
                    });
                }
            }
            Err(format!("no DXGI output named {device_name}"))
        }
    }

    /// Waits up to `timeout_ms` for an update. Copies a new desktop image into
    /// `desktop` and returns true; returns false for timeouts and pointer-only
    /// updates. Errors (e.g. access lost) require reopening.
    fn next(&mut self, timeout_ms: u32, desktop: &mut [u8]) -> windows::core::Result<bool> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
            {
                Ok(()) => {}
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(error) => return Err(error),
            }
            // Map before releasing the frame so the copy is complete.
            let copied = self.copy_frame(&info, resource.as_ref(), desktop);
            let released = self.duplication.ReleaseFrame();
            let copied = copied?;
            released?;
            Ok(copied)
        }
    }

    unsafe fn copy_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<&IDXGIResource>,
        desktop: &mut [u8],
    ) -> windows::core::Result<bool> {
        let Some(resource) = resource else {
            return Ok(false);
        };
        if info.LastPresentTime == 0 {
            return Ok(false);
        }
        let texture: ID3D11Texture2D = resource.cast()?;
        if self.staging.is_none() {
            let mut description = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut description);
            let staging_description = D3D11_TEXTURE2D_DESC {
                Width: description.Width,
                Height: description.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: description.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            self.device
                .CreateTexture2D(&staging_description, None, Some(&mut staging))?;
            self.staging = staging;
        }
        let Some(staging) = self.staging.as_ref() else {
            return Ok(false);
        };
        self.context.CopyResource(staging, &texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let row = self.width as usize * 4;
        let pitch = mapped.RowPitch as usize;
        if !mapped.pData.is_null() && pitch >= row {
            let source = mapped.pData as *const u8;
            for (y, target) in desktop
                .chunks_exact_mut(row)
                .take(self.height as usize)
                .enumerate()
            {
                core::ptr::copy_nonoverlapping(source.add(y * pitch), target.as_mut_ptr(), row);
            }
        }
        self.context.Unmap(staging, 0);
        Ok(true)
    }
}

/// One output to stream: its `\\.\DISPLAYn` name, desktop rectangle and size.
pub(super) struct CaptureTarget {
    pub monitor: u8,
    pub device: String,
    pub rect: RECT,
    pub width: u16,
    pub height: u16,
}

/// Captures `target` into `slot` until `stop` is set.
pub(super) fn spawn_capture(
    target: CaptureTarget,
    slot: Arc<FrameSlot>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("duplex-capture-{}", target.monitor))
        .spawn(move || {
            if let Err(error) = capture_loop(&target, &slot, &stop) {
                warn!(monitor = target.monitor, %error, "Windows desktop capture stopped");
                if let Ok(mut failure) = failure.lock() {
                    *failure = Some(error);
                }
            }
            slot.close();
        })
}

fn capture_loop(target: &CaptureTarget, slot: &FrameSlot, stop: &AtomicBool) -> Result<(), String> {
    let CaptureTarget {
        monitor,
        device: ref device_name,
        rect,
        width,
        height,
    } = *target;
    let (w, h) = (i32::from(width), i32::from(height));
    let frame_len = usize::from(width) * usize::from(height) * 4;
    let interval = Duration::from_secs_f64(1.0 / f64::from(capture_fps_limit()));
    let origin = POINT {
        x: rect.left,
        y: rect.top,
    };
    let mut overlay = CursorOverlay::new();
    let mut desktop = vec![0u8; frame_len];
    let mut have_image = false;
    let mut last_key = None;
    let mut gdi: Option<GdiCapture> = None;
    let mut dxgi = match DxgiCapture::open(device_name, u32::from(width), u32::from(height)) {
        Ok(capture) => {
            info!(
                monitor,
                device = device_name,
                "Capturing with DXGI Desktop Duplication"
            );
            Some(capture)
        }
        Err(reason) => {
            warn!(monitor, device = device_name, %reason, "DXGI Desktop Duplication unavailable; using GDI capture");
            None
        }
    };
    let mut retry_dxgi_at = Instant::now() + DXGI_RETRY;
    let mut last_publish = Instant::now() - interval;
    let mut failing_since: Option<Instant> = None;
    while !stop.load(Ordering::Acquire) {
        let elapsed = last_publish.elapsed();
        if elapsed < interval {
            thread::sleep(interval - elapsed);
        }
        let mut grabbed = true;
        let new_image = if let Some(capture) = dxgi.as_mut() {
            match capture.next(100, &mut desktop) {
                Ok(updated) => updated,
                Err(error) => {
                    warn!(monitor, %error, "DXGI capture interrupted; retrying shortly");
                    dxgi = None;
                    retry_dxgi_at = Instant::now() + Duration::from_secs(1);
                    false
                }
            }
        } else {
            if Instant::now() >= retry_dxgi_at {
                retry_dxgi_at = Instant::now() + DXGI_RETRY;
                if let Ok(capture) =
                    DxgiCapture::open(device_name, u32::from(width), u32::from(height))
                {
                    info!(monitor, "DXGI Desktop Duplication available again");
                    dxgi = Some(capture);
                    gdi = None;
                    continue;
                }
            }
            if gdi.is_none() {
                gdi = GdiCapture::new(rect, w, h);
            }
            grabbed = gdi
                .as_mut()
                .is_some_and(|capture| capture.grab(&mut desktop));
            grabbed
        };
        // A secure desktop (UAC) blocks capture temporarily; only a vanished
        // output is fatal.
        if grabbed {
            failing_since = None;
        } else if failing_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5)
        {
            failing_since = Some(Instant::now());
            if !super::display::attached_virtual_outputs()
                .iter()
                .any(|(name, _)| name == device_name)
            {
                return Err(format!(
                    "virtual output {device_name} is no longer attached"
                ));
            }
        }
        have_image |= new_image;
        if !have_image {
            continue;
        }
        let mut frame = slot.buffer(frame_len);
        frame.copy_from_slice(&desktop);
        let key = overlay.draw(&mut frame, w, h, origin);
        if new_image || key != last_key {
            last_key = key;
            slot.publish(frame);
            last_publish = Instant::now();
        } else {
            slot.recycle(frame);
        }
    }
    Ok(())
}
