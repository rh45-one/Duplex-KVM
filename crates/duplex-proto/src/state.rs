use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Static link-local / NAT passthrough IPv4 address assigned to Computer B (Windows 11 Desktop).
pub const WINDOWS_HOST_IPV4: &str = "192.168.137.1";
/// Static IPv4 address assigned to Computer A (Debian Linux Laptop).
pub const DEBIAN_LAPTOP_IPV4: &str = "192.168.137.2";

/// Dedicated UDP port for low-latency USB HID input packets.
pub const PORT_HID_UDP: u16 = 48100;
/// Dedicated TCP port for state-machine control and 50 Hz peer heartbeats.
pub const PORT_CONTROL_TCP: u16 = 48101;
/// Dedicated UDP port for primary/first virtual monitor HEVC stream.
pub const PORT_VIDEO_STREAM_0_UDP: u16 = 48102;
/// Dedicated UDP port for secondary virtual monitor HEVC stream.
pub const PORT_VIDEO_STREAM_1_UDP: u16 = 48103;

/// Converts a 6-bit DSCP class value into an 8-bit IPv4 `IP_TOS` socket option byte
/// by shifting left 2 bits (`dscp << 2`) so the lower 2 ECN (Explicit Congestion Notification)
/// bits remain zero and never corrupt the DSCP class.
#[inline]
pub const fn dscp_to_tos(dscp: u8) -> u8 {
    (dscp & 0x3F) << 2
}

/// DSCP EF (Expedited Forwarding, decimal 46 / 0x2E) -> IP TOS byte (46 << 2 = 0xB8) for HID UDP.
pub const DSCP_EF: u8 = 0x2E;
pub const TOS_HID_EF: u8 = dscp_to_tos(DSCP_EF); // 0xB8 (184)

/// DSCP CS6 (Network Control, decimal 48 / 0x30) -> IP TOS byte (48 << 2 = 0xC0) for Control/Heartbeat TCP.
pub const DSCP_CS6: u8 = 0x30;
pub const TOS_CONTROL_CS6: u8 = dscp_to_tos(DSCP_CS6); // 0xC0 (192)

/// DSCP AF41 (Multimedia Streaming, decimal 34 / 0x22) -> IP TOS byte (34 << 2 = 0x88) for Video UDP.
pub const DSCP_AF41: u8 = 0x22;
pub const TOS_VIDEO_AF41: u8 = dscp_to_tos(DSCP_AF41); // 0x88 (136)

/// Role of a node in the symmetric Duplex topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRole {
    /// Computer A: Debian Linux Laptop (built-in keyboard/trackpad + 1 internal display).
    DebianLaptop,
    /// Computer B: Windows 11 Desktop PC (dual physical monitors + USB keyboard/mouse + WAN gateway).
    WindowsDesktop,
}

/// Deterministic symmetric operational mode shared across both peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationalMode {
    /// MODE_0_IDLE: Local Only + Internet Passthrough.
    /// All virtual displays detached; all input grabs/hooks released.
    Mode0Idle,
    /// MODE_1_PC_PRIMARY: Debian Laptop controls Windows PC + optional extended virtual display.
    /// Input flows Linux -> Windows; optional 1 virtual display streams Windows -> Linux.
    Mode1PcPrimary { extend_display: bool },
    /// MODE_2_LAPTOP_PRIMARY: Windows Desktop dual monitors & peripherals docked to Debian Laptop.
    /// Input flows Windows -> Linux; 2 virtual displays stream Linux -> Windows.
    Mode2LaptopPrimary,
}

impl OperationalMode {
    #[inline]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Mode0Idle => 0,
            Self::Mode1PcPrimary {
                extend_display: true,
            } => 1,
            Self::Mode2LaptopPrimary => 2,
            Self::Mode1PcPrimary {
                extend_display: false,
            } => 3,
        }
    }

    #[inline]
    pub const fn from_u8_lossy(val: u8) -> Self {
        match val {
            1 => Self::Mode1PcPrimary {
                extend_display: true,
            },
            2 => Self::Mode2LaptopPrimary,
            3 => Self::Mode1PcPrimary {
                extend_display: false,
            },
            _ => Self::Mode0Idle,
        }
    }

    /// Returns true if the given node role captures local physical input in this mode.
    #[inline]
    pub const fn should_capture_input(self, role: NodeRole) -> bool {
        match (self, role) {
            (Self::Mode0Idle, _) => false,
            (Self::Mode1PcPrimary { .. }, NodeRole::DebianLaptop) => true,
            (Self::Mode1PcPrimary { .. }, NodeRole::WindowsDesktop) => false,
            (Self::Mode2LaptopPrimary, NodeRole::WindowsDesktop) => true,
            (Self::Mode2LaptopPrimary, NodeRole::DebianLaptop) => false,
        }
    }

    /// Returns the number of virtual displays the given node role must spawn in this mode.
    #[inline]
    pub const fn required_virtual_displays(self, role: NodeRole) -> u8 {
        match (self, role) {
            (Self::Mode0Idle, _) => 0,
            (Self::Mode1PcPrimary { extend_display: true }, NodeRole::WindowsDesktop) => 1,
            (Self::Mode1PcPrimary { extend_display: false }, _) => 0,
            (Self::Mode1PcPrimary { .. }, NodeRole::DebianLaptop) => 0,
            (Self::Mode2LaptopPrimary, NodeRole::DebianLaptop) => 2,
            (Self::Mode2LaptopPrimary, NodeRole::WindowsDesktop) => 0,
        }
    }

    /// Returns true if the given node role captures and hardware-encodes virtual displays in this mode.
    #[inline]
    pub const fn should_stream_video(self, role: NodeRole) -> bool {
        self.required_virtual_displays(role) > 0
    }

    /// Returns the number of incoming hardware-decoded fullscreen viewer windows the given node role must open.
    #[inline]
    pub const fn expected_incoming_video_streams(self, role: NodeRole) -> u8 {
        match (self, role) {
            (Self::Mode0Idle, _) => 0,
            (Self::Mode1PcPrimary { extend_display: true }, NodeRole::DebianLaptop) => 1,
            (Self::Mode1PcPrimary { .. }, _) => 0,
            (Self::Mode2LaptopPrimary, NodeRole::WindowsDesktop) => 2,
            (Self::Mode2LaptopPrimary, NodeRole::DebianLaptop) => 0,
        }
    }
}

/// Cause of a state machine transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionReason {
    /// Operator invoked `duplex-ctl mode ...`
    OperatorCommand,
    /// Authenticated peer requested mode change over TCP control channel.
    PeerRequest,
    /// Local hardware panic sequence (`Ctrl+Alt+Shift+Backspace` or `5x Escape in 1.5s`).
    HardwarePanicEscape,
    /// 250ms Dead-Man's Switch watchdog timeout or peer disconnect.
    WatchdogTimeout,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StateMachineError {
    #[error("Cannot transition directly from {from:?} to {to:?} without passing through Mode0Idle cleanup")]
    InvalidDirectTransition {
        from: OperationalMode,
        to: OperationalMode,
    },
}

/// Deterministic State Machine governing `duplexd`.
#[derive(Debug, Clone)]
pub struct ModeStateMachine {
    current: OperationalMode,
    epoch: u64,
}

impl Default for ModeStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl ModeStateMachine {
    pub const fn new() -> Self {
        Self {
            current: OperationalMode::Mode0Idle,
            epoch: 0,
        }
    }

    #[inline]
    pub const fn current_mode(&self) -> OperationalMode {
        self.current
    }

    #[inline]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    #[inline]
    pub fn sync_epoch(&mut self, peer_epoch: u64) {
        if peer_epoch > self.epoch {
            self.epoch = peer_epoch;
        }
    }

    /// Transitions the state machine. Emergency transitions (`HardwarePanicEscape`, `WatchdogTimeout`)
    /// unconditionally force `Mode0Idle` without waiting for network acknowledgment.
    pub fn transition(
        &mut self,
        target: OperationalMode,
        reason: TransitionReason,
    ) -> Result<OperationalMode, StateMachineError> {
        self.transition_with_epoch(target, reason, None)
    }

    /// Transitions the state machine, optionally adopting `peer_epoch` when triggered by a remote peer
    /// so both peers converge on the exact same `epoch` without ping-ponging.
    pub fn transition_with_epoch(
        &mut self,
        target: OperationalMode,
        reason: TransitionReason,
        peer_epoch: Option<u64>,
    ) -> Result<OperationalMode, StateMachineError> {
        if matches!(
            reason,
            TransitionReason::HardwarePanicEscape | TransitionReason::WatchdogTimeout
        ) {
            self.current = OperationalMode::Mode0Idle;
            self.epoch = match peer_epoch {
                Some(pe) => self.epoch.max(pe),
                None => self.epoch.wrapping_add(1),
            };
            return Ok(self.current);
        }

        if let Some(pe) = peer_epoch {
            if pe > self.epoch {
                self.epoch = pe;
            }
            self.current = target;
            return Ok(self.current);
        }

        if self.current == target {
            return Ok(self.current);
        }

        self.current = target;
        self.epoch = self.epoch.wrapping_add(1);
        Ok(self.current)
    }
}

/// Physical or virtual monitor geometry and timing descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayDescriptor {
    pub index: u8,
    pub width_px: u32,
    pub height_px: u32,
    pub refresh_hz: u32,
    pub scale_percent: u16,
    pub physical_width_mm: u16,
    pub physical_height_mm: u16,
    /// Raw 128-byte base EDID block for virtual display emulation (`IddCx` / DRM).
    pub edid_block: Vec<u8>,
}

impl DisplayDescriptor {
    /// Generates a deterministic 128-byte VESA EDID 1.4 block matching the requested resolution and refresh rate.
    pub fn synthesize_edid(width_px: u32, height_px: u32, refresh_hz: u32, monitor_index: u8) -> Vec<u8> {
        let mut edid = vec![0u8; 128];
        // Fixed 8-byte EDID 1.4 header
        edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        // Manufacturer ID "DPX" (Duplex)
        edid[8] = 0x12;
        edid[9] = 0x18;
        // Product code + serial
        edid[10] = monitor_index;
        edid[11] = 0x01;
        edid[12..16].copy_from_slice(&(u32::from(monitor_index) + 1000).to_le_bytes());
        // Week 1, Year 2026 (2026 - 1990 = 36)
        edid[16] = 1;
        edid[17] = 36;
        // EDID Version 1.4
        edid[18] = 1;
        edid[19] = 4;
        // Digital input, 8 bits per color, DisplayPort interface
        edid[20] = 0xA5;
        // Physical size in cm (approx 60cm x 34cm)
        edid[21] = 60;
        edid[22] = 34;
        // Gamma 2.20 -> (2.20 * 100) - 100 = 120
        edid[23] = 120;
        // Features: sRGB, preferred timing in DTD 1, continuous frequency
        edid[24] = 0x06;

        // Detailed Timing Descriptor 1 at bytes 54..72
        let h_active = width_px.min(4095) as u16;
        let h_blank: u16 = 160;
        let v_active = height_px.min(4095) as u16;
        let v_blank: u16 = 45;
        let pixel_clock_10khz =
            ((u64::from(h_active + h_blank) * u64::from(v_active + v_blank) * u64::from(refresh_hz)) / 10_000)
                .min(65535) as u16;

        edid[54..56].copy_from_slice(&pixel_clock_10khz.to_le_bytes());
        edid[56] = (h_active & 0xFF) as u8;
        edid[57] = (h_blank & 0xFF) as u8;
        edid[58] = (((h_active >> 8) & 0x0F) << 4 | ((h_blank >> 8) & 0x0F)) as u8;
        edid[59] = (v_active & 0xFF) as u8;
        edid[60] = (v_blank & 0xFF) as u8;
        edid[61] = (((v_active >> 8) & 0x0F) << 4 | ((v_blank >> 8) & 0x0F)) as u8;

        // Checksum at byte 127 so sum(edid[0..128]) % 256 == 0
        let sum: u8 = edid[0..127].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        edid[127] = 0u8.wrapping_sub(sum);
        edid
    }
}
