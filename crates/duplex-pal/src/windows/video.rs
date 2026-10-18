//! Windows 11 Built-In Low-Latency Video Pipeline (Win32 GDI `StretchBlt` Capture + `DPXB` Band Codec + Borderless Fullscreen Win32 Viewers).
//!
//! Implements `VideoPipelineBackend` on Windows 11 with zero external CLI dependencies:
//! - **Mode 1+ Sender (Extended Monitor -> Debian Laptop):** Captures the Windows desktop monitor
//!   at `960x540` via pure Win32 GDI (`GetDC`, `CreateCompatibleDC`, `CreateDIBSection`, `StretchBlt`),
//!   overlays the live hardware cursor, and encodes changed horizontal bands via `encode_bgrx_frame_to_bands` (`DPXB`).
//! - **Mode 2 Viewer (2 Physical Monitors <- Debian Laptop):** Enumerates all physical monitors via
//!   `EnumDisplayMonitors`, spawns borderless fullscreen Win32 viewer windows (`WS_POPUP | WS_VISIBLE`)
//!   covering Monitor 1 and Monitor 2, decodes incoming `DPXB` bands via `decode_band_into_bgrx`, and
//!   blits live frames via `StretchDIBits`.

use crate::shm::monotonic_now_ns;
use crate::{PalError, VideoPipelineBackend};
use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};
use duplex_proto::{
    decode_band_into_bgrx, encode_bgrx_frame_to_bands, CursorTelemetry, DisplayDescriptor,
    EncodedVideoFrame, DPXB_BAND_MAGIC, MAX_VIDEO_BITRATE_KBPS,
};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{debug, info};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
    GetMonitorInfoW, GetStockObject, ReleaseDC, SelectObject, SetStretchBltMode, StretchBlt,
    StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACK_BRUSH, COLORONCOLOR, DIB_RGB_COLORS,
    HBRUSH, HDC, HGDIOBJ, HMONITOR, MONITORINFO, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, PeekMessageW,
    RegisterClassW, SetCursorPos, ShowWindow, TranslateMessage, HMENU, MSG, PM_REMOVE, SW_SHOW,
    WINDOW_EX_STYLE, WM_CLOSE, WM_ERASEBKGND, WS_POPUP, WS_VISIBLE, WNDCLASSW,
};

const CAPTURE_WIDTH: u16 = 960;
const CAPTURE_HEIGHT: u16 = 540;
const CAPTURE_FPS: u32 = 30;

struct MonitorEntry {
    rect: RECT,
    is_primary: bool,
}

unsafe extern "system" fn monitor_enum_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _lprc: *mut RECT,
    dwdata: LPARAM,
) -> BOOL {
    let list = &mut *(dwdata.0 as *mut Vec<MonitorEntry>);
    let mut mi = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmonitor, &mut mi).as_bool() {
        let is_primary = (mi.dwFlags & 1) != 0;
        list.push(MonitorEntry {
            rect: mi.rcMonitor,
            is_primary,
        });
    }
    BOOL(1)
}

/// Enumerates all attached display monitor rectangles on Windows, ordered with the primary monitor first
/// followed by secondary/extended monitors sorted left-to-right.
fn enumerate_monitor_rects() -> Vec<RECT> {
    let mut entries: Vec<MonitorEntry> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(monitor_enum_proc),
            LPARAM(&mut entries as *mut Vec<MonitorEntry> as isize),
        );
    }
    entries.sort_by_key(|e| (!e.is_primary, e.rect.left));
    if entries.is_empty() {
        vec![RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }]
    } else {
        entries.into_iter().map(|e| e.rect).collect()
    }
}

struct WinEncoderWorker {
    monitor_index: u8,
    stop_flag: Arc<AtomicBool>,
    force_idr_flag: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
}

struct SharedViewerCanvas {
    bgrx: Vec<u8>,
    width: u16,
    height: u16,
    dirty: bool,
}

struct WinViewerHandle {
    monitor_index: u8,
    stop_flag: Arc<AtomicBool>,
    canvas: Arc<Mutex<SharedViewerCanvas>>,
    join_handle: Option<JoinHandle<()>>,
}

unsafe extern "system" fn viewer_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub struct WindowsVideoPipelineBackend {
    encoders: Vec<WinEncoderWorker>,
    frame_rx: Option<Receiver<EncodedVideoFrame>>,
    viewers: Vec<WinViewerHandle>,
    last_cursor_pos: (i32, i32),
}

impl Default for WindowsVideoPipelineBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsVideoPipelineBackend {
    pub const fn new() -> Self {
        Self {
            encoders: Vec::new(),
            frame_rx: None,
            viewers: Vec::new(),
            last_cursor_pos: (0, 0),
        }
    }

    fn spawn_gdi_encoder_thread(
        spec: DisplayDescriptor,
        tx: SyncSender<EncodedVideoFrame>,
    ) -> WinEncoderWorker {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let force_idr_flag = Arc::new(AtomicBool::new(true));

        let stop_clone = stop_flag.clone();
        let idr_clone = force_idr_flag.clone();
        let monitor_index = spec.index;

        let join_handle = thread::Builder::new()
            .name(format!("duplex-win-gdi-enc-{monitor_index}"))
            .spawn(move || unsafe {
                let mut frame_seq = 0u32;
                let frame_interval = Duration::from_millis(1000 / u64::from(CAPTURE_FPS));
                let cap_w = CAPTURE_WIDTH as i32;
                let cap_h = CAPTURE_HEIGHT as i32;
                let raw_frame_len = (cap_w as usize) * (cap_h as usize) * 4;
                let mut prev_rgb565: Vec<u16> = Vec::new();
                let mut cycle_counter: u32 = 0;
                let mut last_force_all_ns: u64 = 0;
                let mut encoded_batch: Vec<EncodedVideoFrame> = Vec::with_capacity(32);

                let screen_dc = GetDC(HWND::default());
                let mem_dc = CreateCompatibleDC(screen_dc);
                let _ = SetStretchBltMode(mem_dc, COLORONCOLOR);

                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: cap_w,
                        biHeight: -cap_h, // Top-down BGRx DIB
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        biSizeImage: raw_frame_len as u32,
                        ..Default::default()
                    },
                    ..Default::default()
                };

                let mut bits_ptr: *mut c_void = ptr::null_mut();
                let hbmp_res =
                    CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0);
                let hbmp = hbmp_res.unwrap_or_default();
                let old_obj = if !hbmp.is_invalid() {
                    SelectObject(mem_dc, HGDIOBJ(hbmp.0))
                } else {
                    HGDIOBJ::default()
                };

                let mut raw_buf = vec![0u8; raw_frame_len];

                while !stop_clone.load(Ordering::Relaxed) {
                    let rects = enumerate_monitor_rects();
                    // In Mode 1+ (extend display), stream the secondary/extended monitor if 2+ monitors exist,
                    // or the primary monitor if only 1 monitor is present.
                    let target_rect = if rects.len() >= 2 {
                        rects[(monitor_index as usize + 1).min(rects.len() - 1)]
                    } else {
                        rects[0]
                    };
                    let src_x = target_rect.left;
                    let src_y = target_rect.top;
                    let src_w = (target_rect.right - target_rect.left).max(64);
                    let src_h = (target_rect.bottom - target_rect.top).max(64);

                    if !bits_ptr.is_null() {
                        let _ = StretchBlt(
                            mem_dc,
                            0,
                            0,
                            cap_w,
                            cap_h,
                            screen_dc,
                            src_x,
                            src_y,
                            src_w,
                            src_h,
                            SRCCOPY,
                        );
                        ptr::copy_nonoverlapping(
                            bits_ptr as *const u8,
                            raw_buf.as_mut_ptr(),
                            raw_frame_len,
                        );

                        // Overlay hardware cursor position onto the captured BGRx canvas
                        let mut pt = POINT::default();
                        if GetCursorPos(&mut pt).is_ok()
                            && pt.x >= src_x
                            && pt.x < src_x + src_w
                            && pt.y >= src_y
                            && pt.y < src_y + src_h
                        {
                            let cx = ((pt.x - src_x) * cap_w / src_w).clamp(0, cap_w - 1);
                            let cy = ((pt.y - src_y) * cap_h / src_h).clamp(0, cap_h - 1);
                            for dy in -5i32..=5i32 {
                                for dx in -5i32..=5i32 {
                                    let px = cx + dx;
                                    let py = cy + dy;
                                    if px >= 0 && px < cap_w && py >= 0 && py < cap_h {
                                        let off = ((py as usize) * (cap_w as usize) + (px as usize)) * 4;
                                        let is_border =
                                            dx.abs() == 5 || dy.abs() == 5 || (dx == 0 && dy == 0);
                                        let c = if is_border { 0x00u8 } else { 0xFFu8 };
                                        raw_buf[off] = c;
                                        raw_buf[off + 1] = c;
                                        raw_buf[off + 2] = c;
                                    }
                                }
                            }
                        }
                    }

                    let now_ns = monotonic_now_ns();
                    let req_idr = idr_clone.swap(false, Ordering::AcqRel);
                    let force_all =
                        req_idr && now_ns.saturating_sub(last_force_all_ns) >= 500_000_000;
                    if force_all {
                        last_force_all_ns = now_ns;
                    }

                    encoded_batch.clear();
                    encode_bgrx_frame_to_bands(
                        monitor_index,
                        CAPTURE_WIDTH,
                        CAPTURE_HEIGHT,
                        &raw_buf,
                        &mut prev_rgb565,
                        force_all,
                        cycle_counter,
                        &mut frame_seq,
                        now_ns,
                        &mut encoded_batch,
                    );
                    cycle_counter = cycle_counter.wrapping_add(1);

                    let band_count = encoded_batch.len();
                    for frame in encoded_batch.drain(..) {
                        let _ = tx.send(frame);
                        if band_count > 6 {
                            thread::sleep(Duration::from_micros(150));
                        }
                    }

                    thread::sleep(frame_interval);
                }

                if !old_obj.is_invalid() {
                    let _ = SelectObject(mem_dc, old_obj);
                }
                if !hbmp.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(hbmp.0));
                }
                let _ = DeleteDC(mem_dc);
                let _ = ReleaseDC(HWND::default(), screen_dc);
            })
            .expect("Failed to spawn Windows GDI screen capture thread");

        WinEncoderWorker {
            monitor_index,
            stop_flag,
            force_idr_flag,
            join_handle: Some(join_handle),
        }
    }

    fn spawn_win32_viewer_thread(monitor_index: u8, target_rect: RECT) -> WinViewerHandle {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let canvas = Arc::new(Mutex::new(SharedViewerCanvas {
            bgrx: vec![0u8; (CAPTURE_WIDTH as usize) * (CAPTURE_HEIGHT as usize) * 4],
            width: CAPTURE_WIDTH,
            height: CAPTURE_HEIGHT,
            dirty: true,
        }));
        let canvas_clone = canvas.clone();

        let join_handle = thread::Builder::new()
            .name(format!("duplex-win-viewer-{monitor_index}"))
            .spawn(move || unsafe {
                let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
                let class_name: Vec<u16> = "DuplexKvmFullscreenViewerClass\0"
                    .encode_utf16()
                    .collect();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(viewer_wnd_proc),
                    hInstance: hinstance,
                    lpszClassName: PCWSTR(class_name.as_ptr()),
                    hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                    ..Default::default()
                };
                let _ = RegisterClassW(&wc);

                let title: Vec<u16> = format!(
                    "Duplex-KVM — Debian Laptop Display {} (Mode 2)\0",
                    monitor_index + 1
                )
                .encode_utf16()
                .collect();

                let win_x = target_rect.left;
                let win_y = target_rect.top;
                let win_w = (target_rect.right - target_rect.left).max(640);
                let win_h = (target_rect.bottom - target_rect.top).max(480);

                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    PCWSTR(class_name.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    WS_POPUP | WS_VISIBLE,
                    win_x,
                    win_y,
                    win_w,
                    win_h,
                    HWND::default(),
                    HMENU::default(),
                    hinstance,
                    None,
                )
                .unwrap_or_default();

                if !hwnd.0.is_null() {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                }

                let mut msg = MSG::default();
                let mut local_bgrx: Vec<u8> = Vec::new();

                while !stop_clone.load(Ordering::Relaxed) {
                    while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_CLOSE {
                            stop_clone.store(true, Ordering::Release);
                            break;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }

                    let mut frame_dims: Option<(u16, u16)> = None;
                    if let Ok(mut guard) = canvas_clone.lock() {
                        if guard.dirty {
                            guard.dirty = false;
                            local_bgrx.resize(guard.bgrx.len(), 0);
                            local_bgrx.copy_from_slice(&guard.bgrx);
                            frame_dims = Some((guard.width, guard.height));
                        }
                    }

                    if let Some((fw, fh)) = frame_dims {
                        if !hwnd.0.is_null() && fw > 0 && fh > 0 {
                            let hdc = GetDC(hwnd);
                            let _ = SetStretchBltMode(hdc, COLORONCOLOR);
                            let bmi = BITMAPINFO {
                                bmiHeader: BITMAPINFOHEADER {
                                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                                    biWidth: i32::from(fw),
                                    biHeight: -i32::from(fh), // Top-down BGRx
                                    biPlanes: 1,
                                    biBitCount: 32,
                                    biCompression: BI_RGB.0,
                                    biSizeImage: (fw as u32) * (fh as u32) * 4,
                                    ..Default::default()
                                },
                                ..Default::default()
                            };
                            let _ = StretchDIBits(
                                hdc,
                                0,
                                0,
                                win_w,
                                win_h,
                                0,
                                0,
                                i32::from(fw),
                                i32::from(fh),
                                Some(local_bgrx.as_ptr() as *const c_void),
                                &bmi,
                                DIB_RGB_COLORS,
                                SRCCOPY,
                            );
                            let _ = ReleaseDC(hwnd, hdc);
                        }
                    }

                    thread::sleep(Duration::from_millis(8));
                }

                if !hwnd.0.is_null() {
                    let _ = DestroyWindow(hwnd);
                }
            })
            .expect("Failed to spawn Windows fullscreen viewer thread");

        WinViewerHandle {
            monitor_index,
            stop_flag,
            canvas,
            join_handle: Some(join_handle),
        }
    }
}

impl VideoPipelineBackend for WindowsVideoPipelineBackend {
    fn start_sender(
        &mut self,
        monitors: &[DisplayDescriptor],
        max_bitrate_kbps: u32,
    ) -> Result<(), PalError> {
        self.close_all_viewer_windows()?;
        if monitors.is_empty() {
            return Ok(());
        }

        let capped_bitrate = max_bitrate_kbps.min(MAX_VIDEO_BITRATE_KBPS);
        let (tx, rx) = mpsc::sync_channel::<EncodedVideoFrame>(64);
        self.frame_rx = Some(rx);

        for spec in monitors {
            let worker = Self::spawn_gdi_encoder_thread(spec.clone(), tx.clone());
            self.encoders.push(worker);
        }

        info!(
            streams = self.encoders.len(),
            bitrate_kbps = capped_bitrate,
            "Started Windows 11 built-in GDI screen capture + DPXB band video sender"
        );
        Ok(())
    }

    fn poll_encoded_frames(
        &mut self,
        out_frames: &mut Vec<EncodedVideoFrame>,
        out_cursor: &mut Option<CursorTelemetry>,
    ) -> Result<usize, PalError> {
        let mut count = 0usize;
        if let Some(ref rx) = self.frame_rx {
            while count < 32 {
                match rx.try_recv() {
                    Ok(frame) => {
                        out_frames.push(frame);
                        count += 1;
                    }
                    Err(_) => break,
                }
            }
        }

        if !self.encoders.is_empty() {
            let mut pt = POINT::default();
            if unsafe { GetCursorPos(&mut pt).is_ok() } && (pt.x, pt.y) != self.last_cursor_pos {
                self.last_cursor_pos = (pt.x, pt.y);
                *out_cursor = Some(CursorTelemetry {
                    monitor_index: 0,
                    x: pt.x,
                    y: pt.y,
                    visible: true,
                    shape: None,
                });
            } else {
                *out_cursor = None;
            }
        } else {
            *out_cursor = None;
        }

        Ok(count)
    }

    fn request_keyframe(&mut self, monitor_index: u8) -> Result<(), PalError> {
        for enc in &self.encoders {
            if enc.monitor_index == monitor_index {
                enc.force_idr_flag.store(true, Ordering::Release);
                debug!(
                    monitor_index,
                    "Scheduled immediate Windows full-frame DPXB refresh"
                );
            }
        }
        Ok(())
    }

    fn start_viewer(&mut self, monitors: &[DisplayDescriptor]) -> Result<(), PalError> {
        self.close_all_viewer_windows()?;

        let rects = enumerate_monitor_rects();
        for spec in monitors {
            let idx = spec.index as usize;
            let target_rect = if idx < rects.len() {
                rects[idx]
            } else {
                // If fewer physical monitors exist than requested streams, tile on primary monitor
                let primary = rects[0];
                let pw = (primary.right - primary.left) / 2;
                RECT {
                    left: primary.left + (idx as i32 % 2) * pw,
                    top: primary.top,
                    right: primary.left + ((idx as i32 % 2) + 1) * pw,
                    bottom: primary.bottom,
                }
            };
            let handle = Self::spawn_win32_viewer_thread(spec.index, target_rect);
            self.viewers.push(handle);
        }

        info!(
            requested_viewers = monitors.len(),
            spawned_windows = self.viewers.len(),
            "Initialized Windows 11 native borderless fullscreen viewer windows for Mode 2"
        );
        Ok(())
    }

    fn submit_encoded_frame(&mut self, frame: &EncodedVideoFrame) -> Result<(), PalError> {
        if frame.data.len() < 12 || frame.data[0..4] != DPXB_BAND_MAGIC {
            return Ok(());
        }

        for viewer in &mut self.viewers {
            if viewer.monitor_index == frame.monitor_index {
                if let Ok(mut guard) = viewer.canvas.lock() {
                    let mut w = guard.width;
                    let mut h = guard.height;
                    if decode_band_into_bgrx(&frame.data, &mut w, &mut h, &mut guard.bgrx) {
                        guard.width = w;
                        guard.height = h;
                        guard.dirty = true;
                    }
                }
            }
        }
        Ok(())
    }

    fn update_hardware_cursor(&mut self, cursor: &CursorTelemetry) -> Result<(), PalError> {
        if cursor.visible {
            let x_offset = i32::from(cursor.monitor_index) * 1920;
            unsafe {
                let _ = SetCursorPos(x_offset + cursor.x, cursor.y);
            }
        }
        Ok(())
    }

    fn close_all_viewer_windows(&mut self) -> Result<(), PalError> {
        for mut enc in self.encoders.drain(..) {
            enc.stop_flag.store(true, Ordering::Release);
            if let Some(handle) = enc.join_handle.take() {
                let _ = handle.join();
            }
        }
        self.frame_rx = None;

        for mut viewer in self.viewers.drain(..) {
            viewer.stop_flag.store(true, Ordering::Release);
            if let Some(handle) = viewer.join_handle.take() {
                let _ = handle.join();
            }
        }

        info!("Closed all Windows GDI encoders and borderless fullscreen viewer windows");
        Ok(())
    }
}

impl Drop for WindowsVideoPipelineBackend {
    fn drop(&mut self) {
        let _ = self.close_all_viewer_windows();
    }
}
