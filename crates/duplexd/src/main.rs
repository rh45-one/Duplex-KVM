//! `duplexd` — Symmetric Peer-to-Peer KVM, Virtual Display & Network Multiplexer Daemon.
//!
//! Phase 3 integrates:
//! - Automatic physical monitor enumeration & `WirePayload::AdvertiseDisplays` exchange
//! - Dynamic Virtual Display provisioning (`IddCx` on Windows in Mode 1, Wayland/X11/DRM on Debian in Mode 2)
//! - Startup Virtual Display Crash Sweeper & pre-detach Window Rescue (`rescue_windows_to_primary`)
//! - Strict interface binding (`192.168.137.1` / `192.168.137.2`), DSCP QoS (`0xB8` EF, `0xC0` CS6),
//!   `ChaCha20-Poly1305` framing, and 50 Hz peer heartbeats monitored by `duplex-guard`

mod net;

use core::sync::atomic::Ordering;
use duplex_pal::{
    create_input_backend, create_video_pipeline_backend, create_virtual_display_backend,
    monotonic_now_ns, CapturedInputEvent, InputBackend, ShmWatchdog, VideoPipelineBackend,
    VirtualDisplayBackend,
};
use duplex_proto::{
    DisplayDescriptor, HidEvent, ModeStateMachine, NodeRole, OperationalMode, TransitionReason,
    WirePayload, NO_REQUESTED_MODE,
};
use net::{default_link_ips, resolve_psk_from_env, DuplexTransport};
use std::env;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

const DISPLAY_ADVERTISE_INTERVAL_NS: u64 = 2_000_000_000; // 2 seconds

fn detect_default_role() -> NodeRole {
    #[cfg(target_os = "linux")]
    {
        NodeRole::DebianLaptop
    }
    #[cfg(target_os = "windows")]
    {
        NodeRole::WindowsDesktop
    }
}

fn parse_cli_args() -> (NodeRole, Ipv4Addr, Ipv4Addr, bool) {
    let mut role = detect_default_role();
    let mut custom_local_ip: Option<Ipv4Addr> = None;
    let mut custom_peer_ip: Option<Ipv4Addr> = None;
    let mut check_only = false;

    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--check-init" => check_only = true,
            "--role" if i + 1 < args.len() => {
                role = match args[i + 1].as_str() {
                    "laptop" | "debian" => NodeRole::DebianLaptop,
                    "desktop" | "windows" | "pc" => NodeRole::WindowsDesktop,
                    other => panic!("Unknown --role '{other}' (expected 'laptop' or 'desktop')"),
                };
                i += 1;
            }
            "--local-ip" if i + 1 < args.len() => {
                custom_local_ip = Some(
                    args[i + 1]
                        .parse()
                        .expect("Invalid IPv4 address for --local-ip"),
                );
                i += 1;
            }
            "--peer-ip" if i + 1 < args.len() => {
                custom_peer_ip = Some(
                    args[i + 1]
                        .parse()
                        .expect("Invalid IPv4 address for --peer-ip"),
                );
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    let (default_local, default_peer) = default_link_ips(role);
    (
        role,
        custom_local_ip.unwrap_or(default_local),
        custom_peer_ip.unwrap_or(default_peer),
        check_only,
    )
}

fn build_target_virtual_display_specs(
    required_count: u8,
    peer_displays: &[DisplayDescriptor],
) -> Vec<DisplayDescriptor> {
    let mut specs = Vec::with_capacity(required_count as usize);
    for idx in 0..required_count {
        if let Some(peer_spec) = peer_displays.get(idx as usize) {
            let mut cloned = peer_spec.clone();
            cloned.index = idx;
            specs.push(cloned);
        } else {
            specs.push(DisplayDescriptor {
                index: idx,
                width_px: 1920,
                height_px: 1080,
                refresh_hz: 60,
                scale_percent: 100,
                physical_width_mm: 527,
                physical_height_mm: 296,
                edid_block: DisplayDescriptor::synthesize_edid(1920, 1080, 60, idx),
            });
        }
    }
    specs
}

#[allow(clippy::too_many_arguments)]
fn apply_mode_transition(
    role: NodeRole,
    state_machine: &mut ModeStateMachine,
    target_mode: OperationalMode,
    reason: TransitionReason,
    shm: &Arc<ShmWatchdog>,
    input_pal: &mut dyn InputBackend,
    display_pal: &mut dyn VirtualDisplayBackend,
    video_pal: &mut dyn VideoPipelineBackend,
    transport: &mut Option<DuplexTransport>,
    peer_displays: &[DisplayDescriptor],
    notify_peer: bool,
) {
    let previous_mode = state_machine.current_mode();
    let effective_target = match state_machine.transition(target_mode, reason) {
        Ok(mode) => mode,
        Err(e) => {
            warn!("State machine transition error: {e}");
            return;
        }
    };

    let now_ns = monotonic_now_ns();
    let block = shm.block();

    // If we were previously capturing input and are leaving capture mode, flush remote modifiers
    if previous_mode.should_capture_input(role) && !effective_target.should_capture_input(role) {
        if let Some(ref mut net_tx) = transport {
            net_tx.send_emergency_modifier_flush(now_ns);
        }
    }

    if notify_peer {
        if let Some(ref mut net_tx) = transport {
            net_tx.send_mode_transition(effective_target, reason, state_machine.epoch());
        }
    }

    // Always sanitize local depressed keys across any mode change
    let _ = input_pal.release_all_stuck_keys();

    if effective_target.should_capture_input(role) {
        // Refresh peer heartbeat timestamp when entering an active mode so duplex-guard has a clean 250ms window
        block.beat_peer(now_ns);
        if let Err(e) = input_pal.start_capture() {
            error!("Failed to start input capture for {effective_target:?}: {e}");
            let _ = state_machine.transition(
                OperationalMode::Mode0Idle,
                TransitionReason::HardwarePanicEscape,
            );
            block
                .active_mode
                .store(OperationalMode::Mode0Idle.as_u8(), Ordering::SeqCst);
            return;
        }
    } else {
        let _ = input_pal.stop_capture();
    }

    // Synchronize virtual displays & window rescue for the new mode
    let required_vdisplays = effective_target.required_virtual_displays(role);
    if required_vdisplays == 0 {
        let _ = video_pal.close_all_viewer_windows();
        let _ = display_pal.destroy_all_virtual_displays();
        block.virtual_displays_active.store(0, Ordering::Release);
    } else {
        let target_specs = build_target_virtual_display_specs(required_vdisplays, peer_displays);
        if let Err(e) = display_pal.sync_virtual_displays(&target_specs) {
            warn!("VirtualDisplayBackend sync_virtual_displays reported warning: {e}");
        }
        block.virtual_displays_active.store(
            display_pal.active_virtual_displays().len() as u8,
            Ordering::Release,
        );
    }

    block
        .active_mode
        .store(effective_target.as_u8(), Ordering::SeqCst);
    info!(
        from = ?previous_mode,
        to = ?effective_target,
        ?reason,
        required_vdisplays,
        epoch = state_machine.epoch(),
        "Applied symmetric operational mode transition"
    );
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let (role, local_ip, peer_ip, check_only) = parse_cli_args();
    info!(
        ?role,
        %local_ip,
        %peer_ip,
        "Initializing duplexd symmetric KVM daemon (Phase 3)"
    );

    let shm = Arc::new(
        ShmWatchdog::open(true).expect("Failed to initialize shared memory watchdog block"),
    );

    let mut display_pal =
        create_virtual_display_backend().expect("Failed to create VirtualDisplayBackend");
    let mut video_pal =
        create_video_pipeline_backend().expect("Failed to create VideoPipelineBackend");

    // Mandatory Startup Crash Sweeper: destroy any orphaned virtual monitors from a prior crash
    if let Err(e) = display_pal.sweep_orphaned_displays() {
        warn!("Startup virtual display crash sweeper warning: {e}");
    }

    let local_physical_displays = display_pal.enumerate_physical_displays().unwrap_or_default();
    info!(
        local_monitors = local_physical_displays.len(),
        "Enumerated local physical displays for peer EDID advertisement"
    );

    let mut input_pal =
        create_input_backend(Some(shm.clone())).expect("Failed to create InputBackend");
    let _ = input_pal.release_all_stuck_keys();

    let psk = resolve_psk_from_env();
    let mut transport = match DuplexTransport::bind_strict(role, local_ip, peer_ip, &psk) {
        Ok(t) => Some(t),
        Err(e) => {
            if check_only {
                warn!("Interface {local_ip} not yet up during --check-init: {e}");
                return;
            }
            error!(
                "Failed to bind strict QoS sockets on {local_ip} (run setup script first): {e}"
            );
            std::process::exit(1);
        }
    };

    let mut state_machine = ModeStateMachine::new();
    let mut peer_physical_displays: Vec<DisplayDescriptor> = Vec::new();
    let mut last_display_adv_ns: u64 = 0;

    info!(
        mode = ?state_machine.current_mode(),
        "duplexd active in MODE_0_IDLE with Virtual Display Controller & Window Rescue armed"
    );

    if check_only {
        return;
    }

    let mut captured_buf: Vec<CapturedInputEvent> = Vec::with_capacity(64);
    let mut outgoing_hid_buf: Vec<HidEvent> = Vec::with_capacity(64);
    let mut incoming_payloads: Vec<WirePayload> = Vec::with_capacity(32);

    loop {
        let now_ns = monotonic_now_ns();
        let block = shm.block();
        block.beat_daemon(now_ns);

        // Periodically advertise our physical monitor EDIDs/specs so the peer can spawn matching virtual displays
        if now_ns.saturating_sub(last_display_adv_ns) >= DISPLAY_ADVERTISE_INTERVAL_NS {
            last_display_adv_ns = now_ns;
            if let Some(ref mut net_io) = transport {
                let adv = WirePayload::AdvertiseDisplays {
                    role,
                    displays: local_physical_displays.clone(),
                };
                let _ = net_io.send_control_payload(&adv);
            }
        }

        // 1. Check if duplex-guard requested an emergency reset
        if block.force_reset_requested.swap(false, Ordering::AcqRel) {
            warn!("Received emergency reset signal from duplex-guard; reverting to MODE_0_IDLE");
            apply_mode_transition(
                role,
                &mut state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::WatchdogTimeout,
                &shm,
                input_pal.as_mut(),
                display_pal.as_mut(),
                video_pal.as_mut(),
                &mut transport,
                &peer_physical_displays,
                true,
            );
        }

        // 2. Check if duplex-ctl requested an operator mode transition via shared memory
        let req = block.requested_mode.swap(NO_REQUESTED_MODE, Ordering::AcqRel);
        if req != NO_REQUESTED_MODE {
            let target_mode = OperationalMode::from_u8_lossy(req);
            apply_mode_transition(
                role,
                &mut state_machine,
                target_mode,
                TransitionReason::OperatorCommand,
                &shm,
                input_pal.as_mut(),
                display_pal.as_mut(),
                video_pal.as_mut(),
                &mut transport,
                &peer_physical_displays,
                true,
            );
        }

        // 3. Poll local captured input events & transmit over DSCP EF (0xB8) UDP 48100
        captured_buf.clear();
        outgoing_hid_buf.clear();
        let mut panic_tripped = None;

        if let Ok(count) = input_pal.poll_captured_events(&mut captured_buf) {
            if count > 0 {
                for ev in &captured_buf {
                    match *ev {
                        CapturedInputEvent::Hid(hid_ev) => {
                            outgoing_hid_buf.push(hid_ev);
                        }
                        CapturedInputEvent::PanicTriggered(kind) => {
                            panic_tripped = Some(kind);
                        }
                    }
                }
            }
        }

        if !outgoing_hid_buf.is_empty() {
            if let Some(ref mut net_tx) = transport {
                let _ = net_tx.send_hid_events(&outgoing_hid_buf, now_ns);
            }
        }

        if let Some(kind) = panic_tripped {
            warn!(
                ?kind,
                "Hardware panic escape sequence detected! Flushing remote modifiers and reverting to MODE_0_IDLE"
            );
            if let Some(ref mut net_tx) = transport {
                net_tx.send_emergency_modifier_flush(now_ns);
            }
            apply_mode_transition(
                role,
                &mut state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::HardwarePanicEscape,
                &shm,
                input_pal.as_mut(),
                display_pal.as_mut(),
                video_pal.as_mut(),
                &mut transport,
                &peer_physical_displays,
                true,
            );
        }

        // 4. Tick 50 Hz CS6 (0xC0) Heartbeat & poll incoming encrypted frames
        incoming_payloads.clear();
        if let Some(ref mut net_io) = transport {
            net_io.tick_heartbeat(
                state_machine.current_mode(),
                state_machine.epoch(),
                now_ns,
            );
            net_io.poll_incoming(&mut incoming_payloads);
        }

        for payload in incoming_payloads.drain(..) {
            match payload {
                WirePayload::HidBatch { events, .. } => {
                    block.beat_peer(now_ns);
                    let cur_mode = state_machine.current_mode();
                    let inject_allowed = cur_mode != OperationalMode::Mode0Idle
                        && !cur_mode.should_capture_input(role);

                    for ev in &events {
                        // Always honor ReleaseAllModifiers even if already in Mode0Idle
                        if matches!(ev, HidEvent::ReleaseAllModifiers) || inject_allowed {
                            let _ = input_pal.inject_hid_event(ev);
                        }
                    }
                }
                WirePayload::Heartbeat {
                    sender_timestamp_ns,
                    active_mode,
                    epoch,
                    ..
                } => {
                    block.beat_peer(now_ns);
                    if let Some(ref mut net_io) = transport {
                        let ack = WirePayload::HeartbeatAck {
                            origin_timestamp_ns: sender_timestamp_ns,
                            active_mode: state_machine.current_mode(),
                            epoch: state_machine.epoch(),
                        };
                        let _ = net_io.send_control_payload(&ack);
                    }
                    if epoch > state_machine.epoch() && active_mode != state_machine.current_mode()
                    {
                        apply_mode_transition(
                            role,
                            &mut state_machine,
                            active_mode,
                            TransitionReason::PeerRequest,
                            &shm,
                            input_pal.as_mut(),
                            display_pal.as_mut(),
                            video_pal.as_mut(),
                            &mut transport,
                            &peer_physical_displays,
                            false,
                        );
                    }
                }
                WirePayload::HeartbeatAck {
                    origin_timestamp_ns,
                    ..
                } => {
                    block.beat_peer(now_ns);
                    let rtt_ns = now_ns.saturating_sub(origin_timestamp_ns);
                    block.last_rtt_ns.store(rtt_ns, Ordering::Release);
                }
                WirePayload::ModeTransition {
                    target_mode,
                    reason,
                    ..
                } => {
                    block.beat_peer(now_ns);
                    apply_mode_transition(
                        role,
                        &mut state_machine,
                        target_mode,
                        reason,
                        &shm,
                        input_pal.as_mut(),
                        display_pal.as_mut(),
                        video_pal.as_mut(),
                        &mut transport,
                        &peer_physical_displays,
                        false,
                    );
                }
                WirePayload::AdvertiseDisplays { displays, .. } => {
                    block.beat_peer(now_ns);
                    if peer_physical_displays != displays {
                        info!(
                            peer_monitors = displays.len(),
                            "Received updated physical monitor EDID specifications from peer"
                        );
                        peer_physical_displays = displays;
                    }
                }
            }
        }

        // 500µs–1ms low-latency poll cadence for 1000Hz USB HID responsiveness
        thread::sleep(Duration::from_micros(500));
    }
}
