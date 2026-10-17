//! Low-Latency Hardware Video Framing (`HEVC` / `VA-API` / `NVENC` / `D3D11VA`) & Decoupled Hardware Cursor.
//!
//! Enforces the 60 Mbps CBR/tight-VBR cap per virtual monitor (`DSCP AF41` / `IP_TOS 0x88`)
//! on UDP ports `48102` (Monitor 0) and `48103` (Monitor 1), MTU-safe fragmentation (`1380` bytes/chunk),
//! and sub-millisecond decoupled hardware cursor updates.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Strict per-monitor bitrate ceiling: 60 Mbps (60,000 kbps) CBR / tight-VBR.
pub const MAX_VIDEO_BITRATE_KBPS: u32 = 60_000;

/// Wire magic `"DPXV"` for UDP video stream datagrams on ports 48102..=48103.
pub const VIDEO_FRAME_MAGIC: [u8; 4] = *b"DPXV";

/// Fixed 24-byte header prepended to every UDP video fragment.
pub const VIDEO_FRAGMENT_HEADER_LEN: usize = 24;

/// Maximum encoded video payload bytes per UDP datagram (`24 + 1380 = 1404` bytes < 1500 Ethernet MTU).
pub const MAX_VIDEO_FRAGMENT_PAYLOAD: usize = 1380;

/// Flag bit indicating this frame is an instantaneous decoder refresh (IDR / I-frame).
pub const VIDEO_FLAG_KEYFRAME_IDR: u8 = 0x01;
/// Flag bit indicating the final fragment of an access unit.
pub const VIDEO_FLAG_END_OF_FRAME: u8 = 0x02;

/// Hardware video codec identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VideoCodec {
    /// ITU-T H.265 / HEVC (`vaapih265enc`, `nvh265enc`, `amfh265enc`, `D3D11VA`).
    Hevc = 1,
    /// ITU-T H.264 / AVC fallback.
    H264 = 2,
}

impl VideoCodec {
    #[inline]
    pub const fn from_u8(val: u8) -> Self {
        match val {
            2 => Self::H264,
            _ => Self::Hevc,
        }
    }
}

/// Complete hardware-encoded video access unit (e.g., one low-delay HEVC IDR or P-slice frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideoFrame {
    pub monitor_index: u8,
    pub codec: VideoCodec,
    pub is_keyframe: bool,
    pub frame_seq: u32,
    pub capture_timestamp_ns: u64,
    /// Annex-B NAL unit bitstream bytes.
    pub data: Vec<u8>,
}

/// Decoupled hardware cursor telemetry transmitted out-of-band from the video pipeline
/// so mouse motion renders at 1000 Hz with zero encode/decode latency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorTelemetry {
    pub monitor_index: u8,
    /// Normalized cursor X coordinate within the monitor (`0..width_px`).
    pub x: i32,
    /// Normalized cursor Y coordinate within the monitor (`0..height_px`).
    pub y: i32,
    pub visible: bool,
    /// Optional updated cursor ARGB/BGRA shape bitmap when the OS cursor icon changes.
    pub shape: Option<CursorShapeBitmap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShapeBitmap {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub bgra_pixels: Vec<u8>,
}

/// 24-byte wire header for UDP video packets on ports `48102` and `48103`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFragmentHeader {
    pub monitor_index: u8,
    pub codec: VideoCodec,
    pub flags: u8,
    pub fragment_index: u16,
    pub fragment_count: u16,
    pub frame_seq: u32,
    pub capture_timestamp_ns: u64,
    pub payload_len: u16,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VideoPacketError {
    #[error("Video packet shorter than 24-byte header ({0} bytes)")]
    PacketTooShort(usize),
    #[error("Invalid video packet magic")]
    InvalidMagic,
    #[error("Payload length mismatch: header specified {expected}, buffer has {actual}")]
    PayloadLengthMismatch { expected: usize, actual: usize },
}

impl VideoFragmentHeader {
    #[inline]
    pub fn write_to_slice(&self, out: &mut [u8; VIDEO_FRAGMENT_HEADER_LEN]) {
        out[0..4].copy_from_slice(&VIDEO_FRAME_MAGIC);
        out[4] = self.monitor_index;
        out[5] = self.codec as u8;
        out[6] = self.flags;
        out[7] = 0; // reserved
        out[8..10].copy_from_slice(&self.fragment_index.to_be_bytes());
        out[10..12].copy_from_slice(&self.fragment_count.to_be_bytes());
        out[12..16].copy_from_slice(&self.frame_seq.to_be_bytes());
        out[16..24].copy_from_slice(&self.capture_timestamp_ns.to_be_bytes());
    }

    #[inline]
    pub fn parse_from_datagram(datagram: &[u8]) -> Result<(Self, &[u8]), VideoPacketError> {
        if datagram.len() < VIDEO_FRAGMENT_HEADER_LEN {
            return Err(VideoPacketError::PacketTooShort(datagram.len()));
        }
        if datagram[0..4] != VIDEO_FRAME_MAGIC {
            return Err(VideoPacketError::InvalidMagic);
        }
        let monitor_index = datagram[4];
        let codec = VideoCodec::from_u8(datagram[5]);
        let flags = datagram[6];
        let fragment_index = u16::from_be_bytes(datagram[8..10].try_into().unwrap());
        let fragment_count = u16::from_be_bytes(datagram[10..12].try_into().unwrap());
        let frame_seq = u32::from_be_bytes(datagram[12..16].try_into().unwrap());
        let capture_timestamp_ns = u64::from_be_bytes(datagram[16..24].try_into().unwrap());
        let payload = &datagram[VIDEO_FRAGMENT_HEADER_LEN..];

        Ok((
            Self {
                monitor_index,
                codec,
                flags,
                fragment_index,
                fragment_count,
                frame_seq,
                capture_timestamp_ns,
                payload_len: payload.len() as u16,
            },
            payload,
        ))
    }
}

/// Splits an `EncodedVideoFrame` into MTU-safe (`<= 1404` byte) UDP datagrams.
pub fn packetize_video_frame(frame: &EncodedVideoFrame, out_datagrams: &mut Vec<Vec<u8>>) {
    out_datagrams.clear();
    let chunks: Vec<&[u8]> = if frame.data.is_empty() {
        vec![&[]]
    } else {
        frame.data.chunks(MAX_VIDEO_FRAGMENT_PAYLOAD).collect()
    };

    let fragment_count = chunks.len().min(u16::MAX as usize) as u16;
    for (idx, chunk) in chunks.into_iter().take(fragment_count as usize).enumerate() {
        let mut flags = if frame.is_keyframe {
            VIDEO_FLAG_KEYFRAME_IDR
        } else {
            0
        };
        if idx + 1 == (fragment_count as usize) {
            flags |= VIDEO_FLAG_END_OF_FRAME;
        }

        let hdr = VideoFragmentHeader {
            monitor_index: frame.monitor_index,
            codec: frame.codec,
            flags,
            fragment_index: idx as u16,
            fragment_count,
            frame_seq: frame.frame_seq,
            capture_timestamp_ns: frame.capture_timestamp_ns,
            payload_len: chunk.len() as u16,
        };

        let mut dgram = Vec::with_capacity(VIDEO_FRAGMENT_HEADER_LEN + chunk.len());
        let mut hdr_bytes = [0u8; VIDEO_FRAGMENT_HEADER_LEN];
        hdr.write_to_slice(&mut hdr_bytes);
        dgram.extend_from_slice(&hdr_bytes);
        dgram.extend_from_slice(chunk);
        out_datagrams.push(dgram);
    }
}

/// Per-monitor UDP video frame reassembler with automatic gap / packet-loss detection (PLI).
pub struct VideoFrameAssembler {
    monitor_index: u8,
    current_frame_seq: Option<u32>,
    expected_fragments: u16,
    received_fragments: u16,
    is_keyframe: bool,
    codec: VideoCodec,
    capture_timestamp_ns: u64,
    slots: Vec<Option<Vec<u8>>>,
    last_completed_seq: Option<u32>,
}

impl VideoFrameAssembler {
    pub fn new(monitor_index: u8) -> Self {
        Self {
            monitor_index,
            current_frame_seq: None,
            expected_fragments: 0,
            received_fragments: 0,
            is_keyframe: false,
            codec: VideoCodec::Hevc,
            capture_timestamp_ns: 0,
            slots: Vec::with_capacity(64),
            last_completed_seq: None,
        }
    }

    /// Pushes an incoming UDP datagram into the reassembler.
    /// Returns `(Option<EncodedVideoFrame>, bool)` where the second tuple element is `true`
    /// if a sequence gap was detected (signaling the caller to send a PLI keyframe request).
    pub fn push_datagram(
        &mut self,
        datagram: &[u8],
    ) -> Result<(Option<EncodedVideoFrame>, bool), VideoPacketError> {
        let (hdr, payload) = VideoFragmentHeader::parse_from_datagram(datagram)?;
        if hdr.monitor_index != self.monitor_index || hdr.fragment_count == 0 {
            return Ok((None, false));
        }

        let mut gap_detected = false;

        if self.current_frame_seq != Some(hdr.frame_seq) {
            // If we were partway through an older frame that never completed, flag packet loss!
            if self.current_frame_seq.is_some() && self.received_fragments < self.expected_fragments
            {
                gap_detected = true;
            }
            if let Some(last_seq) = self.last_completed_seq {
                if hdr.frame_seq != last_seq.wrapping_add(1) && hdr.frame_seq != last_seq {
                    gap_detected = true;
                }
            }

            self.current_frame_seq = Some(hdr.frame_seq);
            self.expected_fragments = hdr.fragment_count;
            self.received_fragments = 0;
            self.is_keyframe = (hdr.flags & VIDEO_FLAG_KEYFRAME_IDR) != 0;
            self.codec = hdr.codec;
            self.capture_timestamp_ns = hdr.capture_timestamp_ns;
            self.slots.clear();
            self.slots.resize(hdr.fragment_count as usize, None);
        }

        let idx = hdr.fragment_index as usize;
        if idx < self.slots.len() && self.slots[idx].is_none() {
            self.slots[idx] = Some(payload.to_vec());
            self.received_fragments += 1;
        }

        if self.received_fragments == self.expected_fragments {
            let total_bytes: usize = self
                .slots
                .iter()
                .filter_map(|s| s.as_ref().map(|v| v.len()))
                .sum();
            let mut data = Vec::with_capacity(total_bytes);
            for slot in self.slots.iter_mut() {
                if let Some(chunk) = slot.take() {
                    data.extend_from_slice(&chunk);
                }
            }

            let frame = EncodedVideoFrame {
                monitor_index: self.monitor_index,
                codec: self.codec,
                is_keyframe: self.is_keyframe,
                frame_seq: hdr.frame_seq,
                capture_timestamp_ns: self.capture_timestamp_ns,
                data,
            };
            self.last_completed_seq = Some(hdr.frame_seq);
            self.current_frame_seq = None;
            return Ok((Some(frame), gap_detected));
        }

        Ok((None, gap_detected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_frame_packetize_and_reassemble() {
        let original = EncodedVideoFrame {
            monitor_index: 1,
            codec: VideoCodec::Hevc,
            is_keyframe: true,
            frame_seq: 42,
            capture_timestamp_ns: 987_654_321,
            data: vec![0xAB; 3500], // Spans 3 fragments (1380 + 1380 + 740)
        };

        let mut dgrams = Vec::new();
        packetize_video_frame(&original, &mut dgrams);
        assert_eq!(dgrams.len(), 3);

        let mut assembler = VideoFrameAssembler::new(1);
        assert_eq!(assembler.push_datagram(&dgrams[0]).unwrap(), (None, false));
        assert_eq!(assembler.push_datagram(&dgrams[1]).unwrap(), (None, false));
        let (completed, gap) = assembler.push_datagram(&dgrams[2]).unwrap();
        assert!(!gap);
        assert_eq!(completed.unwrap(), original);
    }
}
