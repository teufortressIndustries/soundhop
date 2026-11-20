//! Audio playback and loopback capture for Windows (`cpal`/WASAPI),
//! with cross-platform mock fallbacks so unit tests run on both Linux and Windows.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_PLAYBACK_BUFFER_SAMPLES: usize = 96_000; // 1.0s @ 48kHz stereo
const RECONNECT_INITIAL_BACKOFF_MS: u64 = 100;
const RECONNECT_MAX_BACKOFF_MS: u64 = 1_000;

#[derive(Debug, Clone)]
pub struct AudioEndpoint {
    pub name: String,
    pub channels: u8,
    pub sample_rate: u32,
}

pub fn list_audio_endpoints() -> Vec<AudioEndpoint> {
    #[cfg(all(windows, not(test)))]
    {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        if let Ok(devices) = host.output_devices() {
            let mut out = Vec::new();
            for dev in devices {
                let name = dev.name().unwrap_or_else(|_| "Speakers".to_string());
                let (channels, sample_rate) = dev
                    .default_output_config()
                    .map(|c| (c.channels() as u8, c.sample_rate().0))
                    .unwrap_or((2, 48_000));
                out.push(AudioEndpoint {
                    name,
                    channels,
                    sample_rate,
                });
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    vec![AudioEndpoint {
        name: "Speakers (Default Audio)".to_string(),
        channels: 2,
        sample_rate: 48_000,
    }]
}

/// Apply volume percentage (`0..=150`) and mute scaling to a signed 16-bit PCM sample.
#[inline]
pub fn apply_gain_i16(sample: i16, volume_pct: u32, muted: bool) -> i16 {
    if muted || volume_pct == 0 {
        return 0;
    }
    if volume_pct == 100 {
        return sample;
    }
    let gain = (volume_pct.min(150) as f32) / 100.0;
    (f32::from(sample) * gain)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

#[inline]
fn lerp_i16(a: i16, b: i16, t: f64) -> i16 {
    let v = f64::from(a) + (f64::from(b) - f64::from(a)) * t;
    v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// Persistent-phase linear resampler for push-based stereo frame streams (capture path).
///
/// Carries fractional sample position and the previous boundary frame across callbacks so
/// sample-rate conversion (e.g. 44.1 kHz <-> 48 kHz) has zero phase discontinuities or clicks.
#[derive(Debug, Clone)]
pub struct LinearResampler {
    src_rate: u32,
    dst_rate: u32,
    /// Exact rational sample position in units of `1 / dst_rate` relative to the start of the
    /// current input slice (may be negative to interpolate between `prev` and `frames[0]`).
    pos_num: i64,
    prev: (i16, i16),
    has_prev: bool,
}

impl LinearResampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            src_rate: src_rate.max(1),
            dst_rate: dst_rate.max(1),
            pos_num: 0,
            prev: (0, 0),
            has_prev: false,
        }
    }

    /// Resample stereo `frames` into interleaved S16LE bytes appended to `out`.
    pub fn resample_to_s16le(&mut self, frames: &[(i16, i16)], out: &mut Vec<u8>) {
        if frames.is_empty() {
            return;
        }
        if self.src_rate == self.dst_rate {
            out.reserve(frames.len() * 4);
            for &(l, r) in frames {
                out.extend_from_slice(&l.to_le_bytes());
                out.extend_from_slice(&r.to_le_bytes());
            }
            return;
        }

        if !self.has_prev {
            self.prev = frames[0];
            self.has_prev = true;
            self.pos_num = 0;
        }

        let src = i64::from(self.src_rate);
        let dst = i64::from(self.dst_rate);
        let dst_f = dst as f64;
        let limit_num = (frames.len() as i64 - 1) * dst;
        let est_out = ((frames.len() as f64) * dst_f / (src as f64)).ceil() as usize + 2;
        out.reserve(est_out * 4);

        while self.pos_num < limit_num {
            let (a, b, t) = if self.pos_num < 0 {
                (self.prev, frames[0], (self.pos_num + dst) as f64 / dst_f)
            } else {
                let idx = (self.pos_num / dst) as usize;
                let rem = self.pos_num % dst;
                (frames[idx], frames[idx + 1], rem as f64 / dst_f)
            };
            let l = lerp_i16(a.0, b.0, t);
            let r = lerp_i16(a.1, b.1, t);
            out.extend_from_slice(&l.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
            self.pos_num += src;
        }

        self.prev = frames[frames.len() - 1];
        self.pos_num -= (frames.len() as i64) * dst;
    }
}

/// Persistent-phase linear resampler for pull-based playback callbacks reading from a `VecDeque<i16>`.
#[derive(Debug, Clone)]
pub struct PlaybackResampler {
    src_rate: u32,
    dst_rate: u32,
    phase: f64,
    prev: (i16, i16),
    next: (i16, i16),
    initialized: bool,
}

impl PlaybackResampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            src_rate: src_rate.max(1),
            dst_rate: dst_rate.max(1),
            phase: 0.0,
            prev: (0, 0),
            next: (0, 0),
            initialized: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn write_frames<F: FnMut(usize, i16)>(
        &mut self,
        buf: &mut VecDeque<i16>,
        src_channels: usize,
        dst_channels: usize,
        out_frames: usize,
        volume_pct: u32,
        muted: bool,
        mut write_sample: F,
    ) {
        let dst_channels = dst_channels.max(1);
        if self.src_rate == self.dst_rate {
            for f in 0..out_frames {
                let (raw_l, raw_r) = pop_frame_pair(buf, src_channels);
                let l = apply_gain_i16(raw_l, volume_pct, muted);
                let r = apply_gain_i16(raw_r, volume_pct, muted);
                write_stereo_to_channels(f, dst_channels, l, r, &mut write_sample);
            }
            return;
        }

        if !self.initialized {
            self.prev = pop_frame_pair(buf, src_channels);
            self.next = pop_frame_pair(buf, src_channels);
            self.initialized = true;
        }

        let step = f64::from(self.src_rate) / f64::from(self.dst_rate);
        for f in 0..out_frames {
            let interp_l = lerp_i16(self.prev.0, self.next.0, self.phase);
            let interp_r = lerp_i16(self.prev.1, self.next.1, self.phase);
            let l = apply_gain_i16(interp_l, volume_pct, muted);
            let r = apply_gain_i16(interp_r, volume_pct, muted);
            write_stereo_to_channels(f, dst_channels, l, r, &mut write_sample);

            self.phase += step;
            while self.phase >= 1.0 {
                self.phase -= 1.0;
                self.prev = self.next;
                self.next = pop_frame_pair(buf, src_channels);
            }
        }
    }
}

#[inline]
fn write_stereo_to_channels<F: FnMut(usize, i16)>(
    frame_idx: usize,
    dst_channels: usize,
    l: i16,
    r: i16,
    write_sample: &mut F,
) {
    let base = frame_idx * dst_channels;
    for ch in 0..dst_channels {
        let s = match ch {
            0 => l,
            1 => r,
            _ => ((i32::from(l) + i32::from(r)) / 2) as i16,
        };
        write_sample(base + ch, s);
    }
}

/// Bounded exponential backoff for WASAPI hotplug / `AUDCLNT_E_DEVICE_INVALIDATED` recovery.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    initial_ms: u64,
    max_ms: u64,
    current_ms: u64,
}

impl ReconnectBackoff {
    pub fn new(initial_ms: u64, max_ms: u64) -> Self {
        let init = initial_ms.max(10);
        let max = max_ms.max(init);
        Self {
            initial_ms: init,
            max_ms: max,
            current_ms: init,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = Duration::from_millis(self.current_ms);
        self.current_ms = self.current_ms.saturating_mul(2).min(self.max_ms);
        delay
    }

    pub fn reset(&mut self) {
        self.current_ms = self.initial_ms;
    }
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self::new(RECONNECT_INITIAL_BACKOFF_MS, RECONNECT_MAX_BACKOFF_MS)
    }
}

#[cfg(all(windows, not(test)))]
fn sleep_interruptible(delay: Duration, stop_flag: &AtomicBool) -> bool {
    let mut remaining = delay;
    let step = Duration::from_millis(25);
    while !remaining.is_zero() {
        if stop_flag.load(Ordering::SeqCst) {
            return true;
        }
        let nap = remaining.min(step);
        thread::sleep(nap);
        remaining = remaining.saturating_sub(nap);
    }
    stop_flag.load(Ordering::SeqCst)
}

pub struct WindowsPlayer {
    pub sample_rate: u32,
    pub channels: u8,
    closed: Arc<AtomicBool>,
    buffer: Arc<Mutex<VecDeque<i16>>>,
    volume_pct: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl WindowsPlayer {
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self::with_volume(
            sample_rate,
            channels,
            Arc::new(AtomicU32::new(100)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub fn with_volume(
        sample_rate: u32,
        channels: u8,
        volume_pct: Arc<AtomicU32>,
        muted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            closed: Arc::new(AtomicBool::new(false)),
            buffer: Arc::new(Mutex::new(VecDeque::with_capacity(16_384))),
            volume_pct,
            muted,
            worker: Mutex::new(None),
        }
    }

    pub fn set_volume(&self, volume_pct: u32) {
        self.volume_pct.store(volume_pct.min(150), Ordering::SeqCst);
    }

    pub fn set_mute(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
    }

    pub fn volume(&self) -> u32 {
        self.volume_pct.load(Ordering::SeqCst)
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    pub fn start(&self) {
        self.closed.store(false, Ordering::SeqCst);
        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return;
        }
        let closed = Arc::clone(&self.closed);
        let buffer = Arc::clone(&self.buffer);
        let volume_pct = Arc::clone(&self.volume_pct);
        let muted = Arc::clone(&self.muted);
        let src_rate = self.sample_rate.max(8_000);
        let src_channels = self.channels.max(1) as usize;
        *guard = Some(thread::spawn(move || {
            run_playback_worker(closed, buffer, volume_pct, muted, src_rate, src_channels);
        }));
    }

    pub fn write(&self, pcm_data: &[u8]) {
        if self.closed.load(Ordering::SeqCst) || pcm_data.is_empty() {
            return;
        }
        let mut buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        let incoming_samples = pcm_data.len() / 2;
        if buf.len() + incoming_samples > MAX_PLAYBACK_BUFFER_SAMPLES {
            let overflow = (buf.len() + incoming_samples) - MAX_PLAYBACK_BUFFER_SAMPLES;
            let drop_n = overflow.min(buf.len());
            // Keep channel alignment when dropping frames
            let aligned_drop = drop_n - (drop_n % self.channels.max(1) as usize);
            buf.drain(..aligned_drop);
        }
        for &pair in pcm_data.as_chunks::<2>().0 {
            buf.push_back(i16::from_le_bytes(pair));
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        if let Some(h) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }
}

impl Drop for WindowsPlayer {
    fn drop(&mut self) {
        self.close();
    }
}

fn pop_frame_pair(buf: &mut VecDeque<i16>, src_channels: usize) -> (i16, i16) {
    if src_channels <= 1 {
        let s = buf.pop_front().unwrap_or(0);
        return (s, s);
    }
    let l = buf.pop_front().unwrap_or(0);
    let r = buf.pop_front().unwrap_or(l);
    for _ in 2..src_channels {
        let _ = buf.pop_front();
    }
    (l, r)
}

#[cfg(all(windows, not(test)))]
fn run_playback_worker(
    closed: Arc<AtomicBool>,
    buffer: Arc<Mutex<VecDeque<i16>>>,
    volume_pct: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    src_rate: u32,
    src_channels: usize,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};
    use std::time::Instant;

    let mut backoff = ReconnectBackoff::default();

    while !closed.load(Ordering::SeqCst) {
        let host = cpal::default_host();
        let Some(device) = host.default_output_device() else {
            drain_stale_buffer(&buffer);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
            continue;
        };
        let active_name = device.name().ok();
        let Ok(default_cfg) = device.default_output_config() else {
            drain_stale_buffer(&buffer);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
            continue;
        };

        let format = default_cfg.sample_format();
        let config: StreamConfig = default_cfg.into();
        let dst_channels = (config.channels as usize).max(1);
        let dst_rate = config.sample_rate.0.max(8_000);
        let stream_broken = Arc::new(AtomicBool::new(false));

        let stream_opt = match format {
            SampleFormat::F32 => {
                let buf_c = Arc::clone(&buffer);
                let vol_c = Arc::clone(&volume_pct);
                let mute_c = Arc::clone(&muted);
                let err_flag = Arc::clone(&stream_broken);
                let mut resampler = PlaybackResampler::new(src_rate, dst_rate);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            let vol = vol_c.load(Ordering::Relaxed);
                            let is_muted = mute_c.load(Ordering::Relaxed);
                            let mut guard = buf_c.lock().unwrap_or_else(|e| e.into_inner());
                            resampler.write_frames(
                                &mut guard,
                                src_channels,
                                dst_channels,
                                data.len() / dst_channels,
                                vol,
                                is_muted,
                                |idx, s| {
                                    data[idx] = f32::from(s) / 32768.0;
                                },
                            );
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            SampleFormat::I16 => {
                let buf_c = Arc::clone(&buffer);
                let vol_c = Arc::clone(&volume_pct);
                let mute_c = Arc::clone(&muted);
                let err_flag = Arc::clone(&stream_broken);
                let mut resampler = PlaybackResampler::new(src_rate, dst_rate);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _| {
                            let vol = vol_c.load(Ordering::Relaxed);
                            let is_muted = mute_c.load(Ordering::Relaxed);
                            let mut guard = buf_c.lock().unwrap_or_else(|e| e.into_inner());
                            resampler.write_frames(
                                &mut guard,
                                src_channels,
                                dst_channels,
                                data.len() / dst_channels,
                                vol,
                                is_muted,
                                |idx, s| {
                                    data[idx] = s;
                                },
                            );
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            SampleFormat::U16 => {
                let buf_c = Arc::clone(&buffer);
                let vol_c = Arc::clone(&volume_pct);
                let mute_c = Arc::clone(&muted);
                let err_flag = Arc::clone(&stream_broken);
                let mut resampler = PlaybackResampler::new(src_rate, dst_rate);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [u16], _| {
                            let vol = vol_c.load(Ordering::Relaxed);
                            let is_muted = mute_c.load(Ordering::Relaxed);
                            let mut guard = buf_c.lock().unwrap_or_else(|e| e.into_inner());
                            resampler.write_frames(
                                &mut guard,
                                src_channels,
                                dst_channels,
                                data.len() / dst_channels,
                                vol,
                                is_muted,
                                |idx, s| {
                                    data[idx] = (i32::from(s) + 32768) as u16;
                                },
                            );
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            _ => None,
        };

        let Some(stream) = stream_opt else {
            drain_stale_buffer(&buffer);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
            continue;
        };

        if stream.play().is_err() {
            drop(stream);
            drain_stale_buffer(&buffer);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
            continue;
        }

        let opened_at = Instant::now();
        let mut ticks: u32 = 0;
        while !closed.load(Ordering::SeqCst) && !stream_broken.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
            ticks = ticks.wrapping_add(1);
            if opened_at.elapsed() >= Duration::from_secs(2) {
                backoff.reset();
            }
            // Periodically check if the Windows default output device changed (hotplug/switch).
            if ticks.is_multiple_of(100) {
                if let Some(ref cur_name) = active_name {
                    let latest = cpal::default_host()
                        .default_output_device()
                        .and_then(|d| d.name().ok());
                    if latest.is_some() && latest.as_ref() != Some(cur_name) {
                        break;
                    }
                }
            }
        }
        drop(stream);

        if !closed.load(Ordering::SeqCst) {
            drain_stale_buffer(&buffer);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
        }
    }
}

#[cfg(all(windows, not(test)))]
fn drain_stale_buffer(buffer: &Mutex<VecDeque<i16>>) {
    let mut g = buffer.lock().unwrap_or_else(|e| e.into_inner());
    let drain_n = g.len().min(3_840);
    g.drain(..drain_n);
}

#[cfg(any(not(windows), test))]
fn run_playback_worker(
    closed: Arc<AtomicBool>,
    buffer: Arc<Mutex<VecDeque<i16>>>,
    _volume_pct: Arc<AtomicU32>,
    _muted: Arc<AtomicBool>,
    _src_rate: u32,
    _src_channels: usize,
) {
    run_fallback_drain(closed, buffer);
}

#[cfg(any(not(windows), test))]
fn run_fallback_drain(closed: Arc<AtomicBool>, buffer: Arc<Mutex<VecDeque<i16>>>) {
    while !closed.load(Ordering::SeqCst) {
        {
            let mut g = buffer.lock().unwrap_or_else(|e| e.into_inner());
            let drain_n = g.len().min(1_920);
            g.drain(..drain_n);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub struct WindowsLoopbackRecorder {
    stop_flag: Arc<AtomicBool>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl WindowsLoopbackRecorder {
    pub fn try_new<F>(callback: F) -> Result<Self, String>
    where
        F: Fn(&[u8]) + Send + Sync + 'static,
    {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&stop_flag);
        let cb = Arc::new(callback);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            run_loopback_worker(flag_clone, cb, tx);
        });
        match rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_flag,
                handle: Mutex::new(Some(handle)),
            }),
            Ok(Err(err)) => {
                stop_flag.store(true, Ordering::SeqCst);
                let _ = handle.join();
                Err(err)
            }
            Err(_) => {
                stop_flag.store(true, Ordering::SeqCst);
                let _ = handle.join();
                Err("WASAPI loopback worker exited unexpectedly".to_string())
            }
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.stop_flag.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }
}

impl Drop for WindowsLoopbackRecorder {
    fn drop(&mut self) {
        self.stop();
    }
}

type CaptureCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[cfg(all(windows, not(test)))]
fn open_loopback_stream(
    cb: &CaptureCallback,
) -> Result<(cpal::Stream, Arc<AtomicBool>, Option<String>), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};

    let host = cpal::default_host();
    // On WASAPI, capturing from an output device provides system loopback audio.
    // Never fall back to `default_input_device()`, which would silently capture a live mic.
    let Some(device) = host.default_output_device() else {
        return Err(
            "No default audio output device available for WASAPI loopback capture".to_string(),
        );
    };
    let active_name = device.name().ok();
    let default_cfg = device
        .default_output_config()
        .map_err(|e| format!("Failed to read default output config for WASAPI loopback: {e}"))?;

    let format = default_cfg.sample_format();
    let config: StreamConfig = default_cfg.into();
    let ch = (config.channels as usize).max(1);
    let rate = config.sample_rate.0.max(8_000);
    let stream_broken = Arc::new(AtomicBool::new(false));

    let stream = match format {
        SampleFormat::F32 => {
            let err_flag = Arc::clone(&stream_broken);
            let cb_c = Arc::clone(cb);
            let mut resampler = LinearResampler::new(rate, 48_000);
            let mut pairs = Vec::with_capacity(1_024);
            let mut bytes = Vec::with_capacity(4_096);
            device
                .build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        pairs.clear();
                        for frame in data.chunks_exact(ch) {
                            let l = (frame[0].clamp(-1.0, 1.0) * 32767.0) as i16;
                            let r = if ch > 1 {
                                (frame[1].clamp(-1.0, 1.0) * 32767.0) as i16
                            } else {
                                l
                            };
                            pairs.push((l, r));
                        }
                        bytes.clear();
                        resampler.resample_to_s16le(&pairs, &mut bytes);
                        if !bytes.is_empty() {
                            cb_c(&bytes);
                        }
                    },
                    move |_| err_flag.store(true, Ordering::SeqCst),
                    None,
                )
                .map_err(|e| format!("Failed to build F32 WASAPI loopback stream: {e}"))?
        }
        SampleFormat::I16 => {
            let err_flag = Arc::clone(&stream_broken);
            let cb_c = Arc::clone(cb);
            let mut resampler = LinearResampler::new(rate, 48_000);
            let mut pairs = Vec::with_capacity(1_024);
            let mut bytes = Vec::with_capacity(4_096);
            device
                .build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        pairs.clear();
                        for frame in data.chunks_exact(ch) {
                            let l = frame[0];
                            let r = if ch > 1 { frame[1] } else { l };
                            pairs.push((l, r));
                        }
                        bytes.clear();
                        resampler.resample_to_s16le(&pairs, &mut bytes);
                        if !bytes.is_empty() {
                            cb_c(&bytes);
                        }
                    },
                    move |_| err_flag.store(true, Ordering::SeqCst),
                    None,
                )
                .map_err(|e| format!("Failed to build I16 WASAPI loopback stream: {e}"))?
        }
        SampleFormat::U16 => {
            let err_flag = Arc::clone(&stream_broken);
            let cb_c = Arc::clone(cb);
            let mut resampler = LinearResampler::new(rate, 48_000);
            let mut pairs = Vec::with_capacity(1_024);
            let mut bytes = Vec::with_capacity(4_096);
            device
                .build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        pairs.clear();
                        for frame in data.chunks_exact(ch) {
                            let l = (i32::from(frame[0]) - 32768) as i16;
                            let r = if ch > 1 {
                                (i32::from(frame[1]) - 32768) as i16
                            } else {
                                l
                            };
                            pairs.push((l, r));
                        }
                        bytes.clear();
                        resampler.resample_to_s16le(&pairs, &mut bytes);
                        if !bytes.is_empty() {
                            cb_c(&bytes);
                        }
                    },
                    move |_| err_flag.store(true, Ordering::SeqCst),
                    None,
                )
                .map_err(|e| format!("Failed to build U16 WASAPI loopback stream: {e}"))?
        }
        other => {
            return Err(format!(
                "Unsupported WASAPI loopback sample format: {other:?}"
            ));
        }
    };

    stream
        .play()
        .map_err(|e| format!("Failed to start WASAPI loopback stream: {e}"))?;

    Ok((stream, stream_broken, active_name))
}

#[cfg(all(windows, not(test)))]
fn run_loopback_worker(
    stop_flag: Arc<AtomicBool>,
    cb: CaptureCallback,
    startup_tx: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    use cpal::traits::{DeviceTrait, HostTrait};
    use std::time::Instant;

    let initial = match open_loopback_stream(&cb) {
        Ok(tuple) => tuple,
        Err(err) => {
            stop_flag.store(true, Ordering::SeqCst);
            let _ = startup_tx.send(Err(err));
            return;
        }
    };

    let _ = startup_tx.send(Ok(()));

    let mut current = Some(initial);
    let mut backoff = ReconnectBackoff::default();

    while !stop_flag.load(Ordering::SeqCst) {
        let (stream, stream_broken, active_name) = match current.take() {
            Some(tuple) => tuple,
            None => match open_loopback_stream(&cb) {
                Ok(tuple) => tuple,
                Err(_) => {
                    if sleep_interruptible(backoff.next_delay(), &stop_flag) {
                        break;
                    }
                    continue;
                }
            },
        };

        let opened_at = Instant::now();
        let mut ticks: u32 = 0;
        while !stop_flag.load(Ordering::SeqCst) && !stream_broken.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
            ticks = ticks.wrapping_add(1);
            if opened_at.elapsed() >= Duration::from_secs(2) {
                backoff.reset();
            }
            if ticks.is_multiple_of(100) {
                if let Some(ref cur_name) = active_name {
                    let latest = cpal::default_host()
                        .default_output_device()
                        .and_then(|d| d.name().ok());
                    if latest.is_some() && latest.as_ref() != Some(cur_name) {
                        break;
                    }
                }
            }
        }
        drop(stream);

        if !stop_flag.load(Ordering::SeqCst)
            && sleep_interruptible(backoff.next_delay(), &stop_flag)
        {
            break;
        }
    }
}

#[cfg(any(not(windows), test))]
fn run_loopback_worker(
    stop_flag: Arc<AtomicBool>,
    cb: CaptureCallback,
    startup_tx: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let _ = startup_tx.send(Ok(()));
    let silence = vec![0u8; 4096];
    while !stop_flag.load(Ordering::SeqCst) {
        cb(&silence);
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_frames(freq_hz: f64, sample_rate: u32, count: usize) -> Vec<(i16, i16)> {
        (0..count)
            .map(|i| {
                let t = (i as f64) / f64::from(sample_rate);
                let l = (2.0 * std::f64::consts::PI * freq_hz * t).sin();
                let r = (2.0 * std::f64::consts::PI * freq_hz * t).cos();
                ((l * 24000.0).round() as i16, (r * 24000.0).round() as i16)
            })
            .collect()
    }

    fn decode_s16le_stereo(bytes: &[u8]) -> Vec<(i16, i16)> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| {
                (
                    i16::from_le_bytes([c[0], c[1]]),
                    i16::from_le_bytes([c[2], c[3]]),
                )
            })
            .collect()
    }

    #[test]
    fn test_linear_resampler_passthrough_48k() {
        let input = sine_frames(440.0, 48_000, 256);
        let mut r = LinearResampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.resample_to_s16le(&input, &mut out);
        assert_eq!(decode_s16le_stereo(&out), input);
    }

    #[test]
    fn test_linear_resampler_chunk_continuity_44100_to_48000() {
        let input = sine_frames(440.0, 44_100, 4_410); // 100ms

        let mut single = LinearResampler::new(44_100, 48_000);
        let mut single_bytes = Vec::new();
        single.resample_to_s16le(&input, &mut single_bytes);

        let mut chunked = LinearResampler::new(44_100, 48_000);
        let mut chunked_bytes = Vec::new();
        let chunk_sizes = [73usize, 128, 17, 500, 311, 441, 64, 999];
        let mut idx = 0;
        let mut ci = 0;
        while idx < input.len() {
            let sz = chunk_sizes[ci % chunk_sizes.len()].min(input.len() - idx);
            chunked.resample_to_s16le(&input[idx..idx + sz], &mut chunked_bytes);
            idx += sz;
            ci += 1;
        }

        // Persistent-phase resampling across arbitrary callback boundaries must be bit-identical
        assert_eq!(chunked_bytes, single_bytes);

        // Output frame count should match 48000 / 44100 ratio closely (~4800 frames)
        let frames = decode_s16le_stereo(&chunked_bytes);
        assert!(
            (4795..=4805).contains(&frames.len()),
            "unexpected frame count: {}",
            frames.len()
        );

        // Verify smooth waveform with no click/discontinuity across callbacks
        // Max step for 440Hz @ 48kHz, amp 24000 is ~24000 * 2*pi*440/48000 ~= 1382
        let max_delta = frames
            .windows(2)
            .map(|w| (i32::from(w[1].0) - i32::from(w[0].0)).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_delta < 1_600,
            "unexpected discontinuity in resampled sine wave: max_delta={max_delta}"
        );
    }

    #[test]
    fn test_linear_resampler_downsampling_96000_to_48000() {
        let input = sine_frames(1_000.0, 96_000, 960);
        let mut single = LinearResampler::new(96_000, 48_000);
        let mut single_bytes = Vec::new();
        single.resample_to_s16le(&input, &mut single_bytes);

        let mut chunked = LinearResampler::new(96_000, 48_000);
        let mut chunked_bytes = Vec::new();
        for chunk in input.chunks(37) {
            chunked.resample_to_s16le(chunk, &mut chunked_bytes);
        }

        assert_eq!(chunked_bytes, single_bytes);
        let frames = decode_s16le_stereo(&chunked_bytes);
        assert!((478..=482).contains(&frames.len()));
    }

    #[test]
    fn test_playback_resampler_continuity_across_callbacks() {
        let input = sine_frames(440.0, 48_000, 4_800);
        let mut queue = VecDeque::with_capacity(input.len() * 2);
        for &(l, r) in &input {
            queue.push_back(l);
            queue.push_back(r);
        }

        let mut resampler = PlaybackResampler::new(48_000, 44_100);
        let mut out = Vec::new();
        // Simulate 10 separate WASAPI callbacks of 400 frames each
        for _ in 0..10 {
            let mut block = vec![0i16; 400 * 2];
            resampler.write_frames(&mut queue, 2, 2, 400, 100, false, |i, s| {
                block[i] = s;
            });
            for pair in block.as_chunks::<2>().0 {
                out.push((pair[0], pair[1]));
            }
        }

        assert_eq!(out.len(), 4_000);
        // Max step for 440Hz @ 44.1kHz, amp 24000 is ~24000 * 2*pi*440/44100 ~= 1504
        let max_delta = out
            .windows(2)
            .map(|w| (i32::from(w[1].0) - i32::from(w[0].0)).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_delta < 1_700,
            "playback resampler click detected: max_delta={max_delta}"
        );
    }

    #[test]
    fn test_apply_gain_i16_scaling_mute_and_clamping() {
        assert_eq!(apply_gain_i16(20_000, 100, false), 20_000);
        assert_eq!(apply_gain_i16(-20_000, 100, false), -20_000);
        assert_eq!(apply_gain_i16(20_000, 50, false), 10_000);
        assert_eq!(apply_gain_i16(-20_000, 50, false), -10_000);
        assert_eq!(apply_gain_i16(20_000, 0, false), 0);
        assert_eq!(apply_gain_i16(20_000, 100, true), 0);
        assert_eq!(apply_gain_i16(20_000, 150, false), 30_000);
        // Boost above 100% must saturate instead of wrapping
        assert_eq!(apply_gain_i16(30_000, 150, false), i16::MAX);
        assert_eq!(apply_gain_i16(-30_000, 150, false), i16::MIN);
        assert_eq!(apply_gain_i16(30_000, 300, false), i16::MAX);
    }

    #[test]
    fn test_windows_player_shared_volume_and_mute() {
        let vol = Arc::new(AtomicU32::new(80));
        let muted = Arc::new(AtomicBool::new(false));
        let player = WindowsPlayer::with_volume(48_000, 2, Arc::clone(&vol), Arc::clone(&muted));
        assert_eq!(player.volume(), 80);
        assert!(!player.is_muted());

        player.set_volume(125);
        player.set_mute(true);
        assert_eq!(vol.load(Ordering::SeqCst), 125);
        assert!(muted.load(Ordering::SeqCst));

        // Clamped to 150%
        player.set_volume(500);
        assert_eq!(player.volume(), 150);
    }

    #[test]
    fn test_reconnect_backoff_bounded_exponential() {
        let mut b = ReconnectBackoff::new(100, 1_000);
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(400));
        assert_eq!(b.next_delay(), Duration::from_millis(800));
        assert_eq!(b.next_delay(), Duration::from_millis(1_000));
        assert_eq!(b.next_delay(), Duration::from_millis(1_000));
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }
}
