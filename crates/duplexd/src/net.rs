//! Encrypted QoS Network Transport Multiplexer (`duplexd::net`).
//!
//! Manages strictly bound (`192.168.137.1` / `192.168.137.2`) sockets across all three QoS tiers:
//! - **HID Stream (`UDP 48100`, `DSCP EF` -> `IP_TOS 0xB8`):** Zero-allocation in-place `ChaCha20-Poly1305`
//! - **Control & 50 Hz Heartbeat (`TCP/UDP 48101`, `DSCP CS6` -> `IP_TOS 0xC0`):** `ChaCha20-Poly1305`
//! - **Video Streams (`UDP 48102-48103`, `DSCP AF41` -> `IP_TOS 0x88`):** dedicated threads: a paced
//!   transmitter (MTU-safe `1380B` fragments) and per-monitor receivers whose `VideoFrameAssembler`
//!   reports loss for refresh requests. Video never runs on the daemon's input/heartbeat loop.

use duplex_pal::{
    bind_qos_tcp_listener, bind_qos_udp_socket, configure_tcp_stream_qos, PalError, QosClass,
    SinkError, VideoFrameConsumer, VideoSink,
};
use duplex_proto::{
    emit_video_datagrams, ChannelId, CryptoSession, EncodedVideoFrame, HidEvent, NodeRole,
    OperationalMode, TransitionReason, VideoFrameAssembler, WirePayload, DEBIAN_LAPTOP_IPV4,
    MAX_VIDEO_FRAGMENT_PAYLOAD, PEER_HEARTBEAT_INTERVAL_NS, PORT_CONTROL_TCP, PORT_HID_UDP,
    PORT_VIDEO_STREAM_0_UDP, PORT_VIDEO_STREAM_1_UDP, VIDEO_FRAGMENT_HEADER_LEN,
    WINDOWS_HOST_IPV4,
};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    mpsc, Arc, Mutex, RwLock,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Encoded units queued for transmission (~4 MB); a full queue makes
/// encoders skip captures instead of building latency.
const VIDEO_TX_QUEUE: usize = 64;

/// Outgoing video throughput limit in bytes per second. `DUPLEX_VIDEO_MBPS`
/// overrides the 700 Mbit/s default, which leaves headroom on the gigabit
/// link for input, control and internet sharing.
fn video_rate_bytes_per_sec() -> f64 {
    std::env::var("DUPLEX_VIDEO_MBPS")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|mbps| (20.0..=10_000.0).contains(mbps))
        .unwrap_or(700.0)
        * 125_000.0
}

/// Token bucket smoothing bursts (a 4K frame is megabytes) into a steady
/// stream that switches and receive buffers absorb without loss.
struct Pacer {
    bytes_per_ns: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl Pacer {
    fn new(bytes_per_sec: f64) -> Self {
        Self {
            bytes_per_ns: bytes_per_sec / 1e9,
            // Two milliseconds of credit amortises timer granularity.
            burst: (bytes_per_sec / 500.0).max(65_536.0),
            tokens: 0.0,
            last: Instant::now(),
        }
    }

    fn wait(&mut self, bytes: usize, stop: &AtomicBool) {
        loop {
            let now = Instant::now();
            let earned = now.duration_since(self.last).as_nanos() as f64 * self.bytes_per_ns;
            self.tokens = (self.tokens + earned).min(self.burst);
            self.last = now;
            if self.tokens >= bytes as f64 || stop.load(Ordering::Relaxed) {
                self.tokens -= bytes as f64;
                return;
            }
            let deficit_ns = (bytes as f64 - self.tokens) / self.bytes_per_ns;
            thread::sleep(Duration::from_nanos(deficit_ns.max(200_000.0) as u64));
        }
    }
}

#[derive(Default)]
pub struct VideoLinkStats {
    pub tx_units: AtomicU64,
    pub rx_units: AtomicU64,
    pub tx_dropped_datagrams: AtomicU64,
}

/// Video sockets serviced by their own threads, never the daemon loop: one
/// paced transmitter for both monitors and one blocking receiver per monitor.
struct VideoLink {
    stop: Arc<AtomicBool>,
    tx: mpsc::SyncSender<EncodedVideoFrame>,
    consumer: Arc<RwLock<Option<Arc<dyn VideoFrameConsumer>>>>,
    gaps: Arc<AtomicU32>,
    generation: Arc<AtomicU32>,
    stats: Arc<VideoLinkStats>,
    threads: Vec<JoinHandle<()>>,
}

struct VideoTxSink(mpsc::SyncSender<EncodedVideoFrame>);

impl VideoSink for VideoTxSink {
    fn try_send(&self, frame: EncodedVideoFrame) -> Result<(), SinkError> {
        match self.0.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(frame)) => Err(SinkError::Full(frame)),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(SinkError::Closed),
        }
    }
}

impl VideoLink {
    fn start(sockets: [&UdpSocket; 2], peers: [SocketAddr; 2]) -> Result<Self, PalError> {
        let clone = |socket: &UdpSocket| socket.try_clone().map_err(|e| PalError::Network(format!("Cannot share video socket: {e}")));
        let tx_sockets = [clone(sockets[0])?, clone(sockets[1])?];
        let rx_sockets = [clone(sockets[0])?, clone(sockets[1])?];
        let stop = Arc::new(AtomicBool::new(false));
        let consumer: Arc<RwLock<Option<Arc<dyn VideoFrameConsumer>>>> = Arc::new(RwLock::new(None));
        let gaps = Arc::new(AtomicU32::new(0));
        let generation = Arc::new(AtomicU32::new(0));
        let stats = Arc::new(VideoLinkStats::default());
        let (tx, rx) = mpsc::sync_channel(VIDEO_TX_QUEUE);
        let mut link = Self { stop, tx, consumer, gaps, generation, stats, threads: Vec::new() };
        let (stop, stats) = (link.stop.clone(), link.stats.clone());
        link.spawn("duplex-video-tx", move || video_tx_loop(rx, tx_sockets, peers, &stop, &stats))?;
        for (index, socket) in rx_sockets.into_iter().enumerate() {
            let peer_ip = peers[index].ip();
            let (stop, stats) = (link.stop.clone(), link.stats.clone());
            let (consumer, gaps, generation) = (link.consumer.clone(), link.gaps.clone(), link.generation.clone());
            link.spawn(&format!("duplex-video-rx-{index}"), move || {
                video_rx_loop(index as u8, socket, peer_ip, &consumer, &gaps, &generation, &stop, &stats)
            })?;
        }
        Ok(link)
    }

    fn spawn(&mut self, name: &str, body: impl FnOnce() + Send + 'static) -> Result<(), PalError> {
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(body)
            .map_err(|e| PalError::Network(format!("Cannot start {name}: {e}")))?;
        self.threads.push(handle);
        Ok(())
    }
}

impl Drop for VideoLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn video_tx_loop(
    rx: mpsc::Receiver<EncodedVideoFrame>,
    sockets: [UdpSocket; 2],
    peers: [SocketAddr; 2],
    stop: &AtomicBool,
    stats: &VideoLinkStats,
) {
    let mut pacer = Pacer::new(video_rate_bytes_per_sec());
    let mut scratch = Vec::with_capacity(VIDEO_FRAGMENT_HEADER_LEN + MAX_VIDEO_FRAGMENT_PAYLOAD);
    let mut last_error = None::<Instant>;
    while !stop.load(Ordering::Acquire) {
        let frame = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => frame,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let index = usize::from(frame.monitor_index.min(1));
        let (socket, peer) = (&sockets[index], peers[index]);
        emit_video_datagrams(&frame, &mut scratch, |datagram| {
            pacer.wait(datagram.len(), stop);
            let deadline = Instant::now() + Duration::from_millis(50);
            loop {
                match socket.send_to(datagram, peer) {
                    Ok(_) => break,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline
                            && !stop.load(Ordering::Relaxed) =>
                    {
                        thread::sleep(Duration::from_micros(200));
                    }
                    Err(error) => {
                        // The receiver detects the incomplete unit and asks for a refresh.
                        stats.tx_dropped_datagrams.fetch_add(1, Ordering::Relaxed);
                        if last_error.is_none_or(|at| at.elapsed() > Duration::from_secs(5)) {
                            last_error = Some(Instant::now());
                            warn!(%error, %peer, "Video datagram not sent");
                        }
                        break;
                    }
                }
            }
            !stop.load(Ordering::Relaxed)
        });
        stats.tx_units.fetch_add(1, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
fn video_rx_loop(
    index: u8,
    socket: UdpSocket,
    peer_ip: IpAddr,
    consumer: &RwLock<Option<Arc<dyn VideoFrameConsumer>>>,
    gaps: &AtomicU32,
    generation: &AtomicU32,
    stop: &AtomicBool,
    stats: &VideoLinkStats,
) {
    // Blocking with a timeout: the thread sleeps in the kernel between
    // datagrams yet notices shutdown promptly.
    let _ = socket.set_nonblocking(false);
    let _ = socket.set_read_timeout(Some(Duration::from_millis(50)));
    let mut assembler = VideoFrameAssembler::new(index);
    let mut seen_generation = generation.load(Ordering::Acquire);
    let mut buffer = [0u8; 2048];
    while !stop.load(Ordering::Acquire) {
        let current = generation.load(Ordering::Acquire);
        if current != seen_generation {
            seen_generation = current;
            assembler = VideoFrameAssembler::new(index);
        }
        let (len, source) = match socket.recv_from(&mut buffer) {
            Ok(received) => received,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                        // Windows reports ICMP port-unreachable this way on UDP.
                        | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                continue
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        if source.ip() != peer_ip {
            continue;
        }
        let Ok((frame, gap)) = assembler.push_datagram(&buffer[..len]) else {
            continue;
        };
        if gap {
            gaps.fetch_or(1 << index, Ordering::AcqRel);
        }
        if let Some(frame) = frame {
            stats.rx_units.fetch_add(1, Ordering::Relaxed);
            if let Ok(consumer) = consumer.read() {
                if let Some(consumer) = consumer.as_ref() {
                    consumer.submit(frame);
                }
            }
        }
    }
}

/// Coalesces consecutive relative mouse motion and wheel micro-deltas within a single poll tick
/// while preserving exact key/button ordering.
pub fn coalesce_hid_events(raw: &[HidEvent], coalesced: &mut Vec<HidEvent>) {
    coalesced.clear();
    for &ev in raw {
        match (coalesced.last_mut(), ev) {
            (
                Some(HidEvent::MouseMoveRel {
                    dx: acc_dx,
                    dy: acc_dy,
                }),
                HidEvent::MouseMoveRel { dx, dy },
            ) => {
                *acc_dx = acc_dx.saturating_add(dx);
                *acc_dy = acc_dy.saturating_add(dy);
            }
            (
                Some(HidEvent::MouseWheel {
                    vertical: acc_v,
                    horizontal: acc_h,
                }),
                HidEvent::MouseWheel {
                    vertical,
                    horizontal,
                },
            ) => {
                *acc_v = acc_v.saturating_add(vertical);
                *acc_h = acc_h.saturating_add(horizontal);
            }
            _ => coalesced.push(ev),
        }
    }
}

/// Resolves the default `(local_ip, peer_ip)` pair for a given `NodeRole`.
pub fn default_link_ips(role: NodeRole) -> (Ipv4Addr, Ipv4Addr) {
    let win_ip: Ipv4Addr = WINDOWS_HOST_IPV4.parse().unwrap();
    let deb_ip: Ipv4Addr = DEBIAN_LAPTOP_IPV4.parse().unwrap();
    match role {
        NodeRole::DebianLaptop => (deb_ip, win_ip),
        NodeRole::WindowsDesktop => (win_ip, deb_ip),
    }
}

/// Resolve an explicit environment override or the user's saved pairing key.
pub fn resolve_psk_from_env() -> Result<[u8; 32], String> {
    crate::pairing::load_key()
}

#[cfg(test)]
fn parse_link_psk(value: &str) -> Result<[u8; 32], String> {
    crate::pairing::parse_key(value)
}

/// Symmetric encrypted & QoS-tagged transport managing:
/// - UDP `48100` (HID, DSCP EF `0xB8`)
/// - TCP/UDP `48101` (Control & 50 Hz Heartbeat, DSCP CS6 `0xC0`)
/// - UDP `48102` & `48103` (per-monitor video streams, DSCP AF41 `0x88`)
pub struct DuplexTransport {
    role: NodeRole,
    peer_hid_addr: SocketAddr,
    peer_ctrl_addr: SocketAddr,
    hid_udp: UdpSocket,
    ctrl_udp: UdpSocket,
    ctrl_tcp_listener: TcpListener,
    ctrl_tcp_stream: Option<TcpStream>,
    video: VideoLink,
    hid_crypto: CryptoSession,
    ctrl_crypto: Arc<Mutex<CryptoSession>>,
    deferred_control_tx: mpsc::SyncSender<WirePayload>,
    deferred_control_rx: mpsc::Receiver<WirePayload>,
    tx_scratch: Vec<u8>,
    rx_scratch: [u8; 2048],
    coalesce_scratch: Vec<HidEvent>,
    last_heartbeat_sent_ns: u64,
    last_tcp_connect_attempt_ns: u64,
    last_authenticated_peer_ns: Arc<AtomicU64>,
}

pub struct TransitionKeepalive {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for TransitionKeepalive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl DuplexTransport {
    pub fn bind_strict(
        role: NodeRole,
        local_ip: Ipv4Addr,
        peer_ip: Ipv4Addr,
        psk: &[u8; 32],
    ) -> Result<Self, PalError> {
        let hid_udp =
            bind_qos_udp_socket(local_ip, PORT_HID_UDP, QosClass::HidExpeditedForwarding)?;
        let ctrl_udp =
            bind_qos_udp_socket(local_ip, PORT_CONTROL_TCP, QosClass::ControlNetworkControl)?;
        let ctrl_tcp_listener =
            bind_qos_tcp_listener(local_ip, PORT_CONTROL_TCP, QosClass::ControlNetworkControl)?;
        let video_0_udp = bind_qos_udp_socket(
            local_ip,
            PORT_VIDEO_STREAM_0_UDP,
            QosClass::VideoAssuredForwarding,
        )?;
        let video_1_udp = bind_qos_udp_socket(
            local_ip,
            PORT_VIDEO_STREAM_1_UDP,
            QosClass::VideoAssuredForwarding,
        )?;

        let (hid_tx_ch, hid_rx_ch, ctrl_tx_ch, ctrl_rx_ch) = match role {
            NodeRole::DebianLaptop => (
                ChannelId::HidLaptopToDesktop,
                ChannelId::HidDesktopToLaptop,
                ChannelId::ControlLaptopToDesktop,
                ChannelId::ControlDesktopToLaptop,
            ),
            NodeRole::WindowsDesktop => (
                ChannelId::HidDesktopToLaptop,
                ChannelId::HidLaptopToDesktop,
                ChannelId::ControlDesktopToLaptop,
                ChannelId::ControlLaptopToDesktop,
            ),
        };

        let peer_hid_addr = SocketAddr::V4(SocketAddrV4::new(peer_ip, PORT_HID_UDP));
        let peer_ctrl_addr = SocketAddr::V4(SocketAddrV4::new(peer_ip, PORT_CONTROL_TCP));
        let peer_video_0_addr = SocketAddr::V4(SocketAddrV4::new(peer_ip, PORT_VIDEO_STREAM_0_UDP));
        let peer_video_1_addr = SocketAddr::V4(SocketAddrV4::new(peer_ip, PORT_VIDEO_STREAM_1_UDP));

        info!(
            ?role,
            %local_ip,
            %peer_ip,
            hid_port = PORT_HID_UDP,
            ctrl_port = PORT_CONTROL_TCP,
            video_ports = ?(PORT_VIDEO_STREAM_0_UDP, PORT_VIDEO_STREAM_1_UDP),
            "Bound strict QoS sockets (EF=0xB8, CS6=0xC0, AF41=0x88)"
        );

        let video = VideoLink::start(
            [&video_0_udp, &video_1_udp],
            [peer_video_0_addr, peer_video_1_addr],
        )?;
        let (deferred_control_tx, deferred_control_rx) = mpsc::sync_channel(256);
        Ok(Self {
            role,
            peer_hid_addr,
            peer_ctrl_addr,
            hid_udp,
            ctrl_udp,
            ctrl_tcp_listener,
            ctrl_tcp_stream: None,
            video,
            hid_crypto: CryptoSession::new(psk, hid_tx_ch, hid_rx_ch),
            ctrl_crypto: Arc::new(Mutex::new(CryptoSession::new(psk, ctrl_tx_ch, ctrl_rx_ch))),
            deferred_control_tx,
            deferred_control_rx,
            tx_scratch: Vec::with_capacity(512),
            rx_scratch: [0u8; 2048],
            coalesce_scratch: Vec::with_capacity(64),
            last_heartbeat_sent_ns: 0,
            last_tcp_connect_attempt_ns: 0,
            last_authenticated_peer_ns: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Paced, thread-safe destination for encoded video to the peer.
    pub fn video_sink(&self) -> Arc<dyn VideoSink> {
        Arc::new(VideoTxSink(self.video.tx.clone()))
    }

    /// Routes received video to `consumer` (or discards it). Reassembly
    /// restarts so a new stream is never compared with an old sequence.
    pub fn set_video_consumer(&self, consumer: Option<Arc<dyn VideoFrameConsumer>>) {
        if let Ok(mut current) = self.video.consumer.write() {
            *current = consumer;
        }
        self.video.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Bitmask of monitors whose received stream lost data since last call.
    pub fn take_video_gaps(&self) -> u32 {
        self.video.gaps.swap(0, Ordering::AcqRel)
    }

    pub fn video_stats(&self) -> &VideoLinkStats {
        &self.video.stats
    }

    /// Keep the existing control channel live during synchronous display setup.
    /// The three-second cap matches the daemon progress keeper; a true hang must
    /// still expire. The previous mode/epoch avoids advertising unfinished setup.
    pub fn transition_keepalive(
        &self,
        previous_mode: OperationalMode,
        previous_epoch: u64,
        on_authenticated_peer: impl Fn(u64) + Send + 'static,
    ) -> Result<TransitionKeepalive, PalError> {
        let socket = self
            .ctrl_udp
            .try_clone()
            .map_err(|e| PalError::Network(e.to_string()))?;
        let crypto = self.ctrl_crypto.clone();
        let deferred = self.deferred_control_tx.clone();
        let peer = self.peer_ctrl_addr;
        let role = self.role;
        let last_authenticated = self.last_authenticated_peer_ns.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("duplex-transition-control".into())
            .spawn(move || {
                let start = Instant::now();
                let mut last_send = Instant::now() - Duration::from_millis(20);
                let mut rx = [0; 2048];
                let mut tx = Vec::with_capacity(128);
                while !worker_stop.load(Ordering::Acquire)
                    && start.elapsed() < Duration::from_secs(3)
                {
                    let now = duplex_pal::monotonic_now_ns();
                    if last_send.elapsed() >= Duration::from_millis(20) {
                        let heartbeat = WirePayload::Heartbeat {
                            sender_role: role,
                            active_mode: previous_mode,
                            epoch: previous_epoch,
                            sender_timestamp_ns: now,
                        };
                        if crypto
                            .lock()
                            .unwrap()
                            .seal_frame_into(&heartbeat, &mut tx)
                            .is_ok()
                        {
                            let _ = socket.send_to(&tx, peer);
                        }
                        last_send = Instant::now();
                    }
                    for _ in 0..128 {
                        let Ok((len, source)) = socket.recv_from(&mut rx) else {
                            break;
                        };
                        if source != peer {
                            continue;
                        }
                        let packet = crypto.lock().unwrap().open_frame_in_place(&mut rx[..len]);
                        if let Ok((_, payload)) = packet {
                            let received_at = duplex_pal::monotonic_now_ns();
                            last_authenticated.store(received_at, Ordering::Release);
                            on_authenticated_peer(received_at);
                            let _ = deferred.try_send(payload);
                        }
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            })
            .map_err(|e| PalError::Network(format!("Cannot start transition heartbeat: {e}")))?;
        Ok(TransitionKeepalive {
            stop,
            worker: Some(worker),
        })
    }

    /// Coalesces micro-deltas and transmits an encrypted `WirePayload::HidBatch` over UDP 48100 (`DSCP EF`).
    pub fn send_hid_events(
        &mut self,
        raw_events: &[HidEvent],
        now_ns: u64,
    ) -> Result<(), PalError> {
        if raw_events.is_empty() {
            return Ok(());
        }
        coalesce_hid_events(raw_events, &mut self.coalesce_scratch);

        let events = core::mem::take(&mut self.coalesce_scratch);
        let payload = WirePayload::HidBatch {
            sender_timestamp_ns: now_ns,
            events,
        };

        let res = self
            .hid_crypto
            .seal_frame_into(&payload, &mut self.tx_scratch)
            .map_err(|e| PalError::Network(format!("HID seal_frame_into failed: {e}")));

        if let WirePayload::HidBatch { mut events, .. } = payload {
            events.clear();
            self.coalesce_scratch = events;
        }

        res?;
        self.hid_udp
            .send_to(&self.tx_scratch, self.peer_hid_addr)
            .map_err(|e| {
                PalError::Network(format!("HID send to {} failed: {e}", self.peer_hid_addr))
            })?;
        Ok(())
    }

    /// Transmits a redundant 3-datagram high-priority burst of `HidEvent::ReleaseAllModifiers`
    /// over UDP 48100 (`DSCP EF` `0xB8`) so the remote peer is guaranteed to release `Ctrl`, `Alt`,
    /// `Shift`, and `Super/Win` immediately when the panic escape hatch or mode exit triggers.
    pub fn send_emergency_modifier_flush(&mut self, now_ns: u64) {
        let flush_event = [HidEvent::ReleaseAllModifiers];
        for _ in 0..3 {
            let _ = self.send_hid_events(&flush_event, now_ns);
        }
    }

    /// Broadcasts a control payload (`Heartbeat`, `HeartbeatAck`, `ModeTransition`, `AdvertiseDisplays`, `VideoKeyframeRequest`)
    /// over the CS6 control channel (`UDP 48101`, `DSCP CS6 0xC0`).
    pub fn send_control_payload(&mut self, payload: &WirePayload) -> Result<(), PalError> {
        self.ctrl_crypto
            .lock()
            .unwrap()
            .seal_frame_into(payload, &mut self.tx_scratch)
            .map_err(|e| PalError::Network(format!("Control seal_frame_into failed: {e}")))?;

        self.ctrl_udp
            .send_to(&self.tx_scratch, self.peer_ctrl_addr)
            .map_err(|e| {
                PalError::Network(format!(
                    "Control send to {} failed: {e}",
                    self.peer_ctrl_addr
                ))
            })?;
        Ok(())
    }

    /// Sends a 3-datagram high-reliability mode transition burst to the peer (each with a fresh sequence number).
    pub fn send_mode_transition(
        &mut self,
        target_mode: OperationalMode,
        reason: TransitionReason,
        epoch: u64,
    ) {
        let msg = WirePayload::ModeTransition {
            target_mode,
            reason,
            epoch,
        };
        for _ in 0..3 {
            let _ = self.send_control_payload(&msg);
        }
    }

    /// Non-blocking 50 Hz (`20ms`) heartbeat transmitter & TCP connection maintenance.
    pub fn tick_heartbeat(&mut self, active_mode: OperationalMode, epoch: u64, now_ns: u64) {
        if let Ok((stream, peer_addr)) = self.ctrl_tcp_listener.accept() {
            if configure_tcp_stream_qos(&stream, QosClass::ControlNetworkControl).is_ok() {
                debug!(%peer_addr, "Accepted incoming CS6 TCP control connection");
                self.ctrl_tcp_stream = Some(stream);
            }
        } else if self.ctrl_tcp_stream.is_none()
            && now_ns.saturating_sub(self.last_tcp_connect_attempt_ns) >= 1_000_000_000
        {
            self.last_tcp_connect_attempt_ns = now_ns;
            if let Ok(stream) = TcpStream::connect_timeout(
                &self.peer_ctrl_addr,
                std::time::Duration::from_millis(2),
            ) {
                if configure_tcp_stream_qos(&stream, QosClass::ControlNetworkControl).is_ok() {
                    debug!("Established outbound CS6 TCP control connection");
                    self.ctrl_tcp_stream = Some(stream);
                }
            }
        }

        if now_ns.saturating_sub(self.last_heartbeat_sent_ns) >= PEER_HEARTBEAT_INTERVAL_NS {
            self.last_heartbeat_sent_ns = now_ns;
            let hb = WirePayload::Heartbeat {
                sender_role: self.role,
                active_mode,
                epoch,
                sender_timestamp_ns: now_ns,
            };
            let _ = self.send_control_payload(&hb);
        }
    }

    pub fn has_recent_authenticated_peer(&self, now_ns: u64) -> bool {
        let last = self.last_authenticated_peer_ns.load(Ordering::Acquire);
        last > 0 && now_ns.saturating_sub(last) <= duplex_proto::WATCHDOG_TIMEOUT_NS
    }

    /// Non-blocking poll draining all authenticated HID, Cursor, and Control packets from UDP 48100 and TCP/UDP 48101.
    pub fn poll_incoming(&mut self, out_payloads: &mut Vec<WirePayload>) {
        out_payloads.extend(self.deferred_control_rx.try_iter());
        // 1. Drain high-priority HID & Decoupled Cursor UDP 48100 packets
        for _ in 0..128 {
            let Ok((len, src_addr)) = self.hid_udp.recv_from(&mut self.rx_scratch) else {
                break;
            };
            if src_addr.ip() != self.peer_hid_addr.ip() {
                continue;
            }
            if let Ok((_seq, payload)) = self
                .hid_crypto
                .open_frame_in_place(&mut self.rx_scratch[..len])
            {
                self.last_authenticated_peer_ns
                    .store(duplex_pal::monotonic_now_ns(), Ordering::Release);
                out_payloads.push(payload);
            }
        }

        // 2. Drain CS6 Control/Heartbeat UDP 48101 packets
        for _ in 0..128 {
            let Ok((len, src_addr)) = self.ctrl_udp.recv_from(&mut self.rx_scratch) else {
                break;
            };
            if src_addr.ip() != self.peer_ctrl_addr.ip() {
                continue;
            }
            if let Ok((_seq, payload)) = self
                .ctrl_crypto
                .lock()
                .unwrap()
                .open_frame_in_place(&mut self.rx_scratch[..len])
            {
                self.last_authenticated_peer_ns
                    .store(duplex_pal::monotonic_now_ns(), Ordering::Release);
                out_payloads.push(payload);
            }
        }

        // 3. Drain any length-prefixed frames on TCP 48101
        if let Some(ref mut stream) = self.ctrl_tcp_stream {
            let mut len_buf = [0u8; 2];
            match stream.read_exact(&mut len_buf) {
                Ok(()) => {
                    let frame_len = u16::from_be_bytes(len_buf) as usize;
                    if frame_len <= self.rx_scratch.len()
                        && stream.read_exact(&mut self.rx_scratch[..frame_len]).is_ok()
                    {
                        if let Ok((_seq, payload)) = self
                            .ctrl_crypto
                            .lock()
                            .unwrap()
                            .open_frame_in_place(&mut self.rx_scratch[..frame_len])
                        {
                            self.last_authenticated_peer_ns
                                .store(duplex_pal::monotonic_now_ns(), Ordering::Release);
                            out_payloads.push(payload);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    self.ctrl_tcp_stream = None;
                }
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use duplex_proto::{ShmWatchdogBlock, TileDecoder, TileEncoder, TileEncoderConfig};
    use std::time::{Duration, Instant};

    // Test fixture only; production requires an explicitly provisioned secret.
    const TEST_LINK_PSK: [u8; 32] = [0x5a; 32];

    #[test]
    fn link_key_rejects_invalid_and_public_secrets() {
        for value in [
            "",
            "short",
            &"g".repeat(64),
            &"é".repeat(32),
            &"00".repeat(32),
        ] {
            assert!(parse_link_psk(value).is_err());
        }
        let old_key: String = b"DuplexKvm-1GbE-Symmetric-Key-V01"
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(parse_link_psk(&old_key).is_err());
        assert_eq!(
            parse_link_psk(&format!(" {} ", "aB".repeat(32))).unwrap(),
            [0xab; 32]
        );
    }

    #[test]
    #[ignore = "requires local UDP/TCP sockets"]
    fn transition_keepalive_preserves_authentication_and_expires_on_stall() {
        use std::sync::atomic::AtomicU64;
        let local_ip = Ipv4Addr::new(127, 0, 0, 4);
        let peer_ip = Ipv4Addr::new(127, 0, 0, 5);
        let mut local = DuplexTransport::bind_strict(
            NodeRole::WindowsDesktop,
            local_ip,
            peer_ip,
            &TEST_LINK_PSK,
        )
        .unwrap();
        let mut peer =
            DuplexTransport::bind_strict(NodeRole::DebianLaptop, peer_ip, local_ip, &TEST_LINK_PSK)
                .unwrap();
        assert!(!local.has_recent_authenticated_peer(duplex_pal::monotonic_now_ns()));
        let last_authenticated = Arc::new(AtomicU64::new(duplex_pal::monotonic_now_ns()));
        let received_stamp = last_authenticated.clone();
        let mode = OperationalMode::Mode1PcPrimary {
            extend_display: true,
        };
        let worker = local
            .transition_keepalive(mode, 1, move |now| {
                received_stamp.store(now, Ordering::Release);
            })
            .unwrap();
        let start = Instant::now();
        let mut payloads = Vec::new();
        let mut heartbeats = 0;
        let mut transition_sent = false;
        // The local daemon does not poll at all while display setup is stalled.
        while start.elapsed() < Duration::from_secs(4) {
            let now = duplex_pal::monotonic_now_ns();
            peer.tick_heartbeat(mode, 1, now);
            if start.elapsed() > Duration::from_secs(1) && !transition_sent {
                peer.send_mode_transition(
                    OperationalMode::Mode2LaptopPrimary,
                    TransitionReason::PeerRequest,
                    2,
                );
                transition_sent = true;
            }
            payloads.clear();
            peer.poll_incoming(&mut payloads);
            heartbeats += payloads
                .iter()
                .filter(|payload| matches!(payload, WirePayload::Heartbeat { .. }))
                .count();
            if start.elapsed() < Duration::from_millis(2500) {
                assert!(
                    now.saturating_sub(last_authenticated.load(Ordering::Acquire))
                        < duplex_proto::WATCHDOG_TIMEOUT_NS
                );
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(heartbeats > 100);
        assert!(!local.has_recent_authenticated_peer(duplex_pal::monotonic_now_ns()));
        // A hung setup must stop extending liveness at the existing 3s progress cap.
        assert!(
            duplex_pal::monotonic_now_ns()
                .saturating_sub(last_authenticated.load(Ordering::Acquire))
                > 750_000_000
        );
        drop(worker);
        payloads.clear();
        local.poll_incoming(&mut payloads);
        assert!(payloads
            .iter()
            .any(|payload| matches!(payload, WirePayload::ModeTransition { epoch: 2, .. })));
        local
            .send_control_payload(&WirePayload::Heartbeat {
                sender_role: NodeRole::WindowsDesktop,
                active_mode: OperationalMode::Mode2LaptopPrimary,
                epoch: 2,
                sender_timestamp_ns: duplex_pal::monotonic_now_ns(),
            })
            .unwrap();
        payloads.clear();
        peer.poll_incoming(&mut payloads);
        assert!(
            payloads
                .iter()
                .any(|payload| matches!(payload, WirePayload::Heartbeat { epoch: 2, .. })),
            "handoff must retain the same crypto sequence and replay window"
        );
    }

    #[test]
    #[ignore = "requires local UDP/TCP socket access; binds isolated loopback addresses"]
    fn synthetic_video_and_control_heartbeats_survive_both_directions() {
        let laptop_ip = Ipv4Addr::new(127, 0, 0, 2);
        let desktop_ip = Ipv4Addr::new(127, 0, 0, 3);
        let mut laptop = DuplexTransport::bind_strict(
            NodeRole::DebianLaptop,
            laptop_ip,
            desktop_ip,
            &TEST_LINK_PSK,
        )
        .unwrap();
        let mut desktop = DuplexTransport::bind_strict(
            NodeRole::WindowsDesktop,
            desktop_ip,
            laptop_ip,
            &TEST_LINK_PSK,
        )
        .unwrap();
        // Private, in-process watchdog blocks; never touch a live daemon's SHM.
        let blocks: [ShmWatchdogBlock; 2] = unsafe { core::mem::zeroed() };
        let start_ns = duplex_pal::monotonic_now_ns();
        for block in &blocks {
            block.init_daemon(std::process::id(), start_ns);
            block
                .active_mode
                .store(1, std::sync::atomic::Ordering::Release);
        }
        // Each side decodes what its receive threads deliver.
        let consumers = [Arc::new(DecodingConsumer::default()), Arc::new(DecodingConsumer::default())];
        laptop.set_video_consumer(Some(consumers[0].clone()));
        let (width, height) = (320usize, 180usize);
        let frame_at = |cycle: usize, monitor: usize| -> Vec<u8> {
            let mut frame = vec![0u8; width * height * 4];
            for (i, pixel) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (i % width, i / width);
                let on = ((x + cycle * 3 + monitor * 50) / 7 + y / 11).is_multiple_of(3);
                pixel.copy_from_slice(if on { &[230, 230, 220, 0] } else { &[20, 24, 30, 0] });
            }
            frame
        };
        let mut heartbeats = [0; 2];
        let mut hid_batches = [0; 2];
        let mut last_hid_at = [Duration::ZERO; 2];
        let mut units_seen = [[0usize; 2]; 2];
        let mut refreshes = [0usize; 2];
        let mut last_refresh = [Instant::now() - Duration::from_secs(1); 2];
        let mut payloads = Vec::new();
        let mut units = Vec::new();
        let mut encoders: Vec<TileEncoder> = vec![TileEncoder::new(0, width as u16, height as u16, TileEncoderConfig::default(), 1)];
        let mut sent = 0usize;
        let mut final_frames = [Vec::new(), Vec::new()];
        let start = Instant::now();
        let mut last_capture = Instant::now() - Duration::from_secs(1);
        let mut cycle = 0;
        let mut switched = false;
        while start.elapsed() < Duration::from_secs(12) {
            let elapsed = start.elapsed();
            let now = duplex_pal::monotonic_now_ns();
            let mode2 = elapsed >= Duration::from_secs(6);
            let mode = if mode2 {
                OperationalMode::Mode2LaptopPrimary
            } else {
                OperationalMode::Mode1PcPrimary {
                    extend_display: true,
                }
            };
            if mode2 && !switched {
                // The laptop now sends two new streams; the desktop views them.
                laptop.set_video_consumer(None);
                desktop.set_video_consumer(Some(consumers[1].clone()));
                encoders = (0..2)
                    .map(|monitor| TileEncoder::new(monitor, width as u16, height as u16, TileEncoderConfig::default(), 10 + u32::from(monitor)))
                    .collect();
                switched = true;
            }
            laptop.tick_heartbeat(mode, 1 + u64::from(mode2), now);
            desktop.tick_heartbeat(mode, 1 + u64::from(mode2), now);
            if last_capture.elapsed() >= Duration::from_millis(33) {
                last_capture = Instant::now();
                let events = [
                    HidEvent::Key {
                        usage: 0x04,
                        pressed: cycle % 2 == 0,
                    },
                    HidEvent::MouseMoveRel { dx: 3, dy: -2 },
                ];
                // Both independently encrypted HID directions remain usable while
                // control and lossy video run. These events never reach OS input.
                laptop.send_hid_events(&events, now).unwrap();
                desktop.send_hid_events(&events, now).unwrap();
                let (sender, receiver) = if mode2 { (&laptop, 1) } else { (&desktop, 0) };
                let sink = sender.video_sink();
                // Content stops changing near the end so the stream must converge.
                let still = elapsed >= Duration::from_millis(10_500);
                for (monitor, encoder) in encoders.iter_mut().enumerate() {
                    let frame = frame_at(if still { 0 } else { cycle }, monitor);
                    units.clear();
                    encoder.encode_frame(&frame, now, &mut units);
                    final_frames[monitor] = frame;
                    for unit in units.drain(..) {
                        sent += 1;
                        // Exercise loss detection and refresh, except while converging.
                        if sent % 37 == 36 && !still {
                            continue;
                        }
                        let mut unit = unit;
                        loop {
                            match sink.try_send(unit) {
                                Ok(()) => break,
                                Err(SinkError::Full(returned)) => {
                                    unit = returned;
                                    thread::sleep(Duration::from_micros(200));
                                }
                                Err(SinkError::Closed) => panic!("video link closed"),
                            }
                        }
                    }
                }
                // Loss reported by reassembly or decoding triggers a refresh.
                let gaps = [&laptop, &desktop][receiver].take_video_gaps()
                    | consumers[receiver].refresh.swap(0, Ordering::AcqRel);
                for (monitor, encoder) in encoders.iter_mut().enumerate() {
                    if gaps & (1 << monitor) != 0 && last_refresh[monitor].elapsed() >= Duration::from_millis(500) {
                        last_refresh[monitor] = Instant::now();
                        refreshes[receiver] += 1;
                        encoder.request_refresh();
                    }
                }
                cycle += 1;
            }
            for (index, transport) in [&mut laptop, &mut desktop].into_iter().enumerate() {
                payloads.clear();
                transport.poll_incoming(&mut payloads);
                for payload in &payloads {
                    if matches!(payload, WirePayload::Heartbeat { .. }) {
                        heartbeats[index] += 1;
                        blocks[index].beat_peer(now);
                    }
                    if let WirePayload::HidBatch { events, .. } = payload {
                        assert_eq!(events.len(), 2);
                        assert_eq!(events[1], HidEvent::MouseMoveRel { dx: 3, dy: -2 });
                        hid_batches[index] += 1;
                        last_hid_at[index] = start.elapsed();
                    }
                }
                blocks[index].beat_daemon(now);
                assert_eq!(blocks[index].evaluate_liveness(now), None);
                for (seen, units) in units_seen[index].iter_mut().zip(&consumers[index].units) {
                    *seen = units.load(Ordering::Acquire);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(200));
        assert!(
            heartbeats.iter().all(|&n| n >= 400),
            "heartbeats: {heartbeats:?}"
        );
        assert!(units_seen[0][0] > 100, "Mode 1+ units: {units_seen:?}");
        assert!(
            hid_batches.iter().all(|&n| n > 300),
            "HID batches: {hid_batches:?}"
        );
        assert!(
            last_hid_at
                .iter()
                .all(|&elapsed| elapsed > Duration::from_secs(11)),
            "Input must still arrive beyond 7s: {last_hid_at:?}"
        );
        assert!(
            units_seen[1].iter().all(|&n| n > 100),
            "Mode 2 units: {units_seen:?}"
        );
        assert!(
            refreshes.iter().all(|&n| n > 0 && n < 30),
            "Refresh requests must happen and stay bounded: {refreshes:?}"
        );
        let decoders = consumers[1].decoders.lock().unwrap();
        for (monitor, decoder) in decoders.iter().enumerate() {
            let (decoded, expected) = (decoder.canvas().as_chunks::<4>().0, final_frames[monitor].as_chunks::<4>().0);
            let exact = decoded.iter().zip(expected).all(|(a, b)| a[..3] == b[..3]);
            assert!(exact, "monitor {monitor} must converge to the exact final frame after loss");
        }
        assert_eq!(desktop.video_stats().tx_dropped_datagrams.load(Ordering::Relaxed), 0);
    }

    #[derive(Default)]
    struct DecodingConsumer {
        decoders: Mutex<[TileDecoder; 2]>,
        units: [std::sync::atomic::AtomicUsize; 2],
        refresh: AtomicU32,
    }

    impl VideoFrameConsumer for DecodingConsumer {
        fn submit(&self, frame: EncodedVideoFrame) {
            let monitor = usize::from(frame.monitor_index.min(1));
            if self.decoders.lock().unwrap()[monitor].decode_slice(&frame.data).refresh_needed {
                self.refresh.fetch_or(1 << monitor, Ordering::AcqRel);
            }
            self.units[monitor].fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    #[ignore = "requires isolated loopback UDP/TCP sockets"]
    fn mismatched_pairing_never_marks_peer_authenticated() {
        let local_ip = Ipv4Addr::new(127, 0, 0, 6);
        let peer_ip = Ipv4Addr::new(127, 0, 0, 7);
        let mut local =
            DuplexTransport::bind_strict(NodeRole::DebianLaptop, local_ip, peer_ip, &TEST_LINK_PSK)
                .unwrap();
        let mut peer =
            DuplexTransport::bind_strict(NodeRole::WindowsDesktop, peer_ip, local_ip, &[0x11; 32])
                .unwrap();
        let mut incoming = Vec::new();
        for _ in 0..10 {
            let now = duplex_pal::monotonic_now_ns();
            peer.tick_heartbeat(OperationalMode::Mode0Idle, 0, now);
            peer.send_hid_events(&[HidEvent::MouseMoveRel { dx: 1, dy: 1 }], now)
                .unwrap();
            local.poll_incoming(&mut incoming);
            thread::sleep(Duration::from_millis(2));
        }
        assert!(incoming.is_empty());
        assert!(!local.has_recent_authenticated_peer(duplex_pal::monotonic_now_ns()));
    }
}
