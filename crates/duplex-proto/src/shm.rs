//! Cross-Process Shared Memory Layout for the 250ms Dead-Man's Switch (`duplex-guard`).
//!
//! Mapped identically by `duplexd` and `duplex-guard` via `shm_open`/`mmap` on Debian Linux
//! and `CreateFileMappingW`/`MapViewOfFile` on Windows 11.

use crate::hid::DepressedKeyBitmap;
use crate::state::OperationalMode;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// Magic value `"DUPLEXWD"` (`0x4455_504C_4558_5744`) identifying an initialized watchdog block.
pub const SHM_WATCHDOG_MAGIC: u64 = 0x4455_504C_4558_5744;
/// Current shared-memory ABI layout version.
pub const SHM_WATCHDOG_VERSION: u32 = 1;

/// 250ms Dead-Man's Switch threshold in nanoseconds.
/// If either the local daemon heartbeat OR the 50 Hz peer network heartbeat lags by > 250ms
/// while in Mode 1 or Mode 2, `duplex-guard` immediately reclaims input and tears down displays.
pub const WATCHDOG_TIMEOUT_NS: u64 = 250_000_000;

/// 50 Hz peer heartbeat interval (20 milliseconds in nanoseconds).
pub const PEER_HEARTBEAT_INTERVAL_NS: u64 = 20_000_000;

/// POSIX shared memory object path on Debian Linux.
pub const LINUX_SHM_NAME: &str = "/duplex_kvm_watchdog_v1";
/// Win32 named file mapping object name on Windows 11.
pub const WINDOWS_SHM_NAME: &str = "Local\\DuplexKvmWatchdogV1";

/// Sentinel value in `requested_mode` indicating no pending operator mode-switch command.
pub const NO_REQUESTED_MODE: u8 = 0xFF;

/// Cache-line aligned shared memory control block between `duplexd`, `duplex-guard`, and `duplex-ctl`.
#[repr(C, align(64))]
pub struct ShmWatchdogBlock {
    pub magic: AtomicU64,
    pub version: AtomicU32,
    pub daemon_pid: AtomicU32,
    pub active_mode: AtomicU8,
    pub input_grab_active: AtomicBool,
    pub virtual_displays_active: AtomicU8,
    pub panic_latched: AtomicBool,
    pub force_reset_requested: AtomicBool,
    /// Operator mode-switch request written by `duplex-ctl mode <idle|pc|laptop>` (`0xFF` = none).
    pub requested_mode: AtomicU8,
    pub peer_connected: AtomicBool,
    pub _pad0: [u8; 1],
    /// Monotonic timestamp (ns) of the last `duplexd` local main-loop tick.
    pub daemon_heartbeat_ns: AtomicU64,
    /// Monotonic timestamp (ns) of the last authenticated 50 Hz network heartbeat received from peer.
    pub peer_heartbeat_ns: AtomicU64,
    /// Latest measured peer round-trip latency in nanoseconds.
    pub last_rtt_ns: AtomicU64,
    /// Minimum observed peer round-trip latency in nanoseconds.
    pub min_rtt_ns: AtomicU64,
    /// Maximum observed peer round-trip latency in nanoseconds.
    pub max_rtt_ns: AtomicU64,
    /// Total HID event batches transmitted over UDP 48100 (DSCP EF).
    pub tx_hid_packets: AtomicU64,
    /// Total HID event batches received over UDP 48100 (DSCP EF).
    pub rx_hid_packets: AtomicU64,
    /// Total hardware-encoded HEVC video access units transmitted over UDP 48102-48103 (DSCP AF41).
    pub tx_video_frames: AtomicU64,
    /// Total hardware-encoded HEVC video access units reassembled and submitted to viewer decoder.
    pub rx_video_frames: AtomicU64,
    /// Total PLI keyframe recovery requests triggered.
    pub pli_keyframes_requested: AtomicU64,
    /// Mirrored 256-bit depressed USB HID key bitmap (`0x00..=0xFF`).
    /// Allows `duplex-guard` to emit synthetic `KEY_UP` for every held key even if `duplexd` hard-locks.
    pub depressed_keys_bitmap: [AtomicU64; 4],
}

impl ShmWatchdogBlock {
    /// Initializes the shared memory block in-place for `duplexd` startup.
    pub fn init_daemon(&self, pid: u32, now_ns: u64) {
        self.version.store(SHM_WATCHDOG_VERSION, Ordering::Release);
        self.daemon_pid.store(pid, Ordering::Release);
        self.active_mode
            .store(OperationalMode::Mode0Idle.as_u8(), Ordering::Release);
        self.input_grab_active.store(false, Ordering::Release);
        self.virtual_displays_active.store(0, Ordering::Release);
        self.panic_latched.store(false, Ordering::Release);
        self.force_reset_requested.store(false, Ordering::Release);
        self.requested_mode
            .store(NO_REQUESTED_MODE, Ordering::Release);
        self.peer_connected.store(false, Ordering::Release);
        self.daemon_heartbeat_ns.store(now_ns, Ordering::Release);
        self.peer_heartbeat_ns.store(now_ns, Ordering::Release);
        self.last_rtt_ns.store(0, Ordering::Release);
        self.min_rtt_ns.store(u64::MAX, Ordering::Release);
        self.max_rtt_ns.store(0, Ordering::Release);
        self.tx_hid_packets.store(0, Ordering::Release);
        self.rx_hid_packets.store(0, Ordering::Release);
        self.tx_video_frames.store(0, Ordering::Release);
        self.rx_video_frames.store(0, Ordering::Release);
        self.pli_keyframes_requested.store(0, Ordering::Release);
        for word in &self.depressed_keys_bitmap {
            word.store(0, Ordering::Release);
        }
        self.magic.store(SHM_WATCHDOG_MAGIC, Ordering::SeqCst);
    }

    /// Records a newly measured peer round-trip time sample (`rtt_ns`), updating `last`, `min`, and `max`.
    #[inline]
    pub fn record_rtt_sample(&self, rtt_ns: u64) {
        self.last_rtt_ns.store(rtt_ns, Ordering::Release);
        let cur_min = self.min_rtt_ns.load(Ordering::Relaxed);
        if rtt_ns < cur_min {
            self.min_rtt_ns.store(rtt_ns, Ordering::Release);
        }
        let cur_max = self.max_rtt_ns.load(Ordering::Relaxed);
        if rtt_ns > cur_max {
            self.max_rtt_ns.store(rtt_ns, Ordering::Release);
        }
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic.load(Ordering::Acquire) == SHM_WATCHDOG_MAGIC
            && self.version.load(Ordering::Acquire) == SHM_WATCHDOG_VERSION
    }

    #[inline]
    pub fn beat_daemon(&self, now_ns: u64) {
        self.daemon_heartbeat_ns.fetch_max(now_ns, Ordering::AcqRel);
    }

    #[inline]
    pub fn beat_peer(&self, now_ns: u64) {
        self.peer_heartbeat_ns.fetch_max(now_ns, Ordering::AcqRel);
    }

    /// Atomically sets or clears a bit in the shared-memory depressed-key bitmap.
    #[inline]
    pub fn set_key_depressed(&self, usage: u8, pressed: bool) {
        let idx = (usage >> 6) as usize;
        let bit = 1u64 << (usage & 63);
        if pressed {
            self.depressed_keys_bitmap[idx].fetch_or(bit, Ordering::AcqRel);
        } else {
            self.depressed_keys_bitmap[idx].fetch_and(!bit, Ordering::AcqRel);
        }
    }

    /// Atomically snapshots and clears the entire 256-bit depressed-key bitmap.
    #[inline]
    pub fn take_depressed_bitmap(&self) -> DepressedKeyBitmap {
        let mut words = [0u64; 4];
        for (i, atom) in self.depressed_keys_bitmap.iter().enumerate() {
            words[i] = atom.swap(0, Ordering::AcqRel);
        }
        DepressedKeyBitmap::from_words(words)
    }

    /// Evaluates whether the 250ms Dead-Man's Switch has tripped at `now_ns`.
    /// Returns `Some(WatchdogTripReason)` if an active session must be emergency-reset to `MODE_0_IDLE`.
    #[inline]
    pub fn evaluate_liveness(&self, now_ns: u64) -> Option<WatchdogTripReason> {
        if !self.is_valid() {
            return None;
        }
        if self.panic_latched.load(Ordering::Acquire) {
            return Some(WatchdogTripReason::HardwarePanicLatched);
        }

        let mode = self.active_mode.load(Ordering::Acquire);
        let grab_active = self.input_grab_active.load(Ordering::Acquire);
        let vdisp_count = self.virtual_displays_active.load(Ordering::Acquire);

        if mode == OperationalMode::Mode0Idle.as_u8() && !grab_active && vdisp_count == 0 {
            return None;
        }

        let daemon_hb = self.daemon_heartbeat_ns.load(Ordering::Acquire);
        let daemon_delta = now_ns.saturating_sub(daemon_hb);
        if daemon_delta > WATCHDOG_TIMEOUT_NS {
            return Some(WatchdogTripReason::DaemonHeartbeatExpired {
                elapsed_ns: daemon_delta,
            });
        }

        let peer_hb = self.peer_heartbeat_ns.load(Ordering::Acquire);
        let peer_delta = now_ns.saturating_sub(peer_hb);
        if peer_delta > WATCHDOG_TIMEOUT_NS {
            return Some(WatchdogTripReason::PeerHeartbeatExpired {
                elapsed_ns: peer_delta,
            });
        }

        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogTripReason {
    HardwarePanicLatched,
    DaemonHeartbeatExpired { elapsed_ns: u64 },
    PeerHeartbeatExpired { elapsed_ns: u64 },
    DaemonProcessTerminated { pid: u32 },
}
