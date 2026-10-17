//! `duplexd` — Symmetric Peer-to-Peer KVM, Virtual Display & Network Multiplexer Daemon.
//!
//! Phase 5 completes the full system integration:
//! - Deterministic 3-mode state machine (`MODE_0_IDLE`, `MODE_1_PC_PRIMARY { extend_display }`, `MODE_2_LAPTOP_PRIMARY`)
//! - Automatic co-process supervision of `duplex-guard` (250ms Dead-Man's Switch)
//! - Live shared-memory telemetry counters (`tx/rx_hid_packets`, `tx/rx_video_frames`, `pli_keyframes_requested`, `min/last/max_rtt_ns`)
//! - Built-in deterministic `--self-test` verification suite

mod net;

use core::sync::atomic::Ordering;
use duplex_pal::{
    create_input_backend, create_video_pipeline_backend, create_virtual_display_backend,
    monotonic_now_ns, CapturedInputEvent, InputBackend, ShmWatchdog, VideoPipelineBackend,
    VirtualDisplayBackend,
};
use duplex_proto::{
    CursorTelemetry, DisplayDescriptor, EncodedVideoFrame, HidEvent, ModeStateMachine, NodeRole,
    OperationalMode, TransitionReason, WirePayload, MAX_VIDEO_BITRATE_KBPS, NO_REQUESTED_MODE,
    WATCHDOG_TIMEOUT_NS,
};
use net::{default_link_ips, resolve_psk_from_env, DuplexTransport};
use std::env;
use std::net::Ipv4Addr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

const DISPLAY_ADVERTISE_INTERVAL_NS: u64 = 2_000_000_000; // 2 seconds

struct CliConfig {
    role: NodeRole,
    local_ip: Ipv4Addr,
    peer_ip: Ipv4Addr,
    check_only: bool,
    self_test: bool,
    spawn_guard: bool,
}

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

fn parse_cli_args(args: &[String]) -> CliConfig {
    let mut role = detect_default_role();
    let mut custom_local_ip: Option<Ipv4Addr> = None;
    let mut custom_peer_ip: Option<Ipv4Addr> = None;
    let mut check_only = false;
    let mut self_test = false;
    let mut spawn_guard = true;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--check-init" => check_only = true,
            "--self-test" => self_test = true,
            "--no-spawn-guard" => spawn_guard = false,
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
    CliConfig {
        role,
        local_ip: custom_local_ip.unwrap_or(default_local),
        peer_ip: custom_peer_ip.unwrap_or(default_peer),
        check_only,
        self_test,
        spawn_guard,
    }
}

/// Spawns the independent 250ms Dead-Man's Switch watchdog co-process by re-invoking
/// the current single-file executable with `--guard`.
fn try_spawn_watchdog_coprocess() -> Option<Child> {
    let exe_path = env::current_exe().ok()?;

    match Command::new(&exe_path)
        .arg("--guard")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            info!(
                guard_pid = child.id(),
                path = %exe_path.display(),
                "Spawned 250ms Dead-Man's Switch watchdog co-process (--guard)"
            );
            Some(child)
        }
        Err(e) => {
            warn!("Could not auto-spawn watchdog co-process: {e}");
            None
        }
    }
}

fn build_target_display_specs(
    count: u8,
    reference_displays: &[DisplayDescriptor],
) -> Vec<DisplayDescriptor> {
    let mut specs = Vec::with_capacity(count as usize);
    for idx in 0..count {
        if let Some(ref_spec) = reference_displays.get(idx as usize) {
            let mut cloned = ref_spec.clone();
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
    local_physical_displays: &[DisplayDescriptor],
    peer_physical_displays: &[DisplayDescriptor],
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

    // Synchronize virtual displays, hardware video encoder/viewer pipelines, and window rescue
    let required_vdisplays = effective_target.required_virtual_displays(role);
    let expected_viewers = effective_target.expected_incoming_video_streams(role);

    if required_vdisplays > 0 {
        let target_specs = build_target_display_specs(required_vdisplays, peer_physical_displays);
        if let Err(e) = display_pal.sync_virtual_displays(&target_specs) {
            warn!("VirtualDisplayBackend sync_virtual_displays reported warning: {e}");
        }
        block.virtual_displays_active.store(
            display_pal.active_virtual_displays().len() as u8,
            Ordering::Release,
        );
        if let Err(e) = video_pal.start_sender(&target_specs, MAX_VIDEO_BITRATE_KBPS) {
            warn!("VideoPipelineBackend start_sender reported warning: {e}");
        }
    } else if expected_viewers > 0 {
        let _ = display_pal.destroy_all_virtual_displays();
        block.virtual_displays_active.store(0, Ordering::Release);

        let viewer_specs = build_target_display_specs(expected_viewers, local_physical_displays);
        if let Err(e) = video_pal.start_viewer(&viewer_specs) {
            warn!("VideoPipelineBackend start_viewer reported warning: {e}");
        }
        if let Some(ref mut net_tx) = transport {
            for idx in 0..expected_viewers {
                let _ = net_tx.send_control_payload(&WirePayload::VideoKeyframeRequest {
                    monitor_index: idx,
                });
            }
        }
    } else {
        let _ = video_pal.close_all_viewer_windows();
        let _ = display_pal.destroy_all_virtual_displays();
        block.virtual_displays_active.store(0, Ordering::Release);
    }

    block
        .active_mode
        .store(effective_target.as_u8(), Ordering::SeqCst);
    info!(
        from = ?previous_mode,
        to = ?effective_target,
        ?reason,
        required_vdisplays,
        expected_viewers,
        epoch = state_machine.epoch(),
        "Applied symmetric operational mode transition"
    );
}

fn run_phase5_self_test(shm: &Arc<ShmWatchdog>) {
    info!("Running Phase 5 deterministic state machine & SHM self-test...");
    let mut sm = ModeStateMachine::new();
    assert_eq!(sm.current_mode(), OperationalMode::Mode0Idle);

    // 1. Mode 1 (PC Primary with Extended Virtual Display)
    let m1_ext = OperationalMode::Mode1PcPrimary {
        extend_display: true,
    };
    assert_eq!(
        sm.transition(m1_ext, TransitionReason::OperatorCommand)
            .unwrap(),
        m1_ext
    );
    assert_eq!(OperationalMode::from_u8_lossy(m1_ext.as_u8()), m1_ext);

    // 2. Mode 1 (PC Primary Input-Only KVM)
    let m1_no_disp = OperationalMode::Mode1PcPrimary {
        extend_display: false,
    };
    assert_eq!(
        sm.transition(m1_no_disp, TransitionReason::OperatorCommand)
            .unwrap(),
        m1_no_disp
    );
    assert_eq!(
        OperationalMode::from_u8_lossy(m1_no_disp.as_u8()),
        m1_no_disp
    );

    // 3. Mode 2 (Laptop Primary + Dual Monitors Docked)
    let m2 = OperationalMode::Mode2LaptopPrimary;
    assert_eq!(
        sm.transition(m2, TransitionReason::PeerRequest).unwrap(),
        m2
    );
    assert_eq!(OperationalMode::from_u8_lossy(m2.as_u8()), m2);

    // 4. Hardware Panic Escape Hatch overrides any target to Mode0Idle
    assert_eq!(
        sm.transition(m2, TransitionReason::HardwarePanicEscape)
            .unwrap(),
        OperationalMode::Mode0Idle
    );

    shm.block().record_rtt_sample(185_000); // 185 µs
    assert_eq!(shm.block().last_rtt_ns.load(Ordering::Acquire), 185_000);
    info!("Phase 5 self-test passed all assertions!");
}

pub fn run_daemon_with_args(args: &[String]) {
    if args.iter().any(|a| a == "--guard" || a == "guard") {
        duplex_guard::run_guard_with_args(args);
        return;
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cfg = parse_cli_args(args);
    info!(
        role = ?cfg.role,
        local_ip = %cfg.local_ip,
        peer_ip = %cfg.peer_ip,
        "Initializing duplexd symmetric KVM, Virtual Display & Video daemon (Phase 5)"
    );

    let shm = Arc::new(
        ShmWatchdog::open(true).expect("Failed to initialize shared memory watchdog block"),
    );

    if cfg.self_test {
        run_phase5_self_test(&shm);
        return;
    }

    let mut display_pal =
        create_virtual_display_backend().expect("Failed to create VirtualDisplayBackend");
    let mut video_pal =
        create_video_pipeline_backend().expect("Failed to create VideoPipelineBackend");

    // Mandatory Startup Crash Sweeper: destroy any orphaned virtual monitors & viewer overlays
    if let Err(e) = display_pal.sweep_orphaned_displays() {
        warn!("Startup virtual display crash sweeper warning: {e}");
    }
    let _ = video_pal.close_all_viewer_windows();

    let local_physical_displays = display_pal.enumerate_physical_displays().unwrap_or_default();
    info!(
        local_monitors = local_physical_displays.len(),
        "Enumerated local physical displays for peer EDID advertisement"
    );

    let mut input_pal =
        create_input_backend(Some(shm.clone())).expect("Failed to create InputBackend");
    let _ = input_pal.release_all_stuck_keys();

    let psk = resolve_psk_from_env();
    let mut transport =
        match DuplexTransport::bind_strict(cfg.role, cfg.local_ip, cfg.peer_ip, &psk) {
            Ok(t) => Some(t),
            Err(e) => {
                if cfg.check_only {
                    warn!("Interface {} not yet up during --check-init: {e}", cfg.local_ip);
                    return;
                }
                warn!(
                    "Interface {} not yet up ({e}); starting in MODE_0_IDLE and retrying bind when cable connects",
                    cfg.local_ip
                );
                None
            }
        };

    let _guard_child = if cfg.spawn_guard && !cfg.check_only {
        try_spawn_watchdog_coprocess()
    } else {
        None
    };

    let mut state_machine = ModeStateMachine::new();
    let mut peer_physical_displays: Vec<DisplayDescriptor> = Vec::new();
    let mut last_display_adv_ns: u64 = 0;
    let mut last_bind_retry_ns: u64 = 0;

    info!(
        mode = ?state_machine.current_mode(),
        "duplexd active in MODE_0_IDLE (Phase 5 Full State Machine Ready)"
    );

    if cfg.check_only {
        return;
    }

    let mut captured_buf: Vec<CapturedInputEvent> = Vec::with_capacity(64);
    let mut outgoing_hid_buf: Vec<HidEvent> = Vec::with_capacity(64);
    let mut incoming_payloads: Vec<WirePayload> = Vec::with_capacity(32);
    let mut encoded_video_buf: Vec<EncodedVideoFrame> = Vec::with_capacity(8);
    let mut incoming_video_buf: Vec<EncodedVideoFrame> = Vec::with_capacity(8);
    let mut pli_monitors_buf: Vec<u8> = Vec::with_capacity(4);

    loop {
        let now_ns = monotonic_now_ns();
        let block = shm.block();
        block.beat_daemon(now_ns);

        // Hot-plug retry: if the 1GbE link was unplugged at startup, attempt strict QoS bind every 1s
        if transport.is_none() && now_ns.saturating_sub(last_bind_retry_ns) >= 1_000_000_000 {
            last_bind_retry_ns = now_ns;
            if let Ok(t) = DuplexTransport::bind_strict(cfg.role, cfg.local_ip, cfg.peer_ip, &psk) {
                info!(
                    local_ip = %cfg.local_ip,
                    peer_ip = %cfg.peer_ip,
                    "1GbE link carrier detected — strict QoS UDP/TCP transport bound"
                );
                transport = Some(t);
            }
        }

        // Update peer_connected status based on 250ms peer heartbeat window
        let peer_hb = block.peer_heartbeat_ns.load(Ordering::Acquire);
        let peer_age = now_ns.saturating_sub(peer_hb);
        block
            .peer_connected
            .store(peer_hb > 0 && peer_age <= WATCHDOG_TIMEOUT_NS, Ordering::Release);

        // Periodically advertise our physical monitor EDIDs/specs so the peer can spawn matching virtual displays
        if now_ns.saturating_sub(last_display_adv_ns) >= DISPLAY_ADVERTISE_INTERVAL_NS {
            last_display_adv_ns = now_ns;
            if let Some(ref mut net_io) = transport {
                let adv = WirePayload::AdvertiseDisplays {
                    role: cfg.role,
                    displays: local_physical_displays.clone(),
                };
                let _ = net_io.send_control_payload(&adv);
            }
        }

        // 1. Check if duplex-guard requested an emergency reset
        if block.force_reset_requested.swap(false, Ordering::AcqRel) {
            warn!("Received emergency reset signal from duplex-guard; reverting to MODE_0_IDLE");
            apply_mode_transition(
                cfg.role,
                &mut state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::WatchdogTimeout,
                &shm,
                input_pal.as_mut(),
                display_pal.as_mut(),
                video_pal.as_mut(),
                &mut transport,
                &local_physical_displays,
                &peer_physical_displays,
                true,
            );
        }

        // 2. Check if duplex-ctl requested an operator mode transition via shared memory
        let req = block.requested_mode.swap(NO_REQUESTED_MODE, Ordering::AcqRel);
        if req != NO_REQUESTED_MODE {
            let target_mode = OperationalMode::from_u8_lossy(req);
            apply_mode_transition(
                cfg.role,
                &mut state_machine,
                target_mode,
                TransitionReason::OperatorCommand,
                &shm,
                input_pal.as_mut(),
                display_pal.as_mut(),
                video_pal.as_mut(),
                &mut transport,
                &local_physical_displays,
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
                if net_tx.send_hid_events(&outgoing_hid_buf, now_ns).is_ok() {
                    block.tx_hid_packets.fetch_add(1, Ordering::Relaxed);
                }
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
                cfg.role,
                &mut state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::HardwarePanicEscape,
                &shm,
                input_pal.as_mut(),
                display_pal.as_mut(),
                video_pal.as_mut(),
                &mut transport,
                &local_physical_displays,
                &peer_physical_displays,
                true,
            );
        }

        // 4. Poll hardware-encoded HEVC video frames & decoupled 1000 Hz cursor telemetry
        if state_machine.current_mode().should_stream_video(cfg.role) {
            encoded_video_buf.clear();
            let mut cursor_telemetry_opt: Option<CursorTelemetry> = None;
            if let Ok(frame_count) =
                video_pal.poll_encoded_frames(&mut encoded_video_buf, &mut cursor_telemetry_opt)
            {
                if let Some(ref mut net_tx) = transport {
                    if let Some(cursor) = cursor_telemetry_opt.take() {
                        let _ = net_tx.send_cursor_update(cursor);
                    }
                    if frame_count > 0 {
                        for frame in &encoded_video_buf {
                            net_tx.send_video_frame(frame);
                        }
                        block
                            .tx_video_frames
                            .fetch_add(frame_count as u64, Ordering::Relaxed);
                    }
                }
            }
        }

        // 5. Tick 50 Hz CS6 (0xC0) Heartbeat & poll incoming encrypted HID/Control/Video frames
        incoming_payloads.clear();
        incoming_video_buf.clear();
        pli_monitors_buf.clear();

        if let Some(ref mut net_io) = transport {
            net_io.tick_heartbeat(
                state_machine.current_mode(),
                state_machine.epoch(),
                now_ns,
            );
            net_io.poll_incoming(&mut incoming_payloads);
            net_io.poll_incoming_video(&mut incoming_video_buf, &mut pli_monitors_buf);

            for mon_idx in pli_monitors_buf.drain(..) {
                block.pli_keyframes_requested.fetch_add(1, Ordering::Relaxed);
                let _ = net_io.send_control_payload(&WirePayload::VideoKeyframeRequest {
                    monitor_index: mon_idx,
                });
            }
        }

        for frame in incoming_video_buf.drain(..) {
            block.beat_peer(now_ns);
            block.rx_video_frames.fetch_add(1, Ordering::Relaxed);
            let _ = video_pal.submit_encoded_frame(&frame);
        }

        for payload in incoming_payloads.drain(..) {
            match payload {
                WirePayload::HidBatch { events, .. } => {
                    block.beat_peer(now_ns);
                    block.rx_hid_packets.fetch_add(1, Ordering::Relaxed);
                    let cur_mode = state_machine.current_mode();
                    let inject_allowed = cur_mode != OperationalMode::Mode0Idle
                        && !cur_mode.should_capture_input(cfg.role);

                    for ev in &events {
                        if matches!(ev, HidEvent::ReleaseAllModifiers) || inject_allowed {
                            let _ = input_pal.inject_hid_event(ev);
                        }
                    }
                }
                WirePayload::CursorUpdate(cursor) => {
                    block.beat_peer(now_ns);
                    let _ = video_pal.update_hardware_cursor(&cursor);
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
                            cfg.role,
                            &mut state_machine,
                            active_mode,
                            TransitionReason::PeerRequest,
                            &shm,
                            input_pal.as_mut(),
                            display_pal.as_mut(),
                            video_pal.as_mut(),
                            &mut transport,
                            &local_physical_displays,
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
                    block.record_rtt_sample(rtt_ns);
                }
                WirePayload::ModeTransition {
                    target_mode,
                    reason,
                    ..
                } => {
                    block.beat_peer(now_ns);
                    apply_mode_transition(
                        cfg.role,
                        &mut state_machine,
                        target_mode,
                        reason,
                        &shm,
                        input_pal.as_mut(),
                        display_pal.as_mut(),
                        video_pal.as_mut(),
                        &mut transport,
                        &local_physical_displays,
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
                WirePayload::VideoKeyframeRequest { monitor_index } => {
                    block.beat_peer(now_ns);
                    let _ = video_pal.request_keyframe(monitor_index);
                }
            }
        }

        // 500µs–1ms low-latency poll cadence for 1000Hz USB HID & decoupled cursor responsiveness
        thread::sleep(Duration::from_micros(500));
    }
}
