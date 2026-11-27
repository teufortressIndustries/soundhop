//! Single-capture shared GStreamer broadcast pipeline (`GstBroadcaster`) for Web Broadcast.
//!
//! Captures live system audio (or a synthetic sine wave in tests) once, normalizes to
//! `S16LE 48 kHz 2ch`, tees into two leaky branches (`opusenc -> appsink` and raw `appsink`),
//! and fans out zero-copy `Arc<BroadcastFrame>` payloads to WebRTC and WebSocket subscribers.

use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(feature = "gstreamer")]
use std::thread::{self, JoinHandle};

#[cfg(feature = "gstreamer")]
use gstreamer as gst;
#[cfg(feature = "gstreamer")]
use gstreamer::prelude::*;
#[cfg(feature = "gstreamer")]
use gstreamer_app as gst_app;

#[cfg(feature = "gstreamer")]
use crate::backend::windows::audio::ReconnectBackoff;
#[cfg(feature = "gstreamer")]
use crate::transport::gst_runtime::{ensure_gst_init, make_element};
#[cfg(feature = "gstreamer")]
use crate::transport::gst_sender::build_capture_source;
use crate::transport::wire::AudioParams;
use crate::transport::{CaptureEndpoint, TransportError, TransportResult};

/// Canonical sample rate for Web Broadcast frames (48 kHz).
pub const BROADCAST_SAMPLE_RATE: u32 = 48_000;
/// Canonical channel count for Web Broadcast frames (stereo).
pub const BROADCAST_CHANNELS: u8 = 2;
/// Canonical samples per channel in one 20 ms frame at 48 kHz.
pub const BROADCAST_FRAME_SAMPLES: u32 = 960;
/// Exact byte length of one interleaved 20 ms `S16LE` 48 kHz stereo PCM frame (`960 * 2 * 2`).
pub const BROADCAST_PCM_FRAME_BYTES: usize =
    (BROADCAST_FRAME_SAMPLES as usize) * (BROADCAST_CHANNELS as usize) * 2;
/// Bounded per-subscriber channel capacity (`32` frames = ~640 ms).
pub const SUBSCRIBER_CHANNEL_CAPACITY: usize = 32;
/// Number of consecutive full-channel drops before evicting a slow subscriber (~5 s @ 20 ms).
pub const SLOW_SUBSCRIBER_MAX_CONSECUTIVE_DROPS: u32 = 250;

#[cfg(feature = "gstreamer")]
const BUS_POLL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_mseconds(100);
#[cfg(feature = "gstreamer")]
const RESTART_INITIAL_BACKOFF_MS: u64 = 250;
#[cfg(feature = "gstreamer")]
const RESTART_MAX_BACKOFF_MS: u64 = 2_000;
#[cfg(feature = "gstreamer")]
const MAX_CONSECUTIVE_RESTARTS: u32 = 8;
#[cfg(feature = "gstreamer")]
const HEALTHY_RUN_RESET_SECS: u64 = 5;
#[cfg(feature = "gstreamer")]
const NORMAL_OPUS_PACKET_LOSS_PCT: i32 = 10;
#[cfg(feature = "gstreamer")]
const NORMAL_OPUS_FRAME_SIZE: &str = "20";
#[cfg(feature = "gstreamer")]
const POTATO_OPUS_BITRATE_BPS: i32 = 48_000;
#[cfg(feature = "gstreamer")]
const POTATO_OPUS_PACKET_LOSS_PCT: i32 = 25;
#[cfg(feature = "gstreamer")]
const POTATO_OPUS_FRAME_SIZE: &str = "40";
#[cfg(feature = "gstreamer")]
const POTATO_OPUS_FRAME_SAMPLES: u32 = BROADCAST_FRAME_SAMPLES * 2;

/// Immutable audio frame fanned out from [`GstBroadcaster`] to WebRTC and WebSocket subscribers.
#[derive(Debug, Clone)]
pub struct BroadcastFrame {
    pub seq: u64,
    pub pts_samples: u64,
    pub duration_samples: u32,
    pub captured_at: Instant,
    pub data: Arc<[u8]>,
}

/// Unique identifier for a registered broadcast subscriber.
pub type SubscriberId = u64;

/// Optional non-blocking wakeup callback invoked whenever a frame is queued for a subscriber
/// (for example, sending a 1-byte loopback UDP poke to `web-rtc-mux`).
pub type WakeupHook = Arc<dyn Fn() + Send + Sync>;

/// Create a non-blocking loopback UDP poke wakeup hook targeting `target`.
pub fn udp_poke_hook(target: SocketAddr) -> TransportResult<WakeupHook> {
    let bind_addr = if target.is_ipv6() {
        "[::1]:0"
    } else {
        "127.0.0.1:0"
    };
    let sock = UdpSocket::bind(bind_addr)?;
    sock.set_nonblocking(true)?;
    Ok(Arc::new(move || {
        let _ = sock.send_to(&[1u8], target);
    }))
}

/// Live atomic delivery and drop counters for a single broadcast subscriber.
#[derive(Debug, Default)]
pub struct SubscriberStats {
    pub delivered_frames: AtomicU64,
    pub dropped_frames: AtomicU32,
    pub consecutive_drops: AtomicU32,
    pub evicted: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriberKind {
    Opus,
    Pcm,
}

struct SubscriberEntry {
    id: SubscriberId,
    tx: SyncSender<Arc<BroadcastFrame>>,
    stats: Arc<SubscriberStats>,
    wakeup: Option<WakeupHook>,
}

/// Thread-safe registry of Opus and PCM broadcast subscribers with atomic listener counts
/// and non-blocking slow-subscriber eviction.
#[derive(Debug)]
pub struct SubscriberRegistry {
    next_id: AtomicU64,
    opus_sub_count: AtomicUsize,
    pcm_sub_count: AtomicUsize,
    opus_subs: Mutex<Vec<SubscriberEntry>>,
    pcm_subs: Mutex<Vec<SubscriberEntry>>,
    dropped_frames_total: AtomicU64,
    evicted_subscribers_total: AtomicU64,
}

impl std::fmt::Debug for SubscriberEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscriberEntry")
            .field("id", &self.id)
            .field("stats", &self.stats)
            .field("has_wakeup", &self.wakeup.is_some())
            .finish()
    }
}

impl Default for SubscriberRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriberRegistry {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            opus_sub_count: AtomicUsize::new(0),
            pcm_sub_count: AtomicUsize::new(0),
            opus_subs: Mutex::new(Vec::new()),
            pcm_subs: Mutex::new(Vec::new()),
            dropped_frames_total: AtomicU64::new(0),
            evicted_subscribers_total: AtomicU64::new(0),
        }
    }

    pub fn opus_count(&self) -> usize {
        self.opus_sub_count.load(Ordering::Acquire)
    }

    pub fn pcm_count(&self) -> usize {
        self.pcm_sub_count.load(Ordering::Acquire)
    }

    pub fn total_count(&self) -> usize {
        self.opus_count().saturating_add(self.pcm_count())
    }

    pub fn dropped_frames_total(&self) -> u64 {
        self.dropped_frames_total.load(Ordering::Relaxed)
    }

    pub fn evicted_subscribers_total(&self) -> u64 {
        self.evicted_subscribers_total.load(Ordering::Relaxed)
    }

    /// Register a bounded Opus subscriber channel (`capacity = 32`).
    pub fn subscribe_opus(self: &Arc<Self>, wakeup: Option<WakeupHook>) -> BroadcastSubscription {
        self.subscribe_kind(SubscriberKind::Opus, wakeup)
    }

    /// Register a bounded PCM16LE subscriber channel (`capacity = 32`).
    pub fn subscribe_pcm(self: &Arc<Self>, wakeup: Option<WakeupHook>) -> BroadcastSubscription {
        self.subscribe_kind(SubscriberKind::Pcm, wakeup)
    }

    fn subscribe_kind(
        self: &Arc<Self>,
        kind: SubscriberKind,
        wakeup: Option<WakeupHook>,
    ) -> BroadcastSubscription {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_CHANNEL_CAPACITY);
        let stats = Arc::new(SubscriberStats::default());
        let entry = SubscriberEntry {
            id,
            tx,
            stats: Arc::clone(&stats),
            wakeup,
        };
        let (list, count) = self.list_and_count(kind);
        let mut guard = list.lock().unwrap_or_else(|e| e.into_inner());
        guard.push(entry);
        count.store(guard.len(), Ordering::Release);
        drop(guard);

        BroadcastSubscription {
            id,
            rx,
            stats,
            kind,
            registry: Some(Arc::clone(self)),
        }
    }

    /// Unsubscribe `id` if present. Returns `true` if the subscriber was removed.
    pub fn unsubscribe(&self, id: SubscriberId) -> bool {
        self.remove_from_kind(SubscriberKind::Opus, id)
            || self.remove_from_kind(SubscriberKind::Pcm, id)
    }

    fn remove_from_kind(&self, kind: SubscriberKind, id: SubscriberId) -> bool {
        let (list, count) = self.list_and_count(kind);
        let mut guard = list.lock().unwrap_or_else(|e| e.into_inner());
        let before = guard.len();
        guard.retain(|entry| entry.id != id);
        let removed = guard.len() < before;
        if removed {
            count.store(guard.len(), Ordering::Release);
        }
        removed
    }

    /// Remove all Opus and PCM subscribers and invoke any registered wakeup hooks so
    /// waiting consumers wake immediately on shutdown.
    pub fn clear(&self) {
        let mut wakeups = Vec::new();
        for kind in [SubscriberKind::Opus, SubscriberKind::Pcm] {
            let (list, count) = self.list_and_count(kind);
            let mut guard = list.lock().unwrap_or_else(|e| e.into_inner());
            for entry in guard.drain(..) {
                if let Some(hook) = entry.wakeup {
                    wakeups.push(hook);
                }
            }
            count.store(0, Ordering::Release);
        }
        for hook in wakeups {
            hook();
        }
    }

    /// Non-blocking fan-out of an Opus [`BroadcastFrame`] to all Opus subscribers.
    pub fn dispatch_opus(&self, frame: Arc<BroadcastFrame>, wakeup_scratch: &mut Vec<WakeupHook>) {
        self.dispatch_kind(SubscriberKind::Opus, frame, wakeup_scratch);
    }

    /// Non-blocking fan-out of a PCM [`BroadcastFrame`] to all PCM subscribers.
    pub fn dispatch_pcm(&self, frame: Arc<BroadcastFrame>, wakeup_scratch: &mut Vec<WakeupHook>) {
        self.dispatch_kind(SubscriberKind::Pcm, frame, wakeup_scratch);
    }

    fn dispatch_kind(
        &self,
        kind: SubscriberKind,
        frame: Arc<BroadcastFrame>,
        wakeup_scratch: &mut Vec<WakeupHook>,
    ) {
        wakeup_scratch.clear();
        let (list, count) = self.list_and_count(kind);
        {
            let mut guard = list.lock().unwrap_or_else(|e| e.into_inner());
            let mut idx = 0;
            while idx < guard.len() {
                let mut remove_entry = false;
                match guard[idx].tx.try_send(Arc::clone(&frame)) {
                    Ok(()) => {
                        guard[idx]
                            .stats
                            .delivered_frames
                            .fetch_add(1, Ordering::Relaxed);
                        guard[idx]
                            .stats
                            .consecutive_drops
                            .store(0, Ordering::Release);
                        if let Some(ref hook) = guard[idx].wakeup {
                            wakeup_scratch.push(Arc::clone(hook));
                        }
                    }
                    Err(TrySendError::Full(_)) => {
                        self.dropped_frames_total.fetch_add(1, Ordering::Relaxed);
                        guard[idx]
                            .stats
                            .dropped_frames
                            .fetch_add(1, Ordering::Relaxed);
                        let prev = guard[idx]
                            .stats
                            .consecutive_drops
                            .fetch_add(1, Ordering::AcqRel);
                        if prev + 1 >= SLOW_SUBSCRIBER_MAX_CONSECUTIVE_DROPS {
                            guard[idx].stats.evicted.store(true, Ordering::Release);
                            self.evicted_subscribers_total
                                .fetch_add(1, Ordering::Relaxed);
                            if let Some(ref hook) = guard[idx].wakeup {
                                wakeup_scratch.push(Arc::clone(hook));
                            }
                            remove_entry = true;
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        remove_entry = true;
                    }
                }
                if remove_entry {
                    guard.swap_remove(idx);
                } else {
                    idx += 1;
                }
            }
            count.store(guard.len(), Ordering::Release);
        }

        // Invoke wakeup hooks outside the registry mutex to prevent lock-order inversions.
        for hook in wakeup_scratch.drain(..) {
            hook();
        }
    }

    fn list_and_count(&self, kind: SubscriberKind) -> (&Mutex<Vec<SubscriberEntry>>, &AtomicUsize) {
        match kind {
            SubscriberKind::Opus => (&self.opus_subs, &self.opus_sub_count),
            SubscriberKind::Pcm => (&self.pcm_subs, &self.pcm_sub_count),
        }
    }
}

/// Handle to a registered Opus or PCM broadcast subscriber channel.
///
/// Dropping this handle automatically unsubscribes it from the broadcaster registry.
pub struct BroadcastSubscription {
    id: SubscriberId,
    rx: Receiver<Arc<BroadcastFrame>>,
    stats: Arc<SubscriberStats>,
    kind: SubscriberKind,
    registry: Option<Arc<SubscriberRegistry>>,
}

impl BroadcastSubscription {
    pub fn id(&self) -> SubscriberId {
        self.id
    }

    pub fn rx(&self) -> &Receiver<Arc<BroadcastFrame>> {
        &self.rx
    }

    pub fn stats(&self) -> &Arc<SubscriberStats> {
        &self.stats
    }

    pub fn is_evicted(&self) -> bool {
        self.stats.evicted.load(Ordering::Acquire)
    }

    pub fn try_recv(&self) -> Result<Arc<BroadcastFrame>, TryRecvError> {
        self.rx.try_recv()
    }

    pub fn recv(&self) -> Result<Arc<BroadcastFrame>, mpsc::RecvError> {
        self.rx.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Arc<BroadcastFrame>, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Explicitly unsubscribe and drop the channel receiver.
    pub fn unsubscribe(mut self) {
        if let Some(reg) = self.registry.take() {
            reg.remove_from_kind(self.kind, self.id);
        }
    }
}

impl Drop for BroadcastSubscription {
    fn drop(&mut self) {
        if let Some(reg) = self.registry.take() {
            reg.remove_from_kind(self.kind, self.id);
        }
    }
}

#[cfg(feature = "gstreamer")]
#[derive(Clone)]
struct PipelineCounters {
    registry: Arc<SubscriberRegistry>,
    opus_seq: Arc<AtomicU64>,
    pcm_seq: Arc<AtomicU64>,
    opus_frames_emitted: Arc<AtomicU64>,
    pcm_frames_emitted: Arc<AtomicU64>,
    opus_frames_skipped: Arc<AtomicU64>,
    pcm_frames_skipped: Arc<AtomicU64>,
    opus_frame_samples: Arc<AtomicU32>,
}

/// Single shared live GStreamer capture + Opus/PCM fan-out pipeline.
#[cfg(feature = "gstreamer")]
pub struct GstBroadcaster {
    registry: Arc<SubscriberRegistry>,
    stop_flag: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    potato_mode: Arc<AtomicBool>,
    base_bitrate_bps: i32,
    restart_count: Arc<AtomicU64>,
    counters: PipelineCounters,
    last_error: Arc<Mutex<Option<String>>>,
    active_bus: Arc<Mutex<Option<gst::Bus>>>,
    active_opusenc: Arc<Mutex<Option<gst::Element>>>,
    bus_thread: Mutex<Option<JoinHandle<()>>>,
}

#[cfg(feature = "gstreamer")]
impl GstBroadcaster {
    /// Build and start a live [`GstBroadcaster`] capture pipeline for `endpoint`.
    pub fn start(endpoint: CaptureEndpoint, audio_params: AudioParams) -> TransportResult<Self> {
        Self::start_with_registry(endpoint, audio_params, Arc::new(SubscriberRegistry::new()))
    }

    /// Build and start a live [`GstBroadcaster`] bound to an existing [`SubscriberRegistry`].
    pub fn start_with_registry(
        endpoint: CaptureEndpoint,
        audio_params: AudioParams,
        registry: Arc<SubscriberRegistry>,
    ) -> TransportResult<Self> {
        ensure_gst_init()?;

        let normalized_params = AudioParams {
            codec: audio_params.codec,
            sample_rate: BROADCAST_SAMPLE_RATE,
            channels: BROADCAST_CHANNELS,
            frame_ms: 20,
            bitrate_kbps: audio_params.bitrate_kbps,
            jitter_ms: audio_params.jitter_ms,
        };
        let base_bitrate_bps =
            (i32::from(normalized_params.bitrate_kbps) * 1000).clamp(16_000, 510_000);
        let potato_mode = Arc::new(AtomicBool::new(false));

        let counters = PipelineCounters {
            registry: Arc::clone(&registry),
            opus_seq: Arc::new(AtomicU64::new(0)),
            pcm_seq: Arc::new(AtomicU64::new(0)),
            opus_frames_emitted: Arc::new(AtomicU64::new(0)),
            pcm_frames_emitted: Arc::new(AtomicU64::new(0)),
            opus_frames_skipped: Arc::new(AtomicU64::new(0)),
            pcm_frames_skipped: Arc::new(AtomicU64::new(0)),
            opus_frame_samples: Arc::new(AtomicU32::new(BROADCAST_FRAME_SAMPLES)),
        };

        let (initial_pipeline, initial_opusenc) = start_broadcaster_pipeline(
            &endpoint,
            &normalized_params,
            &counters,
            potato_mode.load(Ordering::Acquire),
        )?;
        let initial_bus = initial_pipeline.bus();

        let stop_flag = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let restart_count = Arc::new(AtomicU64::new(0));
        let last_error = Arc::new(Mutex::new(None));
        let active_bus = Arc::new(Mutex::new(initial_bus));
        let active_opusenc = Arc::new(Mutex::new(Some(initial_opusenc)));

        let bus_thread = spawn_bus_watchdog(
            initial_pipeline,
            endpoint,
            normalized_params,
            counters.clone(),
            Arc::clone(&stop_flag),
            Arc::clone(&healthy),
            Arc::clone(&potato_mode),
            Arc::clone(&restart_count),
            Arc::clone(&last_error),
            Arc::clone(&active_bus),
            Arc::clone(&active_opusenc),
        )?;

        Ok(Self {
            registry,
            stop_flag,
            healthy,
            potato_mode,
            base_bitrate_bps,
            restart_count,
            counters,
            last_error,
            active_bus,
            active_opusenc,
            bus_thread: Mutex::new(Some(bus_thread)),
        })
    }

    pub fn subscribe_opus(&self, wakeup: Option<WakeupHook>) -> BroadcastSubscription {
        self.registry.subscribe_opus(wakeup)
    }

    pub fn subscribe_pcm(&self, wakeup: Option<WakeupHook>) -> BroadcastSubscription {
        self.registry.subscribe_pcm(wakeup)
    }

    pub fn unsubscribe(&self, id: SubscriberId) -> bool {
        self.registry.unsubscribe(id)
    }

    pub fn registry(&self) -> &Arc<SubscriberRegistry> {
        &self.registry
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn restart_count(&self) -> u64 {
        self.restart_count.load(Ordering::Acquire)
    }

    pub fn opus_subscriber_count(&self) -> usize {
        self.registry.opus_count()
    }

    pub fn pcm_subscriber_count(&self) -> usize {
        self.registry.pcm_count()
    }

    pub fn subscriber_count(&self) -> usize {
        self.registry.total_count()
    }

    pub fn opus_frames_emitted(&self) -> u64 {
        self.counters.opus_frames_emitted.load(Ordering::Relaxed)
    }

    pub fn pcm_frames_emitted(&self) -> u64 {
        self.counters.pcm_frames_emitted.load(Ordering::Relaxed)
    }

    pub fn opus_frames_skipped_no_subs(&self) -> u64 {
        self.counters.opus_frames_skipped.load(Ordering::Relaxed)
    }

    pub fn pcm_frames_skipped_no_subs(&self) -> u64 {
        self.counters.pcm_frames_skipped.load(Ordering::Relaxed)
    }

    pub fn dropped_frames(&self) -> u64 {
        self.registry.dropped_frames_total()
    }

    pub fn evicted_subscribers(&self) -> u64 {
        self.registry.evicted_subscribers_total()
    }

    /// Enable or disable Potato Opus tuning (`48 kbps`, `packet-loss-percentage=25`, `frame-size=40`).
    pub fn set_potato_mode(&self, enabled: bool) {
        self.potato_mode.store(enabled, Ordering::Release);
        if let Some(ref opusenc) = *self
            .active_opusenc
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            apply_opus_tuning(
                opusenc,
                self.base_bitrate_bps,
                enabled,
                &self.counters.opus_frame_samples,
            );
        }
    }

    /// Whether Potato Opus tuning is currently enabled.
    pub fn is_potato_mode(&self) -> bool {
        self.potato_mode.load(Ordering::Acquire)
    }

    /// Deadlock-free stop: mark stopped, clear subscribers, flush bus, and join watchdog thread.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        self.healthy.store(false, Ordering::Release);
        self.registry.clear();

        if let Some(ref bus) = *self.active_bus.lock().unwrap_or_else(|e| e.into_inner()) {
            bus.set_flushing(true);
        }

        if let Some(handle) = self
            .bus_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }
}

#[cfg(feature = "gstreamer")]
impl Drop for GstBroadcaster {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(feature = "gstreamer")]
fn teardown_pipeline(pipeline: &gst::Pipeline) {
    if let Some(bus) = pipeline.bus() {
        bus.set_flushing(true);
    }
    let _ = pipeline.set_state(gst::State::Null);
    let _ = pipeline.state(Some(gst::ClockTime::from_mseconds(500)));
}

#[cfg(feature = "gstreamer")]
fn apply_opus_tuning(
    opusenc: &gst::Element,
    base_bitrate_bps: i32,
    potato: bool,
    opus_frame_samples: &AtomicU32,
) {
    if potato {
        opusenc.set_property("bitrate", POTATO_OPUS_BITRATE_BPS);
        opusenc.set_property("packet-loss-percentage", POTATO_OPUS_PACKET_LOSS_PCT);
        opusenc.set_property_from_str("frame-size", POTATO_OPUS_FRAME_SIZE);
        opus_frame_samples.store(POTATO_OPUS_FRAME_SAMPLES, Ordering::Release);
    } else {
        opusenc.set_property("bitrate", base_bitrate_bps);
        opusenc.set_property("packet-loss-percentage", NORMAL_OPUS_PACKET_LOSS_PCT);
        opusenc.set_property_from_str("frame-size", NORMAL_OPUS_FRAME_SIZE);
        opus_frame_samples.store(BROADCAST_FRAME_SAMPLES, Ordering::Release);
    }
}

#[cfg(feature = "gstreamer")]
fn start_broadcaster_pipeline(
    endpoint: &CaptureEndpoint,
    audio_params: &AudioParams,
    counters: &PipelineCounters,
    potato: bool,
) -> TransportResult<(gst::Pipeline, gst::Element)> {
    let pipeline = gst::Pipeline::new();
    let (source, _netsim_cfg) = build_capture_source(endpoint, audio_params)?;
    let audioconvert = make_element("audioconvert", None)?;
    let audioresample = make_element("audioresample", None)?;

    let raw_caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", BROADCAST_SAMPLE_RATE as i32)
        .field("channels", i32::from(BROADCAST_CHANNELS))
        .field("layout", "interleaved")
        .build();
    let capsfilter = make_element("capsfilter", None)?;
    capsfilter.set_property("caps", &raw_caps);

    let tee = make_element("tee", Some("fanout"))?;
    tee.set_property("allow-not-linked", true);

    // Opus branch: leaky queue -> opusenc (20ms) -> appsink.
    // A deeper queue absorbs short CPU spikes without dropping encoded frames;
    // each dropped frame would otherwise force a gap in the RTP timeline.
    let opus_queue = make_element("queue", Some("opus_queue"))?;
    opus_queue.set_property_from_str("leaky", "downstream");
    opus_queue.set_property("max-size-buffers", 16u32);
    opus_queue.set_property("max-size-bytes", 0u32);
    opus_queue.set_property("max-size-time", 0u64);

    let opusenc = make_element("opusenc", None)?;
    opusenc.set_property_from_str("audio-type", "generic");
    opusenc.set_property_from_str("bandwidth", "fullband");
    opusenc.set_property_from_str("bitrate-type", "constrained-vbr");
    // Complexity 8 is close to max quality but notably cheaper, reducing the
    // chance of encoder overruns that starve the downstream appsink.
    opusenc.set_property("complexity", 8i32);
    opusenc.set_property("dtx", false);
    opusenc.set_property("inband-fec", true);
    let bitrate_bps = (i32::from(audio_params.bitrate_kbps) * 1000).clamp(16_000, 510_000);
    apply_opus_tuning(&opusenc, bitrate_bps, potato, &counters.opus_frame_samples);

    let opus_sink_elem = make_element("appsink", Some("opus_sink"))?;
    let opus_sink = opus_sink_elem
        .downcast::<gst_app::AppSink>()
        .map_err(|_| TransportError::Pipeline("failed downcasting opus_sink".into()))?;
    opus_sink.set_sync(false);
    opus_sink.set_drop(true);
    opus_sink.set_max_buffers(4);

    // PCM branch: leaky queue -> appsink
    let pcm_queue = make_element("queue", Some("pcm_queue"))?;
    pcm_queue.set_property_from_str("leaky", "downstream");
    pcm_queue.set_property("max-size-buffers", 4u32);
    pcm_queue.set_property("max-size-bytes", 0u32);
    pcm_queue.set_property("max-size-time", 0u64);

    let pcm_sink_elem = make_element("appsink", Some("pcm_sink"))?;
    let pcm_sink = pcm_sink_elem
        .downcast::<gst_app::AppSink>()
        .map_err(|_| TransportError::Pipeline("failed downcasting pcm_sink".into()))?;
    pcm_sink.set_sync(false);
    pcm_sink.set_drop(true);
    pcm_sink.set_max_buffers(4);
    pcm_sink.set_caps(Some(&raw_caps));

    // Attach Opus appsink callback (lazy map_readable gated on atomic subscriber count).
    {
        let reg = Arc::clone(&counters.registry);
        let seq_counter = Arc::clone(&counters.opus_seq);
        let emitted_counter = Arc::clone(&counters.opus_frames_emitted);
        let skipped_counter = Arc::clone(&counters.opus_frames_skipped);
        let frame_samples_atomic = Arc::clone(&counters.opus_frame_samples);
        let mut pts_samples: u64 = 0;
        let mut wakeup_scratch: Vec<WakeupHook> = Vec::with_capacity(2);

        opus_sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let frame_samples = frame_samples_atomic.load(Ordering::Acquire);
                    if reg.opus_count() == 0 {
                        skipped_counter.fetch_add(1, Ordering::Relaxed);
                        pts_samples = pts_samples.saturating_add(u64::from(frame_samples));
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let bytes = map.as_slice();
                    if bytes.is_empty() {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    let data: Arc<[u8]> = Arc::from(bytes);
                    let seq = seq_counter.fetch_add(1, Ordering::Relaxed);
                    let frame_pts = pts_samples;
                    pts_samples = pts_samples.saturating_add(u64::from(frame_samples));
                    let frame = Arc::new(BroadcastFrame {
                        seq,
                        pts_samples: frame_pts,
                        duration_samples: frame_samples,
                        captured_at: Instant::now(),
                        data,
                    });
                    emitted_counter.fetch_add(1, Ordering::Relaxed);
                    reg.dispatch_opus(frame, &mut wakeup_scratch);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    // Attach PCM appsink callback (lazy map_readable gated on atomic subscriber count,
    // framing into exact 3840-byte 20ms S16LE chunks).
    {
        let reg = Arc::clone(&counters.registry);
        let seq_counter = Arc::clone(&counters.pcm_seq);
        let emitted_counter = Arc::clone(&counters.pcm_frames_emitted);
        let skipped_counter = Arc::clone(&counters.pcm_frames_skipped);
        let mut pts_samples: u64 = 0;
        let mut pcm_acc: Vec<u8> = Vec::new();
        let mut wakeup_scratch: Vec<WakeupHook> = Vec::with_capacity(8);

        pcm_sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if reg.pcm_count() == 0 {
                        if !pcm_acc.is_empty() {
                            pcm_acc.clear();
                        }
                        skipped_counter.fetch_add(1, Ordering::Relaxed);
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let bytes = map.as_slice();
                    if bytes.is_empty() {
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    let captured_at = Instant::now();
                    if pcm_acc.is_empty() && bytes.len() == BROADCAST_PCM_FRAME_BYTES {
                        let frame = build_pcm_frame(
                            &seq_counter,
                            &mut pts_samples,
                            captured_at,
                            Arc::from(bytes),
                        );
                        emitted_counter.fetch_add(1, Ordering::Relaxed);
                        reg.dispatch_pcm(frame, &mut wakeup_scratch);
                    } else if pcm_acc.is_empty() && bytes.len() > BROADCAST_PCM_FRAME_BYTES {
                        let (chunks, rem) = bytes.as_chunks::<BROADCAST_PCM_FRAME_BYTES>();
                        for chunk in chunks {
                            let frame = build_pcm_frame(
                                &seq_counter,
                                &mut pts_samples,
                                captured_at,
                                Arc::from(chunk.as_slice()),
                            );
                            emitted_counter.fetch_add(1, Ordering::Relaxed);
                            reg.dispatch_pcm(frame, &mut wakeup_scratch);
                        }
                        if !rem.is_empty() {
                            pcm_acc.extend_from_slice(rem);
                        }
                    } else {
                        pcm_acc.extend_from_slice(bytes);
                        while pcm_acc.len() >= BROADCAST_PCM_FRAME_BYTES {
                            let data: Arc<[u8]> = Arc::from(&pcm_acc[..BROADCAST_PCM_FRAME_BYTES]);
                            pcm_acc.drain(..BROADCAST_PCM_FRAME_BYTES);
                            let frame =
                                build_pcm_frame(&seq_counter, &mut pts_samples, captured_at, data);
                            emitted_counter.fetch_add(1, Ordering::Relaxed);
                            reg.dispatch_pcm(frame, &mut wakeup_scratch);
                        }
                    }

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    let opus_sink_up: gst::Element = opus_sink.upcast();
    let pcm_sink_up: gst::Element = pcm_sink.upcast();

    pipeline
        .add_many([
            &source,
            &audioconvert,
            &audioresample,
            &capsfilter,
            &tee,
            &opus_queue,
            &opusenc,
            &opus_sink_up,
            &pcm_queue,
            &pcm_sink_up,
        ])
        .map_err(|e| {
            TransportError::Pipeline(format!("failed adding broadcaster elements: {e}"))
        })?;

    gst::Element::link_many([&source, &audioconvert, &audioresample, &capsfilter, &tee]).map_err(
        |e| TransportError::Pipeline(format!("failed linking broadcaster capture chain: {e}")),
    )?;

    gst::Element::link_many([&opus_queue, &opusenc, &opus_sink_up]).map_err(|e| {
        TransportError::Pipeline(format!("failed linking broadcaster Opus branch: {e}"))
    })?;
    pcm_queue.link(&pcm_sink_up).map_err(|e| {
        TransportError::Pipeline(format!("failed linking broadcaster PCM branch: {e}"))
    })?;

    let tee_opus_pad = tee
        .request_pad_simple("src_%u")
        .ok_or_else(|| TransportError::Pipeline("tee did not provide src pad for Opus".into()))?;
    let opus_q_sink = opus_queue
        .static_pad("sink")
        .ok_or_else(|| TransportError::Pipeline("opus_queue missing sink pad".into()))?;
    tee_opus_pad.link(&opus_q_sink).map_err(|e| {
        TransportError::Pipeline(format!("failed linking tee to opus_queue: {e:?}"))
    })?;

    let tee_pcm_pad = tee
        .request_pad_simple("src_%u")
        .ok_or_else(|| TransportError::Pipeline("tee did not provide src pad for PCM".into()))?;
    let pcm_q_sink = pcm_queue
        .static_pad("sink")
        .ok_or_else(|| TransportError::Pipeline("pcm_queue missing sink pad".into()))?;
    tee_pcm_pad
        .link(&pcm_q_sink)
        .map_err(|e| TransportError::Pipeline(format!("failed linking tee to pcm_queue: {e:?}")))?;

    if let Err(err) = pipeline.set_state(gst::State::Playing) {
        teardown_pipeline(&pipeline);
        return Err(TransportError::Pipeline(format!(
            "failed starting GStreamer broadcaster pipeline: {err:?}"
        )));
    }

    Ok((pipeline, opusenc))
}

#[cfg(feature = "gstreamer")]
#[inline]
fn build_pcm_frame(
    seq_counter: &AtomicU64,
    pts_samples: &mut u64,
    captured_at: Instant,
    data: Arc<[u8]>,
) -> Arc<BroadcastFrame> {
    let seq = seq_counter.fetch_add(1, Ordering::Relaxed);
    let frame_pts = *pts_samples;
    *pts_samples = pts_samples.saturating_add(u64::from(BROADCAST_FRAME_SAMPLES));
    Arc::new(BroadcastFrame {
        seq,
        pts_samples: frame_pts,
        duration_samples: BROADCAST_FRAME_SAMPLES,
        captured_at,
        data,
    })
}

#[cfg(feature = "gstreamer")]
#[allow(clippy::too_many_arguments)]
fn spawn_bus_watchdog(
    mut pipeline: gst::Pipeline,
    endpoint: CaptureEndpoint,
    audio_params: AudioParams,
    counters: PipelineCounters,
    stop_flag: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    potato_mode: Arc<AtomicBool>,
    restart_count: Arc<AtomicU64>,
    last_error: Arc<Mutex<Option<String>>>,
    active_bus: Arc<Mutex<Option<gst::Bus>>>,
    active_opusenc: Arc<Mutex<Option<gst::Element>>>,
) -> TransportResult<JoinHandle<()>> {
    let cleanup_pipeline = pipeline.clone();
    let cleanup_bus = Arc::clone(&active_bus);
    thread::Builder::new()
        .name("gst-broadcaster-bus".into())
        .spawn(move || {
            let mut backoff =
                ReconnectBackoff::new(RESTART_INITIAL_BACKOFF_MS, RESTART_MAX_BACKOFF_MS);
            let mut consecutive_failures: u32 = 0;
            let mut pipeline_started_at = Instant::now();

            while !stop_flag.load(Ordering::Acquire) {
                let bus = pipeline.bus();
                let mut should_restart = false;

                if let Some(ref b) = bus {
                    while !stop_flag.load(Ordering::Acquire) {
                        let Some(msg) = b.timed_pop(BUS_POLL_TIMEOUT) else {
                            continue;
                        };
                        match msg.view() {
                            gst::MessageView::Error(err) => {
                                let detail = format!(
                                    "GStreamer broadcaster error from {}: {} ({:?})",
                                    err.src()
                                        .map(|s| s.path_string().to_string())
                                        .unwrap_or_else(|| "unknown".into()),
                                    err.error(),
                                    err.debug()
                                );
                                *last_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(detail);
                                healthy.store(false, Ordering::Release);
                                should_restart = true;
                                break;
                            }
                            gst::MessageView::Eos(_) => {
                                healthy.store(false, Ordering::Release);
                                should_restart = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                } else {
                    thread::sleep(Duration::from_millis(50));
                }

                if stop_flag.load(Ordering::Acquire) || !should_restart {
                    break;
                }

                *active_bus.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *active_opusenc.lock().unwrap_or_else(|e| e.into_inner()) = None;
                teardown_pipeline(&pipeline);

                if pipeline_started_at.elapsed() >= Duration::from_secs(HEALTHY_RUN_RESET_SECS) {
                    consecutive_failures = 0;
                    backoff.reset();
                }

                loop {
                    if stop_flag.load(Ordering::Acquire) {
                        break;
                    }
                    if consecutive_failures >= MAX_CONSECUTIVE_RESTARTS {
                        healthy.store(false, Ordering::Release);
                        return;
                    }
                    let delay = backoff.next_delay();
                    if sleep_interruptible(delay, &stop_flag) {
                        return;
                    }
                    match start_broadcaster_pipeline(
                        &endpoint,
                        &audio_params,
                        &counters,
                        potato_mode.load(Ordering::Acquire),
                    ) {
                        Ok((new_pipeline, new_opusenc)) => {
                            let new_bus = new_pipeline.bus();
                            *active_bus.lock().unwrap_or_else(|e| e.into_inner()) = new_bus;
                            *active_opusenc.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(new_opusenc);
                            pipeline = new_pipeline;
                            pipeline_started_at = Instant::now();
                            healthy.store(true, Ordering::Release);
                            restart_count.fetch_add(1, Ordering::Release);
                            break;
                        }
                        Err(err) => {
                            *last_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(err.to_string());
                            healthy.store(false, Ordering::Release);
                            consecutive_failures += 1;
                        }
                    }
                }
            }

            *active_bus.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *active_opusenc.lock().unwrap_or_else(|e| e.into_inner()) = None;
            teardown_pipeline(&pipeline);
        })
        .map_err(|err| {
            *cleanup_bus.lock().unwrap_or_else(|e| e.into_inner()) = None;
            teardown_pipeline(&cleanup_pipeline);
            TransportError::Io(err)
        })
}

#[cfg(feature = "gstreamer")]
fn sleep_interruptible(delay: Duration, stop_flag: &AtomicBool) -> bool {
    let mut remaining = delay;
    let step = Duration::from_millis(25);
    while !remaining.is_zero() {
        if stop_flag.load(Ordering::Acquire) {
            return true;
        }
        let nap = remaining.min(step);
        thread::sleep(nap);
        remaining = remaining.saturating_sub(nap);
    }
    stop_flag.load(Ordering::Acquire)
}

/// Stub [`GstBroadcaster`] compiled when `--no-default-features` disables GStreamer.
#[cfg(not(feature = "gstreamer"))]
pub struct GstBroadcaster {
    registry: Arc<SubscriberRegistry>,
}

#[cfg(not(feature = "gstreamer"))]
impl GstBroadcaster {
    pub fn start(_endpoint: CaptureEndpoint, _audio_params: AudioParams) -> TransportResult<Self> {
        Err(TransportError::Pipeline(
            "GStreamer transport support is disabled in this build".into(),
        ))
    }

    pub fn start_with_registry(
        _endpoint: CaptureEndpoint,
        _audio_params: AudioParams,
        _registry: Arc<SubscriberRegistry>,
    ) -> TransportResult<Self> {
        Err(TransportError::Pipeline(
            "GStreamer transport support is disabled in this build".into(),
        ))
    }

    pub fn subscribe_opus(&self, wakeup: Option<WakeupHook>) -> BroadcastSubscription {
        self.registry.subscribe_opus(wakeup)
    }

    pub fn subscribe_pcm(&self, wakeup: Option<WakeupHook>) -> BroadcastSubscription {
        self.registry.subscribe_pcm(wakeup)
    }

    pub fn unsubscribe(&self, id: SubscriberId) -> bool {
        self.registry.unsubscribe(id)
    }

    pub fn registry(&self) -> &Arc<SubscriberRegistry> {
        &self.registry
    }

    pub fn is_healthy(&self) -> bool {
        false
    }

    pub fn last_error(&self) -> Option<String> {
        Some("GStreamer transport support is disabled in this build".into())
    }

    pub fn restart_count(&self) -> u64 {
        0
    }

    pub fn opus_subscriber_count(&self) -> usize {
        0
    }

    pub fn pcm_subscriber_count(&self) -> usize {
        0
    }

    pub fn subscriber_count(&self) -> usize {
        0
    }

    pub fn opus_frames_emitted(&self) -> u64 {
        0
    }

    pub fn pcm_frames_emitted(&self) -> u64 {
        0
    }

    pub fn opus_frames_skipped_no_subs(&self) -> u64 {
        0
    }

    pub fn pcm_frames_skipped_no_subs(&self) -> u64 {
        0
    }

    pub fn dropped_frames(&self) -> u64 {
        0
    }

    pub fn evicted_subscribers(&self) -> u64 {
        0
    }

    pub fn set_potato_mode(&self, _enabled: bool) {}

    pub fn is_potato_mode(&self) -> bool {
        false
    }

    pub fn stop(&self) {
        self.registry.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_frame(seq: u64) -> Arc<BroadcastFrame> {
        Arc::new(BroadcastFrame {
            seq,
            pts_samples: seq * u64::from(BROADCAST_FRAME_SAMPLES),
            duration_samples: BROADCAST_FRAME_SAMPLES,
            captured_at: Instant::now(),
            data: Arc::from([0u8; 4].as_slice()),
        })
    }

    #[test]
    fn slow_subscriber_drops_and_evicts_after_250_consecutive_drops() {
        let reg = Arc::new(SubscriberRegistry::new());
        let fast = reg.subscribe_pcm(None);
        let slow = reg.subscribe_pcm(None);
        assert_eq!(reg.pcm_count(), 2);

        let mut scratch = Vec::new();
        // Fill both bounded channels (capacity = 32).
        for seq in 0..SUBSCRIBER_CHANNEL_CAPACITY as u64 {
            reg.dispatch_pcm(dummy_frame(seq), &mut scratch);
            let _ = fast.try_recv().unwrap();
        }
        assert_eq!(slow.stats().dropped_frames.load(Ordering::Relaxed), 0);

        // Drop 10 frames on slow, then drain 1 to verify consecutive_drops resets.
        for seq in 100..110u64 {
            reg.dispatch_pcm(dummy_frame(seq), &mut scratch);
            let _ = fast.try_recv().unwrap();
        }
        assert_eq!(slow.stats().dropped_frames.load(Ordering::Relaxed), 10);
        assert_eq!(slow.stats().consecutive_drops.load(Ordering::Relaxed), 10);
        assert!(!slow.is_evicted());

        let _ = slow.try_recv().unwrap(); // frees 1 slot
        reg.dispatch_pcm(dummy_frame(110), &mut scratch);
        let _ = fast.try_recv().unwrap();
        assert_eq!(slow.stats().consecutive_drops.load(Ordering::Relaxed), 0);

        // Now saturate slow for 249 consecutive drops: still retained.
        for i in 0..(SLOW_SUBSCRIBER_MAX_CONSECUTIVE_DROPS - 1) {
            reg.dispatch_pcm(dummy_frame(200 + u64::from(i)), &mut scratch);
            let _ = fast.try_recv().unwrap();
        }
        assert!(!slow.is_evicted());
        assert_eq!(
            slow.stats().consecutive_drops.load(Ordering::Relaxed),
            SLOW_SUBSCRIBER_MAX_CONSECUTIVE_DROPS - 1
        );
        assert_eq!(reg.pcm_count(), 2);

        // 250th consecutive drop evicts `slow`, drops its sender, and leaves `fast` intact.
        reg.dispatch_pcm(dummy_frame(999), &mut scratch);
        let fast_frame = fast.try_recv().unwrap();
        assert_eq!(fast_frame.seq, 999);
        assert!(slow.is_evicted());
        assert_eq!(
            slow.stats().consecutive_drops.load(Ordering::Relaxed),
            SLOW_SUBSCRIBER_MAX_CONSECUTIVE_DROPS
        );
        assert_eq!(reg.pcm_count(), 1);
        assert_eq!(reg.evicted_subscribers_total(), 1);

        // Draining the buffered 32 frames on `slow` yields Disconnected immediately.
        for _ in 0..SUBSCRIBER_CHANNEL_CAPACITY {
            assert!(slow.try_recv().is_ok());
        }
        assert!(matches!(slow.try_recv(), Err(TryRecvError::Disconnected)));
    }

    #[cfg(not(feature = "gstreamer"))]
    #[test]
    fn disabled_gstreamer_stub_returns_pipeline_error() {
        let res = GstBroadcaster::start(
            CaptureEndpoint::TestSineWave { freq_hz: 440.0 },
            AudioParams::default(),
        );
        assert!(matches!(res, Err(TransportError::Pipeline(_))));
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn test_sine_wave_dual_appsink_emits_opus_and_pcm_frames() {
        let broadcaster = GstBroadcaster::start(
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            },
            AudioParams::default(),
        )
        .expect("GstBroadcaster must start");

        let poke_sink = UdpSocket::bind("127.0.0.1:0").expect("bind loopback UDP socket");
        poke_sink
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let poke_hook = udp_poke_hook(poke_sink.local_addr().unwrap()).unwrap();

        let opus_sub = broadcaster.subscribe_opus(Some(poke_hook));
        let pcm_sub = broadcaster.subscribe_pcm(None);
        assert_eq!(broadcaster.opus_subscriber_count(), 1);
        assert_eq!(broadcaster.pcm_subscriber_count(), 1);
        assert_eq!(broadcaster.subscriber_count(), 2);

        let mut opus_frames = Vec::new();
        let mut pcm_frames = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(4);
        while (opus_frames.len() < 8 || pcm_frames.len() < 8) && Instant::now() < deadline {
            if opus_frames.len() < 8 {
                if let Ok(frame) = opus_sub.recv_timeout(Duration::from_millis(50)) {
                    opus_frames.push(frame);
                }
            }
            if pcm_frames.len() < 8 {
                if let Ok(frame) = pcm_sub.recv_timeout(Duration::from_millis(50)) {
                    pcm_frames.push(frame);
                }
            }
        }

        assert!(
            opus_frames.len() >= 8,
            "expected >= 8 Opus frames, got {}",
            opus_frames.len()
        );
        assert!(
            pcm_frames.len() >= 8,
            "expected >= 8 PCM frames, got {}",
            pcm_frames.len()
        );

        // Verify UDP poke fired on frame emission.
        let mut poke_buf = [0u8; 8];
        let (n, _) = poke_sink
            .recv_from(&mut poke_buf)
            .expect("expected UDP poke wakeup datagram");
        assert_eq!(n, 1);

        for (idx, frame) in opus_frames.iter().enumerate() {
            assert_eq!(frame.seq, idx as u64);
            assert_eq!(frame.duration_samples, BROADCAST_FRAME_SAMPLES);
            assert!(!frame.data.is_empty());
        }
        for (idx, frame) in pcm_frames.iter().enumerate() {
            assert_eq!(frame.seq, idx as u64);
            assert_eq!(
                frame.pts_samples,
                (idx as u64) * u64::from(BROADCAST_FRAME_SAMPLES)
            );
            assert_eq!(frame.duration_samples, BROADCAST_FRAME_SAMPLES);
            assert_eq!(frame.data.len(), BROADCAST_PCM_FRAME_BYTES);
        }

        // Verify PCM payload actually contains non-silent 440 Hz sine samples.
        let non_zero_samples = pcm_frames[0]
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|&&pair| i16::from_le_bytes(pair) != 0)
            .count();
        assert!(non_zero_samples > 100, "expected non-silent PCM sine wave");

        let t0 = Instant::now();
        broadcaster.stop();
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "stop() took too long: {:?}",
            t0.elapsed()
        );
        assert!(!broadcaster.is_healthy());
        assert_eq!(broadcaster.subscriber_count(), 0);
        // Existing receivers see disconnection cleanly after stop().
        while pcm_sub.try_recv().is_ok() {}
        assert!(matches!(
            pcm_sub.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn lazy_pcm_skips_when_no_pcm_subscribers() {
        let broadcaster = GstBroadcaster::start(
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            },
            AudioParams::default(),
        )
        .expect("GstBroadcaster must start");

        let opus_sub = broadcaster.subscribe_opus(None);
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut received = 0usize;
        while received < 10 && Instant::now() < deadline {
            if opus_sub.recv_timeout(Duration::from_millis(100)).is_ok() {
                received += 1;
            }
        }
        assert!(received >= 10, "expected >= 10 Opus frames, got {received}");
        assert_eq!(broadcaster.pcm_frames_emitted(), 0);
        assert!(
            broadcaster.pcm_frames_skipped_no_subs() > 0,
            "expected pcm_sink to skip mapping when pcm_sub_count == 0"
        );
        assert!(broadcaster.opus_frames_emitted() >= 10);

        // Now attach a PCM subscriber and verify PCM frames start emitting dynamically.
        let pcm_sub = broadcaster.subscribe_pcm(None);
        let pcm_frame = pcm_sub
            .recv_timeout(Duration::from_secs(2))
            .expect("PCM frame should start emitting once subscribed");
        assert_eq!(pcm_frame.data.len(), BROADCAST_PCM_FRAME_BYTES);
        assert!(broadcaster.pcm_frames_emitted() >= 1);

        broadcaster.stop();
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn slow_subscriber_drops_without_stalling_fast_subscriber_and_bus_restart_recovers() {
        let broadcaster = GstBroadcaster::start(
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            },
            AudioParams::default(),
        )
        .expect("GstBroadcaster must start");

        let fast_pcm = broadcaster.subscribe_pcm(None);
        let slow_pcm = broadcaster.subscribe_pcm(None);

        // Drain 40 frames on `fast_pcm` (~800ms) while `slow_pcm` never reads.
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut fast_count = 0usize;
        while fast_count < 40 && Instant::now() < deadline {
            if fast_pcm.recv_timeout(Duration::from_millis(100)).is_ok() {
                fast_count += 1;
            }
        }
        assert!(fast_count >= 40, "fast_pcm got {fast_count} frames");
        assert_eq!(fast_pcm.stats().dropped_frames.load(Ordering::Relaxed), 0);
        assert!(
            slow_pcm.stats().dropped_frames.load(Ordering::Relaxed) >= 5,
            "expected slow_pcm to drop frames after filling 32-slot queue"
        );

        // Simulate a bus error (like Windows WASAPI hotplug) and verify bounded restart recovery.
        {
            let bus_guard = broadcaster.active_bus.lock().unwrap();
            let bus = bus_guard.as_ref().expect("active bus present");
            let err_msg = gst::message::Error::new(
                gst::CoreError::Failed,
                "simulated WASAPI device invalidated",
            );
            assert!(bus.post(err_msg).is_ok());
        }

        let restart_deadline = Instant::now() + Duration::from_secs(4);
        while broadcaster.restart_count() < 1 && Instant::now() < restart_deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            broadcaster.restart_count() >= 1,
            "expected bus error to trigger bounded pipeline restart"
        );
        assert!(broadcaster.is_healthy());

        // Fast subscriber continues receiving frames across the restart without unsubscribing.
        let before_restart_emitted = broadcaster.pcm_frames_emitted();
        let resume_deadline = Instant::now() + Duration::from_secs(3);
        let mut resumed = false;
        while Instant::now() < resume_deadline {
            while fast_pcm.try_recv().is_ok() {}
            if broadcaster.pcm_frames_emitted() > before_restart_emitted + 2 {
                resumed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            resumed,
            "expected PCM frames to resume after pipeline restart"
        );

        broadcaster.stop();
        assert!(!broadcaster.is_healthy());
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn potato_mode_tunes_running_opusenc_safely() {
        let broadcaster = GstBroadcaster::start(
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            },
            AudioParams::default(),
        )
        .expect("GstBroadcaster must start");

        let opus_sub = broadcaster.subscribe_opus(None);
        assert!(!broadcaster.is_potato_mode());

        // Receive at least one normal 20ms (960-sample) frame.
        let normal_frame = opus_sub
            .recv_timeout(Duration::from_secs(2))
            .expect("normal opus frame");
        assert_eq!(normal_frame.duration_samples, BROADCAST_FRAME_SAMPLES);

        // Enable Potato mode on the running pipeline and verify 40ms (1920-sample) frames emit.
        broadcaster.set_potato_mode(true);
        assert!(broadcaster.is_potato_mode());

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_40ms = false;
        while Instant::now() < deadline {
            if let Ok(frame) = opus_sub.recv_timeout(Duration::from_millis(100)) {
                if frame.duration_samples == 1920 {
                    saw_40ms = true;
                    break;
                }
            }
        }
        assert!(
            saw_40ms,
            "expected 40ms (1920-sample) Opus frames in Potato mode"
        );

        // Toggle back to normal mode safely.
        broadcaster.set_potato_mode(false);
        assert!(!broadcaster.is_potato_mode());

        broadcaster.stop();
    }
}
