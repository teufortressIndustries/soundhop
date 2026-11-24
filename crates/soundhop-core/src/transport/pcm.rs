//! Cross-platform 48 kHz stereo S16LE PCM capture/playback boundary traits and
//! headless mock implementations for SHT3 transport pipelines and tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::transport::TransportResult;

/// Canonical SHT3 PCM sample rate (48 kHz).
pub const PCM_SAMPLE_RATE: u32 = 48_000;
/// Canonical SHT3 PCM channel count (stereo).
pub const PCM_CHANNELS: u8 = 2;
/// Interleaved `i16` samples in one 20 ms 48 kHz stereo frame (`960 * 2 = 1920`).
pub const PCM_FRAME_SAMPLES_20MS: usize = 960 * 2;
/// Interleaved `i16` samples in one 10 ms 48 kHz stereo frame (`480 * 2 = 960`).
pub const PCM_FRAME_SAMPLES_10MS: usize = 480 * 2;

/// Pull-based 48 kHz stereo S16LE PCM audio capture source for SHT3 outbound streams.
pub trait PcmCapture: Send {
    /// Fill `buf` with interleaved 48 kHz stereo `i16` PCM samples.
    ///
    /// Returns the number of `i16` samples written into `buf`. Returning `Ok(0)`
    /// signals clean end-of-stream (EOF).
    fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize>;

    /// Sample rate in Hz (canonical SHT3 rate is 48,000 Hz).
    fn sample_rate(&self) -> u32 {
        PCM_SAMPLE_RATE
    }

    /// Channel count (canonical SHT3 layout is 2-channel interleaved stereo).
    fn channels(&self) -> u8 {
        PCM_CHANNELS
    }

    /// Attach an external stop flag so blocked `read_frame` calls wake up cleanly on shutdown.
    fn attach_stop_flag(&mut self, _stop_flag: Arc<AtomicBool>) {}

    /// Return `true` (and clear) if an upstream capture discontinuity occurred
    /// (for example, after respawning a crashed capture subprocess).
    fn take_discontinuity(&mut self) -> bool {
        false
    }
}

/// Push-based 48 kHz stereo S16LE PCM audio playback sink for SHT3 inbound streams.
pub trait PcmPlayback: Send {
    /// Play one frame of interleaved 48 kHz stereo `i16` PCM samples.
    fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()>;

    /// Set output volume percentage (`0..=150`).
    fn set_volume(&self, pct: u8);

    /// Set output mute state.
    fn set_mute(&self, muted: bool);

    /// Current output volume percentage (`0..=150`).
    fn volume(&self) -> u8 {
        100
    }

    /// Whether output is currently muted.
    fn is_muted(&self) -> bool {
        false
    }

    /// Sample rate in Hz (canonical SHT3 rate is 48,000 Hz).
    fn sample_rate(&self) -> u32 {
        PCM_SAMPLE_RATE
    }

    /// Channel count (canonical SHT3 layout is 2-channel interleaved stereo).
    fn channels(&self) -> u8 {
        PCM_CHANNELS
    }

    /// Attach an external stop flag so blocked `read_frame` calls wake up cleanly on shutdown.
    fn attach_stop_flag(&mut self, _stop_flag: Arc<AtomicBool>) {}

    /// Return `true` (and clear) if an upstream capture discontinuity occurred
    /// (for example, after respawning a crashed capture subprocess).
    fn take_discontinuity(&mut self) -> bool {
        false
    }

    /// Current sink buffer fill level in milliseconds, if measurable (`None` for unclocked sinks).
    fn buffered_ms(&self) -> Option<f64> {
        None
    }

    /// Target prebuffer / steady-state sink fill depth in milliseconds.
    fn sink_target_ms(&self) -> f64 {
        40.0
    }

    /// Cumulative number of sink buffer underrun events.
    fn sink_underruns(&self) -> u64 {
        0
    }

    /// Cumulative number of sink buffer overrun (drop) events.
    fn sink_overruns(&self) -> u64 {
        0
    }
}

/// Factory invoked by [`crate::transport::Sht3SessionManager`] to create a [`PcmPlayback`]
/// sink whenever an authenticated peer connects. Receives the peer's `device_id`.
pub type PcmPlaybackFactory =
    Arc<dyn Fn(&str) -> TransportResult<Box<dyn PcmPlayback>> + Send + Sync>;

/// Headless [`PcmCapture`] implementation for unit and integration tests.
///
/// Can stream either a finite sequence of pre-built PCM frames or a continuous
/// 48 kHz stereo sine wave with optional frame limit, pacing delay, and stop flag.
pub struct MockPcmCapture {
    mode: MockCaptureMode,
    pace: Option<Duration>,
    stop_flag: Option<Arc<AtomicBool>>,
}

enum MockCaptureMode {
    Frames(VecDeque<Vec<i16>>),
    Sine {
        freq_hz: f64,
        sample_index: usize,
        frames_emitted: usize,
        max_frames: Option<usize>,
    },
}

impl MockPcmCapture {
    /// Stream a fixed sequence of PCM frames and return `Ok(0)` once exhausted.
    pub fn from_frames<I>(frames: I) -> Self
    where
        I: IntoIterator<Item = Vec<i16>>,
    {
        Self {
            mode: MockCaptureMode::Frames(frames.into_iter().collect()),
            pace: None,
            stop_flag: None,
        }
    }

    /// Generate a 48 kHz stereo sine wave (`freq_hz` on left, inverted on right).
    ///
    /// Pass `max_frames = None` for a continuous stream until stopped.
    pub fn sine_wave(freq_hz: f64, max_frames: Option<usize>) -> Self {
        Self {
            mode: MockCaptureMode::Sine {
                freq_hz,
                sample_index: 0,
                frames_emitted: 0,
                max_frames,
            },
            pace: None,
            stop_flag: None,
        }
    }

    /// Sleep `delay` before emitting each frame (useful for simulating real-time cadence).
    pub fn with_pace(mut self, delay: Duration) -> Self {
        self.pace = Some(delay);
        self
    }

    /// Attach an external stop flag; once set to `true`, `read_frame` returns `Ok(0)`.
    pub fn with_stop_flag(mut self, stop_flag: Arc<AtomicBool>) -> Self {
        self.stop_flag = Some(stop_flag);
        self
    }
}

impl PcmCapture for MockPcmCapture {
    fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize> {
        if let Some(ref flag) = self.stop_flag {
            if flag.load(Ordering::Acquire) {
                return Ok(0);
            }
        }
        if let Some(delay) = self.pace {
            thread::sleep(delay);
            if let Some(ref flag) = self.stop_flag {
                if flag.load(Ordering::Acquire) {
                    return Ok(0);
                }
            }
        }

        match &mut self.mode {
            MockCaptureMode::Frames(queue) => {
                let Some(frame) = queue.pop_front() else {
                    return Ok(0);
                };
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                Ok(n)
            }
            MockCaptureMode::Sine {
                freq_hz,
                sample_index,
                frames_emitted,
                max_frames,
            } => {
                if let Some(limit) = *max_frames {
                    if *frames_emitted >= limit {
                        return Ok(0);
                    }
                }
                let stereo_frames = buf.len() / 2;
                for i in 0..stereo_frames {
                    let t = (*sample_index + i) as f64 / f64::from(PCM_SAMPLE_RATE);
                    let s = ((2.0 * std::f64::consts::PI * *freq_hz * t).sin() * 14_000.0) as i16;
                    buf[i * 2] = s;
                    buf[i * 2 + 1] = -s;
                }
                *sample_index += stereo_frames;
                *frames_emitted += 1;
                Ok(stereo_frames * 2)
            }
        }
    }
}

#[derive(Debug)]
struct MockPlaybackState {
    frames: Mutex<Vec<Vec<i16>>>,
    volume: AtomicU8,
    muted: AtomicBool,
    volume_history: Mutex<Vec<u8>>,
    mute_history: Mutex<Vec<bool>>,
    closed: AtomicBool,
}

impl Default for MockPlaybackState {
    fn default() -> Self {
        Self {
            frames: Mutex::new(Vec::new()),
            volume: AtomicU8::new(100),
            muted: AtomicBool::new(false),
            volume_history: Mutex::new(Vec::new()),
            mute_history: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }
}

/// Inspection handle paired with a [`MockPcmPlayback`] sink.
#[derive(Debug, Clone)]
pub struct MockPcmPlaybackHandle {
    state: Arc<MockPlaybackState>,
}

impl MockPcmPlaybackHandle {
    pub fn frames(&self) -> Vec<Vec<i16>> {
        self.state
            .frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn frame_count(&self) -> usize {
        self.state
            .frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn total_samples(&self) -> usize {
        self.state
            .frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(Vec::len)
            .sum()
    }

    pub fn total_energy(&self) -> i64 {
        self.state
            .frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flat_map(|f| f.iter())
            .map(|&s| i64::from(s).abs())
            .sum()
    }

    pub fn volume(&self) -> u8 {
        self.state.volume.load(Ordering::Acquire)
    }

    pub fn is_muted(&self) -> bool {
        self.state.muted.load(Ordering::Acquire)
    }

    pub fn volume_history(&self) -> Vec<u8> {
        self.state
            .volume_history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn mute_history(&self) -> Vec<bool> {
        self.state
            .mute_history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Returns `true` once the owning [`MockPcmPlayback`] has been dropped.
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }
}

/// Headless [`PcmPlayback`] sink that records written PCM frames and volume/mute changes.
pub struct MockPcmPlayback {
    state: Arc<MockPlaybackState>,
}

impl MockPcmPlayback {
    /// Create a `(MockPcmPlayback, MockPcmPlaybackHandle)` pair.
    pub fn pair() -> (Self, MockPcmPlaybackHandle) {
        let state = Arc::new(MockPlaybackState::default());
        let handle = MockPcmPlaybackHandle {
            state: Arc::clone(&state),
        };
        (Self { state }, handle)
    }

    /// Return a cloned handle for inspecting recorded frames and control state.
    pub fn handle(&self) -> MockPcmPlaybackHandle {
        MockPcmPlaybackHandle {
            state: Arc::clone(&self.state),
        }
    }
}

impl Default for MockPcmPlayback {
    fn default() -> Self {
        Self::pair().0
    }
}

impl PcmPlayback for MockPcmPlayback {
    fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()> {
        self.state
            .frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(samples.to_vec());
        Ok(())
    }

    fn set_volume(&self, pct: u8) {
        let clamped = pct.min(150);
        self.state.volume.store(clamped, Ordering::Release);
        self.state
            .volume_history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(clamped);
    }

    fn set_mute(&self, muted: bool) {
        self.state.muted.store(muted, Ordering::Release);
        self.state
            .mute_history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(muted);
    }

    fn volume(&self) -> u8 {
        self.state.volume.load(Ordering::Acquire)
    }

    fn is_muted(&self) -> bool {
        self.state.muted.load(Ordering::Acquire)
    }
}

impl Drop for MockPcmPlayback {
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Release);
    }
}

const DEFAULT_CLOCKED_TARGET_MS: f64 = 40.0;
const DEFAULT_CLOCKED_MAX_MS: f64 = 160.0;
const REALTIME_CLOCK_TICK: Duration = Duration::from_millis(5);

#[inline]
fn stereo_frames_to_ms(stereo_frames: usize) -> f64 {
    (stereo_frames as f64) * 1000.0 / f64::from(PCM_SAMPLE_RATE)
}

#[inline]
fn ms_to_stereo_frames(ms: f64) -> usize {
    ((ms.max(0.0) / 1000.0) * f64::from(PCM_SAMPLE_RATE)).round() as usize
}

#[derive(Debug)]
struct ClockedPlaybackInner {
    dac_rate_hz: f64,
    target_ms: f64,
    max_ms: f64,
    target_stereo_frames: usize,
    max_stereo_frames: usize,
    queue: VecDeque<i16>,
    primed: bool,
    dac_phase_frames: f64,
    record_output: bool,
    output_samples: Vec<i16>,
    written_samples: u64,
    underrun_samples: u64,
    overrun_dropped_samples: u64,
    fill_history: Vec<f64>,
    min_fill_ms: Option<f64>,
    max_fill_ms: Option<f64>,
    fill_sum_ms: f64,
    fill_count: u64,
    volume_history: Vec<u8>,
    mute_history: Vec<bool>,
}

impl ClockedPlaybackInner {
    fn new(dac_rate_hz: f64, target_ms: f64, max_ms: f64) -> Self {
        let dac_rate_hz = if dac_rate_hz.is_finite() && dac_rate_hz > 0.0 {
            dac_rate_hz
        } else {
            f64::from(PCM_SAMPLE_RATE)
        };
        let target_ms = if target_ms.is_finite() && target_ms >= 0.0 {
            target_ms
        } else {
            DEFAULT_CLOCKED_TARGET_MS
        };
        let max_ms = if max_ms.is_finite() && max_ms > 0.0 {
            max_ms.max(target_ms)
        } else {
            DEFAULT_CLOCKED_MAX_MS.max(target_ms)
        };
        Self {
            dac_rate_hz,
            target_ms,
            max_ms,
            target_stereo_frames: ms_to_stereo_frames(target_ms),
            max_stereo_frames: ms_to_stereo_frames(max_ms).max(1),
            queue: VecDeque::new(),
            primed: false,
            dac_phase_frames: 0.0,
            record_output: true,
            output_samples: Vec::new(),
            written_samples: 0,
            underrun_samples: 0,
            overrun_dropped_samples: 0,
            fill_history: Vec::new(),
            min_fill_ms: None,
            max_fill_ms: None,
            fill_sum_ms: 0.0,
            fill_count: 0,
            volume_history: Vec::new(),
            mute_history: Vec::new(),
        }
    }

    #[inline]
    fn buffered_stereo_frames(&self) -> usize {
        self.queue.len() / 2
    }

    #[inline]
    fn buffered_ms(&self) -> f64 {
        stereo_frames_to_ms(self.buffered_stereo_frames())
    }

    fn record_fill(&mut self) {
        if !self.primed {
            return;
        }
        let fill = self.buffered_ms();
        self.fill_history.push(fill);
        self.min_fill_ms = Some(match self.min_fill_ms {
            Some(prev) => prev.min(fill),
            None => fill,
        });
        self.max_fill_ms = Some(match self.max_fill_ms {
            Some(prev) => prev.max(fill),
            None => fill,
        });
        self.fill_sum_ms += fill;
        self.fill_count += 1;
    }
}

#[derive(Debug)]
struct ClockedPlaybackState {
    inner: Mutex<ClockedPlaybackInner>,
    volume: AtomicU8,
    muted: AtomicBool,
    underruns: AtomicU64,
    overruns: AtomicU64,
    stop_realtime: AtomicBool,
    closed: AtomicBool,
}

impl ClockedPlaybackState {
    fn new(dac_rate_hz: f64, target_ms: f64, max_ms: f64) -> Self {
        Self {
            inner: Mutex::new(ClockedPlaybackInner::new(dac_rate_hz, target_ms, max_ms)),
            volume: AtomicU8::new(100),
            muted: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            stop_realtime: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    fn advance_dac_secs(&self, dt_secs: f64) {
        if !dt_secs.is_finite() || dt_secs <= 0.0 {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.primed {
            return;
        }
        inner.dac_phase_frames += dt_secs * inner.dac_rate_hz;
        let need_frames = inner.dac_phase_frames.floor() as usize;
        if need_frames == 0 {
            return;
        }
        inner.dac_phase_frames -= need_frames as f64;

        let available_frames = inner.buffered_stereo_frames();
        let consume_frames = available_frames.min(need_frames);
        let consume_samples = consume_frames * 2;
        if inner.record_output {
            inner.output_samples.reserve(need_frames * 2);
            for _ in 0..consume_samples {
                if let Some(s) = inner.queue.pop_front() {
                    inner.output_samples.push(s);
                }
            }
        } else {
            inner.queue.drain(..consume_samples);
        }

        if consume_frames < need_frames {
            let missing_frames = need_frames - consume_frames;
            let missing_samples = missing_frames * 2;
            if inner.record_output {
                let new_len = inner.output_samples.len() + missing_samples;
                inner.output_samples.resize(new_len, 0);
            }
            inner.underrun_samples += missing_samples as u64;
            self.underruns.fetch_add(1, Ordering::Release);
        }

        inner.record_fill();
    }

    fn drain_samples_for_frames(&self, nominal_stereo_frames: usize) {
        if nominal_stereo_frames == 0 {
            return;
        }
        let dt_secs = (nominal_stereo_frames as f64) / f64::from(PCM_SAMPLE_RATE);
        self.advance_dac_secs(dt_secs);
    }

    fn buffered_ms(&self) -> f64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffered_ms()
    }

    fn target_ms(&self) -> f64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .target_ms
    }
}

/// Inspection and virtual-clock control handle paired with a [`ClockedMockPcmPlayback`] sink.
#[derive(Debug, Clone)]
pub struct ClockedMockPcmPlaybackHandle {
    state: Arc<ClockedPlaybackState>,
}

impl ClockedMockPcmPlaybackHandle {
    /// Advance the simulated DAC clock by `elapsed` (virtual time, no wall-clock sleep).
    pub fn advance_dac_time(&self, elapsed: Duration) {
        self.state.advance_dac_secs(elapsed.as_secs_f64());
    }

    /// Drain DAC output corresponding to `nominal_stereo_frames` at 48 kHz, scaled by `dac_rate_hz / 48000`.
    pub fn drain_samples_for_frames(&self, nominal_stereo_frames: usize) {
        self.state.drain_samples_for_frames(nominal_stereo_frames);
    }

    /// Current buffered audio fill level in milliseconds.
    pub fn buffered_ms(&self) -> f64 {
        self.state.buffered_ms()
    }

    /// Configured prebuffer target depth in milliseconds.
    pub fn target_ms(&self) -> f64 {
        self.state.target_ms()
    }

    /// Configured maximum buffer capacity before overrun drops in milliseconds.
    pub fn max_ms(&self) -> f64 {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .max_ms
    }

    /// Configured simulated DAC sample rate in stereo frames per second.
    pub fn dac_rate_hz(&self) -> f64 {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dac_rate_hz
    }

    /// Whether the sink has reached `target_ms` prebuffer and begun DAC consumption.
    pub fn is_primed(&self) -> bool {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .primed
    }

    /// Number of currently queued `i16` samples awaiting DAC consumption.
    pub fn buffered_samples(&self) -> usize {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .len()
    }

    /// Total `i16` samples written into the sink via `write_frame`.
    pub fn written_samples(&self) -> u64 {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .written_samples
    }

    /// Cumulative sink underrun event count.
    pub fn underruns(&self) -> u64 {
        self.state.underruns.load(Ordering::Acquire)
    }

    /// Cumulative sink overrun (drop) event count.
    pub fn overruns(&self) -> u64 {
        self.state.overruns.load(Ordering::Acquire)
    }

    /// Cumulative `i16` zero-filled samples inserted due to underruns.
    pub fn underrun_samples(&self) -> u64 {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .underrun_samples
    }

    /// Cumulative oldest `i16` samples dropped due to overruns.
    pub fn overrun_dropped_samples(&self) -> u64 {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .overrun_dropped_samples
    }

    /// Recorded post-priming fill depth history in milliseconds.
    pub fn fill_history(&self) -> Vec<f64> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fill_history
            .clone()
    }

    /// Minimum recorded post-priming fill depth in milliseconds.
    pub fn min_fill_ms(&self) -> Option<f64> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .min_fill_ms
    }

    /// Maximum recorded post-priming fill depth in milliseconds.
    pub fn max_fill_ms(&self) -> Option<f64> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .max_fill_ms
    }

    /// Mean recorded post-priming fill depth in milliseconds.
    pub fn avg_fill_ms(&self) -> Option<f64> {
        let inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.fill_count == 0 {
            None
        } else {
            Some(inner.fill_sum_ms / (inner.fill_count as f64))
        }
    }

    /// Clear recorded fill history and min/max/avg summary counters.
    pub fn reset_fill_history(&self) {
        let mut inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.fill_history.clear();
        inner.min_fill_ms = None;
        inner.max_fill_ms = None;
        inner.fill_sum_ms = 0.0;
        inner.fill_count = 0;
    }

    /// Cloned PCM samples consumed by the simulated DAC (including zero-fill on underruns).
    pub fn output_samples(&self) -> Vec<i16> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .output_samples
            .clone()
    }

    /// Total number of PCM samples consumed by the simulated DAC.
    pub fn total_output_samples(&self) -> usize {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .output_samples
            .len()
    }

    /// Current output volume percentage (`0..=150`).
    pub fn volume(&self) -> u8 {
        self.state.volume.load(Ordering::Acquire)
    }

    /// Whether output is currently muted.
    pub fn is_muted(&self) -> bool {
        self.state.muted.load(Ordering::Acquire)
    }

    /// Recorded volume change history.
    pub fn volume_history(&self) -> Vec<u8> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .volume_history
            .clone()
    }

    /// Recorded mute change history.
    pub fn mute_history(&self) -> Vec<bool> {
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mute_history
            .clone()
    }

    /// Returns `true` once the owning [`ClockedMockPcmPlayback`] has been dropped.
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }
}

/// Asynchronous hardware DAC simulator implementing [`PcmPlayback`] with configurable
/// crystal rate (`dac_rate_hz`), prebuffer depth (`target_ms`), overrun capacity (`max_ms`),
/// and both deterministic virtual-clock stepping and optional real-time background draining.
pub struct ClockedMockPcmPlayback {
    state: Arc<ClockedPlaybackState>,
    realtime_thread: Option<JoinHandle<()>>,
}

impl ClockedMockPcmPlayback {
    /// Create a `(ClockedMockPcmPlayback, ClockedMockPcmPlaybackHandle)` pair with
    /// default `target_ms = 40.0` and `max_ms = 160.0` in deterministic manual-clock mode.
    pub fn pair(dac_rate_hz: f64) -> (Self, ClockedMockPcmPlaybackHandle) {
        let state = Arc::new(ClockedPlaybackState::new(
            dac_rate_hz,
            DEFAULT_CLOCKED_TARGET_MS,
            DEFAULT_CLOCKED_MAX_MS,
        ));
        let handle = ClockedMockPcmPlaybackHandle {
            state: Arc::clone(&state),
        };
        (
            Self {
                state,
                realtime_thread: None,
            },
            handle,
        )
    }

    /// Create a [`ClockedMockPcmPlayback`] sink with default `target_ms = 40.0` and `max_ms = 160.0`.
    pub fn new(dac_rate_hz: f64) -> Self {
        Self::pair(dac_rate_hz).0
    }

    /// Return a cloned [`ClockedMockPcmPlaybackHandle`] for inspecting state or stepping time.
    pub fn handle(&self) -> ClockedMockPcmPlaybackHandle {
        ClockedMockPcmPlaybackHandle {
            state: Arc::clone(&self.state),
        }
    }

    /// Configure the prebuffer depth in milliseconds before DAC consumption begins (default `40.0`).
    pub fn with_target_ms(self, target_ms: f64) -> Self {
        {
            let mut inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            let clamped = if target_ms.is_finite() && target_ms >= 0.0 {
                target_ms
            } else {
                DEFAULT_CLOCKED_TARGET_MS
            };
            inner.target_ms = clamped;
            inner.target_stereo_frames = ms_to_stereo_frames(clamped);
            if inner.max_ms < clamped {
                inner.max_ms = clamped;
                inner.max_stereo_frames = ms_to_stereo_frames(clamped).max(1);
            }
        }
        self
    }

    /// Configure the maximum buffer capacity in milliseconds before oldest stereo frames are dropped (default `160.0`).
    pub fn with_max_ms(self, max_ms: f64) -> Self {
        {
            let mut inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            let clamped = if max_ms.is_finite() && max_ms > 0.0 {
                max_ms.max(inner.target_ms)
            } else {
                DEFAULT_CLOCKED_MAX_MS.max(inner.target_ms)
            };
            inner.max_ms = clamped;
            inner.max_stereo_frames = ms_to_stereo_frames(clamped).max(1);
        }
        self
    }

    /// Enable or disable recording of drained PCM samples in memory (enabled by default).
    pub fn with_record_output(self, record: bool) -> Self {
        {
            let mut inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.record_output = record;
        }
        self
    }

    /// Spawn a background thread that drains the simulated DAC against wall-clock elapsed time.
    pub fn with_realtime_clock(mut self) -> Self {
        if self.realtime_thread.is_some() {
            return self;
        }
        let state = Arc::clone(&self.state);
        state.stop_realtime.store(false, Ordering::Release);
        let handle = thread::Builder::new()
            .name("clocked-mock-dac".to_string())
            .spawn(move || {
                let mut last_tick = Instant::now();
                while !state.stop_realtime.load(Ordering::Acquire) {
                    thread::sleep(REALTIME_CLOCK_TICK);
                    if state.stop_realtime.load(Ordering::Acquire) {
                        break;
                    }
                    let now = Instant::now();
                    let dt = now.saturating_duration_since(last_tick);
                    last_tick = now;
                    state.advance_dac_secs(dt.as_secs_f64());
                }
            })
            .expect("spawn clocked-mock-dac thread");
        self.realtime_thread = Some(handle);
        self
    }

    /// Advance the simulated DAC clock by `elapsed` (virtual time, no wall-clock sleep).
    pub fn advance_dac_time(&self, elapsed: Duration) {
        self.state.advance_dac_secs(elapsed.as_secs_f64());
    }

    /// Drain DAC output corresponding to `nominal_stereo_frames` at 48 kHz, scaled by `dac_rate_hz / 48000`.
    pub fn drain_samples_for_frames(&self, nominal_stereo_frames: usize) {
        self.state.drain_samples_for_frames(nominal_stereo_frames);
    }
}

impl Default for ClockedMockPcmPlayback {
    fn default() -> Self {
        Self::new(f64::from(PCM_SAMPLE_RATE))
    }
}

impl PcmPlayback for ClockedMockPcmPlayback {
    fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()> {
        let even_len = samples.len() & !1;
        if even_len == 0 {
            return Ok(());
        }

        let mut inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.queue.extend(&samples[..even_len]);
        inner.written_samples += even_len as u64;

        let max_samples = inner.max_stereo_frames * 2;
        if inner.queue.len() > max_samples {
            let excess_samples = inner.queue.len() - max_samples;
            // Keep drops stereo-aligned so L/R channel pairing is preserved.
            let drop_samples = excess_samples.div_ceil(2) * 2;
            inner.queue.drain(..drop_samples);
            inner.overrun_dropped_samples += drop_samples as u64;
            self.state.overruns.fetch_add(1, Ordering::Release);
        }

        if !inner.primed && inner.buffered_stereo_frames() >= inner.target_stereo_frames {
            inner.primed = true;
        }
        inner.record_fill();
        Ok(())
    }

    fn set_volume(&self, pct: u8) {
        let clamped = pct.min(150);
        self.state.volume.store(clamped, Ordering::Release);
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .volume_history
            .push(clamped);
    }

    fn set_mute(&self, muted: bool) {
        self.state.muted.store(muted, Ordering::Release);
        self.state
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mute_history
            .push(muted);
    }

    fn volume(&self) -> u8 {
        self.state.volume.load(Ordering::Acquire)
    }

    fn is_muted(&self) -> bool {
        self.state.muted.load(Ordering::Acquire)
    }

    fn buffered_ms(&self) -> Option<f64> {
        Some(self.state.buffered_ms())
    }

    fn sink_target_ms(&self) -> f64 {
        self.state.target_ms()
    }

    fn sink_underruns(&self) -> u64 {
        self.state.underruns.load(Ordering::Acquire)
    }

    fn sink_overruns(&self) -> u64 {
        self.state.overruns.load(Ordering::Acquire)
    }
}

impl Drop for ClockedMockPcmPlayback {
    fn drop(&mut self) {
        self.state.stop_realtime.store(true, Ordering::Release);
        if let Some(handle) = self.realtime_thread.take() {
            let _ = handle.join();
        }
        self.state.closed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_capture_and_playback_roundtrip_and_controls() {
        let mut cap = MockPcmCapture::sine_wave(440.0, Some(3));
        let (mut play, handle) = MockPcmPlayback::pair();

        let mut buf = vec![0i16; PCM_FRAME_SAMPLES_20MS];
        let mut count = 0;
        loop {
            let n = cap.read_frame(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            assert_eq!(n, PCM_FRAME_SAMPLES_20MS);
            play.write_frame(&buf[..n]).unwrap();
            count += 1;
        }
        assert_eq!(count, 3);
        assert_eq!(handle.frame_count(), 3);
        assert_eq!(handle.total_samples(), 3 * PCM_FRAME_SAMPLES_20MS);
        assert!(handle.total_energy() > 100_000);

        play.set_volume(75);
        play.set_mute(true);
        play.set_volume(200); // clamped to 150
        assert_eq!(handle.volume(), 150);
        assert!(handle.is_muted());
        assert_eq!(handle.volume_history(), vec![75, 150]);
        assert_eq!(handle.mute_history(), vec![true]);
        assert!(!handle.is_closed());

        drop(play);
        assert!(handle.is_closed());
    }

    #[test]
    fn mock_playback_uses_default_clock_feedback_methods() {
        let (play, _handle) = MockPcmPlayback::pair();
        assert_eq!(play.buffered_ms(), None);
        assert_eq!(play.sink_target_ms(), 40.0);
        assert_eq!(play.sink_underruns(), 0);
        assert_eq!(play.sink_overruns(), 0);
    }

    #[test]
    fn clocked_mock_prebuffers_and_underruns_on_slow_feed() {
        let (mut play, handle) = ClockedMockPcmPlayback::pair(48_009.6); // +200 ppm fast DAC
        assert_eq!(play.buffered_ms(), Some(0.0));
        assert_eq!(play.sink_target_ms(), 40.0);
        assert!(!handle.is_primed());

        let frame = vec![1234i16; PCM_FRAME_SAMPLES_20MS];
        // First 20 ms frame stays in prebuffer; advancing DAC does not consume yet.
        play.write_frame(&frame).unwrap();
        assert!(!handle.is_primed());
        play.advance_dac_time(Duration::from_millis(20));
        assert_eq!(play.buffered_ms(), Some(20.0));
        assert_eq!(play.sink_underruns(), 0);

        // Second 20 ms frame reaches target_ms = 40.0 and primes the DAC.
        play.write_frame(&frame).unwrap();
        assert!(handle.is_primed());
        assert_eq!(play.buffered_ms(), Some(40.0));

        // 5 simulated minutes (15,000 * 20 ms): +200 ppm DAC consumes ~60 ms more than
        // nominal 48 kHz feed, exhausting the 40 ms cushion and causing underruns.
        for _ in 0..15_000 {
            play.write_frame(&frame).unwrap();
            play.advance_dac_time(Duration::from_millis(20));
        }

        assert!(play.sink_underruns() > 0);
        assert_eq!(play.sink_overruns(), 0);
        assert!(handle.underrun_samples() > 0);
        assert_eq!(handle.min_fill_ms(), Some(0.0));
        assert!(handle.total_output_samples() > 15_000 * PCM_FRAME_SAMPLES_20MS);
    }

    #[test]
    fn clocked_mock_overruns_on_fast_feed_with_stereo_alignment() {
        let mut play = ClockedMockPcmPlayback::new(47_990.4) // -200 ppm slow DAC
            .with_target_ms(40.0)
            .with_max_ms(160.0);
        let handle = play.handle();
        assert_eq!(handle.dac_rate_hz(), 47_990.4);
        assert_eq!(handle.target_ms(), 40.0);
        assert_eq!(handle.max_ms(), 160.0);

        // Build 20 ms stereo-tagged frames where L = +tag and R = -tag.
        let mut frame = vec![0i16; PCM_FRAME_SAMPLES_20MS];
        // Prime to 40 ms first.
        for i in 0..(PCM_FRAME_SAMPLES_20MS / 2) {
            let tag = ((i % 10_000) + 1) as i16;
            frame[i * 2] = tag;
            frame[i * 2 + 1] = -tag;
        }
        play.write_frame(&frame).unwrap();
        play.write_frame(&frame).unwrap();
        assert!(handle.is_primed());

        // 20 simulated minutes (60,000 * 20 ms): -200 ppm DAC consumes ~240 ms less than
        // nominal 48 kHz feed, exceeding the 120 ms headroom (40 -> 160 ms) and triggering overruns.
        for _ in 0..60_000 {
            play.write_frame(&frame).unwrap();
            handle.drain_samples_for_frames(PCM_FRAME_SAMPLES_20MS / 2);
        }

        assert!(play.sink_overruns() > 0);
        assert_eq!(play.sink_underruns(), 0);
        assert!(handle.buffered_ms() <= 160.0 + 1e-9);
        assert!(handle.overrun_dropped_samples() > 0);
        assert_eq!(handle.overrun_dropped_samples() % 2, 0);
        assert!(handle.max_fill_ms().unwrap() <= 160.0 + 1e-9);

        let out = handle.output_samples();
        assert!(!out.is_empty());
        assert_eq!(out.len() % 2, 0);
        let (pairs, remainder) = out.as_chunks::<2>();
        assert!(remainder.is_empty());
        for pair in pairs {
            assert_eq!(
                pair[0], -pair[1],
                "stereo L/R alignment broken across overrun"
            );
        }
    }

    #[test]
    fn clocked_mock_fill_history_controls_and_realtime_clock() {
        let (mut play, handle) = ClockedMockPcmPlayback::pair(48_000.0);
        play = play.with_target_ms(20.0).with_max_ms(100.0);

        let frame = vec![500i16; PCM_FRAME_SAMPLES_20MS];
        play.write_frame(&frame).unwrap(); // 20 ms -> primed
        play.write_frame(&frame).unwrap(); // 40 ms
        play.advance_dac_time(Duration::from_millis(20)); // 20 ms

        assert_eq!(handle.min_fill_ms(), Some(20.0));
        assert_eq!(handle.max_fill_ms(), Some(40.0));
        let avg = handle.avg_fill_ms().unwrap();
        assert!((avg - (20.0 + 40.0 + 20.0) / 3.0).abs() < 1e-6);
        assert_eq!(handle.fill_history().len(), 3);

        handle.reset_fill_history();
        assert_eq!(handle.min_fill_ms(), None);
        assert!(handle.fill_history().is_empty());

        play.set_volume(80);
        play.set_mute(true);
        assert_eq!(play.volume(), 80);
        assert!(play.is_muted());
        assert_eq!(handle.volume_history(), vec![80]);
        assert_eq!(handle.mute_history(), vec![true]);

        // Verify real-time wall-clock background draining and clean Drop join.
        let mut rt_play = ClockedMockPcmPlayback::new(48_000.0)
            .with_target_ms(20.0)
            .with_realtime_clock();
        let rt_handle = rt_play.handle();
        rt_play.write_frame(&frame).unwrap();
        rt_play.write_frame(&frame).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while rt_handle.total_output_samples() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(rt_handle.total_output_samples() > 0);
        assert!(!rt_handle.is_closed());
        drop(rt_play);
        assert!(rt_handle.is_closed());
    }
}
