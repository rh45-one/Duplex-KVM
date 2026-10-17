//! Encrypted QoS Network Transport Multiplexer (`duplexd::net`).
//!
//! Manages strictly bound (`192.168.137.1` / `192.168.137.2`) sockets across all three QoS tiers:
//! - **HID Stream (`UDP 48100`, `DSCP EF` -> `IP_TOS 0xB8`):** Zero-allocation in-place `ChaCha20-Poly1305`
//! - **Control & 50 Hz Heartbeat (`TCP/UDP 48101`, `DSCP CS6` -> `IP_TOS 0xC0`):** `ChaCha20-Poly1305`
//! - **Hardware Video Streams (`UDP 48102-48103`, `DSCP AF41` -> `IP_TOS 0x88`):** MTU-safe (`1380B`)
//!   HEVC slice packetizer & `VideoFrameAssembler` with automatic PLI keyframe recovery.

use duplex_pal::{
    bind_qos_tcp_listener, bind_qos_udp_socket, configure_tcp_stream_qos, PalError, QosClass,
};
use duplex_proto::{
    packetize_video_frame, ChannelId, CryptoSession, CursorTelemetry, EncodedVideoFrame, HidEvent,
    NodeRole, OperationalMode, TransitionReason, VideoFrameAssembler, WirePayload,
    DEBIAN_LAPTOP_IPV4, PEER_HEARTBEAT_INTERVAL_NS, PORT_CONTROL_TCP, PORT_HID_UDP,
    PORT_VIDEO_STREAM_0_UDP, PORT_VIDEO_STREAM_1_UDP, WINDOWS_HOST_IPV4,
};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use tracing::{debug, info, warn};

/// Default pre-shared 256-bit key for link encryption (can be overridden via `DUPLEX_PSK_HEX` env var).
pub const DEFAULT_LINK_PSK: [u8; 32] = *b"DuplexKvm-1GbE-Symmetric-Key-V01";

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

/// Parses a 64-character hex string into a 32-byte ChaCha20-Poly1305 key, falling back to `DEFAULT_LINK_PSK`.
pub fn resolve_psk_from_env() -> [u8; 32] {
    if let Ok(hex_str) = std::env::var("DUPLEX_PSK_HEX") {
        let trimmed = hex_str.trim();
        if trimmed.len() == 64 {
            let mut key = [0u8; 32];
            let mut ok = true;
            for i in 0..32 {
                if let Ok(b) = u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16) {
                    key[i] = b;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok {
                return key;
            }
        }
        warn!("DUPLEX_PSK_HEX must be exactly 64 hex chars; falling back to default link key");
    }
    DEFAULT_LINK_PSK
}

/// Symmetric encrypted & QoS-tagged transport managing:
/// - UDP `48100` (HID, DSCP EF `0xB8`)
/// - TCP/UDP `48101` (Control & 50 Hz Heartbeat, DSCP CS6 `0xC0`)
/// - UDP `48102` & `48103` (Dual-Monitor HEVC Video Streams, DSCP AF41 `0x88`)
pub struct DuplexTransport {
    role: NodeRole,
    peer_hid_addr: SocketAddr,
    peer_ctrl_addr: SocketAddr,
    peer_video_0_addr: SocketAddr,
    peer_video_1_addr: SocketAddr,
    hid_udp: UdpSocket,
    ctrl_udp: UdpSocket,
    ctrl_tcp_listener: TcpListener,
    ctrl_tcp_stream: Option<TcpStream>,
    video_0_udp: UdpSocket,
    video_1_udp: UdpSocket,
    video_assembler_0: VideoFrameAssembler,
    video_assembler_1: VideoFrameAssembler,
    video_dgram_scratch: Vec<Vec<u8>>,
    hid_crypto: CryptoSession,
    ctrl_crypto: CryptoSession,
    tx_scratch: Vec<u8>,
    rx_scratch: [u8; 2048],
    coalesce_scratch: Vec<HidEvent>,
    last_heartbeat_sent_ns: u64,
    last_tcp_connect_attempt_ns: u64,
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

        Ok(Self {
            role,
            peer_hid_addr,
            peer_ctrl_addr,
            peer_video_0_addr,
            peer_video_1_addr,
            hid_udp,
            ctrl_udp,
            ctrl_tcp_listener,
            ctrl_tcp_stream: None,
            video_0_udp,
            video_1_udp,
            video_assembler_0: VideoFrameAssembler::new(0),
            video_assembler_1: VideoFrameAssembler::new(1),
            video_dgram_scratch: Vec::with_capacity(64),
            hid_crypto: CryptoSession::new(psk, hid_tx_ch, hid_rx_ch),
            ctrl_crypto: CryptoSession::new(psk, ctrl_tx_ch, ctrl_rx_ch),
            tx_scratch: Vec::with_capacity(512),
            rx_scratch: [0u8; 2048],
            coalesce_scratch: Vec::with_capacity(64),
            last_heartbeat_sent_ns: 0,
            last_tcp_connect_attempt_ns: 0,
        })
    }

    /// Coalesces micro-deltas and transmits an encrypted `WirePayload::HidBatch` over UDP 48100 (`DSCP EF`).
    pub fn send_hid_events(&mut self, raw_events: &[HidEvent], now_ns: u64) -> Result<(), PalError> {
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
        let _ = self.hid_udp.send_to(&self.tx_scratch, self.peer_hid_addr);
        Ok(())
    }

    /// Transmits a decoupled 1000 Hz hardware cursor update out-of-band over the `DSCP EF` (`0xB8`) socket.
    pub fn send_cursor_update(&mut self, cursor: CursorTelemetry) -> Result<(), PalError> {
        let payload = WirePayload::CursorUpdate(cursor);
        self.hid_crypto
            .seal_frame_into(&payload, &mut self.tx_scratch)
            .map_err(|e| PalError::Network(format!("Cursor seal_frame_into failed: {e}")))?;
        let _ = self.hid_udp.send_to(&self.tx_scratch, self.peer_hid_addr);
        Ok(())
    }

    /// Packetizes a hardware-encoded HEVC access unit into MTU-safe (`<= 1404B`) datagrams
    /// and transmits them over UDP `48102` (Monitor 0) or `48103` (Monitor 1) tagged with `DSCP AF41` (`0x88`).
    pub fn send_video_frame(&mut self, frame: &EncodedVideoFrame) {
        packetize_video_frame(frame, &mut self.video_dgram_scratch);
        let (sock, target_addr) = if frame.monitor_index == 0 {
            (&self.video_0_udp, self.peer_video_0_addr)
        } else {
            (&self.video_1_udp, self.peer_video_1_addr)
        };
        for dgram in &self.video_dgram_scratch {
            let _ = sock.send_to(dgram, target_addr);
        }
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
    /// over the CS6 control channel.
    pub fn send_control_payload(&mut self, payload: &WirePayload) -> Result<(), PalError> {
        self.ctrl_crypto
            .seal_frame_into(payload, &mut self.tx_scratch)
            .map_err(|e| PalError::Network(format!("Control seal_frame_into failed: {e}")))?;

        let _ = self.ctrl_udp.send_to(&self.tx_scratch, self.peer_ctrl_addr);

        if let Some(ref mut stream) = self.ctrl_tcp_stream {
            let len = self.tx_scratch.len() as u16;
            let header = len.to_be_bytes();
            if stream.write_all(&header).is_err() || stream.write_all(&self.tx_scratch).is_err() {
                self.ctrl_tcp_stream = None;
            }
        }
        Ok(())
    }

    /// Sends a mode transition notification to the peer.
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
        let _ = self.send_control_payload(&msg);
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
            if let Ok(stream) =
                TcpStream::connect_timeout(&self.peer_ctrl_addr, std::time::Duration::from_millis(2))
            {
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

    /// Non-blocking poll draining all authenticated HID, Cursor, and Control packets from UDP 48100 and TCP/UDP 48101.
    pub fn poll_incoming(&mut self, out_payloads: &mut Vec<WirePayload>) {
        // 1. Drain high-priority HID & Decoupled Cursor UDP 48100 packets
        while let Ok((len, src_addr)) = self.hid_udp.recv_from(&mut self.rx_scratch) {
            if src_addr.ip() != self.peer_hid_addr.ip() {
                continue;
            }
            if let Ok((_seq, payload)) =
                self.hid_crypto.open_frame_in_place(&mut self.rx_scratch[..len])
            {
                out_payloads.push(payload);
            }
        }

        // 2. Drain CS6 Control/Heartbeat UDP 48101 packets
        while let Ok((len, src_addr)) = self.ctrl_udp.recv_from(&mut self.rx_scratch) {
            if src_addr.ip() != self.peer_ctrl_addr.ip() {
                continue;
            }
            if let Ok((_seq, payload)) =
                self.ctrl_crypto.open_frame_in_place(&mut self.rx_scratch[..len])
            {
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
                            .open_frame_in_place(&mut self.rx_scratch[..frame_len])
                        {
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

    /// Non-blocking poll draining incoming `DSCP AF41` (`0x88`) HEVC video datagrams on UDP `48102` and `48103`.
    /// Reassembles complete access units and flags any packet loss for immediate PLI keyframe recovery.
    pub fn poll_incoming_video(
        &mut self,
        out_frames: &mut Vec<EncodedVideoFrame>,
        out_pli_monitors: &mut Vec<u8>,
    ) {
        while let Ok((len, src_addr)) = self.video_0_udp.recv_from(&mut self.rx_scratch) {
            if src_addr.ip() != self.peer_video_0_addr.ip() {
                continue;
            }
            if let Ok((frame_opt, gap)) =
                self.video_assembler_0.push_datagram(&self.rx_scratch[..len])
            {
                if gap {
                    out_pli_monitors.push(0);
                }
                if let Some(frame) = frame_opt {
                    out_frames.push(frame);
                }
            }
        }

        while let Ok((len, src_addr)) = self.video_1_udp.recv_from(&mut self.rx_scratch) {
            if src_addr.ip() != self.peer_video_1_addr.ip() {
                continue;
            }
            if let Ok((frame_opt, gap)) =
                self.video_assembler_1.push_datagram(&self.rx_scratch[..len])
            {
                if gap {
                    out_pli_monitors.push(1);
                }
                if let Some(frame) = frame_opt {
                    out_frames.push(frame);
                }
            }
        }
    }
}
