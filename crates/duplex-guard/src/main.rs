//! `duplex-guard` — Standalone 250ms Dead-Man's Switch Watchdog Process.
//!
//! Monitors `duplexd` and the 50 Hz peer network heartbeat via OS shared memory
//! (`/duplex_kvm_watchdog_v1` on Linux, `Local\DuplexKvmWatchdogV1` on Windows).
//! If either heartbeat drops for > 250ms, or if the local hardware panic escape hatch latches,
//! `duplex-guard` immediately reclaims input grabs, emits synthetic `KEY_UP` for all depressed
//! and modifier keys, closes fullscreen video viewers, rescues windows, and tears down virtual displays.

use core::sync::atomic::Ordering;
use duplex_pal::{
    create_input_backend, create_video_pipeline_backend, create_virtual_display_backend,
    force_terminate_process, is_process_alive, monotonic_now_ns, ShmWatchdog,
};
use duplex_proto::{OperationalMode, WatchdogTripReason, WATCHDOG_TIMEOUT_NS};
use std::env;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const GRACE_ESCALATION_WAIT: Duration = Duration::from_millis(25);

fn execute_emergency_reclaim(shm: &Arc<ShmWatchdog>, reason: WatchdogTripReason) {
    let block = shm.block();
    let daemon_pid = block.daemon_pid.load(Ordering::Acquire);

    error!(
        ?reason,
        daemon_pid,
        "250ms DEAD-MAN'S SWITCH TRIPPED! Executing immediate hardware input & display reclaim"
    );

    // 1. Signal duplexd to drop capture immediately via shared memory
    block.force_reset_requested.store(true, Ordering::SeqCst);
    block
        .active_mode
        .store(OperationalMode::Mode0Idle.as_u8(), Ordering::SeqCst);

    // If duplexd itself hung (DaemonHeartbeatExpired), kill it immediately so the OS kernel
    // closes all `/dev/input/event*` fds (releasing EVIOCGRAB) or unhooks WH_KEYBOARD_LL / WH_MOUSE_LL.
    if matches!(reason, WatchdogTripReason::DaemonHeartbeatExpired { .. }) && daemon_pid != 0 {
        warn!(
            daemon_pid,
            "Local duplexd heartbeat expired (>250ms); terminating hung daemon to force kernel grab release"
        );
        force_terminate_process(daemon_pid);
    } else if block.input_grab_active.load(Ordering::Acquire) && daemon_pid != 0 {
        // Give a responsive daemon 25ms to cleanly drop its grab on PeerHeartbeatExpired before escalating
        thread::sleep(GRACE_ESCALATION_WAIT);
        if block.input_grab_active.load(Ordering::Acquire) {
            warn!(
                daemon_pid,
                "duplexd did not release input grab within 25ms grace window; escalating to SIGKILL/TerminateProcess"
            );
            force_terminate_process(daemon_pid);
        }
    }

    // 2. Sanitize all depressed keys from the 256-bit shared-memory bitmap + all 8 modifier keys
    match create_input_backend(Some(shm.clone())) {
        Ok(mut input_pal) => {
            if let Err(e) = input_pal.release_all_stuck_keys() {
                warn!("Failsafe modifier key release reported warning: {e}");
            } else {
                info!("Injected synthetic KEY_UP for all depressed keys and modifiers (Ctrl/Alt/Shift/Super)");
            }
        }
        Err(e) => {
            warn!("Could not open failsafe InputBackend during watchdog recovery: {e}");
        }
    }

    // 3. Destroy fullscreen hardware-decoded viewer overlays
    if let Ok(mut video_pal) = create_video_pipeline_backend() {
        let _ = video_pal.close_all_viewer_windows();
    }

    // 4. Rescue windows and detach all virtual displays
    if let Ok(mut display_pal) = create_virtual_display_backend() {
        let _ = display_pal.destroy_all_virtual_displays();
    }

    // 5. Reset shared-memory state machine indicators to MODE_0_IDLE
    let now_ns = monotonic_now_ns();
    block.input_grab_active.store(false, Ordering::SeqCst);
    block.virtual_displays_active.store(0, Ordering::SeqCst);
    block.panic_latched.store(false, Ordering::SeqCst);
    block.daemon_heartbeat_ns.store(now_ns, Ordering::SeqCst);
    block.peer_heartbeat_ns.store(now_ns, Ordering::SeqCst);

    info!("System safely restored to MODE_0_IDLE by duplex-guard");
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    let run_once = args.iter().any(|a| a == "--once");
    let create_shm = args.iter().any(|a| a == "--init-shm");

    info!(
        timeout_ms = WATCHDOG_TIMEOUT_NS / 1_000_000,
        "Starting duplex-guard Dead-Man's Switch watchdog"
    );

    let shm = loop {
        match ShmWatchdog::open(create_shm) {
            Ok(mapping) if mapping.block().is_valid() || create_shm => {
                break Arc::new(mapping);
            }
            _ => {
                if run_once {
                    info!("Shared memory block not yet active (--once mode); exiting cleanly");
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    info!("Attached to Duplex shared-memory watchdog block");

    loop {
        let now_ns = monotonic_now_ns();
        let block = shm.block();

        let daemon_pid = block.daemon_pid.load(Ordering::Acquire);
        let mode = block.active_mode.load(Ordering::Acquire);
        let grab_active = block.input_grab_active.load(Ordering::Acquire);
        let vdisp_active = block.virtual_displays_active.load(Ordering::Acquire);

        if (mode != OperationalMode::Mode0Idle.as_u8() || grab_active || vdisp_active > 0)
            && daemon_pid != 0
            && !is_process_alive(daemon_pid)
        {
            execute_emergency_reclaim(
                &shm,
                WatchdogTripReason::DaemonProcessTerminated { pid: daemon_pid },
            );
        } else if let Some(trip_reason) = block.evaluate_liveness(now_ns) {
            execute_emergency_reclaim(&shm, trip_reason);
        }

        if run_once {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
}
