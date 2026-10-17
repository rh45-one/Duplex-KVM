//! `duplex-ctl` — Operator CLI for mode switching, emergency unlock, and latency telemetry.

use core::sync::atomic::Ordering;
use duplex_pal::{monotonic_now_ns, ShmWatchdog};
use duplex_proto::OperationalMode;
use std::env;

fn print_usage() {
    eprintln!(
        "Usage:\n  \
         duplex-ctl status\n  \
         duplex-ctl mode <idle | pc | laptop>\n  \
         duplex-ctl panic"
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "status" => match ShmWatchdog::open(false) {
            Ok(shm) => {
                let block = shm.block();
                let now_ns = monotonic_now_ns();
                let pid = block.daemon_pid.load(Ordering::Acquire);
                let mode = OperationalMode::from_u8_lossy(block.active_mode.load(Ordering::Acquire));
                let grab = block.input_grab_active.load(Ordering::Acquire);
                let vdisp = block.virtual_displays_active.load(Ordering::Acquire);
                let daemon_age_ms =
                    now_ns.saturating_sub(block.daemon_heartbeat_ns.load(Ordering::Acquire)) as f64
                        / 1_000_000.0;
                let peer_age_ms =
                    now_ns.saturating_sub(block.peer_heartbeat_ns.load(Ordering::Acquire)) as f64
                        / 1_000_000.0;
                let rtt_us = block.last_rtt_ns.load(Ordering::Acquire) as f64 / 1_000.0;

                println!("Duplex-KVM Live Telemetry:");
                println!("  Daemon PID          : {pid}");
                println!("  Active Mode         : {mode:?}");
                println!("  Input Grab Active   : {grab}");
                println!("  Virtual Displays    : {vdisp}");
                println!("  Daemon Heartbeat Age: {daemon_age_ms:.2} ms");
                println!("  Peer Heartbeat Age  : {peer_age_ms:.2} ms");
                println!("  Link RTT            : {rtt_us:.1} µs");
            }
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
            let target = match args[2].as_str() {
                "idle" | "0" => OperationalMode::Mode0Idle,
                "pc" | "1" => OperationalMode::Mode1PcPrimary {
                    extend_display: true,
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
