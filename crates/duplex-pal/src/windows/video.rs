//! Windows 11 screen streaming.
//!
//! - **Sender (Mode 1+):** the Duplex-owned virtual output is captured with
//!   DXGI Desktop Duplication (GDI fallback), the real cursor is composited,
//!   and the shared encoder codes DPXT tiles on the low-priority codec pool.
//! - **Viewer (Mode 2):** one borderless topmost window per physical monitor.
//!   A decoder thread copies changed regions into the window's front buffer and
//!   invalidates them; the window thread repaints at native resolution (1:1
//!   when sizes match, otherwise aspect-preserving HALFTONE scaling). The local
//!   cursor is hidden over viewers while Windows input drives the laptop.

use super::capture::{spawn_capture, CaptureTarget};
use crate::stream::{
    encoder_config_from_env, spawn_encoder, EncoderControl, EncoderSpec, FramePresenter, FrameSlot,
    StreamCodec, ViewerSet,
};
use crate::{PalError, PeerVideoCaps, VideoFrameConsumer, VideoPipelineBackend, VideoSink};
use core::ffi::c_void;
use core::mem::size_of;
use duplex_proto::{DisplayDescriptor, PixelRect};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, FillRect, GetStockObject, InvalidateRect, SetBrushOrgEx,
    SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACK_BRUSH,
    COLORONCOLOR, DIB_RGB_COLORS, HALFTONE, HBRUSH, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, IsWindow, PostMessageW, PostQuitMessage, RegisterClassW,
    SetCursor, SetCursorPos, SetWindowLongPtrW, SetWindowPos, ShowCursor, ShowWindow,
    TranslateMessage, CREATESTRUCTW, GWLP_USERDATA, HCURSOR, HMENU, HWND_TOPMOST, MSG,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_SHOWNA, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_NCCREATE,
    WM_PAINT, WM_SETCURSOR, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

const VIEWER_CLASS: PCWSTR = windows::core::w!("DuplexKvmDisplayViewer");
/// Above this many changed regions, invalidate the whole window instead.
const MAX_INVALIDATE_RECTS: usize = 64;

fn enumerate_monitor_rects() -> Vec<RECT> {
    super::display::physical_monitor_rects()
}

/// Latest presented frame, shared by a decoder thread and a window thread.
#[derive(Default)]
struct FrontBuffer {
    pixels: Vec<u8>,
    width: u16,
    height: u16,
}

struct ViewerWindowState {
    front: Mutex<FrontBuffer>,
    alive: AtomicBool,
}

unsafe fn window_state<'a>(hwnd: HWND) -> Option<&'a ViewerWindowState> {
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ViewerWindowState;
    pointer.as_ref()
}

unsafe fn paint(hwnd: HWND) {
    let mut paint = PAINTSTRUCT::default();
    let dc = BeginPaint(hwnd, &mut paint);
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
    let front = window_state(hwnd).and_then(|state| state.front.lock().ok());
    match front.as_deref() {
        Some(front) if front.width > 0 && front.height > 0 => {
            let (fw, fh) = (i32::from(front.width), i32::from(front.height));
            let (cw, ch) = (client.right - client.left, client.bottom - client.top);
            let (dw, dh) = if (fw, fh) == (cw, ch) {
                (fw, fh)
            } else {
                // Preserve the aspect ratio; letterbox the remainder.
                let scale = (f64::from(cw) / f64::from(fw)).min(f64::from(ch) / f64::from(fh));
                (
                    (f64::from(fw) * scale).round() as i32,
                    (f64::from(fh) * scale).round() as i32,
                )
            };
            let (dx, dy) = ((cw - dw) / 2, (ch - dh) / 2);
            for border in [
                RECT {
                    left: 0,
                    top: 0,
                    right: cw,
                    bottom: dy,
                },
                RECT {
                    left: 0,
                    top: dy + dh,
                    right: cw,
                    bottom: ch,
                },
                RECT {
                    left: 0,
                    top: dy,
                    right: dx,
                    bottom: dy + dh,
                },
                RECT {
                    left: dx + dw,
                    top: dy,
                    right: cw,
                    bottom: dy + dh,
                },
            ] {
                if border.right > border.left && border.bottom > border.top {
                    FillRect(dc, &border, black);
                }
            }
            if (dw, dh) == (fw, fh) {
                SetStretchBltMode(dc, COLORONCOLOR);
            } else {
                SetStretchBltMode(dc, HALFTONE);
                let _ = SetBrushOrgEx(dc, 0, 0, None);
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: fw,
                    biHeight: -fh,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            // BeginPaint clipped the DC to the invalid region, so only
            // changed areas are actually transferred.
            StretchDIBits(
                dc,
                dx,
                dy,
                dw,
                dh,
                0,
                0,
                fw,
                fh,
                Some(front.pixels.as_ptr().cast::<c_void>()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
        _ => {
            FillRect(dc, &client, black);
        }
    }
    drop(front);
    let _ = EndPaint(hwnd, &paint);
}

unsafe extern "system" fn viewer_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SETCURSOR => {
            SetCursor(HCURSOR::default());
            LRESULT(1)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

struct ViewerWindow {
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
}

impl ViewerWindow {
    fn close(mut self) {
        unsafe {
            let _ = PostMessageW(
                HWND(self.hwnd as *mut c_void),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Creates a viewer window covering `rect` on its own message thread.
fn open_viewer_window(
    monitor: u8,
    rect: RECT,
) -> Result<(ViewerWindow, Arc<ViewerWindowState>), PalError> {
    let state = Arc::new(ViewerWindowState {
        front: Mutex::new(FrontBuffer::default()),
        alive: AtomicBool::new(true),
    });
    let thread_state = state.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<isize, String>>(1);
    let thread = thread::Builder::new()
        .name(format!("duplex-viewer-{monitor}"))
        .spawn(move || unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
            let class = WNDCLASSW {
                lpfnWndProc: Some(viewer_wnd_proc),
                hInstance: instance,
                lpszClassName: VIEWER_CLASS,
                ..Default::default()
            };
            // A second viewer finds the class already registered.
            RegisterClassW(&class);
            let title: Vec<u16> = format!("Duplex — laptop display {}\0", monitor + 1)
                .encode_utf16()
                .collect();
            let (width, height) = (
                (rect.right - rect.left).max(64),
                (rect.bottom - rect.top).max(64),
            );
            let created = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                VIEWER_CLASS,
                PCWSTR(title.as_ptr()),
                WS_POPUP | WS_VISIBLE,
                rect.left,
                rect.top,
                width,
                height,
                HWND::default(),
                HMENU::default(),
                instance,
                Some(Arc::as_ptr(&thread_state).cast::<c_void>()),
            );
            let hwnd = match created {
                Ok(hwnd) if !hwnd.0.is_null() => hwnd,
                Ok(_) => {
                    thread_state.alive.store(false, Ordering::Release);
                    let _ = ready_tx.send(Err("CreateWindowExW returned no window".into()));
                    return;
                }
                Err(error) => {
                    thread_state.alive.store(false, Ordering::Release);
                    let _ = ready_tx.send(Err(format!("CreateWindowExW failed: {error}")));
                    return;
                }
            };
            let _ = ShowWindow(hwnd, SW_SHOWNA);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                rect.left,
                rect.top,
                width,
                height,
                SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
            // Hide the frozen local cursor while it rests over this thread's windows.
            ShowCursor(false);
            let _ = ready_tx.send(Ok(hwnd.0 as isize));
            let mut message = MSG::default();
            while GetMessageW(&mut message, HWND::default(), 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            if IsWindow(hwnd).as_bool() {
                let _ = DestroyWindow(hwnd);
            }
            thread_state.alive.store(false, Ordering::Release);
        })
        .map_err(|e| PalError::VideoPipeline(format!("Cannot start viewer window thread: {e}")))?;
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(hwnd)) => Ok((
            ViewerWindow {
                hwnd,
                thread: Some(thread),
            },
            state,
        )),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(PalError::VideoPipeline(error))
        }
        Err(_) => Err(PalError::VideoPipeline(
            "Viewer window startup timed out".into(),
        )),
    }
}

/// Decoder-side handle that updates a viewer window's front buffer.
struct WindowPresenter {
    monitor: u8,
    hwnd: isize,
    client: (i32, i32),
    state: Arc<ViewerWindowState>,
}

impl FramePresenter for WindowPresenter {
    fn present(
        &mut self,
        width: u16,
        height: u16,
        canvas: &[u8],
        dirty: &[PixelRect],
    ) -> Result<(), PalError> {
        if !self.state.alive.load(Ordering::Acquire) {
            return Err(PalError::VideoPipeline(format!(
                "Viewer window {} closed",
                self.monitor
            )));
        }
        let stride = usize::from(width) * 4;
        let length = stride * usize::from(height);
        if canvas.len() < length {
            return Err(PalError::VideoPipeline(
                "Decoded frame is smaller than its size".into(),
            ));
        }
        let resized = {
            let mut front = self
                .state
                .front
                .lock()
                .map_err(|_| PalError::VideoPipeline("Viewer frame lock poisoned".into()))?;
            let resized = (front.width, front.height) != (width, height);
            if resized {
                front.pixels.clear();
                front.pixels.extend_from_slice(&canvas[..length]);
                (front.width, front.height) = (width, height);
            } else {
                for rect in dirty {
                    let (x, w) = (rect.x as usize * 4, rect.width as usize * 4);
                    for row in rect.y as usize..(rect.y + rect.height) as usize {
                        let at = row * stride + x;
                        front.pixels[at..at + w].copy_from_slice(&canvas[at..at + w]);
                    }
                }
            }
            resized
        };
        let hwnd = HWND(self.hwnd as *mut c_void);
        let one_to_one = self.client == (i32::from(width), i32::from(height));
        unsafe {
            if resized || !one_to_one || dirty.len() > MAX_INVALIDATE_RECTS {
                let _ = InvalidateRect(hwnd, None, false);
            } else {
                for rect in dirty {
                    let area = RECT {
                        left: rect.x as i32,
                        top: rect.y as i32,
                        right: (rect.x + rect.width) as i32,
                        bottom: (rect.y + rect.height) as i32,
                    };
                    let _ = InvalidateRect(hwnd, Some(&area), false);
                }
            }
        }
        Ok(())
    }
}

struct WinSender {
    monitor: u8,
    control: Arc<EncoderControl>,
    capture_stop: Arc<AtomicBool>,
    capture_failure: Arc<Mutex<Option<String>>>,
    capture: Option<JoinHandle<()>>,
    encoder: Option<JoinHandle<()>>,
    reported: bool,
}

impl WinSender {
    fn stop(mut self) {
        self.control.stop();
        self.capture_stop.store(true, Ordering::Release);
        for handle in [self.capture.take(), self.encoder.take()]
            .into_iter()
            .flatten()
        {
            let _ = handle.join();
        }
    }
}

pub struct WindowsVideoPipelineBackend {
    caps: PeerVideoCaps,
    senders: Vec<WinSender>,
    viewers: Option<ViewerSet>,
    windows: Vec<ViewerWindow>,
}

impl Default for WindowsVideoPipelineBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsVideoPipelineBackend {
    pub const fn new() -> Self {
        Self {
            caps: PeerVideoCaps {
                native_lossless: false,
                tiles: false,
            },
            senders: Vec::new(),
            viewers: None,
            windows: Vec::new(),
        }
    }

    fn start_stream(
        &mut self,
        spec: &DisplayDescriptor,
        device: String,
        rect: RECT,
        sink: Arc<dyn VideoSink>,
    ) -> Result<(), PalError> {
        let width = spec.width_px.clamp(16, 7680) as u16;
        let height = spec.height_px.clamp(16, 4320) as u16;
        let slot = FrameSlot::new();
        let capture_stop = Arc::new(AtomicBool::new(false));
        let capture_failure = Arc::new(Mutex::new(None));
        let target = CaptureTarget {
            monitor: spec.index,
            device,
            rect,
            width,
            height,
        };
        let capture = spawn_capture(
            target,
            slot.clone(),
            capture_stop.clone(),
            capture_failure.clone(),
        )
        .map_err(|e| PalError::VideoPipeline(format!("Cannot start desktop capture: {e}")))?;
        let control = EncoderControl::new(StreamCodec::for_peer(self.caps));
        let mut sender = WinSender {
            monitor: spec.index,
            control: control.clone(),
            capture_stop,
            capture_failure,
            capture: Some(capture),
            encoder: None,
            reported: false,
        };
        match spawn_encoder(
            EncoderSpec {
                monitor: spec.index,
                width,
                height,
            },
            slot,
            sink,
            control,
            encoder_config_from_env(),
        ) {
            Ok(encoder) => {
                sender.encoder = Some(encoder);
                self.senders.push(sender);
                Ok(())
            }
            Err(error) => {
                sender.stop();
                Err(error)
            }
        }
    }
}

impl VideoPipelineBackend for WindowsVideoPipelineBackend {
    fn set_peer_video_caps(&mut self, caps: PeerVideoCaps) {
        if self.caps != caps {
            info!(?caps, "Peer video capabilities changed");
        }
        self.caps = caps;
        for sender in &self.senders {
            sender.control.set_codec(StreamCodec::for_peer(caps));
        }
    }

    fn start_sender(
        &mut self,
        monitors: &[DisplayDescriptor],
        sink: Arc<dyn VideoSink>,
    ) -> Result<(), PalError> {
        self.close_all_viewer_windows()?;
        if monitors.is_empty() {
            return Ok(());
        }
        let targets = super::display::attached_virtual_outputs();
        if targets.len() < monitors.len() {
            return Err(PalError::VideoPipeline("Mode 1+ requires a real attached virtual display; existing physical monitors are not capture substitutes.".into()));
        }
        for (spec, (device, rect)) in monitors.iter().zip(targets) {
            if let Err(error) = self.start_stream(spec, device, rect, sink.clone()) {
                let _ = self.close_all_viewer_windows();
                return Err(error);
            }
        }
        info!(streams = self.senders.len(), codec = ?StreamCodec::for_peer(self.caps), "Started Windows virtual-display streaming");
        Ok(())
    }

    fn request_keyframe(&mut self, monitor_index: u8) -> Result<(), PalError> {
        for sender in self.senders.iter().filter(|s| s.monitor == monitor_index) {
            sender.control.request_refresh();
        }
        Ok(())
    }

    fn start_viewer(
        &mut self,
        monitors: &[DisplayDescriptor],
    ) -> Result<Arc<dyn VideoFrameConsumer>, PalError> {
        self.close_all_viewer_windows()?;
        let rects = enumerate_monitor_rects();
        if rects.is_empty() {
            return Err(PalError::VideoPipeline(
                "No physical Windows viewer monitors are attached.".into(),
            ));
        }
        let mut presenters: Vec<(u8, Box<dyn FramePresenter>)> = Vec::new();
        for spec in monitors {
            let index = usize::from(spec.index);
            let target = rects.get(index).copied().unwrap_or_else(|| {
                // Fewer physical monitors than streams: tile on the primary monitor.
                let primary = rects[0];
                let half = (primary.right - primary.left) / 2;
                let column = (index % 2) as i32;
                RECT {
                    left: primary.left + column * half,
                    top: primary.top,
                    right: primary.left + (column + 1) * half,
                    bottom: primary.bottom,
                }
            });
            match open_viewer_window(spec.index, target) {
                Ok((window, state)) => {
                    presenters.push((
                        spec.index,
                        Box::new(WindowPresenter {
                            monitor: spec.index,
                            hwnd: window.hwnd,
                            client: (target.right - target.left, target.bottom - target.top),
                            state,
                        }),
                    ));
                    self.windows.push(window);
                }
                Err(error) => {
                    let _ = self.close_all_viewer_windows();
                    return Err(error);
                }
            }
        }
        // An injected no-op move makes Windows ask the window under the
        // frozen cursor for its (empty) cursor image.
        unsafe {
            let mut position = POINT::default();
            if GetCursorPos(&mut position).is_ok() {
                let _ = SetCursorPos(position.x, position.y);
            }
        }
        let (viewers, consumer) = match ViewerSet::spawn(presenters) {
            Ok(spawned) => spawned,
            Err(error) => {
                let _ = self.close_all_viewer_windows();
                return Err(error);
            }
        };
        self.viewers = Some(viewers);
        info!(
            viewers = self.windows.len(),
            "Opened Windows fullscreen display viewers"
        );
        Ok(consumer)
    }

    fn poll_health(&mut self, refresh_requests: &mut Vec<u8>) -> Result<(), PalError> {
        if let Some(viewers) = &self.viewers {
            viewers.take_refresh_requests(refresh_requests);
            if let Some(failure) = viewers.failure() {
                return Err(PalError::VideoPipeline(failure));
            }
        }
        for sender in &mut self.senders {
            if sender.reported || !sender.control.finished() {
                continue;
            }
            sender.reported = true;
            if let Some(failure) = sender.capture_failure.lock().ok().and_then(|f| f.clone()) {
                return Err(PalError::VideoPipeline(format!(
                    "Desktop capture for display {} stopped: {failure}",
                    sender.monitor + 1
                )));
            }
        }
        Ok(())
    }

    fn close_all_viewer_windows(&mut self) -> Result<(), PalError> {
        for sender in self.senders.drain(..) {
            sender.stop();
        }
        if let Some(viewers) = self.viewers.take() {
            viewers.stop();
        }
        for window in self.windows.drain(..) {
            window.close();
        }
        Ok(())
    }
}

impl Drop for WindowsVideoPipelineBackend {
    fn drop(&mut self) {
        if let Err(error) = self.close_all_viewer_windows() {
            warn!(%error, "Video teardown failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::SlotWait;
    use duplex_proto::{TileEncoder, TileEncoderConfig};
    use std::time::Instant;

    fn pattern(width: usize, height: usize) -> Vec<u8> {
        let mut frame = vec![0u8; width * height * 4];
        for (i, pixel) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % width, i / width);
            let colour = match (x * 2 / width, y * 2 / height) {
                (0, 0) => [0, 0, 255, 255],
                (1, 0) => [0, 255, 0, 255],
                (0, _) => [255, 0, 0, 255],
                _ => {
                    // Single-pixel checkerboard: any resampling would blur it.
                    let on = (x + y) % 2 == 0;
                    if on {
                        [255, 255, 255, 255]
                    } else {
                        [16, 16, 16, 255]
                    }
                }
            };
            *pixel = colour;
        }
        frame
    }

    /// Opens a fullscreen viewer, streams a pattern through the decoder, then
    /// captures the screen (DXGI, or GDI where duplication is unavailable)
    /// and requires pixel-exact output.
    #[test]
    #[ignore = "opens windows; run on a Windows desktop or under Wine (xvfb-run wine)"]
    fn viewer_presents_exact_pixels_and_capture_reads_them_back() {
        let rects = enumerate_monitor_rects();
        let primary = *rects.first().expect("a monitor");
        let (width, height) = (
            (primary.right - primary.left) as u16,
            (primary.bottom - primary.top) as u16,
        );
        let spec = DisplayDescriptor {
            index: 0,
            width_px: u32::from(width),
            height_px: u32::from(height),
            refresh_hz: 60,
            scale_percent: 100,
            physical_width_mm: 500,
            physical_height_mm: 300,
            edid_block: DisplayDescriptor::synthesize_edid(
                u32::from(width),
                u32::from(height),
                60,
                0,
            ),
        };
        let mut backend = WindowsVideoPipelineBackend::new();
        let consumer = backend.start_viewer(&[spec]).unwrap();
        let frame = pattern(width.into(), height.into());
        let cfg = TileEncoderConfig {
            lossless_only: true,
            ..Default::default()
        };
        let mut encoder = TileEncoder::new(0, width, height, cfg, 77);
        let mut units = Vec::new();
        encoder.encode_frame(&frame, 1, &mut units);
        for unit in units {
            consumer.submit(unit);
        }
        thread::sleep(Duration::from_millis(1500));
        let mut refresh = Vec::new();
        backend.poll_health(&mut refresh).unwrap();
        assert!(refresh.is_empty(), "no loss expected: {refresh:?}");

        let slot = FrameSlot::new();
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let target = CaptureTarget {
            monitor: 0,
            device: r"\\.\DISPLAY1".into(),
            rect: primary,
            width,
            height,
        };
        let capture = spawn_capture(target, slot.clone(), stop.clone(), failure.clone()).unwrap();
        let started = Instant::now();
        let captured = loop {
            match slot.wait(Duration::from_millis(200)) {
                SlotWait::Frame(captured) => break captured,
                _ => assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "no captured frame"
                ),
            }
        };
        stop.store(true, Ordering::Release);
        capture.join().unwrap();
        assert!(failure.lock().unwrap().is_none());
        assert_eq!(captured.len(), frame.len());
        // Compare every pixel except a square around the composited cursor.
        let mut cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
        }
        let (w, h) = (usize::from(width), usize::from(height));
        let mut mismatched = 0usize;
        let mut compared = 0usize;
        for y in 0..h {
            for x in 0..w {
                let near_cursor = (x as i32 - (cursor.x - primary.left)).abs() < 300
                    && (y as i32 - (cursor.y - primary.top)).abs() < 300;
                if near_cursor {
                    continue;
                }
                let i = (y * w + x) * 4;
                compared += 1;
                if captured[i..i + 3] != frame[i..i + 3] {
                    mismatched += 1;
                }
            }
        }
        backend.close_all_viewer_windows().unwrap();
        assert!(compared > w * h / 2);
        assert_eq!(mismatched, 0, "{mismatched} of {compared} pixels differ");
    }
}
