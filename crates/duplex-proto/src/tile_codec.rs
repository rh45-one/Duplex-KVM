//! DPXT: native-resolution, motion-adaptive desktop codec (up to 8K).
//!
//! Frames are divided into 64×64 tiles. A changed tile is coded as a solid
//! colour, lossless LZ4 (BGR) or, when it is photographic, as part of a JPEG
//! strip covering a horizontal run of tiles. Text and interface content stays
//! lossless. A tile last sent as JPEG is re-sent losslessly once it has been
//! static briefly, so a still desktop always converges to the exact pixels.
//!
//! Each slice is self-contained and sized for UDP fragmentation. Slices carry
//! session, frame and slice numbers; the decoder detects lost slices or frame
//! tails and asks for a refresh. Background refresh repairs anything missed.
//! Encoding and decoding run on the caller's rayon pool.

use crate::video::{EncodedVideoFrame, VideoCodec};
use rayon::prelude::*;

pub const DPXT_MAGIC: [u8; 4] = *b"DPXT";
pub const DPXT_VERSION: u8 = 1;
/// Tile edge in pixels; JPEG MCUs (8 or 16 pixels) divide it evenly.
pub const TILE_SIZE: usize = 64;

const SLICE_HEADER_LEN: usize = 22;
const RECORD_HEADER_LEN: usize = 8;
const FLAG_END_OF_FRAME: u8 = 0x01;
const FLAG_REFRESH: u8 = 0x02;
const FLAG_SYNC: u8 = 0x04;
const KIND_SOLID: u8 = 0;
const KIND_LZ4: u8 = 1;
const KIND_JPEG: u8 = 2;
const MAX_JPEG_RUN: usize = 16;
/// Change detection is memory-bound: beyond two threads it only burns CPU.
const COMPARE_TASKS: usize = 2;
const MAX_SOLID_RUN: usize = 255;
const SYNC_INTERVAL_NS: u64 = 250_000_000;
/// Lossless tiles must be at least this many times smaller than raw BGR;
/// anything less compressible is photographic and may use JPEG.
const LOSSLESS_MIN_RATIO: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileEncoderConfig {
    /// JPEG quality (1..=100) for photographic tiles.
    pub jpeg_quality: u8,
    /// Code JPEG chroma at full resolution (4:4:4) instead of 4:2:0.
    pub jpeg_full_chroma: bool,
    /// Never use JPEG.
    pub lossless_only: bool,
    /// How long a JPEG tile must be unchanged before its lossless refinement.
    pub refine_after_ns: u64,
    /// Upper bound for refinement bytes per frame, so refinement never
    /// delays real changes for long.
    pub refine_budget_bytes: usize,
    /// Every tile is re-sent once per period (self-healing); 0 disables.
    pub refresh_period_ns: u64,
    /// Target slice payload size (one oversized record may exceed it).
    pub slice_bytes: usize,
}

impl Default for TileEncoderConfig {
    fn default() -> Self {
        Self {
            jpeg_quality: 90,
            jpeg_full_chroma: false,
            lossless_only: false,
            refine_after_ns: 250_000_000,
            refine_budget_bytes: 1_500_000,
            refresh_period_ns: 10_000_000_000,
            slice_bytes: 60_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncodeStats {
    pub slices: usize,
    pub bytes: usize,
    pub solid_tiles: usize,
    pub lossless_tiles: usize,
    pub jpeg_tiles: usize,
    pub refined_tiles: usize,
    pub refreshed_tiles: usize,
}

/// A pixel rectangle on the decoded canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Grid {
    width: usize,
    height: usize,
    tiles_x: usize,
    tiles_y: usize,
}

impl Grid {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            tiles_x: width.div_ceil(TILE_SIZE),
            tiles_y: height.div_ceil(TILE_SIZE),
        }
    }

    fn tiles(&self) -> usize {
        self.tiles_x * self.tiles_y
    }

    fn stride(&self) -> usize {
        self.width * 4
    }

    /// Pixel rectangle `(x, y, width, height)` of `run` tiles starting at `index`.
    fn rect(&self, index: usize, run: usize) -> (usize, usize, usize, usize) {
        let (tx, ty) = (index % self.tiles_x, index / self.tiles_x);
        let (x, y) = (tx * TILE_SIZE, ty * TILE_SIZE);
        let right = ((tx + run) * TILE_SIZE).min(self.width);
        (x, y, right - x, TILE_SIZE.min(self.height - y))
    }
}

fn valid_dimensions(width: usize, height: usize) -> bool {
    width > 0 && height > 0 && width * height <= crate::screen_codec::MAX_CANVAS_PIXELS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Change,
    Refine,
    Refresh,
}

enum TileCode {
    Solid([u8; 3]),
    Lossless(Vec<u8>),
    /// Photographic: JPEG candidate, with the LZ4 form kept as a fallback
    /// (`None` when a flatness test skipped it; computed only if needed).
    Photo(Option<Vec<u8>>),
}

enum Payload {
    Solid([u8; 3]),
    Bytes(Vec<u8>),
}

struct Record {
    kind: u8,
    tile: usize,
    run: usize,
    payload: Payload,
}

impl Record {
    fn payload(&self) -> &[u8] {
        match &self.payload {
            Payload::Solid(colour) => colour,
            Payload::Bytes(bytes) => bytes,
        }
    }
}

struct JpegJob {
    first: usize,
    run: usize,
    fallback: Vec<Option<Vec<u8>>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct TileState {
    sent: bool,
    lossy: bool,
    last_change_ns: u64,
}

/// Records what the receiver now holds for a tile. `changed` marks an actual
/// pixel change, which restarts the refinement delay.
fn note_sent(
    tiles: &mut [TileState],
    index: usize,
    purpose: Purpose,
    lossy: bool,
    changed: bool,
    now_ns: u64,
    stats: &mut EncodeStats,
) {
    let state = &mut tiles[index];
    if changed || !state.sent {
        state.last_change_ns = now_ns;
    }
    state.sent = true;
    state.lossy = lossy;
    match purpose {
        Purpose::Refine => stats.refined_tiles += 1,
        Purpose::Refresh => stats.refreshed_tiles += 1,
        Purpose::Change => {}
    }
}

/// Copies a BGRx rectangle as packed BGR and reports whether it is one colour.
fn extract_bgr(
    frame: &[u8],
    stride: usize,
    rect: (usize, usize, usize, usize),
    out: &mut Vec<u8>,
) -> bool {
    let (x, y, w, h) = rect;
    out.resize(w * h * 3, 0);
    let first = y * stride + x * 4;
    let colour = [frame[first], frame[first + 1], frame[first + 2]];
    let mut solid = true;
    for (row, dst) in out.chunks_exact_mut(w * 3).enumerate() {
        let start = (y + row) * stride + x * 4;
        let (pixels, _) = frame[start..start + w * 4].as_chunks::<4>();
        let (bgr, _) = dst.as_chunks_mut::<3>();
        for (src, dst) in pixels.iter().zip(bgr) {
            *dst = [src[0], src[1], src[2]];
            solid &= *dst == colour;
        }
    }
    solid
}

fn classify(
    frame: &[u8],
    grid: Grid,
    index: usize,
    lossless: bool,
    bgr: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> TileCode {
    let rect = grid.rect(index, 1);
    if extract_bgr(frame, grid.stride(), rect, bgr) {
        return TileCode::Solid([bgr[0], bgr[1], bgr[2]]);
    }
    if !lossless && looks_photographic(bgr, rect.2) {
        return TileCode::Photo(None);
    }
    scratch.resize(lz4_flex::block::get_maximum_output_size(bgr.len()), 0);
    let size = lz4_flex::block::compress_into(bgr, scratch).expect("LZ4 output sized to its bound");
    let data = scratch[..size].to_vec();
    if lossless || size * LOSSLESS_MIN_RATIO <= bgr.len() {
        TileCode::Lossless(data)
    } else {
        TileCode::Photo(Some(data))
    }
}

/// Photographs and video rarely repeat a pixel horizontally, while text and
/// interface graphics mostly do. Sampling every fourth row keeps this far
/// cheaper than the LZ4 pass it avoids for photographic tiles.
fn looks_photographic(bgr: &[u8], width: usize) -> bool {
    let (mut equal, mut pairs) = (0usize, 0usize);
    for row in bgr.chunks_exact(width * 3).step_by(4) {
        let (pixels, _) = row.as_chunks::<3>();
        equal += pixels.windows(2).filter(|pair| pair[0] == pair[1]).count();
        pairs += pixels.len().saturating_sub(1);
    }
    equal * 8 < pairs
}

/// The LZ4 form of one tile (fallback when a JPEG strip is not smaller).
fn lossless_tile(frame: &[u8], grid: Grid, index: usize) -> Vec<u8> {
    let mut bgr = Vec::new();
    extract_bgr(frame, grid.stride(), grid.rect(index, 1), &mut bgr);
    lz4_flex::block::compress(&bgr)
}

/// Compares one row of tiles with the previous frame, updating it in place.
fn compare_tile_row(
    band: &mut [u8],
    pixels: &[u8],
    grid: Grid,
    ty: usize,
    changed_row: &mut [bool],
) {
    let stride = grid.stride();
    let rows = band.len() / stride;
    for (tx, changed) in changed_row.iter_mut().enumerate() {
        let x = tx * TILE_SIZE * 4;
        let width = TILE_SIZE.min(grid.width - tx * TILE_SIZE) * 4;
        let mut differs = false;
        for row in 0..rows {
            let local = row * stride + x;
            let source = (ty * TILE_SIZE + row) * stride + x;
            let current = &pixels[source..source + width];
            if differs || band[local..local + width] != *current {
                differs = true;
                band[local..local + width].copy_from_slice(current);
            }
        }
        *changed = differs;
    }
}

fn encode_jpeg_strip(
    frame: &[u8],
    grid: Grid,
    job: &JpegJob,
    cfg: &TileEncoderConfig,
) -> Option<Vec<u8>> {
    let (x, y, w, h) = grid.rect(job.first, job.run);
    let stride = grid.stride();
    let mut strip = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        strip.extend_from_slice(&frame[row * stride + x * 4..row * stride + (x + w) * 4]);
    }
    let mut out = Vec::with_capacity(w * h / 2);
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, cfg.jpeg_quality.clamp(1, 100));
    encoder.set_sampling_factor(if cfg.jpeg_full_chroma {
        jpeg_encoder::SamplingFactor::F_1_1
    } else {
        jpeg_encoder::SamplingFactor::F_2_2
    });
    encoder
        .encode(&strip, w as u16, h as u16, jpeg_encoder::ColorType::Bgra)
        .ok()?;
    Some(out)
}

/// Encodes successive BGRx frames of one monitor into DPXT slices.
pub struct TileEncoder {
    monitor: u8,
    cfg: TileEncoderConfig,
    grid: Grid,
    session: u32,
    frame_no: u32,
    slice_seq: u32,
    emitted_any: bool,
    previous: Vec<u8>,
    tiles: Vec<TileState>,
    changed: Vec<bool>,
    force_refresh: bool,
    refine_cursor: usize,
    refresh_cursor: usize,
    refresh_credit: f64,
    last_refresh_ns: u64,
    last_emit_ns: u64,
}

impl TileEncoder {
    /// `session` must differ between encoder lifetimes (use a random value) so
    /// the receiver restarts frame tracking instead of treating it as loss.
    pub fn new(monitor: u8, width: u16, height: u16, cfg: TileEncoderConfig, session: u32) -> Self {
        let grid = Grid::new(usize::from(width), usize::from(height));
        Self {
            monitor,
            cfg,
            grid,
            session,
            frame_no: 0,
            slice_seq: 0,
            emitted_any: false,
            previous: Vec::new(),
            tiles: vec![TileState::default(); grid.tiles()],
            changed: vec![false; grid.tiles()],
            force_refresh: true,
            refine_cursor: 0,
            refresh_cursor: 0,
            refresh_credit: 0.0,
            last_refresh_ns: 0,
            last_emit_ns: 0,
        }
    }

    pub fn width(&self) -> u16 {
        self.grid.width as u16
    }

    pub fn height(&self) -> u16 {
        self.grid.height as u16
    }

    /// The next frame re-sends every tile (after loss or a new viewer).
    pub fn request_refresh(&mut self) {
        self.force_refresh = true;
    }

    /// Encodes a tightly packed BGRx frame of the configured size. Frames of a
    /// different size are ignored (the stream must be restarted to resize).
    pub fn encode_frame(
        &mut self,
        pixels: &[u8],
        now_ns: u64,
        out: &mut Vec<EncodedVideoFrame>,
    ) -> EncodeStats {
        let grid = self.grid;
        let length = grid.stride() * grid.height;
        if !valid_dimensions(grid.width, grid.height) || pixels.len() < length {
            return EncodeStats::default();
        }
        let mut changed = std::mem::take(&mut self.changed);
        changed.clear();
        changed.resize(grid.tiles(), false);
        if self.previous.len() != length {
            self.previous.clear();
            self.previous.extend_from_slice(&pixels[..length]);
            changed.fill(true);
        } else {
            let stride = grid.stride();
            let rows_per_task = grid.tiles_y.div_ceil(COMPARE_TASKS);
            self.previous
                .par_chunks_mut(rows_per_task * TILE_SIZE * stride)
                .zip(changed.par_chunks_mut(rows_per_task * grid.tiles_x))
                .enumerate()
                .for_each(|(task, (bands, changed_rows))| {
                    let tile_rows = bands
                        .chunks_mut(TILE_SIZE * stride)
                        .zip(changed_rows.chunks_mut(grid.tiles_x));
                    for (offset, (band, changed_row)) in tile_rows.enumerate() {
                        compare_tile_row(
                            band,
                            pixels,
                            grid,
                            task * rows_per_task + offset,
                            changed_row,
                        );
                    }
                });
        }
        let stats = self.encode_current(&changed, now_ns, out);
        self.changed = changed;
        stats
    }

    /// Without a new frame, still performs refinement, background refresh,
    /// requested refreshes and keep-alive synchronisation.
    pub fn encode_idle(&mut self, now_ns: u64, out: &mut Vec<EncodedVideoFrame>) -> EncodeStats {
        if self.previous.is_empty() {
            return EncodeStats::default();
        }
        let mut changed = std::mem::take(&mut self.changed);
        changed.clear();
        changed.resize(self.grid.tiles(), false);
        let stats = self.encode_current(&changed, now_ns, out);
        self.changed = changed;
        stats
    }

    fn encode_current(
        &mut self,
        changed: &[bool],
        now_ns: u64,
        out: &mut Vec<EncodedVideoFrame>,
    ) -> EncodeStats {
        let grid = self.grid;
        let total = grid.tiles();
        let force = std::mem::take(&mut self.force_refresh);
        let mut work: Vec<(usize, Purpose)> = Vec::new();
        let mut dirty = vec![false; total];
        for (index, state) in self.tiles.iter().enumerate() {
            if force || changed[index] || !state.sent {
                dirty[index] = true;
                work.push((index, Purpose::Change));
            }
        }
        let first_refine = work.len();
        let refine_cap = self.cfg.refine_budget_bytes / 6144 + 1;
        for step in 0..total {
            if work.len() - first_refine >= refine_cap {
                break;
            }
            let index = (self.refine_cursor + step) % total;
            let state = self.tiles[index];
            if state.lossy
                && !dirty[index]
                && now_ns.saturating_sub(state.last_change_ns) >= self.cfg.refine_after_ns
            {
                work.push((index, Purpose::Refine));
            }
        }
        let refine_end = work.len();
        if self.cfg.refresh_period_ns > 0 {
            if self.last_refresh_ns == 0 {
                self.last_refresh_ns = now_ns;
            }
            let elapsed = now_ns.saturating_sub(self.last_refresh_ns);
            self.last_refresh_ns = now_ns;
            self.refresh_credit +=
                total as f64 * elapsed as f64 / self.cfg.refresh_period_ns as f64;
            let mut due = (self.refresh_credit.floor() as usize).min(total);
            self.refresh_credit = (self.refresh_credit - due as f64).min(total as f64);
            let mut scanned = 0;
            while due > 0 && scanned < total {
                let index = self.refresh_cursor;
                self.refresh_cursor = (self.refresh_cursor + 1) % total;
                scanned += 1;
                due -= 1;
                let state = self.tiles[index];
                if state.sent && !state.lossy && !dirty[index] {
                    work.push((index, Purpose::Refresh));
                }
            }
        }

        let frame = self.previous.as_slice();
        let lossless_only = self.cfg.lossless_only;
        let coded: Vec<TileCode> = work
            .par_iter()
            .map_init(
                || (Vec::new(), Vec::new()),
                |(bgr, scratch), &(index, purpose)| {
                    let lossless = lossless_only || purpose != Purpose::Change;
                    classify(frame, grid, index, lossless, bgr, scratch)
                },
            )
            .collect();

        let mut stats = EncodeStats::default();
        let mut codes: Vec<Option<(Purpose, TileCode)>> = (0..total).map(|_| None).collect();
        let mut refine_bytes = 0usize;
        let mut refine_resume = None;
        for (position, (&(index, purpose), code)) in work.iter().zip(coded).enumerate() {
            if purpose == Purpose::Refine {
                if refine_resume.is_some() {
                    continue;
                }
                let size = match &code {
                    TileCode::Solid(_) => 3,
                    TileCode::Lossless(data) | TileCode::Photo(Some(data)) => data.len(),
                    TileCode::Photo(None) => TILE_SIZE * TILE_SIZE * 3,
                };
                if refine_bytes > 0 && refine_bytes + size > self.cfg.refine_budget_bytes {
                    refine_resume = Some(index);
                    continue;
                }
                refine_bytes += size;
                if position + 1 == refine_end {
                    self.refine_cursor = (index + 1) % total;
                }
            }
            codes[index] = Some((purpose, code));
        }
        if let Some(index) = refine_resume {
            self.refine_cursor = index;
        }

        let mut records = Vec::new();
        let mut jobs = Vec::new();
        let mut placeholders = Vec::new();
        let mut index = 0;
        while index < total {
            let row_end = (index / grid.tiles_x + 1) * grid.tiles_x;
            match codes[index].take() {
                None => index += 1,
                Some((purpose, TileCode::Solid(colour))) => {
                    note_sent(
                        &mut self.tiles,
                        index,
                        purpose,
                        false,
                        changed[index],
                        now_ns,
                        &mut stats,
                    );
                    stats.solid_tiles += 1;
                    let mut run = 1;
                    while index + run < row_end
                        && run < MAX_SOLID_RUN
                        && matches!(&codes[index + run], Some((_, TileCode::Solid(next))) if *next == colour)
                    {
                        let (purpose, _) = codes[index + run].take().expect("checked");
                        let tile = index + run;
                        note_sent(
                            &mut self.tiles,
                            tile,
                            purpose,
                            false,
                            changed[tile],
                            now_ns,
                            &mut stats,
                        );
                        stats.solid_tiles += 1;
                        run += 1;
                    }
                    records.push(Record {
                        kind: KIND_SOLID,
                        tile: index,
                        run,
                        payload: Payload::Solid(colour),
                    });
                    index += run;
                }
                Some((purpose, TileCode::Lossless(data))) => {
                    note_sent(
                        &mut self.tiles,
                        index,
                        purpose,
                        false,
                        changed[index],
                        now_ns,
                        &mut stats,
                    );
                    stats.lossless_tiles += 1;
                    records.push(Record {
                        kind: KIND_LZ4,
                        tile: index,
                        run: 1,
                        payload: Payload::Bytes(data),
                    });
                    index += 1;
                }
                Some((_, TileCode::Photo(data))) => {
                    let mut fallback = vec![data];
                    while index + fallback.len() < row_end
                        && fallback.len() < MAX_JPEG_RUN
                        && matches!(
                            &codes[index + fallback.len()],
                            Some((_, TileCode::Photo(_)))
                        )
                    {
                        if let Some((_, TileCode::Photo(data))) =
                            codes[index + fallback.len()].take()
                        {
                            fallback.push(data);
                        }
                    }
                    let run = fallback.len();
                    placeholders.push(records.len());
                    records.push(Record {
                        kind: KIND_JPEG,
                        tile: index,
                        run,
                        payload: Payload::Bytes(Vec::new()),
                    });
                    jobs.push(JpegJob {
                        first: index,
                        run,
                        fallback,
                    });
                    index += run;
                }
            }
        }

        let cfg = self.cfg;
        let jpegs: Vec<Option<Vec<u8>>> = jobs
            .par_iter()
            .map(|job| encode_jpeg_strip(frame, grid, job, &cfg))
            .collect();
        let mut expanded: Vec<Option<Vec<Record>>> = (0..records.len()).map(|_| None).collect();
        for ((job, jpeg), &slot) in jobs.into_iter().zip(jpegs).zip(&placeholders) {
            // Unknown fallbacks count as large: such tiles are photographic.
            let lossless_size: usize = job
                .fallback
                .iter()
                .map(|data| data.as_ref().map_or(usize::MAX / MAX_JPEG_RUN, Vec::len))
                .sum();
            match jpeg {
                Some(jpeg) if jpeg.len() < lossless_size => {
                    for (tile, &moved) in changed.iter().enumerate().skip(job.first).take(job.run) {
                        note_sent(
                            &mut self.tiles,
                            tile,
                            Purpose::Change,
                            true,
                            moved,
                            now_ns,
                            &mut stats,
                        );
                    }
                    stats.jpeg_tiles += job.run;
                    records[slot].payload = Payload::Bytes(jpeg);
                }
                _ => {
                    let mut replacement = Vec::with_capacity(job.run);
                    for (offset, data) in job.fallback.into_iter().enumerate() {
                        let tile = job.first + offset;
                        let data = data.unwrap_or_else(|| lossless_tile(frame, grid, tile));
                        note_sent(
                            &mut self.tiles,
                            tile,
                            Purpose::Change,
                            false,
                            changed[tile],
                            now_ns,
                            &mut stats,
                        );
                        stats.lossless_tiles += 1;
                        replacement.push(Record {
                            kind: KIND_LZ4,
                            tile,
                            run: 1,
                            payload: Payload::Bytes(data),
                        });
                    }
                    expanded[slot] = Some(replacement);
                }
            }
        }
        if expanded.iter().any(Option::is_some) {
            let mut flat = Vec::with_capacity(records.len());
            for (record, replacement) in records.into_iter().zip(expanded) {
                match replacement {
                    Some(replacement) => flat.extend(replacement),
                    None => flat.push(record),
                }
            }
            records = flat;
        }

        if records.is_empty() {
            if self.emitted_any && now_ns.saturating_sub(self.last_emit_ns) >= SYNC_INTERVAL_NS {
                let frame_no = self.frame_no;
                self.push_slice(
                    FLAG_SYNC | FLAG_END_OF_FRAME,
                    0,
                    frame_no,
                    &[],
                    now_ns,
                    out,
                    &mut stats,
                );
            }
            return stats;
        }
        self.frame_no = self.frame_no.wrapping_add(1);
        self.emitted_any = true;
        let refresh_flag = if force { FLAG_REFRESH } else { 0 };
        let mut slice_index = 0u16;
        let mut start = 0;
        let mut size = SLICE_HEADER_LEN;
        for (position, record) in records.iter().enumerate() {
            let length = RECORD_HEADER_LEN + record.payload().len();
            let full = position > start
                && (size + length > self.cfg.slice_bytes
                    || position - start == usize::from(u16::MAX));
            if full {
                self.push_slice(
                    refresh_flag,
                    slice_index,
                    self.frame_no,
                    &records[start..position],
                    now_ns,
                    out,
                    &mut stats,
                );
                slice_index = slice_index.wrapping_add(1);
                start = position;
                size = SLICE_HEADER_LEN;
            }
            size += length;
        }
        self.push_slice(
            refresh_flag | FLAG_END_OF_FRAME,
            slice_index,
            self.frame_no,
            &records[start..],
            now_ns,
            out,
            &mut stats,
        );
        stats
    }

    #[allow(clippy::too_many_arguments)]
    fn push_slice(
        &mut self,
        flags: u8,
        slice_index: u16,
        frame_no: u32,
        records: &[Record],
        now_ns: u64,
        out: &mut Vec<EncodedVideoFrame>,
        stats: &mut EncodeStats,
    ) {
        let payload: usize = records
            .iter()
            .map(|r| RECORD_HEADER_LEN + r.payload().len())
            .sum();
        let mut data = Vec::with_capacity(SLICE_HEADER_LEN + payload);
        data.extend_from_slice(&DPXT_MAGIC);
        data.push(DPXT_VERSION);
        data.push(flags);
        data.extend_from_slice(&(self.grid.width as u16).to_be_bytes());
        data.extend_from_slice(&(self.grid.height as u16).to_be_bytes());
        data.extend_from_slice(&self.session.to_be_bytes());
        data.extend_from_slice(&frame_no.to_be_bytes());
        data.extend_from_slice(&slice_index.to_be_bytes());
        data.extend_from_slice(&(records.len() as u16).to_be_bytes());
        for record in records {
            let bytes = record.payload();
            data.push(record.kind);
            data.push(record.run as u8);
            data.extend_from_slice(&(record.tile as u16).to_be_bytes());
            data.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            data.extend_from_slice(bytes);
        }
        stats.slices += 1;
        stats.bytes += data.len();
        out.push(EncodedVideoFrame {
            monitor_index: self.monitor,
            codec: VideoCodec::Tiles,
            is_keyframe: flags & FLAG_REFRESH != 0,
            frame_seq: self.slice_seq,
            capture_timestamp_ns: now_ns,
            data,
        });
        self.slice_seq = self.slice_seq.wrapping_add(1);
        self.last_emit_ns = now_ns;
    }
}

/// Result of applying slices to a [`TileDecoder`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SliceOutcome {
    /// At least one slice was valid DPXT for this decoder.
    pub accepted: bool,
    /// A frame ended (completely or not); present the canvas now.
    pub frame_complete: bool,
    /// Something was lost or corrupt; ask the sender for a refresh.
    pub refresh_needed: bool,
    /// The canvas size changed; the presenter must reallocate.
    pub resized: bool,
}

impl SliceOutcome {
    fn merge(&mut self, other: SliceOutcome) {
        self.accepted |= other.accepted;
        self.frame_complete |= other.frame_complete;
        self.refresh_needed |= other.refresh_needed;
        self.resized |= other.resized;
    }
}

struct SliceHeader {
    flags: u8,
    width: usize,
    height: usize,
    session: u32,
    frame_no: u32,
    slice_index: u16,
    records: usize,
}

fn parse_header(data: &[u8]) -> Option<SliceHeader> {
    if data.len() < SLICE_HEADER_LEN || data[..4] != DPXT_MAGIC || data[4] != DPXT_VERSION {
        return None;
    }
    let u16_at = |at: usize| u16::from_be_bytes([data[at], data[at + 1]]);
    let u32_at =
        |at: usize| u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    let header = SliceHeader {
        flags: data[5],
        width: usize::from(u16_at(6)),
        height: usize::from(u16_at(8)),
        session: u32_at(10),
        frame_no: u32_at(14),
        slice_index: u16_at(18),
        records: usize::from(u16_at(20)),
    };
    valid_dimensions(header.width, header.height).then_some(header)
}

/// `a` is later than `b` in wrapping sequence order.
fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < (1 << 31)
}

struct PendingRecord<'a> {
    kind: u8,
    tile: usize,
    run: usize,
    rect: (usize, usize, usize, usize),
    data: &'a [u8],
}

enum Decoded {
    Solid([u8; 3]),
    Bgr(Vec<u8>),
    Bgra(Vec<u8>),
    Failed,
}

fn decode_record(record: &PendingRecord) -> Decoded {
    let (_, _, w, h) = record.rect;
    match record.kind {
        KIND_SOLID => Decoded::Solid([record.data[0], record.data[1], record.data[2]]),
        KIND_LZ4 => {
            let mut out = vec![0u8; w * h * 3];
            match lz4_flex::block::decompress_into(record.data, &mut out) {
                Ok(size) if size == out.len() => Decoded::Bgr(out),
                _ => Decoded::Failed,
            }
        }
        KIND_JPEG => {
            let data = record.data;
            // The JPEG decoder is fuzzed, but untrusted input must never take
            // the receiver down with it.
            std::panic::catch_unwind(|| {
                let options = zune_core::options::DecoderOptions::default()
                    .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::BGRA)
                    .set_max_width(MAX_JPEG_RUN * TILE_SIZE)
                    .set_max_height(TILE_SIZE);
                let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
                    zune_core::bytestream::ZCursor::new(data),
                    options,
                );
                let pixels = decoder.decode().ok()?;
                let info = decoder.info()?;
                (usize::from(info.width) == w
                    && usize::from(info.height) == h
                    && pixels.len() == w * h * 4)
                    .then_some(pixels)
            })
            .ok()
            .flatten()
            .map_or(Decoded::Failed, Decoded::Bgra)
        }
        _ => Decoded::Failed,
    }
}

/// Reassembles DPXT slices into a BGRx canvas (fourth byte 0xFF).
pub struct TileDecoder {
    grid: Grid,
    canvas: Vec<u8>,
    session: Option<u32>,
    open_frame: Option<u32>,
    next_slice: u16,
    last_complete: Option<u32>,
    dirty: Vec<bool>,
    dirty_any: bool,
}

impl Default for TileDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TileDecoder {
    pub fn new() -> Self {
        Self {
            grid: Grid::new(0, 0),
            canvas: Vec::new(),
            session: None,
            open_frame: None,
            next_slice: 0,
            last_complete: None,
            dirty: Vec::new(),
            dirty_any: false,
        }
    }

    pub fn width(&self) -> u16 {
        self.grid.width as u16
    }

    pub fn height(&self) -> u16 {
        self.grid.height as u16
    }

    /// Tightly packed BGRx pixels (`width * 4` bytes per row).
    pub fn canvas(&self) -> &[u8] {
        &self.canvas
    }

    /// Returns true when `data` is a DPXT slice (regardless of validity).
    pub fn is_dpxt(data: &[u8]) -> bool {
        data.len() >= 4 && data[..4] == DPXT_MAGIC
    }

    fn resize(&mut self, width: usize, height: usize) {
        self.grid = Grid::new(width, height);
        self.canvas.clear();
        self.canvas.resize(width * height * 4, 0);
        for pixel in self.canvas.as_chunks_mut::<4>().0 {
            pixel[3] = 0xFF;
        }
        self.dirty = vec![true; self.grid.tiles()];
        self.dirty_any = true;
        self.open_frame = None;
        self.last_complete = None;
    }

    /// Applies one slice. Never panics on malformed input.
    pub fn decode_slice(&mut self, data: &[u8]) -> SliceOutcome {
        self.decode_batch(&[data])
    }

    /// Applies several consecutive slices, decoding their records in parallel.
    /// Callers that present frame by frame should split batches after slices
    /// that end a frame.
    pub fn decode_batch(&mut self, slices: &[&[u8]]) -> SliceOutcome {
        let mut outcome = SliceOutcome::default();
        let mut pending: Vec<PendingRecord> = Vec::new();
        for &data in slices {
            let Some(header) = parse_header(data) else {
                if Self::is_dpxt(data) {
                    outcome.refresh_needed = true;
                }
                continue;
            };
            if header.width != self.grid.width || header.height != self.grid.height {
                self.apply(&pending);
                pending.clear();
                self.resize(header.width, header.height);
                self.session = None;
                outcome.resized = true;
            }
            let (accepted, tracked) = self.track(&header);
            outcome.merge(tracked);
            if !accepted {
                continue;
            }
            outcome.accepted = true;
            if !self.parse_records(&header, data, &mut pending) {
                outcome.refresh_needed = true;
            }
        }
        if !self.apply(&pending) {
            outcome.refresh_needed = true;
        }
        outcome
    }

    /// Frame continuity. Returns whether the slice's records should be applied.
    fn track(&mut self, header: &SliceHeader) -> (bool, SliceOutcome) {
        let mut outcome = SliceOutcome::default();
        if self.session != Some(header.session) {
            // A new encoder lifetime: keep the image, restart continuity.
            self.session = Some(header.session);
            self.open_frame = None;
            self.last_complete = None;
            self.next_slice = 0;
            if header.flags & FLAG_SYNC != 0 {
                outcome.refresh_needed = true;
                return (false, outcome);
            }
        }
        if header.flags & FLAG_SYNC != 0 {
            if let Some(open) = self.open_frame.take() {
                outcome.frame_complete = true;
                outcome.refresh_needed = true;
                self.last_complete = Some(open);
            }
            if self
                .last_complete
                .is_none_or(|last| newer(header.frame_no, last))
            {
                outcome.refresh_needed = true;
            }
            return (false, outcome);
        }
        if self.open_frame != Some(header.frame_no) {
            if self
                .last_complete
                .is_some_and(|last| !newer(header.frame_no, last))
            {
                // Duplicated or reordered old slice: never overwrite newer pixels.
                return (false, outcome);
            }
            if let Some(open) = self.open_frame.take() {
                outcome.frame_complete = true;
                outcome.refresh_needed = true;
                self.last_complete = Some(open);
            }
            if self
                .last_complete
                .is_some_and(|last| header.frame_no != last.wrapping_add(1))
            {
                outcome.refresh_needed = true;
            }
            self.open_frame = Some(header.frame_no);
            self.next_slice = 0;
        }
        if header.slice_index != self.next_slice {
            outcome.refresh_needed = true;
        }
        self.next_slice = header.slice_index.wrapping_add(1);
        if header.flags & FLAG_END_OF_FRAME != 0 {
            outcome.frame_complete = true;
            self.last_complete = Some(header.frame_no);
            self.open_frame = None;
        }
        (true, outcome)
    }

    /// Validates record framing and geometry. Returns false if any record was
    /// rejected; valid records are still applied.
    fn parse_records<'a>(
        &self,
        header: &SliceHeader,
        data: &'a [u8],
        out: &mut Vec<PendingRecord<'a>>,
    ) -> bool {
        let grid = self.grid;
        let mut at = SLICE_HEADER_LEN;
        for _ in 0..header.records {
            if data.len() < at + RECORD_HEADER_LEN {
                return false;
            }
            let kind = data[at];
            let run = usize::from(data[at + 1]);
            let tile = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
            let length =
                u32::from_be_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]])
                    as usize;
            at += RECORD_HEADER_LEN;
            if data.len() - at < length {
                return false;
            }
            let payload = &data[at..at + length];
            at += length;
            let geometry_ok =
                run > 0 && tile < grid.tiles() && tile % grid.tiles_x + run <= grid.tiles_x;
            let kind_ok = match kind {
                KIND_SOLID => length == 3,
                KIND_LZ4 => run == 1,
                KIND_JPEG => run <= MAX_JPEG_RUN,
                _ => false,
            };
            if !geometry_ok || !kind_ok {
                return false;
            }
            out.push(PendingRecord {
                kind,
                tile,
                run,
                rect: grid.rect(tile, run),
                data: payload,
            });
        }
        at == data.len()
    }

    fn apply(&mut self, records: &[PendingRecord]) -> bool {
        if records.is_empty() {
            return true;
        }
        let decoded: Vec<Decoded> = if records.len() > 1 {
            records.par_iter().map(decode_record).collect()
        } else {
            records.iter().map(decode_record).collect()
        };
        let stride = self.grid.stride();
        let mut ok = true;
        for (record, pixels) in records.iter().zip(decoded) {
            let (x, y, w, h) = record.rect;
            match pixels {
                Decoded::Failed => {
                    ok = false;
                    continue;
                }
                Decoded::Solid(colour) => {
                    let value = [colour[0], colour[1], colour[2], 0xFF];
                    for row in y..y + h {
                        let start = row * stride + x * 4;
                        for pixel in self.canvas[start..start + w * 4].as_chunks_mut::<4>().0 {
                            pixel.copy_from_slice(&value);
                        }
                    }
                }
                Decoded::Bgr(bgr) => {
                    for (row, source) in (y..y + h).zip(bgr.chunks_exact(w * 3)) {
                        let start = row * stride + x * 4;
                        let (pixels, _) = self.canvas[start..start + w * 4].as_chunks_mut::<4>();
                        let (source, _) = source.as_chunks::<3>();
                        for (pixel, src) in pixels.iter_mut().zip(source) {
                            *pixel = [src[0], src[1], src[2], 0xFF];
                        }
                    }
                }
                Decoded::Bgra(bgra) => {
                    for (row, source) in (y..y + h).zip(bgra.chunks_exact(w * 4)) {
                        let start = row * stride + x * 4;
                        self.canvas[start..start + w * 4].copy_from_slice(source);
                    }
                }
            }
            for tile in record.tile..record.tile + record.run {
                self.dirty[tile] = true;
            }
            self.dirty_any = true;
        }
        ok
    }

    pub fn has_dirty(&self) -> bool {
        self.dirty_any
    }

    /// Changed regions since the last call, merged into horizontal runs of
    /// tiles. Clears the record of changes.
    pub fn take_dirty_rects(&mut self, out: &mut Vec<PixelRect>) {
        out.clear();
        if !self.dirty_any {
            return;
        }
        let grid = self.grid;
        for ty in 0..grid.tiles_y {
            let row = &mut self.dirty[ty * grid.tiles_x..(ty + 1) * grid.tiles_x];
            let mut tx = 0;
            while tx < grid.tiles_x {
                if !row[tx] {
                    tx += 1;
                    continue;
                }
                let start = tx;
                while tx < grid.tiles_x && row[tx] {
                    row[tx] = false;
                    tx += 1;
                }
                let (x, y, w, h) = grid.rect(ty * grid.tiles_x + start, tx - start);
                out.push(PixelRect {
                    x: x as u32,
                    y: y as u32,
                    width: w as u32,
                    height: h as u32,
                });
            }
        }
        self.dirty_any = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_like(width: usize, height: usize, seed: usize) -> Vec<u8> {
        let mut frame = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * 4;
                // Dark editor background with glyph-like, anti-aliased strokes.
                let glyph = (x / 7 + y / 13 + seed).is_multiple_of(5) && (x + y * 3) % 11 < 4;
                let shade = if glyph {
                    200 + ((x * 7 + y) % 40) as u8
                } else {
                    30
                };
                frame[i..i + 4].copy_from_slice(&[shade, shade, shade.saturating_add(10), 0xFF]);
            }
        }
        frame
    }

    fn photo_like(width: usize, height: usize, seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2_654_435_761).max(1);
        let mut frame = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let noise = (state & 0x1F) as i32 - 16;
                let i = (y * width + x) * 4;
                let base = [
                    (x * 255 / width) as i32,
                    (y * 255 / height) as i32,
                    ((x + y) % 256) as i32,
                ];
                for c in 0..3 {
                    frame[i + c] = (base[c] + noise).clamp(0, 255) as u8;
                }
                frame[i + 3] = 0xFF;
            }
        }
        frame
    }

    fn decode_all(decoder: &mut TileDecoder, slices: &[EncodedVideoFrame]) -> SliceOutcome {
        let mut outcome = SliceOutcome::default();
        for slice in slices {
            assert_eq!(slice.codec, VideoCodec::Tiles);
            outcome.merge(decoder.decode_slice(&slice.data));
        }
        outcome
    }

    fn pixels(frame: &[u8]) -> &[[u8; 4]] {
        frame.as_chunks::<4>().0
    }

    fn bgr_equal(a: &[u8], b: &[u8]) -> bool {
        a.len() == b.len()
            && pixels(a)
                .iter()
                .zip(pixels(b))
                .all(|(p, q)| p[..3] == q[..3])
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mut error = 0f64;
        let mut count = 0f64;
        for (p, q) in pixels(a).iter().zip(pixels(b)) {
            for c in 0..3 {
                let d = f64::from(p[c]) - f64::from(q[c]);
                error += d * d;
                count += 1.0;
            }
        }
        if error == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (255.0f64 * 255.0 / (error / count)).log10()
    }

    #[test]
    fn text_frames_are_exact_and_only_changed_tiles_are_resent() {
        let (w, h) = (1000, 700); // Edge tiles are partial on both axes.
        let first = text_like(w, h, 0);
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, TileEncoderConfig::default(), 7);
        let mut decoder = TileDecoder::new();
        let mut slices = Vec::new();
        let stats = encoder.encode_frame(&first, 1, &mut slices);
        assert_eq!(stats.jpeg_tiles, 0, "text must never be lossy");
        let outcome = decode_all(&mut decoder, &slices);
        assert!(outcome.frame_complete && outcome.resized && !outcome.refresh_needed);
        assert_eq!((decoder.width(), decoder.height()), (w as u16, h as u16));
        assert!(bgr_equal(decoder.canvas(), &first));
        assert!(decoder
            .canvas()
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[3] == 0xFF));

        let mut second = first.clone();
        for y in 130..140 {
            for x in 200..230 {
                second[(y * w + x) * 4] ^= 0x55;
            }
        }
        slices.clear();
        let stats = encoder.encode_frame(&second, 2, &mut slices);
        assert_eq!(
            stats.solid_tiles + stats.lossless_tiles,
            1,
            "one tile changed: {stats:?}"
        );
        let mut rects = Vec::new();
        decoder.take_dirty_rects(&mut rects);
        let outcome = decode_all(&mut decoder, &slices);
        assert!(outcome.frame_complete && !outcome.refresh_needed);
        assert!(bgr_equal(decoder.canvas(), &second));
        decoder.take_dirty_rects(&mut rects);
        assert_eq!(
            rects,
            vec![PixelRect {
                x: 192,
                y: 128,
                width: 64,
                height: 64
            }]
        );
    }

    #[test]
    fn photographic_tiles_use_jpeg_then_refine_to_exact_pixels() {
        let (w, h) = (512, 256);
        let photo = photo_like(w, h, 3);
        let mut encoder = TileEncoder::new(1, w as u16, h as u16, TileEncoderConfig::default(), 9);
        let mut decoder = TileDecoder::new();
        let mut slices = Vec::new();
        let stats = encoder.encode_frame(&photo, 1_000, &mut slices);
        assert!(stats.jpeg_tiles > 0, "{stats:?}");
        let jpeg_bytes = stats.bytes;
        decode_all(&mut decoder, &slices);
        let quality = psnr(decoder.canvas(), &photo);
        assert!(quality > 30.0, "JPEG PSNR {quality}");
        assert!(!bgr_equal(decoder.canvas(), &photo));
        // Not yet static long enough: nothing to refine.
        slices.clear();
        assert_eq!(
            encoder
                .encode_frame(&photo, 100_000_000, &mut slices)
                .refined_tiles,
            0
        );
        // Static beyond the refinement delay: converge losslessly, within budget.
        let mut now = 400_000_000;
        let mut refined = 0;
        while !bgr_equal(decoder.canvas(), &photo) {
            slices.clear();
            let stats = encoder.encode_idle(now, &mut slices);
            assert!(stats.bytes <= TileEncoderConfig::default().refine_budget_bytes + 64_000);
            refined += stats.refined_tiles;
            decode_all(&mut decoder, &slices);
            now += 20_000_000;
            assert!(now < 5_000_000_000, "refinement must converge");
        }
        assert_eq!(refined, stats.jpeg_tiles);
        assert!(
            jpeg_bytes * 2 < w * h * 3,
            "JPEG must be much smaller than raw"
        );
    }

    #[test]
    fn lossless_only_mode_never_uses_jpeg() {
        let (w, h) = (300, 200);
        let photo = photo_like(w, h, 5);
        let cfg = TileEncoderConfig {
            lossless_only: true,
            ..Default::default()
        };
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, cfg, 1);
        let mut decoder = TileDecoder::new();
        let mut slices = Vec::new();
        assert_eq!(encoder.encode_frame(&photo, 1, &mut slices).jpeg_tiles, 0);
        decode_all(&mut decoder, &slices);
        assert!(bgr_equal(decoder.canvas(), &photo));
    }

    #[test]
    fn slices_respect_size_and_lost_slices_request_refresh() {
        let (w, h) = (1280, 720);
        let frame = photo_like(w, h, 11);
        let cfg = TileEncoderConfig {
            slice_bytes: 16_000,
            lossless_only: true,
            ..Default::default()
        };
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, cfg, 2);
        let mut slices = Vec::new();
        encoder.encode_frame(&frame, 1, &mut slices);
        assert!(slices.len() > 4);
        for slice in &slices {
            // One record (an incompressible 64×64 tile) may exceed the target.
            assert!(
                slice.data.len() <= 16_000 + RECORD_HEADER_LEN + 12_400,
                "{}",
                slice.data.len()
            );
        }
        for pair in slices.windows(2) {
            assert_eq!(pair[1].frame_seq, pair[0].frame_seq.wrapping_add(1));
        }
        let mut decoder = TileDecoder::new();
        let mut outcome = SliceOutcome::default();
        for (index, slice) in slices.iter().enumerate() {
            if index != 2 {
                outcome.merge(decoder.decode_slice(&slice.data));
            }
        }
        assert!(outcome.refresh_needed && outcome.frame_complete);
        // The refresh repairs everything.
        encoder.request_refresh();
        slices.clear();
        let stats = encoder.encode_frame(&frame, 2, &mut slices);
        assert!(slices[0].is_keyframe && stats.lossless_tiles + stats.solid_tiles == 20 * 12);
        let outcome = decode_all(&mut decoder, &slices);
        assert!(!outcome.refresh_needed);
        assert!(bgr_equal(decoder.canvas(), &frame));
    }

    #[test]
    fn lost_frame_tail_is_detected_by_sync_and_next_frame() {
        let (w, h) = (256, 128);
        let mut frame = text_like(w, h, 1);
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, TileEncoderConfig::default(), 3);
        let mut decoder = TileDecoder::new();
        let mut slices = Vec::new();
        encoder.encode_frame(&frame, 1, &mut slices);
        decode_all(&mut decoder, &slices);
        // A one-slice frame is lost entirely; the keep-alive sync reveals it.
        frame[0] ^= 0xFF;
        slices.clear();
        encoder.encode_frame(&frame, 2, &mut slices);
        slices.clear();
        encoder.encode_idle(SYNC_INTERVAL_NS + 10, &mut slices);
        assert_eq!(slices.len(), 1, "sync slice after silence");
        let outcome = decode_all(&mut decoder, &slices);
        assert!(outcome.refresh_needed);
        // A sync with nothing missed is quiet.
        let mut quiet = TileDecoder::new();
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, TileEncoderConfig::default(), 4);
        slices.clear();
        encoder.encode_frame(&frame, 1, &mut slices);
        decode_all(&mut quiet, &slices);
        slices.clear();
        encoder.encode_idle(SYNC_INTERVAL_NS + 10, &mut slices);
        assert!(!decode_all(&mut quiet, &slices).refresh_needed);
    }

    #[test]
    fn replayed_old_slices_never_overwrite_newer_pixels() {
        let (w, h) = (128, 64);
        let old = text_like(w, h, 0);
        let new = text_like(w, h, 2);
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, TileEncoderConfig::default(), 5);
        let mut decoder = TileDecoder::new();
        let mut first = Vec::new();
        encoder.encode_frame(&old, 1, &mut first);
        decode_all(&mut decoder, &first);
        let mut second = Vec::new();
        encoder.encode_frame(&new, 2, &mut second);
        decode_all(&mut decoder, &second);
        let outcome = decode_all(&mut decoder, &first);
        assert!(!outcome.accepted);
        assert!(bgr_equal(decoder.canvas(), &new));
    }

    #[test]
    fn background_refresh_resends_every_tile_over_the_period() {
        let (w, h) = (640, 384);
        let frame = text_like(w, h, 4);
        let cfg = TileEncoderConfig {
            refresh_period_ns: 1_000_000_000,
            ..Default::default()
        };
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, cfg, 6);
        let mut slices = Vec::new();
        encoder.encode_frame(&frame, 1, &mut slices);
        let mut refreshed = 0;
        for step in 1..=11u64 {
            slices.clear();
            refreshed += encoder
                .encode_idle(step * 100_000_000, &mut slices)
                .refreshed_tiles;
        }
        assert!(
            (60..=60 + 6).contains(&refreshed),
            "refreshed {refreshed} of 60 tiles"
        );
    }

    #[test]
    fn malformed_slices_never_panic_or_corrupt_valid_tiles() {
        let (w, h) = (320, 192);
        let frame = photo_like(w, h, 8);
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, TileEncoderConfig::default(), 8);
        let mut slices = Vec::new();
        encoder.encode_frame(&frame, 1, &mut slices);
        let mut state = 0x1234_5678u32;
        let mut decoder = TileDecoder::new();
        decode_all(&mut decoder, &slices);
        let reference = decoder.canvas().to_vec();
        for round in 0..400 {
            let source = &slices[round % slices.len()].data;
            let mut corrupt = source.clone();
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            match round % 4 {
                0 => corrupt.truncate(state as usize % corrupt.len()),
                1 => {
                    let at = SLICE_HEADER_LEN + state as usize % (corrupt.len() - SLICE_HEADER_LEN);
                    corrupt[at] ^= (state >> 8) as u8 | 1;
                }
                2 => corrupt.extend_from_slice(&state.to_be_bytes()),
                _ => {
                    for byte in corrupt.iter_mut().skip(SLICE_HEADER_LEN) {
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        *byte = state as u8;
                    }
                }
            }
            // A new session per round bypasses continuity checks, so every
            // corruption reaches record parsing and pixel decoding.
            if corrupt.len() >= 14 {
                corrupt[10..14].copy_from_slice(&(1000 + round as u32).to_be_bytes());
            }
            let _ = decoder.decode_slice(&corrupt);
            assert_eq!(decoder.canvas().len(), reference.len());
        }
        // A fresh session repairs the canvas exactly as before.
        let mut repair = TileEncoder::new(
            0,
            w as u16,
            h as u16,
            TileEncoderConfig {
                lossless_only: true,
                ..Default::default()
            },
            99,
        );
        slices.clear();
        repair.encode_frame(&frame, 1, &mut slices);
        decode_all(&mut decoder, &slices);
        assert!(bgr_equal(decoder.canvas(), &frame));
    }

    #[test]
    fn new_session_and_size_change_are_handled() {
        let mut decoder = TileDecoder::new();
        for (session, (w, h)) in [(1u32, (200usize, 100usize)), (2, (200, 100)), (3, (96, 64))] {
            let frame = text_like(w, h, session as usize);
            let mut encoder =
                TileEncoder::new(0, w as u16, h as u16, TileEncoderConfig::default(), session);
            let mut slices = Vec::new();
            encoder.encode_frame(&frame, 1, &mut slices);
            let outcome = decode_all(&mut decoder, &slices);
            assert!(
                outcome.frame_complete && !outcome.refresh_needed,
                "{session}: {outcome:?}"
            );
            assert!(bgr_equal(decoder.canvas(), &frame));
        }
    }

    #[test]
    #[ignore = "performance measurement; run with --release --ignored --nocapture"]
    fn uhd_encode_decode_throughput() {
        let (w, h) = (3840usize, 2160usize);
        let frames = [
            text_like(w, h, 0),
            text_like(w, h, 3),
            photo_like(w, h, 1),
            photo_like(w, h, 2),
        ];
        let mut encoder = TileEncoder::new(0, w as u16, h as u16, TileEncoderConfig::default(), 1);
        let mut decoder = TileDecoder::new();
        for (label, index) in [
            ("text", 0),
            ("text scroll", 1),
            ("photo", 2),
            ("photo motion", 3),
        ] {
            let mut slices = Vec::new();
            let started = std::time::Instant::now();
            let stats =
                encoder.encode_frame(&frames[index], (index as u64 + 1) * 10_000_000, &mut slices);
            let encoded = started.elapsed();
            let started = std::time::Instant::now();
            let refs: Vec<&[u8]> = slices.iter().map(|s| s.data.as_slice()).collect();
            decoder.decode_batch(&refs);
            println!(
                "{label:13} {:6.2} MB {:4} slices encode {:6.1} ms decode {:6.1} ms {stats:?}",
                stats.bytes as f64 / 1e6,
                stats.slices,
                encoded.as_secs_f64() * 1e3,
                started.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}
