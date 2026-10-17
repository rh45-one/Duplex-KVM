//! `duplex-ctl` — Operator CLI for symmetric mode switching, emergency unlock, and live link telemetry.
//!
//! Supported commands:
//! - `duplex-ctl status`                       : Print snapshot of mode, RTT min/last/max, HID & Video counters
//! - `duplex-ctl watch`                        : Live 10 Hz updating terminal telemetry dashboard
//! - `duplex-ctl mode idle`                    : Switch both peers to `MODE_0_IDLE`
//! - `duplex-ctl mode pc [--extend|--no-display]` : Switch to `MODE_1_PC_PRIMARY` (Debian controls Windows + optional extended display)
//! - `duplex-ctl mode laptop`                  : Switch to `MODE_2_LAPTOP_PRIMARY` (Windows dual monitors & peripherals docked to Debian)
//! - `duplex-ctl panic`                        : Immediately trip the Hardware Panic Escape Hatch

use core::sync::atomic::Ordering;
use duplex_pal::{monotonic_now_ns, ShmWatchdog};
use duplex_proto::{OperationalMode, ShmWatchdogBlock};
use std::env;
use std::thread;
use std::time::Duration;

fn print_usage() {
    eprintln!(
        "Duplex-KVM Control & Telemetry CLI (duplex-ctl)\n\n\
         Usage:\n  \
           duplex-ctl status\n  \
           duplex-ctl watch\n  \
           duplex-ctl mode <idle | pc [--extend | --no-display] | laptop>\n  \
           duplex-ctl panic"
    );
}

fn render_telemetry(block: &ShmWatchdogBlock) {
    let now_ns = monotonic_now_ns();
    let pid = block.daemon_pid.load(Ordering::Acquire);
    let mode = OperationalMode::from_u8_lossy(block.active_mode.load(Ordering::Acquire));
    let peer_ok = block.peer_connected.load(Ordering::Acquire);
    let grab = block.input_grab_active.load(Ordering::Acquire);
    let vdisp = block.virtual_displays_active.load(Ordering::Acquire);

    let daemon_age_ms =
        now_ns.saturating_sub(block.daemon_heartbeat_ns.load(Ordering::Acquire)) as f64 / 1_000_000.0;
    let peer_age_ms =
        now_ns.saturating_sub(block.peer_heartbeat_ns.load(Ordering::Acquire)) as f64 / 1_000_000.0;

    let last_rtt_us = block.last_rtt_ns.load(Ordering::Acquire) as f64 / 1_000.0;
    let raw_min_rtt = block.min_rtt_ns.load(Ordering::Acquire);
    let min_rtt_us = if raw_min_rtt == u64::MAX {
        0.0
    } else {
        raw_min_rtt as f64 / 1_000.0
    };
    let max_rtt_us = block.max_rtt_ns.load(Ordering::Acquire) as f64 / 1_000.0;

    let tx_hid = block.tx_hid_packets.load(Ordering::Relaxed);
    let rx_hid = block.rx_hid_packets.load(Ordering::Relaxed);
    let tx_vid = block.tx_video_frames.load(Ordering::Relaxed);
    let rx_vid = block.rx_video_frames.load(Ordering::Relaxed);
    let pli_cnt = block.pli_keyframes_requested.load(Ordering::Relaxed);

    println!("============================================================");
    println!(" Duplex-KVM Live Telemetry (duplexd PID: {pid})");
    println!("============================================================");
    println!("  Operational Mode    : {mode:?}");
    println!("  Peer Link Connected : {peer_ok} (250ms Dead-Man's Switch)");
    println!("  Input Grab Active   : {grab}");
    println!("  Virtual Displays    : {vdisp}");
    println!("  Daemon Heartbeat Age: {daemon_age_ms:.2} ms");
    println!("  Peer Heartbeat Age  : {peer_age_ms:.2} ms");
    println!(
        "  Link RTT (min/cur/max): {min_rtt_us:.1} µs / {last_rtt_us:.1} µs / {max_rtt_us:.1} µs"
    );
    println!("  HID Batches (TX/RX) : {tx_hid} / {rx_hid} (UDP 48100 DSCP EF 0xB8)");
    println!("  Video Frames (TX/RX): {tx_vid} / {rx_vid} (UDP 48102-3 DSCP AF41 0x88)");
    println!("  PLI Keyframe Recover: {pli_cnt}");
    println!("============================================================");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "status" => match ShmWatchdog::open(false) {
            Ok(shm) => render_telemetry(shm.block()),
            Err(e) => {
                eprintln!("duplexd shared memory block not active: {e}");
                std::process::exit(1);
            }
        },
        "watch" => match ShmWatchdog::open(false) {
            Ok(shm) => loop {
                print!("\x1B[2J\x1B[H");
                render_telemetry(shm.block());
                thread::sleep(Duration::from_millis(100));
            },
            Err(e) => {
                eprintln!("duplexd shared memory block not active: {e}");
                std::process::exit(1);
            }
        },
        "panic" => match ShmWatchdog::open(false) {
            Ok(shm) => {
                shm.block().panic_latched.store(true, Ordering::SeqCst);
                println!("Latched hardware panic escape hatch in shared memory (duplex-guard will reclaim in <= 5ms).");
            }
            Err(e) => {
                eprintln!("Failed to open shared memory watchdog: {e}");
                std::process::exit(1);
            }
        },
        "mode" => {
            if args.len() < 3 {
                print_usage();
                std::process::exit(1);
            }
            let extend_flag = !args.iter().any(|a| a == "--no-display" || a == "--input-only");
            let target = match args[2].as_str() {
                "idle" | "0" => OperationalMode::Mode0Idle,
                "pc" | "1" => OperationalMode::Mode1PcPrimary {
                    extend_display: extend_flag,
                },
                "laptop" | "2" => OperationalMode::Mode2LaptopPrimary,
                _ => {
                    print_usage();
                    std::process::exit(1);
                }
            };
            if let Ok(shm) = ShmWatchdog::open(false) {
                shm.block()
                    .requested_mode
                    .store(target.as_u8(), Ordering::SeqCst);
                println!("Signaled duplexd to transition peer link to {target:?}");
            } else {
                eprintln!("duplexd is not running (shared memory block unavailable)");
                std::process::exit(1);
            }
        }
        _ => {
            print_usage();
            std::process::exit(1);
        }
    }
}
