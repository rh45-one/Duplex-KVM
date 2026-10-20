//! Video unit framing for UDP ports `48102` (monitor 0) and `48103` (monitor 1)
//! (`DSCP AF41` / `IP_TOS 0x88`): MTU-safe fragmentation (`1380` bytes/chunk),
//! reassembly with loss detection, and the legacy DPXB band codec. The current
//! stream format is DPXT (see `tile_codec`); DPXC/DPXB remain for old peers.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Per-frame budget basis for the legacy DPXC band encoder (60 Mbit/s).
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
    /// Native full-colour desktop bands, compressed independently with zlib.
    DesktopLossless = 3,
    /// DPXT tiles: lossless/JPEG 64×64 tiles with lossless refinement.
    Tiles = 4,
}

impl VideoCodec {
    #[inline]
    pub const fn from_u8(val: u8) -> Self {
        match val {
            2 => Self::H264,
            3 => Self::DesktopLossless,
            4 => Self::Tiles,
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

/// Builds each MTU-safe (`<= 1404` byte) datagram of `frame` in `scratch`
/// and passes it to `emit`, without allocating per datagram. Stops early and
/// returns false when `emit` returns false.
pub fn emit_video_datagrams(
    frame: &EncodedVideoFrame,
    scratch: &mut Vec<u8>,
    mut emit: impl FnMut(&[u8]) -> bool,
) -> bool {
    let fragment_count = frame
        .data
        .len()
        .div_ceil(MAX_VIDEO_FRAGMENT_PAYLOAD)
        .clamp(1, usize::from(u16::MAX));
    for index in 0..fragment_count {
        let start = index * MAX_VIDEO_FRAGMENT_PAYLOAD;
        let chunk = &frame.data[start.min(frame.data.len())..(start + MAX_VIDEO_FRAGMENT_PAYLOAD).min(frame.data.len())];
        let mut flags = if frame.is_keyframe {
            VIDEO_FLAG_KEYFRAME_IDR
        } else {
            0
        };
        if index + 1 == fragment_count {
            flags |= VIDEO_FLAG_END_OF_FRAME;
        }
        let header = VideoFragmentHeader {
            monitor_index: frame.monitor_index,
            codec: frame.codec,
            flags,
            fragment_index: index as u16,
            fragment_count: fragment_count as u16,
            frame_seq: frame.frame_seq,
            capture_timestamp_ns: frame.capture_timestamp_ns,
            payload_len: chunk.len() as u16,
        };
        let mut header_bytes = [0u8; VIDEO_FRAGMENT_HEADER_LEN];
        header.write_to_slice(&mut header_bytes);
        scratch.clear();
        scratch.extend_from_slice(&header_bytes);
        scratch.extend_from_slice(chunk);
        if !emit(scratch) {
            return false;
        }
    }
    true
}

/// Splits an `EncodedVideoFrame` into MTU-safe (`<= 1404` byte) UDP datagrams.
pub fn packetize_video_frame(frame: &EncodedVideoFrame, out_datagrams: &mut Vec<Vec<u8>>) {
    out_datagrams.clear();
    let mut scratch = Vec::with_capacity(VIDEO_FRAGMENT_HEADER_LEN + MAX_VIDEO_FRAGMENT_PAYLOAD);
    emit_video_datagrams(frame, &mut scratch, |datagram| {
        out_datagrams.push(datagram.to_vec());
        true
    });
}

/// Units this far behind the newest sequence are late duplicates; anything
/// further behind means the sender restarted its numbering.
const STALE_SEQUENCE_WINDOW: u32 = 1024;

/// Per-monitor UDP video frame reassembler with automatic gap / packet-loss detection (PLI).
pub struct VideoFrameAssembler {
    monitor_index: u8,
    current_frame_seq: Option<u32>,
    expected_fragments: u16,
    received_fragments: u16,
    is_keyframe: bool,
    codec: VideoCodec,
    capture_timestamp_ns: u64,
    received: Vec<bool>,
    data: Vec<u8>,
    last_fragment_len: usize,
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
            received: Vec::with_capacity(64),
            data: Vec::new(),
            last_fragment_len: 0,
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
        if hdr.monitor_index != self.monitor_index
            || hdr.fragment_count == 0
            || hdr.fragment_index >= hdr.fragment_count
            || payload.len() > MAX_VIDEO_FRAGMENT_PAYLOAD
        {
            return Ok((None, false));
        }

        if let Some(newest) = self.current_frame_seq.or(self.last_completed_seq) {
            let behind = newest.wrapping_sub(hdr.frame_seq);
            if behind == 0 {
                if self.current_frame_seq.is_none() {
                    return Ok((None, false));
                }
            } else if behind < (1u32 << 31) {
                if behind <= STALE_SEQUENCE_WINDOW {
                    return Ok((None, false));
                }
                // Far behind: the sender restarted its numbering. Resynchronise
                // instead of discarding the new stream until it catches up.
                self.current_frame_seq = None;
                self.last_completed_seq = None;
            }
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

            let count = usize::from(hdr.fragment_count);
            self.current_frame_seq = Some(hdr.frame_seq);
            self.expected_fragments = hdr.fragment_count;
            self.received_fragments = 0;
            self.is_keyframe = (hdr.flags & VIDEO_FLAG_KEYFRAME_IDR) != 0;
            self.codec = hdr.codec;
            self.capture_timestamp_ns = hdr.capture_timestamp_ns;
            self.received.clear();
            self.received.resize(count, false);
            self.data.clear();
            self.data.resize(count * MAX_VIDEO_FRAGMENT_PAYLOAD, 0);
            self.last_fragment_len = 0;
        }

        let idx = usize::from(hdr.fragment_index);
        let last = idx + 1 == usize::from(self.expected_fragments);
        // Every fragment except the last is full-sized; anything else is
        // malformed and leaves the unit incomplete.
        if hdr.fragment_count != self.expected_fragments
            || self.received[idx]
            || (!last && payload.len() != MAX_VIDEO_FRAGMENT_PAYLOAD)
        {
            return Ok((None, gap_detected));
        }
        let offset = idx * MAX_VIDEO_FRAGMENT_PAYLOAD;
        self.data[offset..offset + payload.len()].copy_from_slice(payload);
        if last {
            self.last_fragment_len = payload.len();
        }
        self.received[idx] = true;
        self.received_fragments += 1;

        if self.received_fragments == self.expected_fragments {
            let total = (usize::from(self.expected_fragments) - 1) * MAX_VIDEO_FRAGMENT_PAYLOAD
                + self.last_fragment_len;
            let mut data = std::mem::take(&mut self.data);
            data.truncate(total);
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

/// Magic header `"DPXB"` for built-in zero-dependency low-latency RLE RGB565 horizontal screen bands.
pub const DPXB_BAND_MAGIC: [u8; 4] = *b"DPXB";

/// Encodes a `frame_w x frame_h` `BGRx` (`4 bytes/pixel`) screen capture frame into changed horizontal
/// `DPXB` RLE-compressed bands (`EncodedVideoFrame` units) for zero-dependency cross-OS streaming.
#[allow(clippy::too_many_arguments)]
pub fn encode_bgrx_frame_to_bands(
    monitor_index: u8,
    frame_w: u16,
    frame_h: u16,
    bgrx: &[u8],
    prev_rgb565: &mut Vec<u16>,
    force_keyframe: bool,
    cycle_counter: u32,
    frame_seq: &mut u32,
    now_ns: u64,
    out_frames: &mut Vec<EncodedVideoFrame>,
) {
    let total_pixels = (frame_w as usize) * (frame_h as usize);
    if bgrx.len() < total_pixels * 4 || frame_w == 0 || frame_h == 0 {
        return;
    }

    let is_first = prev_rgb565.len() != total_pixels;
    if is_first {
        prev_rgb565.clear();
        prev_rgb565.resize(total_pixels, 0);
    }

    let band_lines: u16 = 18;
    let num_bands = frame_h.div_ceil(band_lines);

    for band_idx in 0..num_bands {
        let band_y = band_idx * band_lines;
        let band_h = band_lines.min(frame_h.saturating_sub(band_y));
        if band_h == 0 {
            continue;
        }

        let start_px = (band_y as usize) * (frame_w as usize);
        let band_px_count = (band_h as usize) * (frame_w as usize);
        let end_px = start_px + band_px_count;

        let mut band_changed = is_first
            || force_keyframe
            || (u32::from(band_idx) + cycle_counter).is_multiple_of(u32::from(num_bands));

        // Convert BGRx -> RGB565 and check whether this horizontal band changed
        let pixels = bgrx[start_px * 4..end_px * 4].as_chunks::<4>().0;
        for (pixel, previous) in pixels.iter().zip(&mut prev_rgb565[start_px..end_px]) {
            let b = u16::from(pixel[0]);
            let g = u16::from(pixel[1]);
            let r = u16::from(pixel[2]);
            let rgb565 = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3);
            if *previous != rgb565 {
                *previous = rgb565;
                band_changed = true;
            }
        }

        if !band_changed {
            continue;
        }

        // Encode 12-byte DPXB band header + 3-byte RLE runs: [run_len: u8, color_hi: u8, color_lo: u8]
        let mut payload = Vec::with_capacity(12 + band_px_count);
        payload.extend_from_slice(&DPXB_BAND_MAGIC);
        payload.extend_from_slice(&frame_w.to_be_bytes());
        payload.extend_from_slice(&frame_h.to_be_bytes());
        payload.extend_from_slice(&band_y.to_be_bytes());
        payload.extend_from_slice(&band_h.to_be_bytes());

        let slice = &prev_rgb565[start_px..end_px];
        let mut i = 0usize;
        while i < slice.len() {
            let color = slice[i];
            let mut run = 1usize;
            while i + run < slice.len() && slice[i + run] == color && run < 255 {
                run += 1;
            }
            payload.push(run as u8);
            payload.extend_from_slice(&color.to_be_bytes());
            i += run;
        }

        let seq = *frame_seq;
        *frame_seq = frame_seq.wrapping_add(1);
        out_frames.push(EncodedVideoFrame {
            monitor_index,
            codec: VideoCodec::Hevc,
            is_keyframe: force_keyframe && band_idx == 0,
            frame_seq: seq,
            capture_timestamp_ns: now_ns,
            data: payload,
        });
    }
}

/// Decodes a `DPXB` RLE-compressed band payload directly into a `BGRx` (`4 bytes/pixel`) canvas buffer.
/// Returns `true` if a valid band was decoded and applied.
pub fn decode_band_into_bgrx(
    data: &[u8],
    canvas_w: &mut u16,
    canvas_h: &mut u16,
    canvas_bgrx: &mut Vec<u8>,
) -> bool {
    if data.len() >= 4 && data[..4] == crate::screen_codec::DPXC_BAND_MAGIC {
        return crate::screen_codec::decode_native_band(data, canvas_w, canvas_h, canvas_bgrx);
    }
    if data.len() < 12 || data[0..4] != DPXB_BAND_MAGIC {
        return false;
    }

    let fw = u16::from_be_bytes([data[4], data[5]]);
    let fh = u16::from_be_bytes([data[6], data[7]]);
    let by = u16::from_be_bytes([data[8], data[9]]);
    let bh = u16::from_be_bytes([data[10], data[11]]);

    if fw == 0
        || fh == 0
        || by >= fh
        || bh == 0
        || u32::from(by) + u32::from(bh) > u32::from(fh)
        || usize::from(fw) * usize::from(fh) > crate::screen_codec::MAX_CANVAS_PIXELS
    {
        return false;
    }

    let expected_pixels = usize::from(fw) * usize::from(bh);
    let mut run_pixels = 0usize;
    if !(data.len() - 12).is_multiple_of(3) {
        return false;
    }
    for run in data[12..].as_chunks::<3>().0 {
        if run[0] == 0 {
            return false;
        }
        run_pixels += usize::from(run[0]);
        if run_pixels > expected_pixels {
            return false;
        }
    }
    if run_pixels != expected_pixels {
        return false;
    }

    let total_bytes = (fw as usize) * (fh as usize) * 4;
    if *canvas_w != fw || *canvas_h != fh || canvas_bgrx.len() != total_bytes {
        *canvas_w = fw;
        *canvas_h = fh;
        canvas_bgrx.clear();
        canvas_bgrx.resize(total_bytes, 0);
    }

    let start_px = (by as usize) * (fw as usize);
    let max_px = ((fw as usize) * (fh as usize)).min(start_px + (bh as usize) * (fw as usize));
    let mut px_idx = start_px;
    let mut pos = 12usize;

    while pos + 3 <= data.len() && px_idx < max_px {
        let run = data[pos] as usize;
        let color = u16::from_be_bytes([data[pos + 1], data[pos + 2]]);
        pos += 3;

        let r5 = ((color >> 11) & 0x1F) as u8;
        let g6 = ((color >> 5) & 0x3F) as u8;
        let b5 = (color & 0x1F) as u8;

        let r = (r5 << 3) | (r5 >> 2);
        let g = (g6 << 2) | (g6 >> 4);
        let b = (b5 << 3) | (b5 >> 2);

        let end = (px_idx + run).min(max_px);
        while px_idx < end {
            let off = px_idx * 4;
            canvas_bgrx[off] = b;
            canvas_bgrx[off + 1] = g;
            canvas_bgrx[off + 2] = r;
            canvas_bgrx[off + 3] = 0xFF;
            px_idx += 1;
        }
    }

    true
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

    #[test]
    fn test_dpxb_band_encode_decode_roundtrip() {
        let w = 64u16;
        let h = 36u16;
        let mut src_bgrx = vec![0u8; (w as usize) * (h as usize) * 4];
        for i in 0..(w as usize * h as usize) {
            src_bgrx[i * 4] = 0x40; // B
            src_bgrx[i * 4 + 1] = 0x80; // G
            src_bgrx[i * 4 + 2] = 0xF8; // R
            src_bgrx[i * 4 + 3] = 0xFF;
        }
        let mut prev = Vec::new();
        let mut seq = 0u32;
        let mut bands = Vec::new();
        encode_bgrx_frame_to_bands(
            0, w, h, &src_bgrx, &mut prev, true, 0, &mut seq, 100, &mut bands,
        );
        assert_eq!(bands.len(), 2);

        let mut dst_w = 0u16;
        let mut dst_h = 0u16;
        let mut dst_bgrx = Vec::new();
        for b in &bands {
            assert!(decode_band_into_bgrx(
                &b.data,
                &mut dst_w,
                &mut dst_h,
                &mut dst_bgrx
            ));
        }
        assert_eq!(dst_w, w);
        assert_eq!(dst_h, h);
        assert_eq!(dst_bgrx.len(), src_bgrx.len());
    }

    #[test]
    fn delayed_video_fragments_cannot_overwrite_a_newer_band() {
        let mut assembler = VideoFrameAssembler::new(0);
        let mut packets = Vec::new();
        let mut frame = EncodedVideoFrame {
            monitor_index: 0,
            codec: VideoCodec::Hevc,
            is_keyframe: false,
            frame_seq: u32::MAX,
            capture_timestamp_ns: 1,
            data: vec![5; 30],
        };
        packetize_video_frame(&frame, &mut packets);
        let old = packets[0].clone();
        assert!(assembler.push_datagram(&old).unwrap().0.is_some());
        frame.frame_seq = 0;
        packetize_video_frame(&frame, &mut packets);
        assert!(assembler.push_datagram(&packets[0]).unwrap().0.is_some());
        assert_eq!(assembler.push_datagram(&old).unwrap(), (None, false));
        assert_eq!(assembler.push_datagram(&packets[0]).unwrap(), (None, false));
    }

    #[test]
    fn restarted_sender_resynchronises_but_late_duplicates_are_ignored() {
        let mut assembler = VideoFrameAssembler::new(0);
        let mut packets = Vec::new();
        let mut frame = EncodedVideoFrame {
            monitor_index: 0,
            codec: VideoCodec::Tiles,
            is_keyframe: false,
            frame_seq: 5000,
            capture_timestamp_ns: 1,
            data: (0..4000).map(|i| i as u8).collect(),
        };
        let mut push = |assembler: &mut VideoFrameAssembler, frame: &EncodedVideoFrame| {
            packetize_video_frame(frame, &mut packets);
            packets
                .iter()
                .filter_map(|p| assembler.push_datagram(p).unwrap().0)
                .next_back()
        };
        assert_eq!(push(&mut assembler, &frame).unwrap(), frame);
        frame.frame_seq = 4990;
        assert!(push(&mut assembler, &frame).is_none(), "late duplicate");
        frame.frame_seq = 0;
        assert_eq!(push(&mut assembler, &frame).unwrap(), frame, "new session");
        frame.frame_seq = 1;
        assert_eq!(push(&mut assembler, &frame).unwrap(), frame);
    }

    #[test]
    fn short_middle_fragments_and_bad_indices_never_complete_a_unit() {
        let frame = EncodedVideoFrame {
            monitor_index: 1,
            codec: VideoCodec::Tiles,
            is_keyframe: true,
            frame_seq: 9,
            capture_timestamp_ns: 2,
            data: vec![7; MAX_VIDEO_FRAGMENT_PAYLOAD * 2],
        };
        let mut packets = Vec::new();
        packetize_video_frame(&frame, &mut packets);
        let mut assembler = VideoFrameAssembler::new(1);
        let mut short = packets[0].clone();
        short.truncate(short.len() - 1);
        assert!(assembler.push_datagram(&short).unwrap().0.is_none());
        let mut bad_index = packets[1].clone();
        bad_index[8..10].copy_from_slice(&7u16.to_be_bytes());
        assert!(assembler.push_datagram(&bad_index).unwrap().0.is_none());
        assert!(assembler.push_datagram(&packets[1]).unwrap().0.is_none());
        // The correct first fragment completes the unit intact.
        assert_eq!(assembler.push_datagram(&packets[0]).unwrap().0.unwrap(), frame);
    }

    #[test]
    fn truncated_legacy_band_does_not_partially_paint_the_canvas() {
        let mut previous = Vec::new();
        let mut sequence = 0;
        let mut frames = Vec::new();
        encode_bgrx_frame_to_bands(
            0,
            64,
            18,
            &vec![90; 64 * 18 * 4],
            &mut previous,
            true,
            0,
            &mut sequence,
            1,
            &mut frames,
        );
        let mut canvas = vec![123; 64 * 18 * 4];
        let original = canvas.clone();
        let frame = &frames[0].data;
        assert!(!decode_band_into_bgrx(
            &frame[..frame.len() - 3],
            &mut 64,
            &mut 18,
            &mut canvas
        ));
        assert_eq!(canvas, original);
    }
}
