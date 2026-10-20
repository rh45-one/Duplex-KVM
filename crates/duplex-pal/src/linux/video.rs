//! Debian Linux screen streaming.
//!
//! - **Sender (Mode 2):** a GStreamer helper captures each Mutter/portal
//!   virtual monitor (or an X11 output region) at native resolution and
//!   writes BGRx frames to a pipe. A reader thread keeps only the newest frame;
//!   the shared encoder codes DPXT tiles on the low-priority codec pool.
//! - **Viewer (Mode 1+):** a decoder thread applies tiles and copies changed
//!   regions into a shared-memory canvas under a sequence lock. A GTK 4 viewer
//!   process maps it and presents frames at exact device pixels, including
//!   fractional desktop scaling (GTK 3 fallback).
//! - **Emergency teardown:** helper PIDs are persisted to
//!   `/tmp/duplex-kvm-linux-video.pids` so `duplex-guard` can kill them too.

use crate::stream::{
    capture_fps_limit, encoder_config_from_env, spawn_encoder, EncoderControl, EncoderSpec,
    FramePresenter, FrameSlot, StreamCodec, ViewerSet,
};
use crate::{
    PalError, PeerVideoCaps, VideoFrameConsumer, VideoPipelineBackend, VideoSink,
    VirtualDisplayBackend,
};
use duplex_proto::{DisplayDescriptor, PixelRect};
use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use tracing::{info, warn};

const LINUX_VIDEO_PIDS_FILE: &str = "/tmp/duplex-kvm-linux-video.pids";

/// Capture virtual outputs through Mutter/PipeWire or the desktop portal.
/// X11 captures the provisioned output regions; synthetic input is test-only.
const CAPTURE_PY: &str = include_str!("capture.py");
/// Fullscreen GTK 4 viewer of the shared-memory canvas (GTK 3 fallback).
const VIEWER_PY: &str = include_str!("viewer.py");
const VIRTUAL_LAYOUT_PY: &str = include_str!("layout.py");

/// Shared canvas layout: a 4 KiB header followed by BGRx rows.
const SHM_HEADER: usize = 4096;
const SHM_MAGIC: u32 = u32::from_le_bytes(*b"DPXS");
const SHM_VERSION: u32 = 1;
const SHM_MAX_PIXELS: usize = duplex_proto::screen_codec::MAX_CANVAS_PIXELS;
/// Notification messages stay below PIPE_BUF so non-blocking writes are
/// all-or-nothing; more regions collapse into their bounding box.
const MAX_NOTIFY_RECTS: usize = 64;
/// Capture frame ring shared with the helper (see capture.py).
const FRAME_RING_MAGIC: u32 = u32::from_le_bytes(*b"DPXF");
/// Three slots let the helper write one frame while the daemon copies another.
const FRAME_RING_SLOTS: usize = 3;

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

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// An anonymous shared mapping that a helper process receives as descriptor 3.
struct SharedMemory {
    fd: OwnedFd,
    base: *mut u8,
    len: usize,
}

// SAFETY: the mapping is owned by this value; concurrent access by the helper
// process is coordinated through the sequence numbers in each layout.
unsafe impl Send for SharedMemory {}

impl SharedMemory {
    /// Pages are allocated only when written, so generous sizes cost address
    /// space rather than memory.
    fn create(name: &str, len: usize) -> Result<Self, PalError> {
        let failed = |what: &str| {
            PalError::VideoPipeline(format!(
                "Cannot {what} shared memory: {}",
                std::io::Error::last_os_error()
            ))
        };
        let name = CString::new(name).expect("static name");
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(failed("create"));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(failed("size"));
        }
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(failed("map"));
        }
        Ok(Self {
            fd,
            base: base.cast(),
            len,
        })
    }

    fn put_u32(&self, offset: usize, value: u32) {
        unsafe { core::ptr::write_volatile(self.base.add(offset).cast::<u32>(), value.to_le()) };
    }

    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        unsafe { &*self.base.add(offset).cast::<AtomicU32>() }
    }

    /// Makes `command`'s child inherit this memory as descriptor 3 (dup2
    /// clears close-on-exec on the copy).
    fn pass_as_fd3(&self, command: &mut Command) {
        let shared_fd = self.fd.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                let result = if shared_fd == 3 {
                    libc::fcntl(3, libc::F_SETFD, 0)
                } else {
                    libc::dup2(shared_fd, 3)
                };
                if result < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base.cast(), self.len);
        }
    }
}

/// Frames written by the capture helper into rotating slots. Each slot's
/// sequence number is odd while the helper writes it, so a copy taken
/// between two equal even values is complete.
struct FrameRing {
    memory: SharedMemory,
    frame_len: usize,
    slot_bytes: usize,
}

impl FrameRing {
    fn create(width: u16, height: u16) -> Result<Self, PalError> {
        let stride = usize::from(width) * 4;
        let frame_len = stride * usize::from(height);
        let slot_bytes = frame_len.div_ceil(4096) * 4096;
        let memory = SharedMemory::create(
            "duplex-kvm-capture",
            SHM_HEADER + FRAME_RING_SLOTS * slot_bytes,
        )?;
        memory.put_u32(0, FRAME_RING_MAGIC);
        memory.put_u32(4, 1);
        memory.put_u32(8, u32::from(width));
        memory.put_u32(12, u32::from(height));
        memory.put_u32(16, stride as u32);
        memory.put_u32(20, FRAME_RING_SLOTS as u32);
        Ok(Self {
            memory,
            frame_len,
            slot_bytes,
        })
    }

    /// Copies slot `index` into `frame`; false if it was being rewritten.
    fn read(&self, index: usize, frame: &mut [u8]) -> bool {
        if index >= FRAME_RING_SLOTS || frame.len() != self.frame_len {
            return false;
        }
        let sequence = self.memory.atomic_u32(64 + 4 * index);
        let before = sequence.load(Ordering::Acquire);
        if before & 1 != 0 {
            return false;
        }
        unsafe {
            let source = self.memory.base.add(SHM_HEADER + index * self.slot_bytes);
            core::ptr::copy_nonoverlapping(source, frame.as_mut_ptr(), self.frame_len);
        }
        fence(Ordering::Acquire);
        sequence.load(Ordering::Relaxed) == before
    }
}

/// Memory shared with a viewer process. The daemon writes; the viewer copies
/// a consistent snapshot by checking the sequence number around its read.
struct SharedCanvas {
    memory: SharedMemory,
    width: u16,
    height: u16,
}

impl SharedCanvas {
    fn create() -> Result<Self, PalError> {
        // Sized for the 8K ceiling; only the pages in use are allocated.
        let memory = SharedMemory::create("duplex-kvm-viewer", SHM_HEADER + SHM_MAX_PIXELS * 4)?;
        memory.put_u32(0, SHM_MAGIC);
        memory.put_u32(4, SHM_VERSION);
        Ok(Self {
            memory,
            width: 0,
            height: 0,
        })
    }

    fn put_u32(&self, offset: usize, value: u32) {
        self.memory.put_u32(offset, value);
    }

    fn sequence(&self) -> &AtomicU32 {
        self.memory.atomic_u32(20)
    }

    fn frames(&self) -> &AtomicU64 {
        unsafe { &*self.memory.base.add(24).cast::<AtomicU64>() }
    }

    /// Copies `rects` (or the whole canvas after a size change). Returns true
    /// when the size changed and the viewer must redraw everything.
    fn write(
        &mut self,
        width: u16,
        height: u16,
        canvas: &[u8],
        rects: &[PixelRect],
    ) -> Result<bool, PalError> {
        let stride = usize::from(width) * 4;
        let length = stride * usize::from(height);
        if usize::from(width) * usize::from(height) > SHM_MAX_PIXELS || canvas.len() < length {
            return Err(PalError::VideoPipeline(format!(
                "Unsupported stream size {width}x{height}"
            )));
        }
        let resized = (width, height) != (self.width, self.height);
        let sequence = self.sequence();
        let start = sequence.load(Ordering::Relaxed);
        sequence.store(start.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let pixels = unsafe {
            core::slice::from_raw_parts_mut(
                self.memory.base.add(SHM_HEADER),
                self.memory.len - SHM_HEADER,
            )
        };
        if resized {
            self.put_u32(8, u32::from(width));
            self.put_u32(12, u32::from(height));
            self.put_u32(16, stride as u32);
            pixels[..length].copy_from_slice(&canvas[..length]);
            (self.width, self.height) = (width, height);
        } else {
            for rect in rects {
                let (x, w) = (rect.x as usize * 4, rect.width as usize * 4);
                for row in rect.y as usize..(rect.y + rect.height) as usize {
                    let at = row * stride + x;
                    pixels[at..at + w].copy_from_slice(&canvas[at..at + w]);
                }
            }
        }
        self.sequence()
            .store(start.wrapping_add(2), Ordering::Release);
        self.frames().fetch_add(1, Ordering::Release);
        Ok(resized)
    }
}

/// Presents frames through a GTK viewer process and shared memory.
struct ShmPresenter {
    monitor: u8,
    canvas: SharedCanvas,
    child: Child,
    notify: ChildStdin,
    redraw_all: bool,
    message: Vec<u8>,
}

impl ShmPresenter {
    fn spawn(monitor: u8) -> Result<Self, PalError> {
        let canvas = SharedCanvas::create()?;
        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg(VIEWER_PY)
            .arg(monitor.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        configure_desktop_user_env(&mut command);
        canvas.memory.pass_as_fd3(&mut command);
        let mut child = command.spawn().map_err(|e| {
            PalError::VideoPipeline(format!("Cannot launch the display viewer (python3): {e}"))
        })?;
        let Some(notify) = child.stdin.take() else {
            kill_child(&mut child);
            return Err(PalError::VideoPipeline(
                "Viewer notification pipe unavailable".into(),
            ));
        };
        unsafe {
            let fd = notify.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Ok(Self {
            monitor,
            canvas,
            child,
            notify,
            redraw_all: true,
            message: Vec::with_capacity(1100),
        })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl FramePresenter for ShmPresenter {
    fn present(
        &mut self,
        width: u16,
        height: u16,
        canvas: &[u8],
        dirty: &[PixelRect],
    ) -> Result<(), PalError> {
        if let Ok(Some(status)) = self.child.try_wait() {
            return Err(PalError::VideoPipeline(format!(
                "Display viewer {} exited ({status}); see its messages in the daemon log",
                self.monitor
            )));
        }
        self.redraw_all |= self.canvas.write(width, height, canvas, dirty)?;
        if self.redraw_all {
            return self.notify_rects(&[PixelRect {
                x: 0,
                y: 0,
                width: u32::from(width),
                height: u32::from(height),
            }]);
        }
        if dirty.len() > MAX_NOTIFY_RECTS {
            let left = dirty.iter().map(|r| r.x).min().unwrap_or(0);
            let top = dirty.iter().map(|r| r.y).min().unwrap_or(0);
            let right = dirty.iter().map(|r| r.x + r.width).max().unwrap_or(0);
            let bottom = dirty.iter().map(|r| r.y + r.height).max().unwrap_or(0);
            return self.notify_rects(&[PixelRect {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            }]);
        }
        self.notify_rects(dirty)
    }
}

impl ShmPresenter {
    fn notify_rects(&mut self, rects: &[PixelRect]) -> Result<(), PalError> {
        self.message.clear();
        self.message
            .extend_from_slice(&(rects.len() as u32).to_le_bytes());
        for rect in rects {
            for value in [rect.x, rect.y, rect.width, rect.height] {
                self.message.extend_from_slice(&value.to_le_bytes());
            }
        }
        let written = unsafe {
            libc::write(
                self.notify.as_raw_fd(),
                self.message.as_ptr().cast(),
                self.message.len(),
            )
        };
        if written == self.message.len() as isize {
            self.redraw_all = false;
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            // The viewer is behind; it will redraw everything next time.
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => {
                self.redraw_all = true;
                Ok(())
            }
            _ => Err(PalError::VideoPipeline(format!(
                "Display viewer {} closed: {error}",
                self.monitor
            ))),
        }
    }
}

impl Drop for ShmPresenter {
    fn drop(&mut self) {
        kill_child(&mut self.child);
    }
}

struct LinuxSender {
    monitor: u8,
    control: Arc<EncoderControl>,
    child: Child,
    reader: Option<JoinHandle<()>>,
    encoder: Option<JoinHandle<()>>,
    reported: bool,
}

impl LinuxSender {
    fn stop(mut self) {
        self.control.stop();
        // Killing the helper ends the reader, which closes the frame slot and
        // wakes the encoder even if it is waiting for a frame.
        kill_child(&mut self.child);
        for handle in [self.reader.take(), self.encoder.take()]
            .into_iter()
            .flatten()
        {
            let _ = handle.join();
        }
    }
}

/// Copies each frame the helper announces from the ring into the slot. Only
/// the newest announcement matters when several are pending.
fn spawn_capture_reader(
    monitor: u8,
    mut announcements: ChildStdout,
    ring: FrameRing,
    slot: Arc<FrameSlot>,
) -> Result<JoinHandle<()>, PalError> {
    thread::Builder::new()
        .name(format!("duplex-capture-{monitor}"))
        .spawn(move || {
            let mut buffer = [0u8; 64];
            let mut pending = 0usize;
            loop {
                let read = match announcements.read(&mut buffer[pending..]) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        warn!(monitor, %error, "Desktop capture channel failed");
                        break;
                    }
                };
                pending += read;
                let complete = pending / 4 * 4;
                if complete == 0 {
                    continue;
                }
                let index = u32::from_le_bytes(
                    buffer[complete - 4..complete]
                        .try_into()
                        .expect("four bytes"),
                );
                buffer.copy_within(complete..pending, 0);
                pending -= complete;
                let mut frame = slot.buffer(ring.frame_len);
                if ring.read(index as usize, &mut frame) {
                    slot.publish(frame);
                } else {
                    slot.recycle(frame);
                }
            }
            slot.close();
        })
        .map_err(|e| PalError::VideoPipeline(format!("Cannot start capture reader: {e}")))
}

pub struct LinuxVideoPipelineBackend {
    pids_path: std::path::PathBuf,
    caps: PeerVideoCaps,
    senders: Vec<LinuxSender>,
    viewers: Option<ViewerSet>,
    viewer_pids: Vec<u32>,
    layout_child: Option<Child>,
}

impl Default for LinuxVideoPipelineBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxVideoPipelineBackend {
    pub fn new() -> Self {
        Self {
            pids_path: LINUX_VIDEO_PIDS_FILE.into(),
            caps: PeerVideoCaps::default(),
            senders: Vec::new(),
            viewers: None,
            viewer_pids: Vec::new(),
            layout_child: None,
        }
    }

    fn persist_pids(&self) {
        let pids: Vec<u32> = self
            .layout_child
            .iter()
            .map(Child::id)
            .chain(self.senders.iter().map(|s| s.child.id()))
            .chain(self.viewer_pids.iter().copied())
            .collect();
        if pids.is_empty() {
            let _ = fs::remove_file(&self.pids_path);
        } else {
            let text: String = pids.iter().map(|pid| format!("{pid}\n")).collect();
            let _ = fs::write(&self.pids_path, text);
        }
    }

    fn kill_persisted_pids(&self) {
        if let Ok(text) = fs::read_to_string(&self.pids_path) {
            for pid in text
                .lines()
                .filter_map(|line| line.trim().parse::<i32>().ok())
            {
                if pid > 0 {
                    unsafe {
                        let _ = libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
            let _ = fs::remove_file(&self.pids_path);
        }
    }

    fn spawn_capture(
        spec: &DisplayDescriptor,
        x_offset: u32,
        mode: &str,
        ring: &FrameRing,
    ) -> Result<Child, PalError> {
        let width = ring.memory.atomic_u32(8).load(Ordering::Relaxed);
        let height = ring.memory.atomic_u32(12).load(Ordering::Relaxed);
        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg(CAPTURE_PY)
            .arg(mode)
            .arg(width.to_string())
            .arg(height.to_string())
            .arg(capture_fps_limit().to_string())
            .arg(x_offset.to_string())
            .arg(spec.width_px.to_string())
            .arg(spec.height_px.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        configure_desktop_user_env(&mut command);
        ring.memory.pass_as_fd3(&mut command);
        command.spawn().map_err(|e| {
            PalError::VideoPipeline(format!("Cannot launch desktop capture helper: {e}"))
        })
    }

    /// Starts one capture helper, reader and encoder for `spec`.
    fn start_stream(
        &mut self,
        spec: &DisplayDescriptor,
        x_offset: u32,
        mode: &str,
        sink: Arc<dyn VideoSink>,
    ) -> Result<(), PalError> {
        let width = spec.width_px.clamp(16, 7680) as u16;
        let height = spec.height_px.clamp(16, 4320) as u16;
        if usize::from(width) * usize::from(height) > SHM_MAX_PIXELS {
            return Err(PalError::VideoPipeline(format!(
                "Unsupported stream size {width}x{height}"
            )));
        }
        let ring = FrameRing::create(width, height)?;
        let mut child = Self::spawn_capture(spec, x_offset, mode, &ring)?;
        let Some(stdout) = child.stdout.take() else {
            kill_child(&mut child);
            return Err(PalError::VideoPipeline(
                "Capture channel unavailable".into(),
            ));
        };
        let slot = FrameSlot::new();
        let reader = match spawn_capture_reader(spec.index, stdout, ring, slot.clone()) {
            Ok(reader) => reader,
            Err(error) => {
                kill_child(&mut child);
                return Err(error);
            }
        };
        let control = EncoderControl::new(StreamCodec::for_peer(self.caps));
        let encoder = spawn_encoder(
            EncoderSpec {
                monitor: spec.index,
                width,
                height,
            },
            slot,
            sink,
            control.clone(),
            encoder_config_from_env(),
        );
        let sender = LinuxSender {
            monitor: spec.index,
            control,
            child,
            reader: Some(reader),
            encoder: None,
            reported: false,
        };
        match encoder {
            Ok(encoder) => {
                self.senders.push(LinuxSender {
                    encoder: Some(encoder),
                    ..sender
                });
                Ok(())
            }
            Err(error) => {
                sender.stop();
                Err(error)
            }
        }
    }
}

impl VideoPipelineBackend for LinuxVideoPipelineBackend {
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
        let mut snapshot = Command::new("python3");
        snapshot
            .args(["-c", VIRTUAL_LAYOUT_PY, "snapshot"])
            .stdin(Stdio::null());
        configure_desktop_user_env(&mut snapshot);
        let baseline = snapshot
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());

        // X11 virtual outputs are placed to the right of the physical panel.
        let mut x_offset: u32 = super::display::LinuxVirtualDisplayBackend::new()
            .enumerate_physical_displays()?
            .iter()
            .map(|d| d.width_px)
            .sum();
        for spec in monitors {
            if let Err(error) = self.start_stream(spec, x_offset, "virtual", sink.clone()) {
                let _ = self.close_all_viewer_windows();
                return Err(error);
            }
            x_offset += spec.width_px;
        }

        if let Some(baseline) = baseline {
            let mut layout = Command::new("python3");
            layout
                .args([
                    "-c",
                    VIRTUAL_LAYOUT_PY,
                    "arrange",
                    &baseline,
                    &monitors.len().to_string(),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            configure_desktop_user_env(&mut layout);
            self.layout_child = layout.spawn().ok();
        }
        self.persist_pids();
        info!(
            streams = self.senders.len(),
            codec = ?StreamCodec::for_peer(self.caps),
            max_fps = capture_fps_limit(),
            "Started Linux native-resolution desktop streaming"
        );
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
        let mut presenters: Vec<(u8, Box<dyn FramePresenter>)> = Vec::new();
        for spec in monitors {
            let presenter = ShmPresenter::spawn(spec.index)?;
            self.viewer_pids.push(presenter.pid());
            presenters.push((spec.index, Box::new(presenter)));
        }
        let (viewers, consumer) = ViewerSet::spawn(presenters)?;
        self.viewers = Some(viewers);
        self.persist_pids();
        info!(
            viewers = monitors.len(),
            "Opened Linux fullscreen display viewers"
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
            if let Ok(Some(status)) = sender.child.try_wait() {
                return Err(PalError::VideoPipeline(format!(
                    "Desktop capture for display {} stopped ({status}). The capture helper's reason is in the daemon log; check screen-sharing permission and python3-gi/GStreamer PipeWire packages.",
                    sender.monitor + 1
                )));
            }
        }
        Ok(())
    }

    fn close_all_viewer_windows(&mut self) -> Result<(), PalError> {
        if let Some(mut child) = self.layout_child.take() {
            kill_child(&mut child);
        }
        for sender in self.senders.drain(..) {
            sender.stop();
        }
        if let Some(viewers) = self.viewers.take() {
            viewers.stop();
        }
        self.viewer_pids.clear();
        self.kill_persisted_pids();
        Ok(())
    }
}

impl Drop for LinuxVideoPipelineBackend {
    fn drop(&mut self) {
        let _ = self.close_all_viewer_windows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duplex_proto::{
        packetize_video_frame, EncodedVideoFrame, TileEncoder, TileEncoderConfig,
        VideoFrameAssembler,
    };
    use std::net::UdpSocket;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    fn isolated_backend(label: &str) -> LinuxVideoPipelineBackend {
        let mut backend = LinuxVideoPipelineBackend::new();
        backend.pids_path = std::env::temp_dir().join(format!(
            "duplex-video-test-{}-{label}.pids",
            std::process::id()
        ));
        backend
    }

    fn specs() -> Vec<DisplayDescriptor> {
        (0..2)
            .map(|index| DisplayDescriptor {
                index,
                width_px: if index == 0 { 3840 } else { 1920 },
                height_px: if index == 0 { 2160 } else { 1080 },
                refresh_hz: 60,
                scale_percent: 100,
                physical_width_mm: 300,
                physical_height_mm: 170,
                edid_block: DisplayDescriptor::synthesize_edid(1920, 1080, 60, index),
            })
            .collect()
    }

    struct CountingSink {
        tx: Mutex<std::sync::mpsc::SyncSender<EncodedVideoFrame>>,
    }

    impl VideoSink for CountingSink {
        fn try_send(&self, frame: EncodedVideoFrame) -> Result<(), crate::SinkError> {
            use std::sync::mpsc::TrySendError;
            match self.tx.lock().unwrap().try_send(frame) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(frame)) => Err(crate::SinkError::Full(frame)),
                Err(TrySendError::Disconnected(_)) => Err(crate::SinkError::Closed),
            }
        }
    }

    #[test]
    fn shared_canvas_writes_consistent_regions() {
        let mut canvas = SharedCanvas::create().unwrap();
        let (w, h) = (130u16, 70u16);
        let frame: Vec<u8> = (0..usize::from(w) * usize::from(h) * 4)
            .map(|i| (i % 251) as u8)
            .collect();
        assert!(canvas.write(w, h, &frame, &[]).unwrap());
        assert_eq!(canvas.sequence().load(Ordering::Acquire) % 2, 0);
        let stored =
            unsafe { core::slice::from_raw_parts(canvas.memory.base.add(SHM_HEADER), frame.len()) };
        assert_eq!(stored, &frame[..]);
        let mut changed = frame.clone();
        changed[(10 * usize::from(w) + 5) * 4] = 7;
        let rect = PixelRect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        };
        assert!(!canvas.write(w, h, &changed, &[rect]).unwrap());
        let stored =
            unsafe { core::slice::from_raw_parts(canvas.memory.base.add(SHM_HEADER), frame.len()) };
        assert_eq!(stored, &changed[..]);
        let header = unsafe { core::slice::from_raw_parts(canvas.memory.base, 20) };
        assert_eq!(&header[..4], b"DPXS");
        assert_eq!(
            u32::from_le_bytes(header[8..12].try_into().unwrap()),
            u32::from(w)
        );
    }

    /// Real GStreamer capture → DPXT → UDP → decoder → GTK viewer process.
    #[test]
    #[ignore = "requires python3-gi, GStreamer, GTK 4 or 3 and a display (use xvfb-run)"]
    fn synthetic_capture_udp_viewer_survives_stream_and_idle_beyond_seven_seconds() {
        let specs = specs();
        let mut viewer = isolated_backend("viewer");
        let consumer = viewer.start_viewer(&specs).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let sink: Arc<dyn VideoSink> = Arc::new(CountingSink { tx: Mutex::new(tx) });
        let mut sender = isolated_backend("sender");
        for spec in &specs {
            sender.start_stream(spec, 0, "test", sink.clone()).unwrap();
        }
        let udp_tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp_rx.set_nonblocking(true).unwrap();
        let mut assemblers = [VideoFrameAssembler::new(0), VideoFrameAssembler::new(1)];
        let mut received = [0usize; 2];
        let mut late = [0usize; 2];
        let mut datagrams = Vec::new();
        let mut scratch = [0u8; 2048];
        let mut refresh = Vec::new();
        let start = Instant::now();
        let mut next_report = Duration::from_secs(2);
        while start.elapsed() < Duration::from_secs(12) {
            if start.elapsed() >= next_report {
                eprintln!("{:?}: received units {received:?}", start.elapsed());
                next_report += Duration::from_secs(2);
            }
            for frame in rx.try_iter().take(256) {
                packetize_video_frame(&frame, &mut datagrams);
                for datagram in &datagrams {
                    udp_tx
                        .send_to(datagram, udp_rx.local_addr().unwrap())
                        .unwrap();
                }
                while let Ok((len, _)) = udp_rx.recv_from(&mut scratch) {
                    let index = usize::from(scratch[4]);
                    if let (Some(frame), _) =
                        assemblers[index].push_datagram(&scratch[..len]).unwrap()
                    {
                        consumer.submit(frame);
                        received[index] += 1;
                        if start.elapsed() > Duration::from_secs(8) {
                            late[index] += 1;
                        }
                    }
                }
            }
            viewer.poll_health(&mut refresh).unwrap();
            sender.poll_health(&mut refresh).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(received.iter().all(|&n| n > 100), "received: {received:?}");
        assert!(late.iter().all(|&n| n > 20), "late units: {late:?}");
        // A static/stalled stream must leave the last image and windows alive.
        drop(sink);
        thread::sleep(Duration::from_secs(8));
        viewer.poll_health(&mut refresh).unwrap();
        for pid in &viewer.viewer_pids {
            assert_eq!(unsafe { libc::kill(*pid as i32, 0) }, 0, "viewer exited");
        }
        let teardown = Instant::now();
        sender.close_all_viewer_windows().unwrap();
        viewer.close_all_viewer_windows().unwrap();
        assert!(teardown.elapsed() < Duration::from_secs(2));
    }

    fn cpu_seconds(pid: u32) -> f64 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .map_or("", |(_, rest)| rest)
            .split_whitespace()
            .collect();
        let ticks: f64 = fields.get(11..13).map_or(0.0, |f| {
            f.iter().filter_map(|v| v.parse::<f64>().ok()).sum()
        });
        ticks / unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64
    }

    /// Real Mutter RecordVirtual capture → encoder throughput. Optional load:
    /// DUPLEX_TEST_ANIMATE='python3 animate.py 1 photo' (started after 3 s).
    #[test]
    #[ignore = "creates a real Mutter virtual monitor; run inside a headless GNOME session"]
    fn virtual_monitor_capture_throughput() {
        let size = std::env::var("DUPLEX_TEST_SIZE").unwrap_or_else(|_| "3840x2160".into());
        let (w, h) = size
            .split_once('x')
            .map(|(w, h)| (w.parse().unwrap(), h.parse().unwrap()))
            .unwrap();
        let spec = DisplayDescriptor {
            width_px: w,
            height_px: h,
            ..specs()[0].clone()
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(512);
        let sink: Arc<dyn VideoSink> = Arc::new(CountingSink { tx: Mutex::new(tx) });
        let mut sender = isolated_backend("throughput");
        sender.start_stream(&spec, 0, "virtual", sink).unwrap();
        let capture_pid = sender.senders[0].child.id();
        let start = Instant::now();
        let mut animation = None;
        let mut decoder = duplex_proto::TileDecoder::new();
        let (mut bytes, mut units, mut frames) = (0usize, 0usize, 0usize);
        let mut cpu_start = None;
        let mut refresh = Vec::new();
        while start.elapsed() < Duration::from_secs(16) {
            if animation.is_none() && start.elapsed() > Duration::from_secs(3) {
                animation = std::env::var("DUPLEX_TEST_ANIMATE")
                    .ok()
                    .map(|command| Command::new("sh").arg("-c").arg(command).spawn().unwrap());
            }
            if cpu_start.is_none() && start.elapsed() > Duration::from_secs(6) {
                cpu_start = Some((
                    Instant::now(),
                    cpu_seconds(std::process::id()),
                    cpu_seconds(capture_pid),
                ));
            }
            for unit in rx.try_iter().take(256) {
                // Decoding here would measure this test thread, not the
                // sender: decode only until the stream size is known.
                if decoder.width() == 0 {
                    decoder.decode_slice(&unit.data);
                }
                if cpu_start.is_some() {
                    bytes += unit.data.len();
                    units += 1;
                    frames += usize::from(crate::stream::StreamDecoder::ends_frame(&unit));
                }
            }
            sender.poll_health(&mut refresh).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        let (since, own, helper) = cpu_start.unwrap();
        let seconds = since.elapsed().as_secs_f64();
        println!(
            "{w}x{h}: {:.1} frames/s, {:.0} Mbit/s, {units} units; CPU: daemon {:.0}% capture helper {:.0}% (of one core)",
            frames as f64 / seconds,
            bytes as f64 * 8.0 / seconds / 1e6,
            (cpu_seconds(std::process::id()) - own) / seconds * 100.0,
            (cpu_seconds(capture_pid) - helper) / seconds * 100.0
        );
        assert_eq!(
            (u32::from(decoder.width()), u32::from(decoder.height())),
            (w, h)
        );
        if let Some(mut animation) = animation {
            kill_child(&mut animation);
        }
        sender.close_all_viewer_windows().unwrap();
    }

    /// Feeds a 4K tile stream directly to a viewer to measure presentation.
    #[test]
    #[ignore = "requires a desktop session or xvfb-run; prints throughput"]
    fn uhd_viewer_presents_tile_stream() {
        let spec = &specs()[..1];
        let mut viewer = isolated_backend("uhd");
        let consumer = viewer.start_viewer(spec).unwrap();
        let (w, h) = (3840usize, 2160usize);
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, TileEncoderConfig::default(), 42);
        let mut frame = vec![0u8; w * h * 4];
        let start = Instant::now();
        let mut units = Vec::new();
        let mut count = 0;
        while start.elapsed() < Duration::from_secs(5) {
            for (i, pixel) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (i % w, i / w);
                let v = ((x / 8 + y / 8 + count) % 2 * 200) as u8;
                pixel.copy_from_slice(&[v, v, v, 255]);
            }
            units.clear();
            crate::stream::video_pool().install(|| {
                encoder.encode_frame(&frame, start.elapsed().as_nanos() as u64 + 1, &mut units)
            });
            for unit in units.drain(..) {
                consumer.submit(unit);
            }
            count += 1;
            thread::sleep(Duration::from_millis(16));
        }
        let mut refresh = Vec::new();
        viewer.poll_health(&mut refresh).unwrap();
        println!("submitted {count} full 4K frames in 5 s; refresh requests {refresh:?}");
        viewer.close_all_viewer_windows().unwrap();
    }
}
