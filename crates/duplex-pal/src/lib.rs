//! Platform Abstraction Layer (`duplex-pal`) for Duplex-KVM.
//!
//! Abstracts all OS-specific kernel input hooks, virtual display drivers, hardware video pipelines,
//! and shared-memory watchdog primitives behind clean, symmetric Rust traits.

pub mod net;
pub mod ring;
pub mod shm;
pub mod stream;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;

use duplex_proto::{DisplayDescriptor, EncodedVideoFrame, HidEvent, PanicTriggerKind};
use std::sync::Arc;
use thiserror::Error;

pub use net::{
    apply_udp_qos, bind_qos_tcp_listener, bind_qos_udp_socket, configure_tcp_stream_qos,
    local_link_address_available, validate_strict_interface_ip, QosClass,
};
pub use ring::SpscRingBuffer;
pub use shm::{force_terminate_process, is_process_alive, monotonic_now_ns, ShmWatchdog};

#[derive(Debug, Error)]
pub enum PalError {
    #[error("Input subsystem error: {0}")]
    Input(String),
    #[error("Virtual display subsystem error: {0}")]
    VirtualDisplay(String),
    #[error("Video pipeline error: {0}")]
    VideoPipeline(String),
    #[error("Shared memory watchdog error: {0}")]
    SharedMemory(String),
    #[error("Network QoS / socket error: {0}")]
    Network(String),
}

/// Event yielded by an active `InputBackend` capture loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturedInputEvent {
    /// Normalized USB HID keyboard or mouse event ready for wire transmission.
    Hid(HidEvent),
    /// Local hardware panic escape hatch (`Ctrl+Alt+Shift+Backspace` or `5x Escape in 1.5s`)
    /// tripped inside the capture loop. Capture has already been released locally.
    PanicTriggered(PanicTriggerKind),
}

/// Cross-platform Input Capture & Injection Backend (`evdev`/`uinput` on Linux, `SetWindowsHookExW`/`SendInput` on Windows).
pub trait InputBackend: Send {
    /// Exclusively grabs/hooks local physical keyboard and mouse/trackpad input.
    fn start_capture(&mut self) -> Result<(), PalError>;
    /// Releases all exclusive input grabs/hooks and restores local peripheral control.
    fn stop_capture(&mut self) -> Result<(), PalError>;
    /// Non-blocking poll draining captured events into `out`.
    fn poll_captured_events(
        &mut self,
        out: &mut Vec<CapturedInputEvent>,
    ) -> Result<usize, PalError>;
    /// Injects a normalized USB HID event into the local OS input stack.
    fn inject_hid_event(&mut self, event: &HidEvent) -> Result<(), PalError>;
    /// Iterates the 256-bit depressed-key bitmap (plus all 8 modifiers) and emits synthetic `KEY_UP` events.
    fn release_all_stuck_keys(&mut self) -> Result<(), PalError>;
    /// Emergency panic sequence handler: releases capture grab, sanitizes all depressed modifiers, and latches panic in SHM.
    fn emergency_panic_release(&mut self) -> Result<(), PalError>;
}

/// Cross-platform Virtual Display Controller (`IddCx` on Windows, Mutter/KWin/`vkms`/`xrandr` on Debian Linux).
pub trait VirtualDisplayBackend: Send {
    /// Queries the host OS for all connected physical monitors (excluding Duplex virtual displays)
    /// and returns their geometry, refresh rate, and 128-byte VESA EDID 1.4 descriptors.
    fn enumerate_physical_displays(&self) -> Result<Vec<DisplayDescriptor>, PalError>;
    /// Crash sweeper executed at startup: destroys any orphaned virtual monitors from a prior crash.
    fn sweep_orphaned_displays(&mut self) -> Result<usize, PalError>;
    /// Spawns or updates virtual displays to match the peer's physical monitor specifications.
    fn sync_virtual_displays(&mut self, specs: &[DisplayDescriptor]) -> Result<(), PalError>;
    /// Moves any application windows positioned on virtual display coordinates back to the primary physical monitor.
    fn rescue_windows_to_primary(&mut self) -> Result<(), PalError>;
    /// Rescues windows and detaches all virtual displays.
    fn destroy_all_virtual_displays(&mut self) -> Result<(), PalError>;
    /// Returns the slice of currently attached virtual display descriptors.
    fn active_virtual_displays(&self) -> &[DisplayDescriptor];
}

/// Video decoding capabilities advertised by the peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerVideoCaps {
    /// Legacy native-resolution zlib bands (DPXC).
    pub native_lossless: bool,
    /// DPXT lossless/JPEG tiles with lossless refinement.
    pub tiles: bool,
}

/// Why a [`VideoSink`] did not take a unit.
#[derive(Debug)]
pub enum SinkError {
    /// The link is saturated; retry later with the returned unit.
    Full(EncodedVideoFrame),
    /// The link is gone; the encoder should stop.
    Closed,
}

/// Destination for encoded video produced by a sender pipeline.
pub trait VideoSink: Send + Sync {
    /// Queues one encoded unit without blocking. Encoders retry `Full` units
    /// (skipping stale captures meanwhile) rather than dropping encoded data.
    fn try_send(&self, frame: EncodedVideoFrame) -> Result<(), SinkError>;
}

/// Accepts reassembled video units from the network receive thread.
pub trait VideoFrameConsumer: Send + Sync {
    /// Enqueues for decoding without blocking the network thread.
    fn submit(&self, frame: EncodedVideoFrame);
}

/// Screen streaming: capture + encode on the sender, decode + present on the
/// receiver. All heavy work runs on dedicated threads, never the daemon loop.
pub trait VideoPipelineBackend: Send {
    /// Selects the best codec the peer can decode (takes effect immediately).
    fn set_peer_video_caps(&mut self, caps: PeerVideoCaps);

    /// Starts capture/encoding for the specified virtual monitors.
    fn start_sender(
        &mut self,
        monitors: &[DisplayDescriptor],
        sink: Arc<dyn VideoSink>,
    ) -> Result<(), PalError>;

    /// Requests a full refresh of `monitor_index` (packet loss or viewer attach).
    fn request_keyframe(&mut self, monitor_index: u8) -> Result<(), PalError>;

    /// Opens fullscreen viewers and returns the consumer for incoming video.
    fn start_viewer(
        &mut self,
        monitors: &[DisplayDescriptor],
    ) -> Result<Arc<dyn VideoFrameConsumer>, PalError>;

    /// Reports monitors whose decoders detected loss (send them a refresh
    /// request) and fails when a capture helper or viewer stopped working.
    fn poll_health(&mut self, refresh_requests: &mut Vec<u8>) -> Result<(), PalError>;

    /// Immediately stops all encoders/decoders and destroys all viewer windows.
    fn close_all_viewer_windows(&mut self) -> Result<(), PalError>;
}

/// Instantiates the native `InputBackend` for the target operating system.
pub fn create_input_backend(
    shm: Option<Arc<ShmWatchdog>>,
) -> Result<Box<dyn InputBackend>, PalError> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxInputBackend::new(shm)?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsInputBackend::new(shm)?))
    }
}

/// Instantiates the native `VirtualDisplayBackend` for the target operating system.
pub fn create_virtual_display_backend() -> Result<Box<dyn VirtualDisplayBackend>, PalError> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxVirtualDisplayBackend::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsVirtualDisplayBackend::new()))
    }
}

/// Instantiates the native `VideoPipelineBackend` for the target operating system.
pub fn create_video_pipeline_backend() -> Result<Box<dyn VideoPipelineBackend>, PalError> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxVideoPipelineBackend::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsVideoPipelineBackend::new()))
    }
}
