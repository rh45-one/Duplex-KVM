//! Screen-stream machinery shared by the Linux and Windows backends.
//!
//! Capture threads publish raw BGRx frames to a [`FrameSlot`] that always
//! holds only the newest frame. An encoder thread codes them on a
//! low-priority pool and pushes slices into a blocking [`VideoSink`], so a
//! saturated link lowers the frame rate rather than dropping encoded data.
//! On the receiving side, a decoder thread per monitor applies slices and
//! presents only complete frames (or the partial frame after a short silence).

use crate::shm::monotonic_now_ns;
use crate::{PalError, PeerVideoCaps, SinkError, VideoFrameConsumer, VideoSink};
use duplex_proto::{
    decode_band_into_bgrx, encode_native_bands, EncodedVideoFrame, PixelRect, TileDecoder,
    TileEncoder, TileEncoderConfig, DPXB_BAND_MAGIC, DPXC_BAND_MAGIC, MAX_VIDEO_BITRATE_KBPS,
};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const IDLE_TICK: Duration = Duration::from_millis(50);
const PARTIAL_FRAME_DELAY: Duration = Duration::from_millis(30);
const DECODE_QUEUE: usize = 512;
const LEGACY_PRESENT_INTERVAL: Duration = Duration::from_millis(16);
const REPORT_INTERVAL_NS: u64 = 10_000_000_000;

/// Shared low-priority pool for tile coding. Half the CPUs (at most eight)
/// code a full-motion 4K frame in ~30 ms; more threads mostly add scheduling
/// overhead. Below-normal priority keeps the daemon's heartbeat, input loop
/// and the user's applications responsive under heavy streams.
pub fn video_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = (thread::available_parallelism().map_or(4, |n| n.get()) / 2).clamp(2, 8);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("duplex-codec-{index}"))
            .start_handler(|_| lower_current_thread_priority())
            .build()
            .expect("cannot create the video codec thread pool")
    })
}

fn lower_current_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        // Linux applies PRIO_PROCESS with who=0 to the calling thread only.
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 5);
    }
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Encoder settings. Overrides: `DUPLEX_VIDEO_LOSSLESS=1` (never lossy),
/// `DUPLEX_VIDEO_JPEG_QUALITY=30..100`, `DUPLEX_VIDEO_CHROMA=444`.
pub fn encoder_config_from_env() -> TileEncoderConfig {
    let mut cfg = TileEncoderConfig::default();
    if let Some(quality) = std::env::var("DUPLEX_VIDEO_JPEG_QUALITY")
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
    {
        cfg.jpeg_quality = quality.clamp(30, 100);
    }
    if std::env::var("DUPLEX_VIDEO_CHROMA").is_ok_and(|v| v.trim() == "444") {
        cfg.jpeg_full_chroma = true;
    }
    if std::env::var("DUPLEX_VIDEO_LOSSLESS")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
    {
        cfg.lossless_only = true;
    }
    cfg
}

/// Capture frame-rate ceiling (`DUPLEX_VIDEO_MAX_FPS` overrides).
pub fn capture_fps_limit() -> u32 {
    std::env::var("DUPLEX_VIDEO_MAX_FPS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(60, |fps| fps.clamp(5, 144))
}

/// A value unlikely to repeat between encoder lifetimes.
pub fn session_id() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let mut z = monotonic_now_ns()
        ^ (u64::from(std::process::id()) << 32)
        ^ u64::from(COUNTER.fetch_add(0x9E37_79B9, Ordering::Relaxed));
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// Codec chosen for a sender from the peer's capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamCodec {
    Tiles = 0,
    /// Legacy DPXC bands for peers that predate DPXT.
    NativeBands = 1,
}

impl StreamCodec {
    /// Unknown peers get DPXT: current peers advertise it on connection.
    pub fn for_peer(caps: PeerVideoCaps) -> Self {
        if !caps.tiles && caps.native_lossless {
            Self::NativeBands
        } else {
            Self::Tiles
        }
    }

    fn from_u8(value: u8) -> Self {
        if value == Self::NativeBands as u8 {
            Self::NativeBands
        } else {
            Self::Tiles
        }
    }
}

pub enum SlotWait {
    Frame(Vec<u8>),
    Timeout,
    Closed,
}

#[derive(Default)]
struct SlotState {
    latest: Option<Vec<u8>>,
    spare: Vec<Vec<u8>>,
    closed: bool,
}

/// Capture → encoder hand-off holding only the newest frame. A busy encoder
/// skips stale frames instead of building latency.
#[derive(Default)]
pub struct FrameSlot {
    state: Mutex<SlotState>,
    ready: Condvar,
}

impl FrameSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A buffer of `len` bytes for the next capture (recycled when possible).
    pub fn buffer(&self, len: usize) -> Vec<u8> {
        let mut buffer = self
            .state
            .lock()
            .map(|mut state| state.spare.pop())
            .ok()
            .flatten()
            .unwrap_or_default();
        buffer.resize(len, 0);
        buffer
    }

    pub fn publish(&self, frame: Vec<u8>) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(stale) = state.latest.replace(frame) {
                if state.spare.len() < 2 {
                    state.spare.push(stale);
                }
            }
        }
        self.ready.notify_one();
    }

    pub fn recycle(&self, buffer: Vec<u8>) {
        if let Ok(mut state) = self.state.lock() {
            if state.spare.len() < 2 {
                state.spare.push(buffer);
            }
        }
    }

    /// Wakes the encoder permanently (capture ended).
    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        self.ready.notify_all();
    }

    pub fn wait(&self, timeout: Duration) -> SlotWait {
        let Ok(mut state) = self.state.lock() else {
            return SlotWait::Closed;
        };
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = state.latest.take() {
                return SlotWait::Frame(frame);
            }
            if state.closed {
                return SlotWait::Closed;
            }
            let now = Instant::now();
            if now >= deadline {
                return SlotWait::Timeout;
            }
            state = match self.ready.wait_timeout(state, deadline - now) {
                Ok((guard, _)) => guard,
                Err(_) => return SlotWait::Closed,
            };
        }
    }
}

/// Live controls for one encoder thread.
pub struct EncoderControl {
    stop: AtomicBool,
    refresh: AtomicBool,
    codec: AtomicU8,
    finished: AtomicBool,
}

impl EncoderControl {
    pub fn new(codec: StreamCodec) -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            refresh: AtomicBool::new(false),
            codec: AtomicU8::new(codec as u8),
            finished: AtomicBool::new(false),
        })
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn request_refresh(&self) {
        self.refresh.store(true, Ordering::Release);
    }

    pub fn set_codec(&self, codec: StreamCodec) {
        self.codec.store(codec as u8, Ordering::Release);
    }

    /// The encoder ended on its own (capture closed or link gone).
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderSpec {
    pub monitor: u8,
    pub width: u16,
    pub height: u16,
}

pub fn spawn_encoder(
    spec: EncoderSpec,
    slot: Arc<FrameSlot>,
    sink: Arc<dyn VideoSink>,
    control: Arc<EncoderControl>,
    cfg: TileEncoderConfig,
) -> Result<JoinHandle<()>, PalError> {
    thread::Builder::new()
        .name(format!("duplex-encode-{}", spec.monitor))
        .spawn(move || {
            encoder_loop(spec, &slot, sink.as_ref(), &control, cfg);
            control.finished.store(true, Ordering::Release);
        })
        .map_err(|e| PalError::VideoPipeline(format!("Cannot start video encoder: {e}")))
}

fn encoder_loop(
    spec: EncoderSpec,
    slot: &FrameSlot,
    sink: &dyn VideoSink,
    control: &EncoderControl,
    cfg: TileEncoderConfig,
) {
    let pool = video_pool();
    let mut tiles = TileEncoder::new(spec.monitor, spec.width, spec.height, cfg, session_id());
    let mut bands_previous = Vec::new();
    let (mut band_seq, mut band_cycle, mut last_band_refresh) = (0u32, 0u32, 0u64);
    let band_budget = MAX_VIDEO_BITRATE_KBPS as usize * 1000 / 8 / 30;
    let mut codec = StreamCodec::from_u8(control.codec.load(Ordering::Acquire));
    let mut out = Vec::new();
    let (mut frames, mut bytes, mut report_at) =
        (0u64, 0u64, monotonic_now_ns() + REPORT_INTERVAL_NS);
    info!(
        monitor = spec.monitor,
        width = spec.width,
        height = spec.height,
        ?codec,
        ?cfg,
        "Video encoder started"
    );
    while !control.stop.load(Ordering::Acquire) {
        let wait = slot.wait(IDLE_TICK);
        let now = monotonic_now_ns();
        let wanted = StreamCodec::from_u8(control.codec.load(Ordering::Acquire));
        if wanted != codec {
            info!(monitor = spec.monitor, from = ?codec, to = ?wanted, "Video codec changed for peer");
            codec = wanted;
            tiles.request_refresh();
            bands_previous.clear();
        }
        let refresh = control.refresh.swap(false, Ordering::AcqRel);
        match wait {
            SlotWait::Closed => break,
            SlotWait::Frame(frame) => {
                match codec {
                    StreamCodec::Tiles => {
                        if refresh {
                            tiles.request_refresh();
                        }
                        pool.install(|| tiles.encode_frame(&frame, now, &mut out));
                    }
                    StreamCodec::NativeBands => {
                        let force = refresh && now.saturating_sub(last_band_refresh) >= 500_000_000;
                        if force {
                            last_band_refresh = now;
                        } else if refresh {
                            control.request_refresh();
                        }
                        encode_native_bands(
                            spec.monitor,
                            spec.width,
                            spec.height,
                            &frame,
                            &mut bands_previous,
                            force,
                            band_cycle,
                            &mut band_seq,
                            now,
                            band_budget,
                            &mut out,
                        );
                        band_cycle = band_cycle.wrapping_add(1);
                    }
                }
                slot.recycle(frame);
            }
            SlotWait::Timeout => match codec {
                StreamCodec::Tiles => {
                    if refresh {
                        tiles.request_refresh();
                    }
                    pool.install(|| tiles.encode_idle(now, &mut out));
                }
                StreamCodec::NativeBands if refresh => control.request_refresh(),
                StreamCodec::NativeBands => {}
            },
        }
        if !out.is_empty() {
            frames += 1;
        }
        for frame in out.drain(..) {
            bytes += frame.data.len() as u64;
            if !deliver(sink, frame, control) {
                info!(
                    monitor = spec.monitor,
                    "Video link closed or stopping; encoder exits"
                );
                return;
            }
        }
        if now >= report_at {
            info!(
                monitor = spec.monitor,
                updates_per_s = frames as f64 * 1e9 / REPORT_INTERVAL_NS as f64,
                mbps = bytes as f64 * 8.0 / (REPORT_INTERVAL_NS as f64 / 1e9) / 1e6,
                "Video encoder throughput"
            );
            (frames, bytes, report_at) = (0, 0, now + REPORT_INTERVAL_NS);
        }
    }
}

/// Hands one unit to the sink, waiting while the link is saturated. Gives up
/// when the link closes or the encoder is told to stop, so teardown never
/// waits on a consumer that stopped draining.
fn deliver(sink: &dyn VideoSink, mut frame: EncodedVideoFrame, control: &EncoderControl) -> bool {
    loop {
        match sink.try_send(frame) {
            Ok(()) => return true,
            Err(SinkError::Closed) => return false,
            Err(SinkError::Full(returned)) => {
                if control.stop.load(Ordering::Acquire) {
                    return false;
                }
                frame = returned;
                thread::sleep(Duration::from_micros(500));
            }
        }
    }
}

/// Shows decoded frames (fullscreen window, viewer process, ...).
pub trait FramePresenter: Send {
    /// `canvas` is tightly packed BGRx (`width * 4` bytes per row) whose
    /// fourth byte is 0xFF. `dirty` lists the regions that changed.
    fn present(
        &mut self,
        width: u16,
        height: u16,
        canvas: &[u8],
        dirty: &[PixelRect],
    ) -> Result<(), PalError>;
}

/// Outcome of applying a batch of received units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeResult {
    pub present_now: bool,
    pub refresh_needed: bool,
}

/// Decodes DPXT tiles, plus legacy DPXC/DPXB bands from older peers.
pub struct StreamDecoder {
    tiles: TileDecoder,
    legacy_width: u16,
    legacy_height: u16,
    legacy: Vec<u8>,
    legacy_dirty: Option<(u32, u32)>,
    legacy_presented: Instant,
    showing_legacy: bool,
    whole: Vec<PixelRect>,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self {
            tiles: TileDecoder::new(),
            legacy_width: 0,
            legacy_height: 0,
            legacy: Vec::new(),
            legacy_dirty: None,
            legacy_presented: Instant::now(),
            showing_legacy: false,
            whole: Vec::new(),
        }
    }

    /// True when the unit ends a frame (present after decoding it).
    pub fn ends_frame(frame: &EncodedVideoFrame) -> bool {
        !TileDecoder::is_dpxt(&frame.data) || frame.data.get(5).is_some_and(|flags| flags & 1 != 0)
    }

    pub fn decode(&mut self, batch: &[EncodedVideoFrame]) -> DecodeResult {
        let mut result = DecodeResult::default();
        let slices: Vec<&[u8]> = batch
            .iter()
            .filter(|frame| TileDecoder::is_dpxt(&frame.data))
            .map(|frame| frame.data.as_slice())
            .collect();
        if !slices.is_empty() {
            let outcome = self.tiles.decode_batch(&slices);
            if outcome.accepted {
                self.showing_legacy = false;
            }
            result.refresh_needed |= outcome.refresh_needed;
            result.present_now |= outcome.frame_complete || outcome.resized;
        }
        for frame in batch
            .iter()
            .filter(|frame| !TileDecoder::is_dpxt(&frame.data))
        {
            let data = &frame.data;
            if data.len() < 12 || !(data[..4] == DPXB_BAND_MAGIC || data[..4] == DPXC_BAND_MAGIC) {
                continue;
            }
            let band_y = u32::from(u16::from_be_bytes([data[8], data[9]]));
            let band_h = u32::from(u16::from_be_bytes([data[10], data[11]]));
            let (mut width, mut height) = (self.legacy_width, self.legacy_height);
            if !decode_band_into_bgrx(data, &mut width, &mut height, &mut self.legacy) {
                result.refresh_needed = true;
                continue;
            }
            if (width, height) != (self.legacy_width, self.legacy_height) {
                (self.legacy_width, self.legacy_height) = (width, height);
                self.legacy_dirty = Some((0, u32::from(height)));
            }
            self.showing_legacy = true;
            // Captured pixels may carry an undefined fourth byte.
            let stride = usize::from(width) * 4;
            let rows = band_y as usize..(band_y + band_h).min(u32::from(height)) as usize;
            for pixel in self.legacy[rows.start * stride..rows.end * stride]
                .as_chunks_mut::<4>()
                .0
            {
                pixel[3] = 0xFF;
            }
            let (top, bottom) = self.legacy_dirty.unwrap_or((band_y, band_y + band_h));
            self.legacy_dirty = Some((top.min(band_y), bottom.max(band_y + band_h)));
            if band_y + band_h >= u32::from(height)
                || self.legacy_presented.elapsed() >= LEGACY_PRESENT_INTERVAL
            {
                result.present_now = true;
            }
        }
        result
    }

    pub fn has_pending(&self) -> bool {
        if self.showing_legacy {
            self.legacy_dirty.is_some()
        } else {
            self.tiles.has_dirty()
        }
    }

    /// The canvas to show and its changed regions, if anything changed.
    pub fn take_present(&mut self) -> Option<(u16, u16, &[u8], &[PixelRect])> {
        self.whole.clear();
        if self.showing_legacy {
            let (top, bottom) = self.legacy_dirty.take()?;
            self.legacy_presented = Instant::now();
            let bottom = bottom.min(u32::from(self.legacy_height));
            if bottom <= top {
                return None;
            }
            self.whole.push(PixelRect {
                x: 0,
                y: top,
                width: u32::from(self.legacy_width),
                height: bottom - top,
            });
            return Some((
                self.legacy_width,
                self.legacy_height,
                &self.legacy,
                &self.whole,
            ));
        }
        if !self.tiles.has_dirty() {
            return None;
        }
        self.tiles.take_dirty_rects(&mut self.whole);
        Some((
            self.tiles.width(),
            self.tiles.height(),
            self.tiles.canvas(),
            &self.whole,
        ))
    }
}

/// State shared by a viewer set's decoders and the backend.
#[derive(Default)]
pub struct ViewerShared {
    stop: AtomicBool,
    refresh: AtomicU32,
    failed: Mutex<Option<String>>,
}

impl ViewerShared {
    fn request_refresh(&self, monitor: u8) {
        self.refresh
            .fetch_or(1 << u32::from(monitor.min(31)), Ordering::AcqRel);
    }
}

/// Routes received units to per-monitor decoder queues without blocking.
struct ViewerInput {
    routes: Vec<(u8, SyncSender<EncodedVideoFrame>)>,
    shared: Arc<ViewerShared>,
}

impl VideoFrameConsumer for ViewerInput {
    fn submit(&self, frame: EncodedVideoFrame) {
        let monitor = frame.monitor_index;
        if let Some((_, route)) = self.routes.iter().find(|(index, _)| *index == monitor) {
            if let Err(TrySendError::Full(_)) = route.try_send(frame) {
                // Dropping a unit is loss: repair it with a refresh.
                self.shared.request_refresh(monitor);
            }
        }
    }
}

/// Running decoders for one viewing session.
pub struct ViewerSet {
    shared: Arc<ViewerShared>,
    decoders: Vec<JoinHandle<()>>,
}

impl ViewerSet {
    /// Starts one decoder thread per presenter and returns the consumer that
    /// the network receive thread feeds.
    pub fn spawn(
        presenters: Vec<(u8, Box<dyn FramePresenter>)>,
    ) -> Result<(Self, Arc<dyn VideoFrameConsumer>), PalError> {
        let shared = Arc::new(ViewerShared::default());
        let mut routes = Vec::new();
        let mut decoders = Vec::new();
        for (monitor, presenter) in presenters {
            let (tx, rx) = mpsc::sync_channel(DECODE_QUEUE);
            let decoder_shared = shared.clone();
            let handle = thread::Builder::new()
                .name(format!("duplex-decode-{monitor}"))
                .spawn(move || decoder_loop(monitor, rx, presenter, &decoder_shared));
            match handle {
                Ok(handle) => decoders.push(handle),
                Err(error) => {
                    let set = Self { shared, decoders };
                    set.stop();
                    return Err(PalError::VideoPipeline(format!(
                        "Cannot start video decoder: {error}"
                    )));
                }
            }
            routes.push((monitor, tx));
        }
        let consumer: Arc<dyn VideoFrameConsumer> = Arc::new(ViewerInput {
            routes,
            shared: shared.clone(),
        });
        Ok((Self { shared, decoders }, consumer))
    }

    pub fn take_refresh_requests(&self, out: &mut Vec<u8>) {
        let bits = self.shared.refresh.swap(0, Ordering::AcqRel);
        out.extend((0..32u8).filter(|monitor| bits & (1 << monitor) != 0));
    }

    /// The first presenter failure, if any.
    pub fn failure(&self) -> Option<String> {
        self.shared
            .failed
            .lock()
            .ok()
            .and_then(|failed| failed.clone())
    }

    pub fn stop(self) {
        self.shared.stop.store(true, Ordering::Release);
        for decoder in self.decoders {
            let _ = decoder.join();
        }
    }
}

fn decoder_loop(
    monitor: u8,
    rx: Receiver<EncodedVideoFrame>,
    mut presenter: Box<dyn FramePresenter>,
    shared: &ViewerShared,
) {
    let pool = video_pool();
    let mut decoder = StreamDecoder::new();
    let mut batch = Vec::new();
    while !shared.stop.load(Ordering::Acquire) {
        let first = match rx.recv_timeout(PARTIAL_FRAME_DELAY) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => {
                // A frame tail was lost or is late: show what has arrived.
                if decoder.has_pending()
                    && !present(monitor, &mut decoder, presenter.as_mut(), shared)
                {
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut done = StreamDecoder::ends_frame(&first);
        batch.push(first);
        while !done && batch.len() < DECODE_QUEUE {
            match rx.try_recv() {
                Ok(frame) => {
                    done = StreamDecoder::ends_frame(&frame);
                    batch.push(frame);
                }
                Err(_) => break,
            }
        }
        let result = pool.install(|| decoder.decode(&batch));
        batch.clear();
        if result.refresh_needed {
            shared.request_refresh(monitor);
        }
        if result.present_now && !present(monitor, &mut decoder, presenter.as_mut(), shared) {
            return;
        }
    }
}

fn present(
    monitor: u8,
    decoder: &mut StreamDecoder,
    presenter: &mut dyn FramePresenter,
    shared: &ViewerShared,
) -> bool {
    let Some((width, height, canvas, dirty)) = decoder.take_present() else {
        return true;
    };
    match presenter.present(width, height, canvas, dirty) {
        Ok(()) => true,
        Err(error) => {
            warn!(monitor, %error, "Video viewer stopped");
            if let Ok(mut failed) = shared.failed.lock() {
                failed.get_or_insert_with(|| error.to_string());
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duplex_proto::{TileEncoder, VideoCodec};

    type Shown = Arc<Mutex<Vec<(u16, u16, Vec<u8>, Vec<PixelRect>)>>>;

    struct Collect(Shown);

    impl FramePresenter for Collect {
        fn present(
            &mut self,
            width: u16,
            height: u16,
            canvas: &[u8],
            dirty: &[PixelRect],
        ) -> Result<(), PalError> {
            self.0
                .lock()
                .unwrap()
                .push((width, height, canvas.to_vec(), dirty.to_vec()));
            Ok(())
        }
    }

    struct ChannelSink(Mutex<SyncSender<EncodedVideoFrame>>);

    impl VideoSink for ChannelSink {
        fn try_send(&self, frame: EncodedVideoFrame) -> Result<(), SinkError> {
            match self.0.lock().unwrap().try_send(frame) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(frame)) => Err(SinkError::Full(frame)),
                Err(TrySendError::Disconnected(_)) => Err(SinkError::Closed),
            }
        }
    }

    fn pattern(width: usize, height: usize, shift: usize) -> Vec<u8> {
        let mut frame = vec![0u8; width * height * 4];
        for (i, pixel) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % width, i / width);
            let on = ((x + shift) / 9 + y / 17).is_multiple_of(4);
            pixel.copy_from_slice(&if on {
                [220, 220, 230, 0]
            } else {
                [25, 25, 30, 0]
            });
        }
        frame
    }

    #[test]
    fn slot_keeps_only_the_newest_frame_and_recycles() {
        let slot = FrameSlot::new();
        let mut first = slot.buffer(8);
        first[0] = 1;
        slot.publish(first);
        let mut second = slot.buffer(8);
        second[0] = 2;
        slot.publish(second);
        match slot.wait(Duration::from_millis(1)) {
            SlotWait::Frame(frame) => assert_eq!(frame[0], 2),
            _ => panic!("expected the newest frame"),
        }
        assert!(matches!(
            slot.wait(Duration::from_millis(1)),
            SlotWait::Timeout
        ));
        slot.close();
        assert!(matches!(
            slot.wait(Duration::from_secs(5)),
            SlotWait::Closed
        ));
    }

    #[test]
    fn encoder_to_viewer_pipeline_presents_exact_complete_frames() {
        let (w, h) = (700u16, 300u16);
        let slot = FrameSlot::new();
        let (tx, rx) = mpsc::sync_channel(64);
        let sink: Arc<dyn VideoSink> = Arc::new(ChannelSink(Mutex::new(tx)));
        let control = EncoderControl::new(StreamCodec::Tiles);
        let encoder = spawn_encoder(
            EncoderSpec {
                monitor: 1,
                width: w,
                height: h,
            },
            slot.clone(),
            sink,
            control.clone(),
            TileEncoderConfig::default(),
        )
        .unwrap();
        let shown = Arc::new(Mutex::new(Vec::new()));
        let (viewers, consumer) = ViewerSet::spawn(vec![(
            1,
            Box::new(Collect(shown.clone())) as Box<dyn FramePresenter>,
        )])
        .unwrap();
        let frames: Vec<Vec<u8>> = (0..3)
            .map(|shift| pattern(w.into(), h.into(), shift * 5))
            .collect();
        for frame in &frames {
            slot.publish(frame.clone());
            let started = Instant::now();
            loop {
                while let Ok(unit) = rx.try_recv() {
                    assert_eq!(unit.codec, VideoCodec::Tiles);
                    consumer.submit(unit);
                }
                let done = shown
                    .lock()
                    .unwrap()
                    .last()
                    .is_some_and(|(_, _, canvas, _)| {
                        let (shown, expected) =
                            (canvas.as_chunks::<4>().0, frame.as_chunks::<4>().0);
                        shown.iter().zip(expected).all(|(a, b)| a[..3] == b[..3])
                    });
                if done {
                    break;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "frame was not presented"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
        let presented = shown.lock().unwrap();
        assert!(presented.iter().all(|(width, height, canvas, dirty)| {
            (*width, *height) == (w, h)
                && !dirty.is_empty()
                && canvas.as_chunks::<4>().0.iter().all(|p| p[3] == 0xFF)
        }));
        drop(presented);
        let mut refresh = Vec::new();
        viewers.take_refresh_requests(&mut refresh);
        assert!(refresh.is_empty(), "no loss expected: {refresh:?}");
        control.stop();
        encoder.join().unwrap();
        viewers.stop();
    }

    #[test]
    fn full_decoder_queue_requests_a_refresh_instead_of_blocking() {
        struct Stall;
        impl FramePresenter for Stall {
            fn present(
                &mut self,
                _: u16,
                _: u16,
                _: &[u8],
                _: &[PixelRect],
            ) -> Result<(), PalError> {
                thread::sleep(Duration::from_millis(200));
                Ok(())
            }
        }
        let (viewers, consumer) =
            ViewerSet::spawn(vec![(0, Box::new(Stall) as Box<dyn FramePresenter>)]).unwrap();
        let frame = pattern(64, 64, 0);
        let mut encoder = TileEncoder::new(0, 64, 64, TileEncoderConfig::default(), 5);
        let mut units = Vec::new();
        encoder.encode_frame(&frame, 1, &mut units);
        let started = Instant::now();
        for _ in 0..DECODE_QUEUE * 2 {
            consumer.submit(units[0].clone());
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "submit must not block"
        );
        let mut refresh = Vec::new();
        viewers.take_refresh_requests(&mut refresh);
        assert_eq!(refresh, vec![0]);
        viewers.stop();
    }
}
