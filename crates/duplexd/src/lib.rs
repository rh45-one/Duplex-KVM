//! `duplexd` — Symmetric Peer-to-Peer KVM, Virtual Display & Network Multiplexer Daemon.
//!
//! Phase 5 completes the full system integration:
//! - Deterministic 3-mode state machine (`MODE_0_IDLE`, `MODE_1_PC_PRIMARY { extend_display }`, `MODE_2_LAPTOP_PRIMARY`)
//! - Automatic co-process supervision of `duplex-guard` (250ms Dead-Man's Switch)
//! - Live shared-memory telemetry counters (`tx/rx_hid_packets`, `tx/rx_video_frames`, `pli_keyframes_requested`, `min/last/max_rtt_ns`)
//! - Built-in deterministic `--self-test` verification suite

mod net;
pub mod pairing;

use core::sync::atomic::Ordering;
use duplex_pal::{
    create_input_backend, create_video_pipeline_backend, create_virtual_display_backend,
    monotonic_now_ns, CapturedInputEvent, InputBackend, PeerVideoCaps, ShmWatchdog,
    VideoPipelineBackend, VirtualDisplayBackend,
};
use duplex_proto::{
    DisplayDescriptor, HidEvent, ModeStateMachine, NodeRole, OperationalMode, TransitionReason,
    WirePayload, NO_REQUESTED_MODE, VIDEO_CAP_NATIVE_BANDS, VIDEO_CAP_TILES,
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
/// Minimum spacing of refresh requests per monitor, so a burst of loss costs
/// one full refresh rather than many.
const VIDEO_REFRESH_INTERVAL_NS: u64 = 250_000_000;

/// Check pairing before a GUI starts a background daemon so errors are visible.
pub fn validate_link_key_config() -> Result<(), String> {
    resolve_psk_from_env().map(|_| ())
}

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
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
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
    peer_epoch: Option<u64>,
    shm: &Arc<ShmWatchdog>,
    input_pal: &mut dyn InputBackend,
    display_pal: &mut dyn VirtualDisplayBackend,
    video_pal: &mut dyn VideoPipelineBackend,
    transport: &mut Option<DuplexTransport>,
    local_physical_displays: &[DisplayDescriptor],
    peer_physical_displays: &[DisplayDescriptor],
    notify_peer: bool,
) {
    if peer_epoch.is_none()
        && target_mode != OperationalMode::Mode0Idle
        && !matches!(
            reason,
            TransitionReason::HardwarePanicEscape | TransitionReason::WatchdogTimeout
        )
        && !transport
            .as_ref()
            .is_some_and(|net| net.has_recent_authenticated_peer(monotonic_now_ns()))
    {
        warn!(?target_mode, "Mode request rejected: no recent authenticated peer. Input remains local; check Ethernet, pairing, and the other daemon");
        return;
    }
    if peer_epoch.is_some_and(|epoch| epoch <= state_machine.epoch()) {
        return;
    }
    let previous_mode = state_machine.current_mode();
    let previous_epoch = state_machine.epoch();
    let effective_target =
        match state_machine.transition_with_epoch(target_mode, reason, peer_epoch) {
            Ok(mode) => mode,
            Err(e) => {
                warn!("State machine transition error: {e}");
                return;
            }
        };

    // Mode transitions are sent as a three-packet burst. Only the first changes
    // resources; replaying setup closes viewers and can stall the heartbeat loop.
    if previous_mode == effective_target
        && !matches!(
            reason,
            TransitionReason::HardwarePanicEscape | TransitionReason::WatchdogTimeout
        )
    {
        return;
    }

    let _transition_keepalive = transport.as_ref().and_then(|net| {
        let shm_peer = shm.clone();
        match net.transition_keepalive(previous_mode, previous_epoch, move |now| {
            shm_peer.block().beat_peer(now);
        }) {
            Ok(worker) => Some(worker),
            Err(error) => {
                warn!(%error, "Transition control keepalive unavailable");
                None
            }
        }
    });

    let now_ns = monotonic_now_ns();
    let block = shm.block();
    block.beat_daemon(now_ns);
    // Grant a 1.5s transition grace window so duplex-guard does not trip while both peers configure displays/pipelines
    block.beat_peer(now_ns.saturating_add(1_500_000_000));

    // If we were previously capturing input and are leaving capture mode, flush remote modifiers
    if previous_mode.should_capture_input(role) && !effective_target.should_capture_input(role) {
        if let Some(ref mut net_tx) = transport {
            net_tx.send_emergency_modifier_flush(now_ns);
        }
    }

    // When returning to Mode0Idle, disarm the watchdog and notify the peer BEFORE running
    // kernel EVIOCGRAB release / xrandr teardown so neither side trips a 250ms timeout during cleanup.
    if effective_target == OperationalMode::Mode0Idle {
        block
            .active_mode
            .store(OperationalMode::Mode0Idle.as_u8(), Ordering::SeqCst);
        block.input_grab_active.store(false, Ordering::SeqCst);
        block.virtual_displays_active.store(0, Ordering::SeqCst);
        if notify_peer {
            if let Some(ref mut net_tx) = transport {
                net_tx.send_mode_transition(effective_target, reason, state_machine.epoch());
                let _ = net_tx.send_control_payload(&WirePayload::Heartbeat {
                    sender_role: role,
                    active_mode: effective_target,
                    epoch: state_machine.epoch(),
                    sender_timestamp_ns: now_ns,
                });
            }
        }
    }

    // Always sanitize local depressed keys across any mode change
    let _ = input_pal.release_all_stuck_keys();
    if !effective_target.should_capture_input(role) {
        let _ = input_pal.stop_capture();
    }

    // Synchronize virtual displays, hardware video encoder/viewer pipelines, and window rescue BEFORE grabbing input
    let required_vdisplays = effective_target.required_virtual_displays(role);
    let expected_viewers = effective_target.expected_incoming_video_streams(role);

    if required_vdisplays > 0 {
        let target_specs = build_target_display_specs(required_vdisplays, peer_physical_displays);
        if let Err(e) = display_pal.sync_virtual_displays(&target_specs) {
            error!("Virtual display setup failed: {e}");
            if let Some(net) = transport {
                let _ = net.send_control_payload(&WirePayload::OperationError {
                    message: e.to_string(),
                });
            }
            apply_mode_transition(
                role,
                state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::HardwarePanicEscape,
                None,
                shm,
                input_pal,
                display_pal,
                video_pal,
                transport,
                local_physical_displays,
                peer_physical_displays,
                true,
            );
            return;
        }
        block.virtual_displays_active.store(
            display_pal.active_virtual_displays().len() as u8,
            Ordering::Release,
        );
        if let Some(net) = transport.as_ref() {
            net.set_video_consumer(None);
        }
        let started = match transport.as_ref() {
            Some(net) => video_pal.start_sender(&target_specs, net.video_sink()),
            None => Err(duplex_pal::PalError::Network(
                "The peer link is down; video cannot be sent".into(),
            )),
        };
        if let Err(e) = started {
            error!("Video sender setup failed: {e}");
            if let Some(net) = transport {
                let _ = net.send_control_payload(&WirePayload::OperationError {
                    message: e.to_string(),
                });
            }
            apply_mode_transition(
                role,
                state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::HardwarePanicEscape,
                None,
                shm,
                input_pal,
                display_pal,
                video_pal,
                transport,
                local_physical_displays,
                peer_physical_displays,
                true,
            );
            return;
        }
    } else if expected_viewers > 0 {
        if !display_pal.active_virtual_displays().is_empty() {
            let _ = display_pal.destroy_all_virtual_displays();
        }
        block.virtual_displays_active.store(0, Ordering::Release);

        let viewer_specs = build_target_display_specs(expected_viewers, local_physical_displays);
        let consumer = match video_pal.start_viewer(&viewer_specs) {
            Ok(consumer) => consumer,
            Err(e) => {
                error!("Video receiver setup failed: {e}");
                if let Some(net) = transport {
                    let _ = net.send_control_payload(&WirePayload::OperationError {
                        message: e.to_string(),
                    });
                }
                apply_mode_transition(
                    role,
                    state_machine,
                    OperationalMode::Mode0Idle,
                    TransitionReason::HardwarePanicEscape,
                    None,
                    shm,
                    input_pal,
                    display_pal,
                    video_pal,
                    transport,
                    local_physical_displays,
                    peer_physical_displays,
                    true,
                );
                return;
            }
        };
        if let Some(ref mut net_tx) = transport {
            net_tx.set_video_consumer(Some(consumer));
            for idx in 0..expected_viewers {
                let _ = net_tx.send_control_payload(&WirePayload::VideoKeyframeRequest {
                    monitor_index: idx,
                });
            }
        }
    } else {
        if let Some(net) = transport.as_ref() {
            net.set_video_consumer(None);
        }
        let _ = video_pal.close_all_viewer_windows();
        if !display_pal.active_virtual_displays().is_empty() {
            let _ = display_pal.destroy_all_virtual_displays();
        }
        block.virtual_displays_active.store(0, Ordering::Release);
    }

    // Refresh heartbeats right before acquiring exclusive input grab so duplex-guard sees fresh timestamps
    let pre_grab_ns = monotonic_now_ns();
    block.beat_daemon(pre_grab_ns);
    block.beat_peer(pre_grab_ns.saturating_add(1_000_000_000));

    if effective_target.should_capture_input(role) {
        if let Err(e) = input_pal.start_capture() {
            error!("Failed to start input capture for {effective_target:?}: {e}");
            apply_mode_transition(
                role,
                state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::HardwarePanicEscape,
                None,
                shm,
                input_pal,
                display_pal,
                video_pal,
                transport,
                local_physical_displays,
                peer_physical_displays,
                true,
            );
            return;
        }
    }

    let post_grab_ns = monotonic_now_ns();
    block.beat_daemon(post_grab_ns);
    block.beat_peer(post_grab_ns.saturating_add(1_000_000_000));
    block
        .active_mode
        .store(effective_target.as_u8(), Ordering::SeqCst);

    // When entering an active mode, notify peer AFTER local display/video/input transition has finished
    // so the peer immediately receives uninterrupted 50 Hz heartbeats without any transition stall gap.
    if notify_peer && effective_target != OperationalMode::Mode0Idle {
        if let Some(ref mut net_tx) = transport {
            net_tx.send_mode_transition(effective_target, reason, state_machine.epoch());
            let _ = net_tx.send_control_payload(&WirePayload::Heartbeat {
                sender_role: role,
                active_mode: effective_target,
                epoch: state_machine.epoch(),
                sender_timestamp_ns: post_grab_ns,
            });
        }
    }

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

    // Validate before touching input, displays, shared memory, or network sockets.
    let psk = if cfg.self_test {
        None
    } else {
        match resolve_psk_from_env() {
            Ok(key) => Some(key),
            Err(error) => {
                error!("Cannot start daemon: {error}");
                std::process::exit(1);
            }
        }
    };

    let shm = match ShmWatchdog::open(true) {
        Ok(mapping) => Arc::new(mapping),
        Err(error) => {
            error!("Cannot start daemon: {error}");
            std::process::exit(1);
        }
    };

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

    let local_physical_displays = display_pal
        .enumerate_physical_displays()
        .unwrap_or_default();
    info!(
        local_monitors = local_physical_displays.len(),
        "Enumerated local physical displays for peer EDID advertisement"
    );

    let mut input_pal =
        create_input_backend(Some(shm.clone())).expect("Failed to create InputBackend");
    let _ = input_pal.release_all_stuck_keys();

    let psk = psk.expect("link key was validated before initialization");
    let mut transport = match DuplexTransport::bind_strict(
        cfg.role,
        cfg.local_ip,
        cfg.peer_ip,
        &psk,
    ) {
        Ok(t) => Some(t),
        Err(e) => {
            if cfg.check_only {
                warn!(
                    "Interface {} not yet up during --check-init: {e}",
                    cfg.local_ip
                );
                return;
            }
            warn!(
                    "Peer transport unavailable at {} ({e}); staying idle and retrying. Check the link address and whether another process owns the ports",
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

    let main_loop_progress_ns = Arc::new(core::sync::atomic::AtomicU64::new(monotonic_now_ns()));
    {
        let shm_bg = shm.clone();
        let progress_bg = main_loop_progress_ns.clone();
        let _ = thread::Builder::new()
            .name("duplexd-hb-keeper".to_string())
            .spawn(move || loop {
                let now = monotonic_now_ns();
                let last_progress = progress_bg.load(Ordering::Acquire);
                // Keep the 250ms SHM daemon heartbeat fresh while the main loop is executing
                // synchronous xrandr/GStreamer/EVIOCGRAB mode transitions (up to 3s stall cap).
                if now.saturating_sub(last_progress) <= 3_000_000_000 {
                    shm_bg.block().beat_daemon(now);
                }
                thread::sleep(Duration::from_millis(20));
            });
    }

    let mut captured_buf: Vec<CapturedInputEvent> = Vec::with_capacity(64);
    let mut outgoing_hid_buf: Vec<HidEvent> = Vec::with_capacity(64);
    let mut incoming_payloads: Vec<WirePayload> = Vec::with_capacity(32);
    let mut refresh_monitors: Vec<u8> = Vec::with_capacity(4);
    let mut last_refresh_request_ns = [0u64; 2];
    let mut peer_video = PeerVideoCaps::default();
    let mut last_local_transition_ns: u64 = 0;
    let mut last_input_error_ns: u64 = 0;

    loop {
        let now_ns = monotonic_now_ns();
        main_loop_progress_ns.store(now_ns, Ordering::Release);
        let block = shm.block();
        block.beat_daemon(now_ns);

        // Address removal leaves old bound sockets alive. Drop them so reconnect
        // rebuilds TCP and UDP state instead of retaining a dead connection.
        if now_ns.saturating_sub(last_bind_retry_ns) >= 1_000_000_000 {
            last_bind_retry_ns = now_ns;
            if transport.is_some() && !duplex_pal::local_link_address_available(cfg.local_ip) {
                warn!(local_ip = %cfg.local_ip, "PC-link address disappeared; dropping stale transport and waiting for network reconnection");
                transport = None;
            }
            if transport.is_none() {
                if let Ok(t) =
                    DuplexTransport::bind_strict(cfg.role, cfg.local_ip, cfg.peer_ip, &psk)
                {
                    info!(
                        local_ip = %cfg.local_ip,
                        peer_ip = %cfg.peer_ip,
                        "PC-link address available — strict QoS UDP/TCP transport bound; waiting for authenticated peer"
                    );
                    transport = Some(t);
                }
            }
        }

        // Transition grace timestamps are for the guard, not proof of a peer.
        let connected = transport
            .as_ref()
            .is_some_and(|net| net.has_recent_authenticated_peer(now_ns));
        let previously_connected = block.peer_connected.swap(connected, Ordering::AcqRel);
        if previously_connected && !connected {
            peer_video = PeerVideoCaps::default();
            video_pal.set_peer_video_caps(peer_video);
        }

        // Advertise displays and video capabilities immediately when a peer
        // appears (so the first stream already uses the best codec), then
        // periodically so the peer can spawn matching virtual displays.
        if (connected && !previously_connected)
            || now_ns.saturating_sub(last_display_adv_ns) >= DISPLAY_ADVERTISE_INTERVAL_NS
        {
            last_display_adv_ns = now_ns;
            if let Some(ref mut net_io) = transport {
                let adv = WirePayload::AdvertiseDisplays {
                    role: cfg.role,
                    displays: local_physical_displays.clone(),
                };
                let _ = net_io.send_control_payload(&adv);
                let _ = net_io.send_control_payload(&WirePayload::VideoCapabilities {
                    native_lossless: true,
                });
                let _ = net_io.send_control_payload(&WirePayload::VideoCapabilitiesV2 {
                    codecs: VIDEO_CAP_NATIVE_BANDS | VIDEO_CAP_TILES,
                });
            }
        }

        // Video health: decoder/reassembly loss becomes a rate-limited refresh
        // request; a failed capture helper or viewer ends the session loudly.
        refresh_monitors.clear();
        let video_health = video_pal.poll_health(&mut refresh_monitors);
        if let Some(net_io) = transport.as_ref() {
            let gaps = net_io.take_video_gaps();
            refresh_monitors.extend((0..2u8).filter(|monitor| gaps & (1 << monitor) != 0));
            let stats = net_io.video_stats();
            block
                .tx_video_frames
                .store(stats.tx_units.load(Ordering::Relaxed), Ordering::Relaxed);
            block
                .rx_video_frames
                .store(stats.rx_units.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        if state_machine
            .current_mode()
            .expected_incoming_video_streams(cfg.role)
            > 0
        {
            for &monitor in &refresh_monitors {
                let slot = usize::from(monitor.min(1));
                if now_ns.saturating_sub(last_refresh_request_ns[slot]) < VIDEO_REFRESH_INTERVAL_NS {
                    continue;
                }
                last_refresh_request_ns[slot] = now_ns;
                block
                    .pli_keyframes_requested
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(ref mut net_io) = transport {
                    let _ = net_io.send_control_payload(&WirePayload::VideoKeyframeRequest {
                        monitor_index: monitor,
                    });
                }
            }
        }
        if let Err(error) = video_health {
            if state_machine.current_mode() != OperationalMode::Mode0Idle {
                error!(%error, "Video session failed; returning to Idle");
                if let Some(ref mut net_io) = transport {
                    let _ = net_io.send_control_payload(&WirePayload::OperationError {
                        message: error.to_string(),
                    });
                }
                block
                    .requested_mode
                    .store(OperationalMode::Mode0Idle.as_u8(), Ordering::Release);
            }
        }

        // 1. Check if duplex-guard requested an emergency reset
        if block.force_reset_requested.swap(false, Ordering::AcqRel) {
            warn!("Received emergency reset signal from duplex-guard; reverting to MODE_0_IDLE");
            last_local_transition_ns = now_ns;
            apply_mode_transition(
                cfg.role,
                &mut state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::WatchdogTimeout,
                None,
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

        // 2. Check if duplex-ctl / GUI requested an operator mode transition via shared memory
        let req = block
            .requested_mode
            .swap(NO_REQUESTED_MODE, Ordering::AcqRel);
        if req != NO_REQUESTED_MODE {
            let target_mode = OperationalMode::from_u8_lossy(req);
            last_local_transition_ns = now_ns;
            apply_mode_transition(
                cfg.role,
                &mut state_machine,
                target_mode,
                TransitionReason::OperatorCommand,
                None,
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

        match input_pal.poll_captured_events(&mut captured_buf) {
            Ok(count) => {
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
            Err(error) => {
                if now_ns.saturating_sub(last_input_error_ns) >= 1_000_000_000 {
                    last_input_error_ns = now_ns;
                    error!(%error, "Local input capture failed; returning to Idle");
                }
                block
                    .requested_mode
                    .store(OperationalMode::Mode0Idle.as_u8(), Ordering::Release);
            }
        }

        if !outgoing_hid_buf.is_empty() {
            if let Some(ref mut net_tx) = transport {
                match net_tx.send_hid_events(&outgoing_hid_buf, now_ns) {
                    Ok(()) => {
                        block.tx_hid_packets.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => {
                        if now_ns.saturating_sub(last_input_error_ns) >= 1_000_000_000 {
                            last_input_error_ns = now_ns;
                            error!(%error, "Remote input send failed; returning to Idle");
                        }
                        block
                            .requested_mode
                            .store(OperationalMode::Mode0Idle.as_u8(), Ordering::Release);
                    }
                }
            }
        }

        if let Some(kind) = panic_tripped {
            warn!(
                ?kind,
                "Hardware panic escape sequence detected! Flushing remote modifiers and reverting to MODE_0_IDLE"
            );
            last_local_transition_ns = now_ns;
            if let Some(ref mut net_tx) = transport {
                net_tx.send_emergency_modifier_flush(now_ns);
            }
            apply_mode_transition(
                cfg.role,
                &mut state_machine,
                OperationalMode::Mode0Idle,
                TransitionReason::HardwarePanicEscape,
                None,
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

        // A transition can take time; never send a pre-transition heartbeat timestamp.
        let now_ns = monotonic_now_ns();

        // 4. Tick 50 Hz CS6 (0xC0) heartbeat & poll incoming encrypted HID/control.
        // Video has its own threads and never delays this loop.
        incoming_payloads.clear();
        if let Some(ref mut net_io) = transport {
            net_io.tick_heartbeat(state_machine.current_mode(), state_machine.epoch(), now_ns);
            net_io.poll_incoming(&mut incoming_payloads);
        }

        for payload in incoming_payloads.drain(..) {
            match payload {
                WirePayload::OperationError { message } => {
                    block.beat_peer(now_ns);
                    error!(message = %message, "Peer operation failed");
                }
                WirePayload::VideoCapabilities { native_lossless } => {
                    block.beat_peer(now_ns);
                    peer_video.native_lossless = native_lossless;
                    video_pal.set_peer_video_caps(peer_video);
                }
                WirePayload::VideoCapabilitiesV2 { codecs } => {
                    block.beat_peer(now_ns);
                    peer_video.native_lossless |= codecs & VIDEO_CAP_NATIVE_BANDS != 0;
                    peer_video.tiles = codecs & VIDEO_CAP_TILES != 0;
                    video_pal.set_peer_video_caps(peer_video);
                }
                WirePayload::HidBatch { events, .. } => {
                    block.beat_peer(now_ns);
                    block.rx_hid_packets.fetch_add(1, Ordering::Relaxed);
                    let cur_mode = state_machine.current_mode();
                    let inject_allowed = cur_mode != OperationalMode::Mode0Idle
                        && !cur_mode.should_capture_input(cfg.role);

                    for ev in &events {
                        if matches!(ev, HidEvent::ReleaseAllModifiers) || inject_allowed {
                            if let Err(error) = input_pal.inject_hid_event(ev) {
                                if now_ns.saturating_sub(last_input_error_ns) >= 1_000_000_000 {
                                    last_input_error_ns = now_ns;
                                    error!(%error, "Remote input injection failed; returning to Idle");
                                }
                                block
                                    .requested_mode
                                    .store(OperationalMode::Mode0Idle.as_u8(), Ordering::Release);
                                break;
                            }
                        }
                    }
                }
                WirePayload::CursorUpdate(_) => {
                    // Cursors are composited into the video by current senders.
                    block.beat_peer(now_ns);
                }
                WirePayload::Heartbeat {
                    sender_timestamp_ns,
                    active_mode,
                    epoch,
                    ..
                } => {
                    block.beat_peer(now_ns);
                    if active_mode == state_machine.current_mode() {
                        state_machine.sync_epoch(epoch);
                    } else if epoch > state_machine.epoch()
                        && now_ns.saturating_sub(last_local_transition_ns) > 1_000_000_000
                    {
                        apply_mode_transition(
                            cfg.role,
                            &mut state_machine,
                            active_mode,
                            TransitionReason::PeerRequest,
                            Some(epoch),
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
                    if let Some(ref mut net_io) = transport {
                        let ack = WirePayload::HeartbeatAck {
                            origin_timestamp_ns: sender_timestamp_ns,
                            active_mode: state_machine.current_mode(),
                            epoch: state_machine.epoch(),
                        };
                        let _ = net_io.send_control_payload(&ack);
                    }
                }
                WirePayload::HeartbeatAck {
                    origin_timestamp_ns,
                    active_mode,
                    epoch,
                } => {
                    block.beat_peer(now_ns);
                    if active_mode == state_machine.current_mode() {
                        state_machine.sync_epoch(epoch);
                    }
                    let rtt_ns = now_ns.saturating_sub(origin_timestamp_ns);
                    block.record_rtt_sample(rtt_ns);
                }
                WirePayload::ModeTransition {
                    target_mode,
                    reason,
                    epoch,
                } => {
                    block.beat_peer(now_ns);
                    apply_mode_transition(
                        cfg.role,
                        &mut state_machine,
                        target_mode,
                        reason,
                        Some(epoch),
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
