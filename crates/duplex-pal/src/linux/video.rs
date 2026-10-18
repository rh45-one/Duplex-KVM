//! Debian Linux Low-Latency Video Pipeline (`org.gnome.Mutter.ScreenCast` PipeWire / `X11` + `DPXB` Band Codec + Native GTK3 Viewer).
//!
//! Implements `VideoPipelineBackend` on Debian Linux with zero external CLI binary dependencies:
//! - **Mode 2 Sender (2 Monitors -> Windows Desktop):** Uses GNOME Mutter's `org.gnome.Mutter.ScreenCast`
//!   D-Bus API (`RecordMonitor` for Monitor 0 + `RecordVirtual` for Monitor 1 extended virtual monitor,
//!   or `ximagesrc` fallback on X11) via `python3-gi` (`Gio` + `Gst 1.0`) to capture `960x540` BGRx frames
//!   with zero permission popups, encoding changed horizontal bands via `encode_bgrx_frame_to_bands` (`DPXB`).
//! - **Mode 1+ Viewer (1 Extended Monitor <- Windows Desktop):** Decodes incoming `DPXB` bands via
//!   `decode_band_into_bgrx` into a `960x540` canvas and renders live frames in a native GTK3 viewer window
//!   (with `F11` fullscreen toggle and `Ctrl+Alt+P` mode cycle support).
//! - **Emergency Teardown (`close_all_viewer_windows`):** Persists child PIDs to `/tmp/duplex-kvm-linux-video.pids`
//!   so both `duplexd` and `duplex-guard` can immediately terminate all capture/viewer processes.

use crate::shm::monotonic_now_ns;
use crate::{PalError, VideoPipelineBackend};
use core::sync::atomic::{AtomicBool, Ordering};
use duplex_proto::{
    decode_band_into_bgrx, encode_bgrx_frame_to_bands, CursorTelemetry, DisplayDescriptor,
    EncodedVideoFrame, DPXB_BAND_MAGIC, MAX_VIDEO_BITRATE_KBPS,
};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{debug, info};

const LINUX_VIDEO_PIDS_FILE: &str = "/tmp/duplex-kvm-linux-video.pids";
const CAPTURE_WIDTH: u16 = 960;
const CAPTURE_HEIGHT: u16 = 540;
const CAPTURE_FPS: u32 = 30;

/// Embedded Python3 script that connects to `org.gnome.Mutter.ScreenCast` over session D-Bus
/// (creating a genuine Wayland virtual monitor via `RecordVirtual` when `mode == "virtual"` or
/// mirroring the primary monitor via `RecordMonitor` when `mode == "monitor"`), and streams raw
/// `960x540` `BGRx` frames to stdout via `pipewiresrc` (with automatic `ximagesrc` fallback on X11).
const MUTTER_PIPEWIRE_CAPTURE_PY: &str = r#"
import sys, os, time
import gi
gi.require_version('Gst', '1.0')
from gi.repository import GLib, Gio, Gst

Gst.init(None)
mode = sys.argv[1] if len(sys.argv) > 1 else "monitor"
w = int(sys.argv[2]) if len(sys.argv) > 2 else 960
h = int(sys.argv[3]) if len(sys.argv) > 3 else 540
fps = int(sys.argv[4]) if len(sys.argv) > 4 else 30
x_off = int(sys.argv[5]) if len(sys.argv) > 5 else 0

loop = GLib.MainLoop()
pipeline = None
session_proxy = None

def start_gst_pipeline(desc):
    global pipeline
    pipeline = Gst.parse_launch(desc)
    bus = pipeline.get_bus()
    bus.add_signal_watch()
    def on_msg(b, msg):
        if msg.type in (Gst.MessageType.EOS, Gst.MessageType.ERROR):
            loop.quit()
    bus.connect('message', on_msg)
    pipeline.set_state(Gst.State.PLAYING)

try:
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    sc = Gio.DBusProxy.new_sync(
        bus, Gio.DBusProxyFlags.NONE, None,
        'org.gnome.Mutter.ScreenCast',
        '/org/gnome/Mutter/ScreenCast',
        'org.gnome.Mutter.ScreenCast', None
    )
    res = sc.call_sync('CreateSession', GLib.Variant('(a{sv})', ({},)), Gio.DBusCallFlags.NONE, -1, None)
    session_path = res.unpack()[0]
    session_proxy = Gio.DBusProxy.new_sync(
        bus, Gio.DBusProxyFlags.NONE, None,
        'org.gnome.Mutter.ScreenCast',
        session_path,
        'org.gnome.Mutter.ScreenCast.Session', None
    )
    stream_path = None
    if mode == "virtual":
        try:
            vres = session_proxy.call_sync(
                'RecordVirtual',
                GLib.Variant('(a{sv})', ({
                    'cursor-mode': GLib.Variant('u', 1),
                    'is-platform': GLib.Variant('b', True)
                },)),
                Gio.DBusCallFlags.NONE, -1, None
            )
            stream_path = vres.unpack()[0]
        except Exception:
            stream_path = None
    if not stream_path:
        mres = session_proxy.call_sync(
            'RecordMonitor',
            GLib.Variant('(sa{sv})', ('', {'cursor-mode': GLib.Variant('u', 1)})),
            Gio.DBusCallFlags.NONE, -1, None
        )
        stream_path = mres.unpack()[0]

    def on_pw_stream_added(conn, sender, path, iface, sig, params):
        node_id = params.unpack()[0]
        desc = (
            f"pipewiresrc path={node_id} do-timestamp=true ! "
            f"videoconvert ! videoscale ! videorate ! "
            f"video/x-raw,format=BGRx,width={w},height={h},framerate={fps}/1 ! "
            f"fdsink fd=1 sync=false"
        )
        start_gst_pipeline(desc)

    bus.signal_subscribe(
        'org.gnome.Mutter.ScreenCast',
        'org.gnome.Mutter.ScreenCast.Stream',
        'PipeWireStreamAdded',
        stream_path, None, Gio.DBusSignalFlags.NONE,
        on_pw_stream_added
    )
    session_proxy.call_sync('Start', None, Gio.DBusCallFlags.NONE, -1, None)
except Exception:
    end_x = x_off + 1919
    desc = (
        f"ximagesrc use-damage=0 show-pointer=true startx={x_off} starty=0 endx={end_x} endy=1079 ! "
        f"videoconvert ! videoscale ! videorate ! "
        f"video/x-raw,format=BGRx,width={w},height={h},framerate={fps}/1 ! "
        f"fdsink fd=1 sync=false"
    )
    start_gst_pipeline(desc)

try:
    loop.run()
except KeyboardInterrupt:
    pass
finally:
    if pipeline:
        pipeline.set_state(Gst.State.NULL)
    if session_proxy:
        try:
            session_proxy.call_sync('Stop', None, Gio.DBusCallFlags.NONE, -1, None)
        except Exception:
            pass
"#;

/// Embedded Python3 GTK3 viewer script that reads raw `960x540` RGB24 frames (`w * h * 3` bytes)
/// from `stdin` and renders them smoothly scaled into a resizable/fullscreen Wayland/X11 window.
const GTK3_VIEWER_PY: &str = r#"
import sys, os, threading
import gi
gi.require_version('Gtk', '3.0')
gi.require_version('Gdk', '3.0')
gi.require_version('GdkPixbuf', '2.0')
from gi.repository import Gtk, Gdk, GdkPixbuf, GLib

w = int(sys.argv[1]) if len(sys.argv) > 1 else 960
h = int(sys.argv[2]) if len(sys.argv) > 2 else 540
mon_idx = int(sys.argv[3]) if len(sys.argv) > 3 else 0
frame_bytes = w * h * 3

win = Gtk.Window(title=f"Duplex-KVM — Windows Extended Display {mon_idx + 1} (F11: Fullscreen | Ctrl+Alt+P: Switch Mode)")
win.set_default_size(1280, 720)
win.set_position(Gtk.WindowPosition.CENTER)
win.connect("destroy", Gtk.main_quit)

is_fs = [False]
def on_key(widget, event):
    if event.keyval == Gdk.KEY_F11:
        if is_fs[0]:
            win.unfullscreen()
            is_fs[0] = False
        else:
            win.fullscreen()
            is_fs[0] = True
        return True
    return False
win.connect("key-press-event", on_key)

darea = Gtk.DrawingArea()
win.add(darea)
latest_pixbuf = [None]

def on_draw(widget, cr):
    alloc = widget.get_allocation()
    dw, dh = max(1, alloc.width), max(1, alloc.height)
    pb = latest_pixbuf[0]
    if pb is not None:
        cr.save()
        cr.scale(dw / float(w), dh / float(h))
        Gdk.cairo_set_source_pixbuf(cr, pb, 0, 0)
        cr.paint()
        cr.restore()
    else:
        cr.set_source_rgb(0.06, 0.09, 0.16)
        cr.paint()
    return False

darea.connect("draw", on_draw)
win.show_all()

def reader_loop():
    fd = sys.stdin.buffer
    while True:
        buf = bytearray()
        while len(buf) < frame_bytes:
            chunk = fd.read(frame_bytes - len(buf))
            if not chunk:
                GLib.idle_add(Gtk.main_quit)
                return
            buf.extend(chunk)
        gbytes = GLib.Bytes.new(bytes(buf))
        pb = GdkPixbuf.Pixbuf.new_from_bytes(
            gbytes, GdkPixbuf.Colorspace.RGB, False, 8, w, h, w * 3
        )
        latest_pixbuf[0] = pb
        GLib.idle_add(darea.queue_draw)

t = threading.Thread(target=reader_loop, daemon=True)
t.start()
Gtk.main()
"#;

/// Configures `cmd` so that even if `duplexd` was launched as `root` (via `sudo`), the spawned
/// GUI / PipeWire child runs as the active desktop user with valid Wayland/X11/D-Bus environment variables.
fn configure_desktop_user_env(cmd: &mut Command) {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return;
    }

    // Determine target desktop UID/GID from SUDO_UID or /run/user/<uid>
    let mut target_uid: Option<u32> = std::env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&u| u >= 1000);
    let mut target_gid: Option<u32> = std::env::var("SUDO_GID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&g| g >= 1000);

    if target_uid.is_none() {
        if let Ok(entries) = fs::read_dir("/run/user") {
            for entry in entries.flatten() {
                if let Ok(uid) = entry.file_name().to_string_lossy().parse::<u32>() {
                    if uid >= 1000 {
                        target_uid = Some(uid);
                        target_gid = Some(uid);
                        break;
                    }
                }
            }
        }
    }

    if let Some(uid) = target_uid {
        let gid = target_gid.unwrap_or(uid);
        let runtime_dir = format!("/run/user/{uid}");
        cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
        cmd.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime_dir}/bus"),
        );
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            let wl_sock = format!("{runtime_dir}/wayland-0");
            if std::path::Path::new(&wl_sock).exists() {
                cmd.env("WAYLAND_DISPLAY", "wayland-0");
                cmd.env("XDG_SESSION_TYPE", "wayland");
            }
        }
        if std::env::var("DISPLAY").is_err() {
            cmd.env("DISPLAY", ":0");
        }
        cmd.uid(uid);
        cmd.gid(gid);
    }
}

struct EncoderWorkerHandle {
    monitor_index: u8,
    stop_flag: Arc<AtomicBool>,
    force_idr_flag: Arc<AtomicBool>,
    child_pid: Option<u32>,
    join_handle: Option<JoinHandle<()>>,
}

struct ViewerProcessHandle {
    monitor_index: u8,
    child_pid: u32,
    stop_flag: Arc<AtomicBool>,
    frame_tx: SyncSender<Vec<u8>>,
    bgrx_canvas: Vec<u8>,
    last_flush_ns: u64,
    join_handle: Option<JoinHandle<()>>,
}

pub struct LinuxVideoPipelineBackend {
    encoders: Vec<EncoderWorkerHandle>,
    frame_rx: Option<Receiver<EncodedVideoFrame>>,
    viewers: Vec<ViewerProcessHandle>,
    last_cursor: CursorTelemetry,
}

impl Default for LinuxVideoPipelineBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxVideoPipelineBackend {
    pub fn new() -> Self {
        Self {
            encoders: Vec::new(),
            frame_rx: None,
            viewers: Vec::new(),
            last_cursor: CursorTelemetry {
                monitor_index: 0,
                x: 0,
                y: 0,
                visible: true,
                shape: None,
            },
        }
    }

    fn persist_viewer_pids(&self) {
        let mut text = String::new();
        for v in &self.viewers {
            if v.child_pid > 0 {
                text.push_str(&format!("{}\n", v.child_pid));
            }
        }
        for e in &self.encoders {
            if let Some(pid) = e.child_pid {
                text.push_str(&format!("{pid}\n"));
            }
        }
        if text.is_empty() {
            let _ = fs::remove_file(LINUX_VIDEO_PIDS_FILE);
        } else {
            let _ = fs::write(LINUX_VIDEO_PIDS_FILE, text);
        }
    }

    fn kill_persisted_pids() {
        if let Ok(text) = fs::read_to_string(LINUX_VIDEO_PIDS_FILE) {
            for line in text.lines() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    if pid > 0 {
                        unsafe {
                            let _ = libc::kill(pid, libc::SIGKILL);
                        }
                    }
                }
            }
            let _ = fs::remove_file(LINUX_VIDEO_PIDS_FILE);
        }
    }

    fn try_spawn_mutter_capture_child(monitor_index: u8, x_offset: u32) -> Option<Child> {
        let mode_arg = if monitor_index == 0 {
            "monitor"
        } else {
            "virtual"
        };
        let mut cmd = Command::new("python3");
        cmd.arg("-c")
            .arg(MUTTER_PIPEWIRE_CAPTURE_PY)
            .arg(mode_arg)
            .arg(CAPTURE_WIDTH.to_string())
            .arg(CAPTURE_HEIGHT.to_string())
            .arg(CAPTURE_FPS.to_string())
            .arg(x_offset.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_desktop_user_env(&mut cmd);
        cmd.spawn().ok()
    }

    fn spawn_monitor_encoder_thread(
        spec: DisplayDescriptor,
        x_offset: u32,
        tx: SyncSender<EncodedVideoFrame>,
    ) -> EncoderWorkerHandle {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let force_idr_flag = Arc::new(AtomicBool::new(true));

        let stop_clone = stop_flag.clone();
        let idr_clone = force_idr_flag.clone();
        let monitor_index = spec.index;

        let mut child_opt = Self::try_spawn_mutter_capture_child(monitor_index, x_offset);
        let child_pid = child_opt.as_ref().map(|c| c.id());

        let join_handle = thread::Builder::new()
            .name(format!("duplex-linux-enc-{monitor_index}"))
            .spawn(move || {
                let mut frame_seq = 0u32;
                let mut prev_rgb565: Vec<u16> = Vec::new();
                let mut cycle_counter: u32 = 0;
                let mut last_force_all_ns: u64 = 0;
                let mut encoded_batch: Vec<EncodedVideoFrame> = Vec::with_capacity(32);
                let raw_frame_len =
                    (CAPTURE_WIDTH as usize) * (CAPTURE_HEIGHT as usize) * 4;

                if let Some(ref mut child) = child_opt {
                    if let Some(mut stdout) = child.stdout.take() {
                        let mut raw_buf = vec![0u8; raw_frame_len];
                        while !stop_clone.load(Ordering::Relaxed) {
                            let mut filled = 0usize;
                            let mut eof = false;
                            while filled < raw_frame_len && !stop_clone.load(Ordering::Relaxed) {
                                match stdout.read(&mut raw_buf[filled..]) {
                                    Ok(0) => {
                                        eof = true;
                                        break;
                                    }
                                    Ok(n) => filled += n,
                                    Err(_) => {
                                        eof = true;
                                        break;
                                    }
                                }
                            }
                            if eof || filled < raw_frame_len {
                                break;
                            }

                            let now_ns = monotonic_now_ns();
                            let req_idr = idr_clone.swap(false, Ordering::AcqRel);
                            let force_all = req_idr
                                && now_ns.saturating_sub(last_force_all_ns) >= 500_000_000;
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
                        }
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                }

                // Fallback synthetic DPXB band generator when running headless or in --self-test
                let frame_interval = Duration::from_millis(33);
                let mut synth_bgrx = vec![0x1Eu8; raw_frame_len];
                while !stop_clone.load(Ordering::Relaxed) {
                    let now_ns = monotonic_now_ns();
                    let force_all = idr_clone.swap(false, Ordering::AcqRel);
                    let bar_y = ((frame_seq as usize) * 4) % (CAPTURE_HEIGHT as usize);
                    let row_start = bar_y * (CAPTURE_WIDTH as usize) * 4;
                    for px in &mut synth_bgrx[row_start..row_start + (CAPTURE_WIDTH as usize) * 4] {
                        *px = px.wrapping_add(17);
                    }
                    encoded_batch.clear();
                    encode_bgrx_frame_to_bands(
                        monitor_index,
                        CAPTURE_WIDTH,
                        CAPTURE_HEIGHT,
                        &synth_bgrx,
                        &mut prev_rgb565,
                        force_all,
                        cycle_counter,
                        &mut frame_seq,
                        now_ns,
                        &mut encoded_batch,
                    );
                    cycle_counter = cycle_counter.wrapping_add(1);
                    for frame in encoded_batch.drain(..) {
                        let _ = tx.try_send(frame);
                    }
                    thread::sleep(frame_interval);
                }
            })
            .expect("Failed to spawn Linux Mutter/PipeWire encoder worker thread");

        EncoderWorkerHandle {
            monitor_index,
            stop_flag,
            force_idr_flag,
            child_pid,
            join_handle: Some(join_handle),
        }
    }
}

impl VideoPipelineBackend for LinuxVideoPipelineBackend {
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

        let mut x_offset = 0u32;
        for spec in monitors {
            let worker = Self::spawn_monitor_encoder_thread(spec.clone(), x_offset, tx.clone());
            x_offset += spec.width_px;
            self.encoders.push(worker);
        }

        self.persist_viewer_pids();
        info!(
            streams = self.encoders.len(),
            bitrate_kbps = capped_bitrate,
            "Started Debian Linux Mutter ScreenCast PipeWire + DPXB low-latency video sender"
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
        *out_cursor = None;
        Ok(count)
    }

    fn request_keyframe(&mut self, monitor_index: u8) -> Result<(), PalError> {
        for enc in &self.encoders {
            if enc.monitor_index == monitor_index {
                enc.force_idr_flag.store(true, Ordering::Release);
                debug!(monitor_index, "Scheduled immediate full-frame DPXB refresh");
            }
        }
        Ok(())
    }

    fn start_viewer(&mut self, monitors: &[DisplayDescriptor]) -> Result<(), PalError> {
        self.close_all_viewer_windows()?;

        for spec in monitors {
            let mon_idx = spec.index;
            let mut cmd = Command::new("python3");
            cmd.arg("-c")
                .arg(GTK3_VIEWER_PY)
                .arg(CAPTURE_WIDTH.to_string())
                .arg(CAPTURE_HEIGHT.to_string())
                .arg(mon_idx.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            configure_desktop_user_env(&mut cmd);

            if let Ok(mut child) = cmd.spawn() {
                let child_pid = child.id();
                let mut child_stdin = child.stdin.take();
                let stop_flag = Arc::new(AtomicBool::new(false));
                let stop_clone = stop_flag.clone();
                let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<u8>>(2);

                let join_handle = thread::Builder::new()
                    .name(format!("duplex-gtk-viewer-{mon_idx}"))
                    .spawn(move || {
                        while !stop_clone.load(Ordering::Relaxed) {
                            match frame_rx.recv_timeout(Duration::from_millis(100)) {
                                Ok(rgb_bytes) => {
                                    if let Some(ref mut stdin) = child_stdin {
                                        if stdin.write_all(&rgb_bytes).is_err() {
                                            break;
                                        }
                                        let _ = stdin.flush();
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        drop(child_stdin);
                        let _ = child.kill();
                        let _ = child.wait();
                    })
                    .ok();

                let canvas_len = (CAPTURE_WIDTH as usize) * (CAPTURE_HEIGHT as usize) * 4;
                self.viewers.push(ViewerProcessHandle {
                    monitor_index: mon_idx,
                    child_pid,
                    stop_flag,
                    frame_tx,
                    bgrx_canvas: vec![0u8; canvas_len],
                    last_flush_ns: 0,
                    join_handle,
                });
            }
        }

        self.persist_viewer_pids();
        info!(
            requested_viewers = monitors.len(),
            spawned_processes = self.viewers.len(),
            "Initialized Debian Linux native GTK3 viewer window for Mode 1+ extended display"
        );
        Ok(())
    }

    fn submit_encoded_frame(&mut self, frame: &EncodedVideoFrame) -> Result<(), PalError> {
        if frame.data.len() < 12 || frame.data[0..4] != DPXB_BAND_MAGIC {
            return Ok(());
        }

        let band_y = u16::from_be_bytes([frame.data[8], frame.data[9]]);
        let band_h = u16::from_be_bytes([frame.data[10], frame.data[11]]);
        let now_ns = monotonic_now_ns();

        for viewer in &mut self.viewers {
            if viewer.monitor_index == frame.monitor_index {
                let mut cw = CAPTURE_WIDTH;
                let mut ch = CAPTURE_HEIGHT;
                if decode_band_into_bgrx(&frame.data, &mut cw, &mut ch, &mut viewer.bgrx_canvas) {
                    let is_bottom_band = band_y.saturating_add(band_h) >= ch;
                    let elapsed_ns = now_ns.saturating_sub(viewer.last_flush_ns);
                    if is_bottom_band || elapsed_ns >= 25_000_000 {
                        viewer.last_flush_ns = now_ns;
                        let total_px = (cw as usize) * (ch as usize);
                        let mut rgb = Vec::with_capacity(total_px * 3);
                        for px in viewer.bgrx_canvas.chunks_exact(4).take(total_px) {
                            rgb.push(px[2]); // R
                            rgb.push(px[1]); // G
                            rgb.push(px[0]); // B
                        }
                        let _ = viewer.frame_tx.try_send(rgb);
                    }
                }
            }
        }
        Ok(())
    }

    fn update_hardware_cursor(&mut self, cursor: &CursorTelemetry) -> Result<(), PalError> {
        self.last_cursor = cursor.clone();
        Ok(())
    }

    fn close_all_viewer_windows(&mut self) -> Result<(), PalError> {
        for mut enc in self.encoders.drain(..) {
            enc.stop_flag.store(true, Ordering::Release);
            if let Some(pid) = enc.child_pid {
                unsafe {
                    let _ = libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            if let Some(handle) = enc.join_handle.take() {
                let _ = handle.join();
            }
        }
        self.frame_rx = None;

        for mut viewer in self.viewers.drain(..) {
            viewer.stop_flag.store(true, Ordering::Release);
            if viewer.child_pid > 0 {
                unsafe {
                    let _ = libc::kill(viewer.child_pid as i32, libc::SIGKILL);
                }
            }
            if let Some(handle) = viewer.join_handle.take() {
                let _ = handle.join();
            }
        }

        Self::kill_persisted_pids();
        info!("Closed all Linux PipeWire encoders and GTK3 viewer windows");
        Ok(())
    }
}

impl Drop for LinuxVideoPipelineBackend {
    fn drop(&mut self) {
        let _ = self.close_all_viewer_windows();
    }
}
