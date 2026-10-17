//! Authenticated Encryption (`ChaCha20-Poly1305`) & Anti-Replay Packet Framing.
//!
//! Protects all HID input and control/heartbeat messages across the bridged 1 Gbps link
//! using a 64-bit strictly monotonic sequence counter and a 64-packet sliding replay window.

use crate::hid::HidEvent;
use crate::state::{DisplayDescriptor, NodeRole, OperationalMode, TransitionReason};
use crate::video::CursorTelemetry;
use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    ChaCha20Poly1305, Key, Nonce, Tag,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 4-byte protocol magic header (`"DPLX"`).
pub const WIRE_MAGIC: [u8; 4] = *b"DPLX";
/// Wire protocol version 1.
pub const WIRE_VERSION: u8 = 1;
/// Header length in bytes: 4 (magic) + 1 (version) + 1 (channel_id) + 2 (reserved) + 8 (seq_no) = 16 bytes.
pub const FRAME_HEADER_LEN: usize = 16;
/// Poly1305 authentication tag length (16 bytes).
pub const POLY1305_TAG_LEN: usize = 16;

/// Logical multiplexed channel identifier used in the 96-bit AEAD nonce to prevent cross-channel replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ChannelId {
    HidDesktopToLaptop = 1,
    HidLaptopToDesktop = 2,
    ControlDesktopToLaptop = 3,
    ControlLaptopToDesktop = 4,
}

/// Top-level payload carried inside an authenticated `ChaCha20-Poly1305` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WirePayload {
    /// Batch of normalized low-latency USB HID events (sent over UDP 48100, DSCP EF).
    HidBatch {
        sender_timestamp_ns: u64,
        events: Vec<HidEvent>,
    },
    /// Decoupled hardware cursor position & shape bitmap update (sent out-of-band from video).
    CursorUpdate(CursorTelemetry),
    /// 50 Hz peer liveness heartbeat (sent every 20ms over TCP/UDP 48101, DSCP CS6).
    Heartbeat {
        sender_role: NodeRole,
        active_mode: OperationalMode,
        epoch: u64,
        sender_timestamp_ns: u64,
    },
    /// Echoed heartbeat acknowledgment for sub-millisecond RTT measurement.
    HeartbeatAck {
        origin_timestamp_ns: u64,
        active_mode: OperationalMode,
        epoch: u64,
    },
    /// Mode transition request or emergency notification.
    ModeTransition {
        target_mode: OperationalMode,
        reason: TransitionReason,
        epoch: u64,
    },
    /// Advertises physical monitor geometry and 128-byte EDID blocks so peer can spawn matching virtual displays.
    AdvertiseDisplays {
        role: NodeRole,
        displays: Vec<DisplayDescriptor>,
    },
    /// Picture Loss Indication (PLI) requesting an immediate IDR keyframe on `monitor_index`.
    VideoKeyframeRequest {
        monitor_index: u8,
    },
}

#[derive(Debug, Error)]
pub enum CryptoFrameError {
    #[error("Frame too short: {0} bytes")]
    FrameTooShort(usize),
    #[error("Invalid wire magic header")]
    InvalidMagic,
    #[error("Unsupported wire protocol version: {0}")]
    UnsupportedVersion(u8),
    #[error("Unexpected channel ID in header: expected {expected}, got {actual}")]
    ChannelMismatch { expected: u8, actual: u8 },
    #[error("Replay attack or duplicate/out-of-window sequence number: {0}")]
    ReplayDetected(u64),
    #[error("ChaCha20-Poly1305 AEAD authentication failed")]
    AuthenticationFailed,
    #[error("Bincode serialization error: {0}")]
    Serialization(#[from] bincode::Error),
}

/// 64-bit sliding window anti-replay tracker (RFC 6479 style).
#[derive(Debug, Clone, Default)]
pub struct ReplayWindow {
    highest_seq: u64,
    bitmap: u64,
    initialized: bool,
}

impl ReplayWindow {
    pub const fn new() -> Self {
        Self {
            highest_seq: 0,
            bitmap: 0,
            initialized: false,
        }
    }

    /// Checks whether `seq` is fresh (not yet seen and within the 64-packet sliding window)
    /// without mutating state. Call `commit(seq)` only AFTER AEAD tag verification succeeds.
    #[inline]
    pub fn check(&self, seq: u64) -> bool {
        if !self.initialized {
            return true;
        }
        if seq > self.highest_seq {
            return true;
        }
        let diff = self.highest_seq - seq;
        if diff >= 64 {
            return false;
        }
        (self.bitmap & (1u64 << diff)) == 0
    }

    /// Commits an authenticated sequence number into the sliding window.
    #[inline]
    pub fn commit(&mut self, seq: u64) {
        if !self.initialized {
            self.initialized = true;
            self.highest_seq = seq;
            self.bitmap = 1;
            return;
        }
        if seq > self.highest_seq {
            let shift = seq - self.highest_seq;
            if shift >= 64 {
                self.bitmap = 1;
            } else {
                self.bitmap = (self.bitmap << shift) | 1;
            }
            self.highest_seq = seq;
        } else {
            let diff = self.highest_seq - seq;
            if diff < 64 {
                self.bitmap |= 1u64 << diff;
            }
        }
    }
}

/// Stateful ChaCha20-Poly1305 session with a 64-bit monotonic sequence counter and anti-replay window.
pub struct CryptoSession {
    cipher: ChaCha20Poly1305,
    tx_channel: ChannelId,
    rx_channel: ChannelId,
    tx_seq: u64,
    rx_replay: ReplayWindow,
}

impl CryptoSession {
    pub fn new(shared_key: &[u8; 32], tx_channel: ChannelId, rx_channel: ChannelId) -> Self {
        let key = Key::from_slice(shared_key);
        let initial_seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(1)
            .max(1);
        Self {
            cipher: ChaCha20Poly1305::new(key),
            tx_channel,
            rx_channel,
            tx_seq: initial_seq,
            rx_replay: ReplayWindow::new(),
        }
    }

    #[inline]
    fn build_nonce(channel_id: u8, seq: u64) -> Nonce {
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[0] = channel_id;
        nonce_bytes[4..12].copy_from_slice(&seq.to_be_bytes());
        *Nonce::from_slice(&nonce_bytes)
    }

    /// Zero-allocation in-place frame sealing into a reusable caller-owned `out_buf`.
    /// Clears `out_buf`, writes the 16-byte AAD header, serializes `payload` directly into `out_buf`,
    /// encrypts the payload slice in-place via `ChaCha20-Poly1305`, and appends the 16-byte Poly1305 tag.
    pub fn seal_frame_into(
        &mut self,
        payload: &WirePayload,
        out_buf: &mut Vec<u8>,
    ) -> Result<usize, CryptoFrameError> {
        let seq = self.tx_seq;
        self.tx_seq = self
            .tx_seq
            .checked_add(1)
            .expect("64-bit sequence counter exhausted");

        out_buf.clear();
        // 16-byte associated data (AAD) header
        out_buf.extend_from_slice(&WIRE_MAGIC);
        out_buf.push(WIRE_VERSION);
        out_buf.push(self.tx_channel as u8);
        out_buf.extend_from_slice(&[0u8, 0u8]); // reserved
        out_buf.extend_from_slice(&seq.to_be_bytes());

        // Serialize plaintext directly into `out_buf` after the 16-byte header (zero intermediate Vec)
        bincode::serialize_into(&mut *out_buf, payload)?;

        let header_aad: [u8; FRAME_HEADER_LEN] = out_buf[0..FRAME_HEADER_LEN].try_into().unwrap();
        let nonce = Self::build_nonce(self.tx_channel as u8, seq);

        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce, &header_aad, &mut out_buf[FRAME_HEADER_LEN..])
            .map_err(|_| CryptoFrameError::AuthenticationFailed)?;

        out_buf.extend_from_slice(tag.as_slice());
        Ok(out_buf.len())
    }

    /// Seals a `WirePayload` into a newly allocated `Vec<u8>` (convenience wrapper around `seal_frame_into`).
    pub fn seal_frame(&mut self, payload: &WirePayload) -> Result<Vec<u8>, CryptoFrameError> {
        let mut frame = Vec::with_capacity(128);
        self.seal_frame_into(payload, &mut frame)?;
        Ok(frame)
    }

    /// Zero-allocation in-place frame authentication, anti-replay verification, and decryption.
    /// Decrypts the ciphertext directly inside `frame` without allocating an intermediate buffer.
    pub fn open_frame_in_place(
        &mut self,
        frame: &mut [u8],
    ) -> Result<(u64, WirePayload), CryptoFrameError> {
        if frame.len() < FRAME_HEADER_LEN + POLY1305_TAG_LEN {
            return Err(CryptoFrameError::FrameTooShort(frame.len()));
        }
        if frame[0..4] != WIRE_MAGIC {
            return Err(CryptoFrameError::InvalidMagic);
        }
        if frame[4] != WIRE_VERSION {
            return Err(CryptoFrameError::UnsupportedVersion(frame[4]));
        }
        let ch = frame[5];
        if ch != (self.rx_channel as u8) {
            return Err(CryptoFrameError::ChannelMismatch {
                expected: self.rx_channel as u8,
                actual: ch,
            });
        }

        let seq = u64::from_be_bytes(frame[8..16].try_into().unwrap());
        if !self.rx_replay.check(seq) {
            return Err(CryptoFrameError::ReplayDetected(seq));
        }

        let tag_offset = frame.len() - POLY1305_TAG_LEN;
        let header_aad: [u8; FRAME_HEADER_LEN] = frame[0..FRAME_HEADER_LEN].try_into().unwrap();
        let tag_bytes: [u8; POLY1305_TAG_LEN] = frame[tag_offset..].try_into().unwrap();
        let tag = Tag::from_slice(&tag_bytes);
        let nonce = Self::build_nonce(ch, seq);

        let ciphertext = &mut frame[FRAME_HEADER_LEN..tag_offset];
        self.cipher
            .decrypt_in_place_detached(&nonce, &header_aad, ciphertext, tag)
            .map_err(|_| CryptoFrameError::AuthenticationFailed)?;

        // Commit sequence number only AFTER Poly1305 authentication succeeds
        self.rx_replay.commit(seq);

        let payload: WirePayload = bincode::deserialize(ciphertext)?;
        Ok((seq, payload))
    }

    /// Opens, authenticates, checks anti-replay, and deserializes a received wire frame slice.
    pub fn open_frame(&mut self, frame: &[u8]) -> Result<(u64, WirePayload), CryptoFrameError> {
        let mut copy = frame.to_vec();
        self.open_frame_in_place(&mut copy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seal_open_and_anti_replay() {
        let key = [0x42u8; 32];
        let mut alice = CryptoSession::new(
            &key,
            ChannelId::HidLaptopToDesktop,
            ChannelId::HidDesktopToLaptop,
        );
        let mut bob = CryptoSession::new(
            &key,
            ChannelId::HidDesktopToLaptop,
            ChannelId::HidLaptopToDesktop,
        );

        let msg = WirePayload::HidBatch {
            sender_timestamp_ns: 123_456_789,
            events: vec![HidEvent::Key {
                usage: 0x04,
                pressed: true,
            }],
        };

        let frame = alice.seal_frame(&msg).expect("seal must succeed");
        let (seq, decoded) = bob.open_frame(&frame).expect("open must succeed");
        assert!(seq >= 1);
        assert_eq!(decoded, msg);

        // Replaying the exact same packet must be rejected
        assert!(matches!(
            bob.open_frame(&frame),
            Err(CryptoFrameError::ReplayDetected(s)) if s == seq
        ));
    }
}
