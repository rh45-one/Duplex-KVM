//! `duplex-gui` — Cross-Platform Graphical Control & Live Telemetry Center for Duplex-KVM.
//!
//! Provides:
//! 1. **Native Desktop GUI Window**:
//!    - On Debian Linux (`#[cfg(target_os = "linux")]`): Native GTK+ 3 Wayland/X11 dark-themed window.
//!    - On Windows 11 (`#[cfg(target_os = "windows")]`): Native Win32 (`CreateWindowExW`) desktop window.
//! 2. **Embedded Localhost Web Dashboard (`http://127.0.0.1:48180`)**:
//!    - Runs automatically in a background thread alongside the native window (or standalone via `--web`).

pub mod web;

#[cfg(target_os = "linux")]
pub mod linux_gtk;
#[cfg(target_os = "windows")]
pub mod win32_gui;

use core::sync::atomic::Ordering;
use duplex_pal::{is_process_alive, monotonic_now_ns, ShmWatchdog};
use duplex_proto::OperationalMode;
use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct TelemetrySnapshot {
    pub daemon_online: bool,
    pub daemon_pid: u32,
    pub mode: OperationalMode,
    pub mode_code: u8,
    pub mode_label: &'static str,
    pub mode_subtitle: &'static str,
    pub peer_connected: bool,
    pub input_grab_active: bool,
    pub virtual_displays_active: u8,
    pub daemon_age_ms: f64,
    pub peer_age_ms: f64,
    pub min_rtt_us: f64,
    pub last_rtt_us: f64,
    pub max_rtt_us: f64,
    pub tx_hid_packets: u64,
    pub rx_hid_packets: u64,
    pub tx_video_frames: u64,
    pub rx_video_frames: u64,
    pub pli_keyframes_requested: u64,
}

impl TelemetrySnapshot {
    pub fn describe_mode(mode: OperationalMode) -> (&'static str, &'static str) {
        match mode {
            OperationalMode::Mode0Idle => (
                "MODE 0: IDLE (LOCAL ONLY)",
                "Local peripherals & displays independent; 1GbE internet passthrough active",
            ),
            OperationalMode::Mode1PcPrimary {
                extend_display: false,
            } => (
                "MODE 1: PC PRIMARY (KVM INPUT ONLY)",
                "Debian Laptop keyboard & trackpad control Windows 11 Desktop",
            ),
            OperationalMode::Mode1PcPrimary {
                extend_display: true,
            } => (
                "MODE 1+: PC PRIMARY (+ EXTENDED DISPLAY)",
                "Laptop controls Windows PC + streams 1 Windows virtual display to Laptop",
            ),
            OperationalMode::Mode2LaptopPrimary => (
                "MODE 2: LAPTOP PRIMARY (DUAL-MONITOR DOCK)",
                "Windows USB peripherals & 2 physical monitors docked to Debian Laptop",
            ),
        }
    }

    pub fn read_current() -> Self {
        if let Ok(shm) = ShmWatchdog::open(false) {
            let block = shm.block();
            if block.is_valid() {
                let now_ns = monotonic_now_ns();
                let pid = block.daemon_pid.load(Ordering::Acquire);
                let daemon_hb = block.daemon_heartbeat_ns.load(Ordering::Acquire);
                let daemon_age_ms = now_ns.saturating_sub(daemon_hb) as f64 / 1_000_000.0;
                let alive = pid > 0 && is_process_alive(pid) && daemon_age_ms < 2_500.0;

                let mode_code = block.active_mode.load(Ordering::Acquire);
                let mode = OperationalMode::from_u8_lossy(mode_code);
                let (mode_label, mode_subtitle) = Self::describe_mode(mode);

                let peer_hb = block.peer_heartbeat_ns.load(Ordering::Acquire);
                let peer_age_ms = if peer_hb > 0 {
                    now_ns.saturating_sub(peer_hb) as f64 / 1_000_000.0
                } else {
                    0.0
                };

                let raw_min = block.min_rtt_ns.load(Ordering::Acquire);
                let min_rtt_us = if raw_min == u64::MAX {
                    0.0
                } else {
                    raw_min as f64 / 1_000.0
                };

                return Self {
                    daemon_online: alive,
                    daemon_pid: pid,
                    mode,
                    mode_code: mode.as_u8(),
                    mode_label,
                    mode_subtitle,
                    peer_connected: alive && block.peer_connected.load(Ordering::Acquire),
                    input_grab_active: alive && block.input_grab_active.load(Ordering::Acquire),
                    virtual_displays_active: if alive {
                        block.virtual_displays_active.load(Ordering::Acquire)
                    } else {
                        0
                    },
                    daemon_age_ms: if alive { daemon_age_ms } else { 0.0 },
                    peer_age_ms: if alive { peer_age_ms } else { 0.0 },
                    min_rtt_us,
                    last_rtt_us: block.last_rtt_ns.load(Ordering::Acquire) as f64 / 1_000.0,
                    max_rtt_us: block.max_rtt_ns.load(Ordering::Acquire) as f64 / 1_000.0,
                    tx_hid_packets: block.tx_hid_packets.load(Ordering::Relaxed),
                    rx_hid_packets: block.rx_hid_packets.load(Ordering::Relaxed),
                    tx_video_frames: block.tx_video_frames.load(Ordering::Relaxed),
                    rx_video_frames: block.rx_video_frames.load(Ordering::Relaxed),
                    pli_keyframes_requested: block.pli_keyframes_requested.load(Ordering::Relaxed),
                };
            }
        }

        let (mode_label, mode_subtitle) = Self::describe_mode(OperationalMode::Mode0Idle);
        Self {
            daemon_online: false,
            daemon_pid: 0,
            mode: OperationalMode::Mode0Idle,
            mode_code: 0,
            mode_label,
            mode_subtitle,
            peer_connected: false,
            input_grab_active: false,
            virtual_displays_active: 0,
            daemon_age_ms: 0.0,
            peer_age_ms: 0.0,
            min_rtt_us: 0.0,
            last_rtt_us: 0.0,
            max_rtt_us: 0.0,
            tx_hid_packets: 0,
            rx_hid_packets: 0,
            tx_video_frames: 0,
            rx_video_frames: 0,
            pli_keyframes_requested: 0,
        }
    }

    pub fn to_json(&self) -> String {
        format!(
            "{{\"daemon_online\":{},\"daemon_pid\":{},\"mode_code\":{},\"mode_label\":\"{}\",\
             \"mode_subtitle\":\"{}\",\"peer_connected\":{},\"input_grab_active\":{},\
             \"virtual_displays_active\":{},\"daemon_age_ms\":{:.2},\"peer_age_ms\":{:.2},\
             \"min_rtt_us\":{:.2},\"last_rtt_us\":{:.2},\"max_rtt_us\":{:.2},\
             \"tx_hid_packets\":{},\"rx_hid_packets\":{},\"tx_video_frames\":{},\
             \"rx_video_frames\":{},\"pli_keyframes_requested\":{}}}",
            self.daemon_online,
            self.daemon_pid,
            self.mode_code,
            self.mode_label,
            self.mode_subtitle,
            self.peer_connected,
            self.input_grab_active,
            self.virtual_displays_active,
            self.daemon_age_ms,
            self.peer_age_ms,
            self.min_rtt_us,
            self.last_rtt_us,
            self.max_rtt_us,
            self.tx_hid_packets,
            self.rx_hid_packets,
            self.tx_video_frames,
            self.rx_video_frames,
            self.pli_keyframes_requested,
        )
    }
}

pub fn request_mode(target: OperationalMode) -> Result<String, String> {
    match ShmWatchdog::open(false) {
        Ok(shm) => {
            let block = shm.block();
            if !block.is_valid() {
                return Err(
                    "duplexd shared memory is not initialized — click 'Start Local Daemon' first"
                        .to_string(),
                );
            }
            block.requested_mode.store(target.as_u8(), Ordering::SeqCst);
            let (lbl, _) = TelemetrySnapshot::describe_mode(target);
            Ok(format!("Requested transition -> {lbl}"))
        }
        Err(_) => Err(
            "duplexd is not running — click 'Start Local Daemon' first".to_string(),
        ),
    }
}

pub fn trigger_panic_release() -> Result<String, String> {
    match ShmWatchdog::open(false) {
        Ok(shm) => {
            let block = shm.block();
            block.panic_latched.store(true, Ordering::SeqCst);
            block
                .requested_mode
                .store(OperationalMode::Mode0Idle.as_u8(), Ordering::SeqCst);
            Ok(
                "EMERGENCY PANIC LATCHED — Reclaiming input grabs, modifiers & virtual displays"
                    .to_string(),
            )
        }
        Err(_) => Err("duplexd shared memory not active (already in local mode)".to_string()),
    }
}

pub fn start_local_daemon() -> Result<String, String> {
    let snap = TelemetrySnapshot::read_current();
    if snap.daemon_online {
        return Ok(format!(
            "duplexd is already running (PID {})",
            snap.daemon_pid
        ));
    }

    let bin_name = if cfg!(target_os = "windows") {
        "duplexd.exe"
    } else {
        "duplexd"
    };

    let candidate: PathBuf = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(bin_name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(bin_name));

    match Command::new(&candidate)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => Ok(format!("Started local duplexd daemon (PID {})", child.id())),
        Err(e) => Err(format!("Failed to spawn {}: {e}", candidate.display())),
    }
}

pub fn open_url_in_browser(url: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let web_only = args.iter().any(|a| a == "--web");
    let no_browser = args.iter().any(|a| a == "--no-browser");
    let self_test = args.iter().any(|a| a == "--self-test");

    let mut port = web::DEFAULT_WEB_PORT;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--port" && i + 1 < args.len() {
            if let Ok(p) = args[i + 1].parse::<u16>() {
                port = p;
            }
            i += 1;
        }
        i += 1;
    }

    if self_test {
        let snap = TelemetrySnapshot::read_current();
        let json = snap.to_json();
        assert!(json.contains("\"mode_code\":"));
        println!("duplex-gui self-test passed: {json}");
        return;
    }

    if web_only {
        web::run_web_server_blocking(port, !no_browser);
        return;
    }

    // Spawn the embedded localhost Web UI in the background so both native window and browser work
    let bound_port = web::spawn_background_web_server(port)
        .map(|a| a.port())
        .unwrap_or(port);

    #[cfg(target_os = "linux")]
    {
        if !linux_gtk::run_native_gtk_window(bound_port) {
            eprintln!(
                "No Wayland/X11 display detected; running Duplex-KVM Web UI on http://127.0.0.1:{bound_port}"
            );
            web::run_web_server_blocking(bound_port, false);
        }
    }

    #[cfg(target_os = "windows")]
    {
        if !win32_gui::run_native_win32_window(bound_port) {
            web::run_web_server_blocking(bound_port, !no_browser);
        }
    }
}
