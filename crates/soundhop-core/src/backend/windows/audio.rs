//! Audio playback and loopback capture for Windows (`cpal`/WASAPI),
//! with cross-platform mock fallbacks so unit tests run on both Linux and Windows.

#[cfg(any(windows, test))]
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ringbuf::traits::{
    Consumer as RbConsumer, Observer as RbObserver, Producer as RbProducer, Split as RbSplit,
};
use ringbuf::{HeapCons, HeapProd, HeapRb};

const MAX_PLAYBACK_BUFFER_SAMPLES: usize = 96_000; // 1.0s @ 48kHz stereo
const RECONNECT_INITIAL_BACKOFF_MS: u64 = 100;
const RECONNECT_MAX_BACKOFF_MS: u64 = 1_000;

#[cfg(test)]
use crate::transport::dsp::pop_frame_pair;
pub use crate::transport::dsp::{apply_gain_i16, LinearResampler, PlaybackResampler};

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
                let name = dev
                    .description()
                    .map(|d| d.name().to_string())
                    .unwrap_or_else(|_| "Speakers".to_string());
                let (channels, sample_rate) = dev
                    .default_output_config()
                    .map(|c| (c.channels() as u8, c.sample_rate()))
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

struct ReturnCons {
    cons: Option<HeapCons<i16>>,
    slot: Arc<Mutex<Option<HeapCons<i16>>>>,
}

impl ReturnCons {
    fn take_from(slot: &Arc<Mutex<Option<HeapCons<i16>>>>) -> Option<Self> {
        let cons = slot.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        Some(Self {
            cons: Some(cons),
            slot: Arc::clone(slot),
        })
    }

    #[cfg(all(windows, not(test)))]
    fn drain_into_scratch(&mut self, scratch: &mut VecDeque<i16>, target_len: usize) {
        let Some(cons) = self.cons.as_mut() else {
            return;
        };
        if scratch.len() >= target_len {
            return;
        }
        let needed = target_len - scratch.len();
        let avail = cons.occupied_len().min(needed);
        if avail > 0 {
            let mut tmp = [0i16; 1024];
            let mut remaining = avail;
            while remaining > 0 {
                let chunk = remaining.min(tmp.len());
                let popped = cons.pop_slice(&mut tmp[..chunk]);
                if popped == 0 {
                    break;
                }
                scratch.extend(&tmp[..popped]);
                remaining -= popped;
            }
        }
    }
}

impl Drop for ReturnCons {
    fn drop(&mut self) {
        if let Some(cons) = self.cons.take() {
            *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(cons);
        }
    }
}

pub struct WindowsPlayer {
    pub sample_rate: u32,
    pub channels: u8,
    max_buffer_samples: usize,
    prebuffer_samples: usize,
    closed: Arc<AtomicBool>,
    prod: Mutex<HeapProd<i16>>,
    cons_slot: Arc<Mutex<Option<HeapCons<i16>>>>,
    volume_pct: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    underruns: Arc<AtomicU64>,
    overruns: Arc<AtomicU64>,
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

    pub fn with_max_buffer_samples(sample_rate: u32, channels: u8, max_samples: usize) -> Self {
        let ch = usize::from(channels.max(1));
        let aligned = (max_samples - (max_samples % ch)).max(ch);
        let rb = HeapRb::<i16>::new(aligned);
        let (prod, cons) = rb.split();
        let mut player = Self::new(sample_rate, channels);
        player.max_buffer_samples = aligned;
        player.prod = Mutex::new(prod);
        player.cons_slot = Arc::new(Mutex::new(Some(cons)));
        player
    }

    pub fn with_volume(
        sample_rate: u32,
        channels: u8,
        volume_pct: Arc<AtomicU32>,
        muted: Arc<AtomicBool>,
    ) -> Self {
        let rb = HeapRb::<i16>::new(MAX_PLAYBACK_BUFFER_SAMPLES);
        let (prod, cons) = rb.split();
        Self {
            sample_rate,
            channels,
            max_buffer_samples: MAX_PLAYBACK_BUFFER_SAMPLES,
            prebuffer_samples: 0,
            closed: Arc::new(AtomicBool::new(false)),
            prod: Mutex::new(prod),
            cons_slot: Arc::new(Mutex::new(Some(cons))),
            volume_pct,
            muted,
            underruns: Arc::new(AtomicU64::new(0)),
            overruns: Arc::new(AtomicU64::new(0)),
            worker: Mutex::new(None),
        }
    }

    /// Require `prebuffer_samples` interleaved samples before playout begins draining.
    pub fn with_prebuffer_samples(mut self, prebuffer_samples: usize) -> Self {
        let ch = usize::from(self.channels.max(1));
        let aligned = prebuffer_samples - (prebuffer_samples % ch);
        self.prebuffer_samples = aligned.min(self.max_buffer_samples);
        self
    }

    pub fn prebuffer_samples(&self) -> usize {
        self.prebuffer_samples
    }

    pub fn buffered_samples(&self) -> usize {
        self.prod
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .occupied_len()
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
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
        let cons_slot = Arc::clone(&self.cons_slot);
        let volume_pct = Arc::clone(&self.volume_pct);
        let muted = Arc::clone(&self.muted);
        let underruns = Arc::clone(&self.underruns);
        let src_rate = self.sample_rate.max(8_000);
        let src_channels = self.channels.max(1) as usize;
        let prebuffer_samples = self.prebuffer_samples;
        let max_buffer_samples = self.max_buffer_samples;
        *guard = Some(thread::spawn(move || {
            run_playback_worker(
                closed,
                cons_slot,
                volume_pct,
                muted,
                underruns,
                src_rate,
                src_channels,
                prebuffer_samples,
                max_buffer_samples,
            );
        }));
    }

    pub fn write(&self, pcm_data: &[u8]) {
        if self.closed.load(Ordering::SeqCst) || pcm_data.is_empty() {
            return;
        }
        let ch = usize::from(self.channels.max(1));
        let mut prod = self.prod.lock().unwrap_or_else(|e| e.into_inner());
        let incoming_samples = pcm_data.len() / 2;
        let vacant = prod.vacant_len();
        let aligned_vacant = vacant - (vacant % ch);
        let to_push = if incoming_samples > aligned_vacant {
            self.overruns.fetch_add(1, Ordering::Relaxed);
            aligned_vacant
        } else {
            incoming_samples - (incoming_samples % ch)
        };
        for &pair in pcm_data.as_chunks::<2>().0.iter().take(to_push) {
            if prod.try_push(i16::from_le_bytes(pair)).is_err() {
                break;
            }
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
        if let Some(cons) = self
            .cons_slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            cons.clear();
        }
    }
}

impl Drop for WindowsPlayer {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(all(windows, not(test)))]
#[allow(clippy::too_many_arguments)]
fn run_playback_worker(
    closed: Arc<AtomicBool>,
    cons_slot: Arc<Mutex<Option<HeapCons<i16>>>>,
    volume_pct: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    underruns: Arc<AtomicU64>,
    src_rate: u32,
    src_channels: usize,
    prebuffer_samples: usize,
    _max_buffer_samples: usize,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};
    use std::time::Instant;

    let mut backoff = ReconnectBackoff::default();

    while !closed.load(Ordering::SeqCst) {
        let host = cpal::default_host();
        let Some(device) = host.default_output_device() else {
            drain_stale_buffer(&cons_slot);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
            continue;
        };
        let active_name = device.description().ok().map(|d| d.name().to_string());
        let Ok(default_cfg) = device.default_output_config() else {
            drain_stale_buffer(&cons_slot);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
            continue;
        };

        let format = default_cfg.sample_format();
        let config: StreamConfig = default_cfg.into();
        let dst_channels = (config.channels as usize).max(1);
        let dst_rate = config.sample_rate.max(8_000);
        let stream_broken = Arc::new(AtomicBool::new(false));
        let Some(mut rb_cons) = ReturnCons::take_from(&cons_slot) else {
            break;
        };
        let mut scratch: VecDeque<i16> = VecDeque::with_capacity(16_384);

        let stream_opt = match format {
            SampleFormat::F32 => {
                let vol_c = Arc::clone(&volume_pct);
                let mute_c = Arc::clone(&muted);
                let underruns_c = Arc::clone(&underruns);
                let err_flag = Arc::clone(&stream_broken);
                let mut resampler =
                    PlaybackResampler::with_prebuffer(src_rate, dst_rate, prebuffer_samples);
                device
                    .build_output_stream(
                        config.clone(),
                        move |data: &mut [f32], _| {
                            let vol = vol_c.load(Ordering::Relaxed);
                            let is_muted = mute_c.load(Ordering::Relaxed);
                            let out_frames = data.len() / dst_channels;
                            let target_len = prebuffer_samples.max(out_frames * src_channels * 2);
                            rb_cons.drain_into_scratch(&mut scratch, target_len);
                            if resampler.write_frames_checked(
                                &mut scratch,
                                src_channels,
                                dst_channels,
                                out_frames,
                                vol,
                                is_muted,
                                |idx, s| {
                                    data[idx] = f32::from(s) / 32768.0;
                                },
                            ) {
                                underruns_c.fetch_add(1, Ordering::Relaxed);
                            }
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            SampleFormat::I16 => {
                let vol_c = Arc::clone(&volume_pct);
                let mute_c = Arc::clone(&muted);
                let underruns_c = Arc::clone(&underruns);
                let err_flag = Arc::clone(&stream_broken);
                let mut resampler =
                    PlaybackResampler::with_prebuffer(src_rate, dst_rate, prebuffer_samples);
                device
                    .build_output_stream(
                        config.clone(),
                        move |data: &mut [i16], _| {
                            let vol = vol_c.load(Ordering::Relaxed);
                            let is_muted = mute_c.load(Ordering::Relaxed);
                            let out_frames = data.len() / dst_channels;
                            let target_len = prebuffer_samples.max(out_frames * src_channels * 2);
                            rb_cons.drain_into_scratch(&mut scratch, target_len);
                            if resampler.write_frames_checked(
                                &mut scratch,
                                src_channels,
                                dst_channels,
                                out_frames,
                                vol,
                                is_muted,
                                |idx, s| {
                                    data[idx] = s;
                                },
                            ) {
                                underruns_c.fetch_add(1, Ordering::Relaxed);
                            }
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            SampleFormat::U16 => {
                let vol_c = Arc::clone(&volume_pct);
                let mute_c = Arc::clone(&muted);
                let underruns_c = Arc::clone(&underruns);
                let err_flag = Arc::clone(&stream_broken);
                let mut resampler =
                    PlaybackResampler::with_prebuffer(src_rate, dst_rate, prebuffer_samples);
                device
                    .build_output_stream(
                        config.clone(),
                        move |data: &mut [u16], _| {
                            let vol = vol_c.load(Ordering::Relaxed);
                            let is_muted = mute_c.load(Ordering::Relaxed);
                            let out_frames = data.len() / dst_channels;
                            let target_len = prebuffer_samples.max(out_frames * src_channels * 2);
                            rb_cons.drain_into_scratch(&mut scratch, target_len);
                            if resampler.write_frames_checked(
                                &mut scratch,
                                src_channels,
                                dst_channels,
                                out_frames,
                                vol,
                                is_muted,
                                |idx, s| {
                                    data[idx] = (i32::from(s) + 32768) as u16;
                                },
                            ) {
                                underruns_c.fetch_add(1, Ordering::Relaxed);
                            }
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            _ => None,
        };

        let Some(stream) = stream_opt else {
            drain_stale_buffer(&cons_slot);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
            continue;
        };

        if stream.play().is_err() {
            drop(stream);
            drain_stale_buffer(&cons_slot);
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
                        .and_then(|d| d.description().ok().map(|desc| desc.name().to_string()));
                    if latest.is_some() && latest.as_ref() != Some(cur_name) {
                        break;
                    }
                }
            }
        }
        drop(stream);

        if !closed.load(Ordering::SeqCst) {
            drain_stale_buffer(&cons_slot);
            if sleep_interruptible(backoff.next_delay(), &closed) {
                break;
            }
        }
    }
}

#[cfg(all(windows, not(test)))]
fn drain_stale_buffer(cons_slot: &Mutex<Option<HeapCons<i16>>>) {
    let mut g = cons_slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cons) = g.as_mut() {
        let mut tmp = [0i16; 1024];
        let mut remaining = cons.occupied_len().min(3_840);
        while remaining > 0 {
            let chunk = remaining.min(tmp.len());
            let n = cons.pop_slice(&mut tmp[..chunk]);
            if n == 0 {
                break;
            }
            remaining -= n;
        }
    }
}

#[cfg(any(not(windows), test))]
#[allow(clippy::too_many_arguments)]
fn run_playback_worker(
    closed: Arc<AtomicBool>,
    cons_slot: Arc<Mutex<Option<HeapCons<i16>>>>,
    _volume_pct: Arc<AtomicU32>,
    _muted: Arc<AtomicBool>,
    underruns: Arc<AtomicU64>,
    src_rate: u32,
    src_channels: usize,
    prebuffer_samples: usize,
    max_buffer_samples: usize,
) {
    run_fallback_drain(
        closed,
        cons_slot,
        underruns,
        src_rate,
        src_channels,
        prebuffer_samples,
        max_buffer_samples,
    );
}

#[cfg(any(not(windows), test))]
fn run_fallback_drain(
    closed: Arc<AtomicBool>,
    cons_slot: Arc<Mutex<Option<HeapCons<i16>>>>,
    underruns: Arc<AtomicU64>,
    src_rate: u32,
    src_channels: usize,
    prebuffer_samples: usize,
    max_buffer_samples: usize,
) {
    let ch = src_channels.max(1);
    let drain_step = ((src_rate as usize * ch) / 50)
        .max(ch)
        .min(max_buffer_samples.max(ch));
    let Some(mut rb_cons) = ReturnCons::take_from(&cons_slot) else {
        return;
    };
    let mut primed = false;
    let mut discard = vec![0i16; drain_step];
    while !closed.load(Ordering::SeqCst) {
        if let Some(cons) = rb_cons.cons.as_mut() {
            if !primed {
                let min_ready = if prebuffer_samples > 0 {
                    prebuffer_samples
                } else {
                    ch
                };
                if cons.occupied_len() >= min_ready {
                    primed = true;
                }
            }
            if primed {
                if cons.occupied_len() >= drain_step {
                    let _ = cons.pop_slice(&mut discard);
                } else {
                    cons.clear();
                    underruns.fetch_add(1, Ordering::Relaxed);
                    primed = false;
                }
            }
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
    let active_name = device.description().ok().map(|d| d.name().to_string());
    let default_cfg = device
        .default_output_config()
        .map_err(|e| format!("Failed to read default output config for WASAPI loopback: {e}"))?;

    let format = default_cfg.sample_format();
    let config: StreamConfig = default_cfg.into();
    let ch = (config.channels as usize).max(1);
    let rate = config.sample_rate.max(8_000);
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
                    config.clone(),
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
                    config.clone(),
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
                    config.clone(),
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
                        .and_then(|d| d.description().ok().map(|desc| desc.name().to_string()));
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

    #[test]
    fn test_windows_player_ring_buffer_overflow_preserves_stereo_alignment() {
        let player = WindowsPlayer::new(48_000, 2);

        // Fill buffer to capacity with recognizable stereo marker pairs: (L=100, R=-100)
        let mut initial = Vec::with_capacity(MAX_PLAYBACK_BUFFER_SAMPLES * 2);
        for _ in 0..(MAX_PLAYBACK_BUFFER_SAMPLES / 2) {
            initial.extend_from_slice(&100i16.to_le_bytes());
            initial.extend_from_slice(&(-100i16).to_le_bytes());
        }
        player.write(&initial);
        assert_eq!(player.buffered_samples(), MAX_PLAYBACK_BUFFER_SAMPLES);

        // Push 100 more stereo frames (200 samples) with distinct markers (L=2000, R=-2000)
        let mut extra = Vec::with_capacity(100 * 4);
        for _ in 0..100 {
            extra.extend_from_slice(&2000i16.to_le_bytes());
            extra.extend_from_slice(&(-2000i16).to_le_bytes());
        }
        player.write(&extra);

        assert_eq!(player.buffered_samples(), MAX_PLAYBACK_BUFFER_SAMPLES);
        assert_eq!(
            player.buffered_samples() % 2,
            0,
            "stereo channel alignment must be preserved"
        );
        assert_eq!(player.overruns(), 1);
        let mut slot = player.cons_slot.lock().unwrap();
        let cons = slot.as_mut().unwrap();
        let mut pair = [0i16; 2];
        assert_eq!(cons.pop_slice(&mut pair), 2);
        assert_eq!(pair, [100, -100]);
    }

    #[test]
    fn test_windows_player_with_max_buffer_samples_caps_ring_buffer() {
        let cap = 19_200usize;
        let player = WindowsPlayer::with_max_buffer_samples(48_000, 2, cap);
        let mut batch = Vec::with_capacity(cap * 2);
        for _ in 0..(cap / 2) {
            batch.extend_from_slice(&300i16.to_le_bytes());
            batch.extend_from_slice(&(-300i16).to_le_bytes());
        }
        player.write(&batch);
        assert_eq!(player.buffered_samples(), cap);

        let mut extra = Vec::with_capacity(400);
        for _ in 0..100 {
            extra.extend_from_slice(&900i16.to_le_bytes());
            extra.extend_from_slice(&(-900i16).to_le_bytes());
        }
        player.write(&extra);

        assert_eq!(player.buffered_samples(), cap);
        assert_eq!(player.overruns(), 1);
        let mut slot = player.cons_slot.lock().unwrap();
        let cons = slot.as_mut().unwrap();
        let mut pair = [0i16; 2];
        assert_eq!(cons.pop_slice(&mut pair), 2);
        assert_eq!(pair, [300, -300]);
    }

    #[test]
    fn test_windows_player_write_ignores_empty_odd_trailing_byte_and_closed() {
        let player = WindowsPlayer::new(48_000, 1);
        player.write(&[]);
        assert_eq!(player.buffered_samples(), 0);

        // 5 bytes -> 2 complete i16 samples, 1 trailing byte ignored
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1234i16.to_le_bytes());
        bytes.extend_from_slice(&(-5678i16).to_le_bytes());
        bytes.push(0xFF);
        player.write(&bytes);
        assert_eq!(player.buffered_samples(), 2);
        {
            let mut slot = player.cons_slot.lock().unwrap();
            let cons = slot.as_mut().unwrap();
            let mut out = [0i16; 2];
            assert_eq!(cons.pop_slice(&mut out), 2);
            assert_eq!(out, [1234, -5678]);
        }

        // After close(), buffer is cleared and subsequent writes are ignored
        player.write(&bytes);
        assert_eq!(player.buffered_samples(), 2);
        player.close();
        assert_eq!(player.buffered_samples(), 0);
        player.write(&bytes);
        assert_eq!(player.buffered_samples(), 0);
    }

    #[test]
    fn test_playback_resampler_underrun_fills_silence_passthrough_and_resampled() {
        // 1. Passthrough (48k -> 48k) with 2 stereo frames available, requesting 5 frames
        let mut queue = VecDeque::from(vec![1000i16, -1000, 2000, -2000]);
        let mut passthrough = PlaybackResampler::new(48_000, 48_000);
        let mut out = vec![999i16; 5 * 2];
        passthrough.write_frames(&mut queue, 2, 2, 5, 100, false, |i, s| out[i] = s);
        assert_eq!(out, vec![1000, -1000, 2000, -2000, 0, 0, 0, 0, 0, 0]);
        assert!(queue.is_empty());

        // 2. Empty queue underrun in resampled path (44.1k -> 48k) produces pure silence
        let mut empty_q = VecDeque::new();
        let mut resampled = PlaybackResampler::new(44_100, 48_000);
        let mut silent = [777i16; 32 * 2];
        resampled.write_frames(&mut empty_q, 2, 2, 32, 100, false, |i, s| silent[i] = s);
        assert!(silent.iter().all(|&s| s == 0));

        // 3. Partial stereo frame (odd single sample left in queue) duplicates L->R, then silence
        let mut odd_q = VecDeque::from(vec![4200i16]);
        assert_eq!(pop_frame_pair(&mut odd_q, 2), (4200, 4200));
        assert_eq!(pop_frame_pair(&mut odd_q, 2), (0, 0));
    }

    #[test]
    fn test_mono_to_stereo_and_multichannel_conversion() {
        // Mono source -> stereo destination (duplicates mono to L and R)
        let mut mono_q = VecDeque::from(vec![1000i16, -2000, 3000]);
        let mut r = PlaybackResampler::new(48_000, 48_000);
        let mut stereo_out = vec![0i16; 3 * 2];
        r.write_frames(&mut mono_q, 1, 2, 3, 100, false, |i, s| stereo_out[i] = s);
        assert_eq!(stereo_out, vec![1000, 1000, -2000, -2000, 3000, 3000]);

        // Stereo source -> mono destination (takes L channel)
        let mut stereo_q = VecDeque::from(vec![1000i16, -1000, 4000, -4000]);
        let mut mono_out = vec![0i16; 2];
        r.write_frames(&mut stereo_q, 2, 1, 2, 100, false, |i, s| mono_out[i] = s);
        assert_eq!(mono_out, vec![1000, 4000]);

        // Stereo source -> 4-channel destination (ch0=L, ch1=R, ch2..=avg without i16 overflow)
        let mut wide_q = VecDeque::from(vec![30_000i16, 30_000, -30_000, -30_000, 2000, -1000]);
        let mut quad_out = vec![0i16; 3 * 4];
        r.write_frames(&mut wide_q, 2, 4, 3, 100, false, |i, s| quad_out[i] = s);
        assert_eq!(
            quad_out,
            vec![
                30_000, 30_000, 30_000, 30_000, -30_000, -30_000, -30_000, -30_000, 2000, -1000,
                500, 500,
            ]
        );

        // 6-channel source -> stereo pair extraction discards extra surround channels
        let mut surround_q = VecDeque::from(vec![
            10i16, 20, 30, 40, 50, 60, // frame 0
            70, 80, 90, 100, 110, 120, // frame 1
        ]);
        assert_eq!(pop_frame_pair(&mut surround_q, 6), (10, 20));
        assert_eq!(pop_frame_pair(&mut surround_q, 6), (70, 80));
        assert!(surround_q.is_empty());

        // Gain + mute applied during mono-to-stereo conversion
        let mut muted_q = VecDeque::from(vec![10_000i16, -10_000]);
        let mut muted_out = vec![123i16; 2 * 2];
        r.write_frames(&mut muted_q, 1, 2, 2, 100, true, |i, s| muted_out[i] = s);
        assert_eq!(muted_out, vec![0, 0, 0, 0]);
    }

    fn encode_stereo_bytes(frames: &[(i16, i16)]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(frames.len() * 4);
        for &(l, r) in frames {
            bytes.extend_from_slice(&l.to_le_bytes());
            bytes.extend_from_slice(&r.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn test_windows_player_overrun_counts_and_preserves_capacity() {
        // 200 stereo frames = 400 samples max buffer
        let player = WindowsPlayer::with_max_buffer_samples(48_000, 2, 400);
        let mut initial = vec![(10_000i16, -10_000i16); 100];
        initial.extend(vec![(-10_000i16, 10_000i16); 100]);
        player.write(&encode_stereo_bytes(&initial));
        assert_eq!(player.buffered_samples(), 400);
        assert_eq!(player.overruns(), 0);

        // Overflow by 100 frames: bounded SPSC ring preserves capacity and records overrun.
        let extra = vec![(-10_000i16, 10_000i16); 100];
        player.write(&encode_stereo_bytes(&extra));
        assert_eq!(player.buffered_samples(), 400);
        assert_eq!(player.overruns(), 1);
    }

    #[test]
    fn test_windows_player_prebuffering_and_underrun_counters() {
        let player =
            WindowsPlayer::with_max_buffer_samples(48_000, 2, 19_200).with_prebuffer_samples(3_840);
        assert_eq!(player.prebuffer_samples(), 3_840);
        assert_eq!(player.buffered_samples(), 0);
        assert_eq!(player.underruns(), 0);
        assert_eq!(player.overruns(), 0);

        player.start();
        let frame_20ms = encode_stereo_bytes(&vec![(2_000, -2_000); 960]);

        // Below 3_840 prebuffer: stays buffered and counts 0 underruns.
        player.write(&frame_20ms);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(player.buffered_samples(), 1_920);
        assert_eq!(player.underruns(), 0);

        // Reaching 3_840 primes playout; worker drains and then records 1 underrun when empty.
        player.write(&frame_20ms);
        thread::sleep(Duration::from_millis(120));
        assert_eq!(player.buffered_samples(), 0);
        assert_eq!(player.underruns(), 1);
    }

    #[test]
    fn test_playback_resampler_prebuffer_underrun_fade_out_and_resume_fade_in() {
        let mut r = PlaybackResampler::with_prebuffer(48_000, 48_000, 400); // 200 stereo frames
        let mut queue = VecDeque::new();
        for _ in 0..100 {
            queue.push_back(20_000);
            queue.push_back(-20_000);
        }

        // Below prebuffer: emits silence, does not consume queue, no underrun.
        let mut block = vec![777i16; 100 * 2];
        let underrun = r.write_frames_checked(&mut queue, 2, 2, 100, 100, false, |i, s| {
            block[i] = s;
        });
        assert!(!underrun);
        assert_eq!(queue.len(), 200);
        assert!(block.iter().all(|&s| s == 0));

        // Fill up to prebuffer (200 frames): primes and fades in over RESUME_FADE_IN_FRAMES.
        for _ in 0..100 {
            queue.push_back(20_000);
            queue.push_back(-20_000);
        }
        let underrun = r.write_frames_checked(&mut queue, 2, 2, 100, 100, false, |i, s| {
            block[i] = s;
        });
        assert!(!underrun);
        assert_eq!(queue.len(), 200);
        assert!(block[0] > 0 && block[0] < 1_000);
        assert_eq!(block[198], 20_000);
        assert_eq!(block[199], -20_000);

        // Request 150 frames when only 100 remain -> underrun at frame 100, smooth tail fade-out.
        let mut tail_block = vec![0i16; 150 * 2];
        let underrun = r.write_frames_checked(&mut queue, 2, 2, 150, 100, false, |i, s| {
            tail_block[i] = s;
        });
        assert!(underrun);
        assert!(queue.is_empty());
        assert_eq!(tail_block[198], 20_000);
        assert!(tail_block[200] < 20_000 && tail_block[200] > 19_000);
        assert!(tail_block[298] < 5_000 && tail_block[298] > 0);
        let tail_max_step = tail_block[198..]
            .as_chunks::<2>()
            .0
            .windows(2)
            .map(|w| (i32::from(w[1][0]) - i32::from(w[0][0])).abs())
            .max()
            .unwrap_or(0);
        assert!(
            tail_max_step < 400,
            "underrun tail fade-out must be click-free: tail_max_step={tail_max_step}"
        );

        // Subsequent empty callback while re-priming finishes tail to zero and does not double-count.
        let mut post_block = vec![777i16; 64 * 2];
        let underrun2 = r.write_frames_checked(&mut queue, 2, 2, 64, 100, false, |i, s| {
            post_block[i] = s;
        });
        assert!(!underrun2);
        assert_eq!(post_block[126], 0);
        assert_eq!(post_block[127], 0);

        // Resampled path (44.1k -> 48k) also smooths underruns and re-primes cleanly.
        let mut rs = PlaybackResampler::with_prebuffer(44_100, 48_000, 200);
        let mut rs_q = VecDeque::new();
        for _ in 0..100 {
            rs_q.push_back(12_000);
            rs_q.push_back(-12_000);
        }
        let mut rs_out = vec![0i16; 150 * 2];
        assert!(
            rs.write_frames_checked(&mut rs_q, 2, 2, 150, 100, false, |i, s| {
                rs_out[i] = s;
            })
        );
        let rs_max_step = rs_out
            .as_chunks::<2>()
            .0
            .windows(2)
            .map(|w| (i32::from(w[1][0]) - i32::from(w[0][0])).abs())
            .max()
            .unwrap_or(0);
        assert!(
            rs_max_step < 400,
            "resampled underrun must be click-free: rs_max_step={rs_max_step}"
        );
    }
}
