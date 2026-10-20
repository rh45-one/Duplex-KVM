#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
//! `duplex-kvm` — All-in-One Single-File Executable (GUI + Daemon + 250ms Watchdog + CLI + 1-Click Network Setup).
//!
//! Everything required to run Duplex-KVM on either Debian Linux or Windows 11 is bundled inside
//! this single standalone executable:
//! - **Double-click / run with no arguments**: Launches the Native Desktop Graphical Control Center
//!   (GTK+ 3 on Linux Wayland/X11, Win32 on Windows 11) + background Web UI (`http://127.0.0.1:48180`).
//! - **`--daemon` / `daemon`**: Runs the symmetric KVM, virtual display & screen streaming daemon (`duplexd`).
//! - **`--guard` / `guard`**: Runs the 250ms Dead-Man's Switch hardware watchdog (`duplex-guard`).
//! - **`--setup` / `setup`**: Executes the embedded OS network & permissions setup script.
//! - **`status` | `watch` | `mode` | `panic`**: Operator CLI subcommands.

pub mod design;
mod diagnostics;
pub mod updater;
pub mod web;

#[cfg(target_os = "linux")]
pub mod linux_gtk;
#[cfg(target_os = "windows")]
pub mod win32_gui;
#[cfg(target_os = "windows")]
mod windows_setup;

use core::sync::atomic::Ordering;
use duplex_pal::{is_process_alive, monotonic_now_ns, ShmWatchdog};
use duplex_proto::OperationalMode;
use std::env;
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const EMBEDDED_WINDOWS_SETUP_PS1: &str =
    include_str!("../../../scripts/windows-setup-passthrough.ps1");
const EMBEDDED_DEBIAN_SETUP_SH: &str = include_str!("../../../scripts/debian-setup-link.sh");

/// Mode register used only in demo mode (`DUPLEX_UI_DEMO`), so UI previews never write to the
/// real shared-memory block.
static DEMO_MODE_CODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(1);

/// `DUPLEX_UI_DEMO` value, read once: `1` / `online` (linked), `standby` (peer down), `offline`.
fn demo_variant() -> Option<&'static str> {
    static VARIANT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    VARIANT
        .get_or_init(|| {
            env::var("DUPLEX_UI_DEMO")
                .ok()
                .map(|v| v.trim().to_ascii_lowercase())
                .filter(|v| !v.is_empty() && v != "0")
        })
        .as_deref()
}

/// `true` when the UI runs on synthetic telemetry (`DUPLEX_UI_DEMO=1|standby|offline`).
pub fn demo_mode_enabled() -> bool {
    demo_variant().is_some()
}

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
                "Local peripherals and displays independent; internet sharing uses the network configuration",
            ),
            OperationalMode::Mode1PcPrimary {
                extend_display: false,
            } => (
                "MODE 1: PC PRIMARY (KVM INPUT ONLY)",
                "Debian Laptop keyboard and trackpad control Windows 11 Desktop",
            ),
            OperationalMode::Mode1PcPrimary {
                extend_display: true,
            } => (
                "MODE 1+: PC PRIMARY (+ EXTENDED DISPLAY)",
                "Laptop controls Windows PC + streams 1 Windows virtual display to Laptop",
            ),
            OperationalMode::Mode2LaptopPrimary => (
                "MODE 2: LAPTOP PRIMARY (DUAL-MONITOR DOCK)",
                "Windows USB peripherals and 2 physical monitors docked to Debian Laptop",
            ),
        }
    }

    /// Synthetic, gently fluctuating telemetry used when `DUPLEX_UI_DEMO=1` is set. Lets every
    /// front-end be previewed and screenshotted without touching the real shared-memory block.
    pub fn demo() -> Self {
        let mode = OperationalMode::from_u8_lossy(DEMO_MODE_CODE.load(Ordering::Relaxed));
        let (mode_label, mode_subtitle) = Self::describe_mode(mode);
        let variant = demo_variant().unwrap_or("1");
        if variant == "offline" {
            return Self::offline();
        }
        let linked = variant != "standby";
        let t = monotonic_now_ns() as f64 / 1_000_000_000.0;
        let wobble = (t * 1.7).sin() * 0.5 + (t * 0.43).sin() * 0.5;
        let last_rtt_us = 182.0 + wobble * 46.0 + ((t * 9.1).sin() * 9.0);
        let ticks = (t * 125.0) as u64;
        let active = linked && !matches!(mode, OperationalMode::Mode0Idle);
        Self {
            daemon_online: true,
            daemon_pid: 4211,
            mode,
            mode_code: mode.as_u8(),
            mode_label,
            mode_subtitle,
            peer_connected: linked,
            input_grab_active: active,
            virtual_displays_active: match (linked, mode) {
                (
                    true,
                    OperationalMode::Mode1PcPrimary {
                        extend_display: true,
                    },
                ) => 1,
                (true, OperationalMode::Mode2LaptopPrimary) => 2,
                _ => 0,
            },
            daemon_age_ms: 2.0 + (t * 3.3).sin().abs() * 6.0,
            peer_age_ms: if linked {
                9.0 + (t * 2.1).sin().abs() * 24.0
            } else {
                0.0
            },
            min_rtt_us: if linked { 121.0 } else { 0.0 },
            last_rtt_us: if linked { last_rtt_us } else { 0.0 },
            max_rtt_us: if linked { 418.0 } else { 0.0 },
            tx_hid_packets: 120_034 + if linked { ticks } else { 0 },
            rx_hid_packets: 118_902 + if linked { ticks / 2 } else { 0 },
            tx_video_frames: if active { 48_311 + ticks / 2 } else { 48_311 },
            rx_video_frames: 47_990,
            pli_keyframes_requested: 3,
        }
    }

    /// Snapshot reported when no daemon shared-memory block is available.
    pub fn offline() -> Self {
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

    pub fn read_current() -> Self {
        if demo_mode_enabled() {
            return Self::demo();
        }
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

        Self::offline()
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
    if demo_mode_enabled() {
        if demo_variant() == Some("offline") {
            return Err("[DEMO] Daemon is not running — start the local daemon first".to_string());
        }
        DEMO_MODE_CODE.store(target.as_u8(), Ordering::Relaxed);
        let (lbl, _) = TelemetrySnapshot::describe_mode(target);
        return Ok(format!("[DEMO] Requested transition -> {lbl}"));
    }

    if !TelemetrySnapshot::read_current().daemon_online {
        start_local_daemon()?;
        for _ in 0..12 {
            thread::sleep(Duration::from_millis(50));
            if TelemetrySnapshot::read_current().daemon_online {
                break;
            }
        }
    }

    let snapshot = TelemetrySnapshot::read_current();
    if target != OperationalMode::Mode0Idle && !snapshot.peer_connected {
        return Err(diagnostics::link_status(&snapshot).message);
    }

    match ShmWatchdog::open(false) {
        Ok(shm) => {
            let block = shm.block();
            if !block.is_valid() {
                return Err(
                    "Daemon shared memory is not initialized — click 'Start Local Daemon' first"
                        .to_string(),
                );
            }
            block.requested_mode.store(target.as_u8(), Ordering::SeqCst);
            let (lbl, _) = TelemetrySnapshot::describe_mode(target);
            Ok(format!("Requested transition -> {lbl}"))
        }
        Err(_) => Err("Daemon is not running — click 'Start Local Daemon' first".to_string()),
    }
}

pub fn trigger_panic_release() -> Result<String, String> {
    if demo_mode_enabled() {
        DEMO_MODE_CODE.store(OperationalMode::Mode0Idle.as_u8(), Ordering::Relaxed);
        return Ok("[DEMO] Emergency panic latched — input grabs released".to_string());
    }
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
        Err(_) => Err("Daemon shared memory not active (already in local mode)".to_string()),
    }
}

/// Spawns the built-in daemon (`--daemon`) directly from this single executable.
pub fn start_local_daemon() -> Result<String, String> {
    if demo_mode_enabled() {
        return Ok("[DEMO] Local daemon is already running (PID 4211)".to_string());
    }
    static LAUNCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _launch = LAUNCH_LOCK
        .try_lock()
        .map_err(|_| "The local daemon is already starting; wait for its status.".to_string())?;
    let snap = TelemetrySnapshot::read_current();
    if snap.daemon_online {
        return Ok(format!(
            "Local daemon is already running (PID {})",
            snap.daemon_pid
        ));
    }

    duplexd::validate_link_key_config()?;

    let self_exe = env::current_exe().map_err(|e| format!("Could not resolve executable: {e}"))?;

    let log_dir = updater::get_config_dir().join("logs");
    fs::create_dir_all(&log_dir).map_err(|e| format!("Cannot create daemon log directory: {e}"))?;
    let log_path = log_dir.join("daemon.log");
    if log_path.exists() {
        fs::copy(&log_path, log_dir.join("daemon.previous.log"))
            .map_err(|e| format!("Cannot preserve previous daemon log: {e}"))?;
    }
    let log = fs::File::create(&log_path).map_err(|e| format!("Cannot create daemon log: {e}"))?;
    let error_log = log
        .try_clone()
        .map_err(|e| format!("Cannot open daemon error log: {e}"))?;

    match Command::new(&self_exe)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .spawn()
    {
        Ok(mut child) => {
            let pid = child.id();
            // Keep concurrent GUI/web clicks serialized until the child has
            // registered its identity, and report immediate startup failures.
            for _ in 0..40 {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    return Err(format!(
                        "Daemon exited during startup ({status}). Log: {}",
                        log_path.display()
                    ));
                }
                let status = TelemetrySnapshot::read_current();
                if status.daemon_pid == pid && status.daemon_online {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            // Reap the child when it exits; an unreaped PID is not a daemon.
            thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(format!(
                "Started built-in Duplex-KVM daemon + watchdog (PID {pid}). Log: {}",
                log_path.display()
            ))
        }
        Err(e) => Err(format!("Failed to start built-in daemon: {e}")),
    }
}

/// Apply a replaced executable without terminating an active input session.
pub fn restart_local_daemon() -> Result<String, String> {
    if demo_mode_enabled() {
        return start_local_daemon();
    }
    duplexd::validate_link_key_config()?;
    let snapshot = TelemetrySnapshot::read_current();
    if snapshot.daemon_online {
        if snapshot.mode != OperationalMode::Mode0Idle
            || snapshot.input_grab_active
            || snapshot.virtual_displays_active != 0
        {
            return Err("Return to Idle before restarting the daemon.".into());
        }
        if !duplex_pal::force_terminate_process(snapshot.daemon_pid) {
            return Err("Could not stop the Idle daemon.".into());
        }
        let reader = ShmWatchdog::open(false).map_err(|e| e.to_string())?;
        let mut released = false;
        for _ in 0..150 {
            if !is_process_alive(snapshot.daemon_pid) && reader.daemon_ownership_available() {
                released = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        if !released {
            return Err(
                "The old daemon is still closing its devices; wait and click Start daemon.".into(),
            );
        }
    }
    start_local_daemon()
}

/// Extracts and runs the embedded OS network & permissions setup script.
pub fn run_embedded_network_setup() -> Result<String, String> {
    if demo_mode_enabled() {
        return Ok("[DEMO] Network setup skipped (192.168.137.0/24)".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        windows_setup::request_elevated_setup()
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        let script_path = env::temp_dir().join("duplex-kvm-debian-setup.sh");
        fs::write(&script_path, EMBEDDED_DEBIAN_SETUP_SH)
            .map_err(|e| format!("Failed to write temp setup script: {e}"))?;
        let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755));

        let is_root = unsafe { libc::geteuid() == 0 };
        let res = if is_root {
            Command::new("bash")
                .arg(&script_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        } else {
            Command::new("pkexec")
                .arg("bash")
                .arg(&script_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .or_else(|_| {
                    Command::new("sudo")
                        .arg("-n")
                        .arg("bash")
                        .arg(&script_path)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                })
        };

        match res {
            Ok(_) => Ok(
                "Launched Debian manual KVM profile & udev setup. Normal Ethernet/Wi-Fi preserved; select duplex-kvm-1gbe explicitly for the PC link."
                    .to_string(),
            ),
            Err(e) => Err(format!("Failed to launch setup script: {e}")),
        }
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
        windows_setup::open_browser(url);
    }
}

fn print_cli_telemetry() {
    let s = TelemetrySnapshot::read_current();
    println!("============================================================");
    println!(
        " Duplex-KVM Live Telemetry (Daemon Online: {}, PID: {})",
        s.daemon_online, s.daemon_pid
    );
    println!("============================================================");
    println!("  Operational Mode    : {}", s.mode_label);
    println!(
        "  Peer Link Connected : {} (250ms Dead-Man's Switch)",
        s.peer_connected
    );
    println!("  Input Grab Active   : {}", s.input_grab_active);
    println!(
        "  Link diagnosis     : {}",
        diagnostics::link_status(&s).message
    );
    println!("  Virtual Displays    : {}", s.virtual_displays_active);
    println!("  Daemon Heartbeat Age: {:.2} ms", s.daemon_age_ms);
    println!("  Peer Heartbeat Age  : {:.2} ms", s.peer_age_ms);
    println!(
        "  Link RTT (min/cur/max): {:.1} µs / {:.1} µs / {:.1} µs",
        s.min_rtt_us, s.last_rtt_us, s.max_rtt_us
    );
    println!(
        "  HID Batches (TX/RX) : {} / {} (UDP 48100 DSCP EF 0xB8)",
        s.tx_hid_packets, s.rx_hid_packets
    );
    println!(
        "  Video Frames (TX/RX): {} / {} (UDP 48102-3 DSCP AF41 0x88)",
        s.tx_video_frames, s.rx_video_frames
    );
    println!("  PLI Keyframe Recover: {}", s.pli_keyframes_requested);
    println!("============================================================");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    #[cfg(target_os = "windows")]
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--daemon" | "daemon" | "--guard" | "guard"))
    {
        unsafe {
            use windows::Win32::UI::HiDpi::{
                SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            };
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }

    #[cfg(target_os = "windows")]
    if args.get(1).map(String::as_str) == Some("--setup-elevated") {
        windows_setup::run_elevated_setup();
        return;
    }

    if let Some(first) = args.get(1).map(|s| s.as_str()) {
        match first {
            "--daemon" | "daemon" => {
                duplexd::run_daemon_with_args(&args);
                return;
            }
            "--guard" | "guard" => {
                duplex_guard::run_guard_with_args(&args);
                return;
            }
            "--setup" | "setup" => {
                match run_embedded_network_setup() {
                    Ok(msg) => println!("{msg}"),
                    Err(err) => {
                        eprintln!("{err}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            "status" => {
                print_cli_telemetry();
                return;
            }
            "watch" => loop {
                print!("\x1B[2J\x1B[H");
                print_cli_telemetry();
                thread::sleep(Duration::from_millis(100));
            },
            "panic" => {
                match trigger_panic_release() {
                    Ok(msg) => println!("{msg}"),
                    Err(err) => eprintln!("{err}"),
                }
                return;
            }
            "mode" => {
                let extend_flag = !args
                    .iter()
                    .any(|a| a == "--no-display" || a == "--input-only");
                let target = match args.get(2).map(|s| s.as_str()).unwrap_or("idle") {
                    "pc" | "1" => OperationalMode::Mode1PcPrimary {
                        extend_display: extend_flag,
                    },
                    "laptop" | "2" => OperationalMode::Mode2LaptopPrimary,
                    _ => OperationalMode::Mode0Idle,
                };
                match request_mode(target) {
                    Ok(msg) => println!("{msg}"),
                    Err(err) => {
                        eprintln!("{err}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            "update" | "--update" => {
                let sub = args.get(2).map(|s| s.as_str()).unwrap_or("check");
                match sub {
                    "install" | "apply" | "now" => {
                        println!("Checking and installing latest Duplex-KVM release...");
                        match updater::install_update() {
                            Ok(msg) => println!("{msg}"),
                            Err(e) => {
                                eprintln!("Update failed: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    _ => {
                        let repo = updater::AppConfig::load().effective_repo();
                        println!("Checking for updates from https://github.com/{repo}...");
                        match updater::check_for_updates(true) {
                            Ok(status) => {
                                println!("Current Version : {}", status.current_version);
                                println!(
                                    "Latest Release  : {}",
                                    status
                                        .latest_version
                                        .unwrap_or_else(|| "Unknown".to_string())
                                );
                                if status.update_available {
                                    println!("Status          : UPDATE AVAILABLE!");
                                    println!(
                                        "Run 'duplex-kvm update install' to download and install."
                                    );
                                    if let Some(notes) = status.release_notes {
                                        println!("\nRelease Notes:\n{notes}");
                                    }
                                } else {
                                    println!("Status          : Duplex-KVM is up to date.");
                                }
                            }
                            Err(e) => {
                                eprintln!("Error checking for updates: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                }
                return;
            }
            "--check-update" => {
                match updater::check_for_updates(true) {
                    Ok(status) => {
                        if status.update_available {
                            println!(
                                "Update available: {}",
                                status.latest_version.unwrap_or_default()
                            );
                        } else {
                            println!("Up to date ({})", status.current_version);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error checking for updates: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            "config" => {
                let sub = args.get(2).map(|s| s.as_str()).unwrap_or("show");
                let mut cfg = updater::AppConfig::load();
                match sub {
                    "set-token" => {
                        if let Some(token) = args.get(3) {
                            cfg.github_token = Some(token.trim().to_string());
                            match cfg.save() {
                                Ok(()) => println!(
                                    "GitHub Personal Access Token saved successfully to {:?}.\nMasked: {}",
                                    updater::get_config_path(),
                                    updater::mask_token(token)
                                ),
                                Err(e) => {
                                    eprintln!("Failed to save config: {e}");
                                    std::process::exit(1);
                                }
                            }
                        } else {
                            eprintln!("Usage: duplex-kvm config set-token <ghp_your_token>");
                            std::process::exit(1);
                        }
                    }
                    "set-repo" => {
                        if let Some(repo) = args.get(3) {
                            cfg.repo = repo.trim().to_string();
                            match cfg.save() {
                                Ok(()) => println!(
                                    "Repository updated to '{}' in {:?}",
                                    repo,
                                    updater::get_config_path()
                                ),
                                Err(e) => {
                                    eprintln!("Failed to save config: {e}");
                                    std::process::exit(1);
                                }
                            }
                        } else {
                            eprintln!("Usage: duplex-kvm config set-repo <owner/repo>");
                            std::process::exit(1);
                        }
                    }
                    _ => {
                        println!(
                            "Duplex-KVM Configuration ({:?}):",
                            updater::get_config_path()
                        );
                        println!("  Repository          : {}", cfg.effective_repo());
                        println!(
                            "  GitHub Token        : {}",
                            cfg.masked_token().unwrap_or_else(|| {
                                "Not configured (set via 'duplex-kvm config set-token <tok>' or DUPLEX_GITHUB_TOKEN)".to_string()
                            })
                        );
                        println!("  Auto Check Updates  : {}", cfg.auto_check_updates);
                        println!("  Auto Install Updates: {}", cfg.auto_install_updates);
                    }
                }
                return;
            }
            _ => {}
        }
    }

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
        assert!(!EMBEDDED_WINDOWS_SETUP_PS1.is_empty());
        assert!(!EMBEDDED_DEBIAN_SETUP_SH.is_empty());
        println!("duplex-kvm all-in-one self-test passed: {json}");
        return;
    }

    // Start background update checker
    updater::start_background_checker();

    // Input capture and network control start only after an explicit request.
    if args.iter().any(|a| a == "--autostart") && !args.iter().any(|a| a == "--no-autostart") {
        let _ = start_local_daemon();
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
