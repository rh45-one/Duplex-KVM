//! Native-pixel, full-colour desktop bands. The byte budget bounds animated
//! content without changing resolution or forgetting bands that were deferred.
use crate::video::{EncodedVideoFrame, VideoCodec};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use std::io::{Read, Write};

pub const DPXC_BAND_MAGIC: [u8; 4] = *b"DPXC";
pub const MAX_CANVAS_PIXELS: usize = 7680 * 4320;

#[allow(clippy::too_many_arguments)]
pub fn encode_native_bands(
    monitor: u8,
    width: u16,
    height: u16,
    pixels: &[u8],
    previous: &mut Vec<u8>,
    force: bool,
    cycle: u32,
    sequence: &mut u32,
    timestamp: u64,
    byte_budget: usize,
    out: &mut Vec<EncodedVideoFrame>,
) {
    let length = usize::from(width) * usize::from(height) * 4;
    if length == 0 || length / 4 > MAX_CANVAS_PIXELS || pixels.len() < length {
        return;
    }
    let first = previous.len() != length;
    if first {
        previous.resize(length, 0);
    }
    let count = u32::from(height).div_ceil(18);
    let mut candidates = Vec::with_capacity(count as usize);
    for step in 0..count {
        let band = (step + cycle) % count;
        let y = band as u16 * 18;
        let rows = (height - y).min(18);
        let start = usize::from(y) * usize::from(width) * 4;
        let end = start + usize::from(rows) * usize::from(width) * 4;
        let changed = first || previous[start..end] != pixels[start..end];
        let refresh = (band + cycle).is_multiple_of(30);
        if changed || force || refresh {
            candidates.push((!changed, y, rows, start, end));
        }
    }
    // At 4K a few unchanged wallpaper bands can consume the entire budget.
    // Sending those first starves real cursor/text changes for many frames.
    // Keep rotation within each group, but service actual changes before refresh.
    candidates.sort_by_key(|candidate| candidate.0);
    let mut used = 0;
    for (_, y, rows, start, end) in candidates {
        if used > 0 && used >= byte_budget {
            break;
        }
        let mut compressor = ZlibEncoder::new(Vec::new(), Compression::fast());
        if compressor.write_all(&pixels[start..end]).is_err() {
            continue;
        }
        let Ok(compressed) = compressor.finish() else {
            continue;
        };
        let size = compressed.len() + 12;
        if used > 0 && used + size > byte_budget {
            continue;
        }
        // A single band may exceed a small budget; always make forward progress.
        let mut data = Vec::with_capacity(size);
        data.extend_from_slice(&DPXC_BAND_MAGIC);
        for value in [width, height, y, rows] {
            data.extend_from_slice(&value.to_be_bytes());
        }
        data.extend_from_slice(&compressed);
        previous[start..end].copy_from_slice(&pixels[start..end]);
        out.push(EncodedVideoFrame {
            monitor_index: monitor,
            codec: VideoCodec::DesktopLossless,
            is_keyframe: first || force,
            frame_seq: *sequence,
            capture_timestamp_ns: timestamp,
            data,
        });
        *sequence = sequence.wrapping_add(1);
        used += size;
    }
}

pub fn decode_native_band(
    data: &[u8],
    width: &mut u16,
    height: &mut u16,
    canvas: &mut Vec<u8>,
) -> bool {
    if data.len() < 12 || data[..4] != DPXC_BAND_MAGIC {
        return false;
    }
    let read = |offset| u16::from_be_bytes([data[offset], data[offset + 1]]);
    let (w, h, y, rows) = (read(4), read(6), read(8), read(10));
    let total = usize::from(w) * usize::from(h);
    if w == 0
        || h == 0
        || rows == 0
        || rows > 18
        || u32::from(y) + u32::from(rows) > u32::from(h)
        || total > MAX_CANVAS_PIXELS
    {
        return false;
    }
    let expected = usize::from(w) * usize::from(rows) * 4;
    let mut decoded = Vec::with_capacity(expected + 1);
    if ZlibDecoder::new(&data[12..])
        .take(expected as u64 + 1)
        .read_to_end(&mut decoded)
        .is_err()
        || decoded.len() != expected
    {
        return false;
    }
    if *width != w || *height != h || canvas.len() != total * 4 {
        canvas.clear();
        canvas.resize(total * 4, 0);
        *width = w;
        *height = h;
    }
    let start = usize::from(y) * usize::from(w) * 4;
    canvas[start..start + expected].copy_from_slice(&decoded);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_colours_and_small_text_pixels_roundtrip_exactly() {
        let (w, h) = (3840, 36);
        let src: Vec<u8> = (0..w as usize * h as usize * 4)
            .map(|i| ((i * 37 + i / 101) % 256) as u8)
            .collect();
        let (mut previous, mut bands, mut seq) = (Vec::new(), Vec::new(), 0);
        encode_native_bands(
            0,
            w,
            h,
            &src,
            &mut previous,
            true,
            0,
            &mut seq,
            1,
            usize::MAX,
            &mut bands,
        );
        let (mut cw, mut ch, mut canvas) = (0, 0, Vec::new());
        for frame in &bands {
            assert!(decode_native_band(
                &frame.data,
                &mut cw,
                &mut ch,
                &mut canvas
            ));
        }
        assert_eq!((cw, ch), (w, h));
        assert_eq!(canvas, src);
        let before = canvas.clone();
        let last = bands.last().unwrap();
        assert!(!decode_native_band(
            &last.data[..last.data.len() - 3],
            &mut cw,
            &mut ch,
            &mut canvas
        ));
        assert_eq!(
            canvas, before,
            "corrupt bands must not partially modify the image"
        );
    }
    #[test]
    fn deferred_bands_eventually_arrive_under_small_budget() {
        let (w, h) = (160, 90);
        let src = vec![73; w as usize * h as usize * 4];
        let (mut previous, mut seq, mut canvas, mut cw, mut ch) = (Vec::new(), 0, Vec::new(), 0, 0);
        for cycle in 0..10 {
            let mut bands = Vec::new();
            encode_native_bands(
                0,
                w,
                h,
                &src,
                &mut previous,
                false,
                cycle,
                &mut seq,
                1,
                1,
                &mut bands,
            );
            assert!(bands.len() <= 1);
            for frame in bands {
                assert!(decode_native_band(
                    &frame.data,
                    &mut cw,
                    &mut ch,
                    &mut canvas
                ));
            }
        }
        assert_eq!(canvas, src);
    }

    #[test]
    fn cursor_change_is_not_starved_by_unchanged_background_refresh() {
        let (w, h) = (160, 90);
        let background = vec![73; w as usize * h as usize * 4];
        let mut updated = background.clone();
        updated[(80 * w as usize + 100) * 4] = 201;
        for force in [false, true] {
            let mut previous = background.clone();
            let mut bands = Vec::new();
            let mut sequence = 0;
            encode_native_bands(
                0,
                w,
                h,
                &updated,
                &mut previous,
                force,
                0,
                &mut sequence,
                1,
                1,
                &mut bands,
            );
            assert_eq!(bands.len(), 1);
            let (mut width, mut height, mut canvas) = (w, h, background.clone());
            assert!(decode_native_band(
                &bands[0].data,
                &mut width,
                &mut height,
                &mut canvas
            ));
            assert_eq!(canvas, updated,
                "the next budgeted update must show the change before refreshing unchanged wallpaper");
        }
    }
}
