//! Audio playback and loopback capture for Windows (`cpal`/WASAPI),
//! with cross-platform mock fallbacks so unit tests run on both Linux and Windows.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_PLAYBACK_BUFFER_SAMPLES: usize = 96_000; // 1.0s @ 48kHz stereo

#[derive(Debug, Clone)]
pub struct AudioEndpoint {
    pub name: String,
    pub channels: u8,
    pub sample_rate: u32,
}

pub fn list_audio_endpoints() -> Vec<AudioEndpoint> {
    #[cfg(windows)]
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

pub struct WindowsPlayer {
    pub sample_rate: u32,
    pub channels: u8,
    closed: Arc<AtomicBool>,
    buffer: Arc<Mutex<VecDeque<i16>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl WindowsPlayer {
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            sample_rate,
            channels,
            closed: Arc::new(AtomicBool::new(false)),
            buffer: Arc::new(Mutex::new(VecDeque::with_capacity(16_384))),
            worker: Mutex::new(None),
        }
    }

    pub fn start(&self) {
        self.closed.store(false, Ordering::SeqCst);
        let mut guard = self.worker.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let closed = Arc::clone(&self.closed);
        let buffer = Arc::clone(&self.buffer);
        let src_rate = self.sample_rate.max(8_000);
        let src_channels = self.channels.max(1) as usize;
        *guard = Some(thread::spawn(move || {
            run_playback_worker(closed, buffer, src_rate, src_channels);
        }));
    }

    pub fn write(&self, pcm_data: &[u8]) {
        if self.closed.load(Ordering::SeqCst) || pcm_data.is_empty() {
            return;
        }
        let mut buf = self.buffer.lock().unwrap();
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
        self.buffer.lock().unwrap().clear();
        if let Some(h) = self.worker.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl Drop for WindowsPlayer {
    fn drop(&mut self) {
        self.close();
    }
}

#[allow(dead_code)]
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

#[cfg(windows)]
fn run_playback_worker(
    closed: Arc<AtomicBool>,
    buffer: Arc<Mutex<VecDeque<i16>>>,
    src_rate: u32,
    src_channels: usize,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};

    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        run_fallback_drain(closed, buffer);
        return;
    };
    let Ok(default_cfg) = device.default_output_config() else {
        run_fallback_drain(closed, buffer);
        return;
    };

    let format = default_cfg.sample_format();
    let config: StreamConfig = default_cfg.into();
    let dst_channels = (config.channels as usize).max(1);
    let dst_rate = config.sample_rate.0.max(8_000);

    let stream_opt = match format {
        SampleFormat::F32 => {
            let buf_c = Arc::clone(&buffer);
            let err_flag = Arc::clone(&closed);
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _| {
                        write_resampled_output(
                            &buf_c,
                            src_rate,
                            src_channels,
                            dst_rate,
                            dst_channels,
                            data.len() / dst_channels,
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
            let err_flag = Arc::clone(&closed);
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [i16], _| {
                        write_resampled_output(
                            &buf_c,
                            src_rate,
                            src_channels,
                            dst_rate,
                            dst_channels,
                            data.len() / dst_channels,
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
            let err_flag = Arc::clone(&closed);
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [u16], _| {
                        write_resampled_output(
                            &buf_c,
                            src_rate,
                            src_channels,
                            dst_rate,
                            dst_channels,
                            data.len() / dst_channels,
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
        run_fallback_drain(closed, buffer);
        return;
    };
    if stream.play().is_err() {
        run_fallback_drain(closed, buffer);
        return;
    }

    while !closed.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(20));
    }
    drop(stream);
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn write_resampled_output<F: FnMut(usize, i16)>(
    buffer: &Mutex<VecDeque<i16>>,
    src_rate: u32,
    src_channels: usize,
    dst_rate: u32,
    dst_channels: usize,
    out_frames: usize,
    mut write_sample: F,
) {
    let mut guard = buffer.lock().unwrap();
    if dst_rate == src_rate {
        for f in 0..out_frames {
            let (l, r) = pop_frame_pair(&mut guard, src_channels);
            for ch in 0..dst_channels {
                let s = match ch {
                    0 => l,
                    1 => r,
                    _ => ((i32::from(l) + i32::from(r)) / 2) as i16,
                };
                write_sample(f * dst_channels + ch, s);
            }
        }
    } else {
        let ratio = src_rate as f64 / dst_rate as f64;
        let mut frac = 0.0_f64;
        let mut cur = pop_frame_pair(&mut guard, src_channels);
        for f in 0..out_frames {
            while frac >= 1.0 {
                cur = pop_frame_pair(&mut guard, src_channels);
                frac -= 1.0;
            }
            for ch in 0..dst_channels {
                let s = match ch {
                    0 => cur.0,
                    1 => cur.1,
                    _ => ((i32::from(cur.0) + i32::from(cur.1)) / 2) as i16,
                };
                write_sample(f * dst_channels + ch, s);
            }
            frac += ratio;
        }
    }
}

#[cfg(not(windows))]
fn run_playback_worker(
    closed: Arc<AtomicBool>,
    buffer: Arc<Mutex<VecDeque<i16>>>,
    _src_rate: u32,
    _src_channels: usize,
) {
    run_fallback_drain(closed, buffer);
}

fn run_fallback_drain(closed: Arc<AtomicBool>, buffer: Arc<Mutex<VecDeque<i16>>>) {
    while !closed.load(Ordering::SeqCst) {
        {
            let mut g = buffer.lock().unwrap();
            let drain_n = g.len().min(1920);
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
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(&[u8]) + Send + Sync + 'static,
    {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&stop_flag);
        let cb = Arc::new(callback);
        let handle = thread::spawn(move || {
            run_loopback_worker(flag_clone, cb);
        });
        Self {
            stop_flag,
            handle: Mutex::new(Some(handle)),
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.stop_flag.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

type CaptureCallback = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[cfg(windows)]
fn emit_captured_s16le(frames: &[(i16, i16)], src_rate: u32, cb: &CaptureCallback) {
    if frames.is_empty() {
        return;
    }
    let mut bytes = Vec::with_capacity(frames.len() * 4);
    if src_rate == 48_000 {
        for &(l, r) in frames {
            bytes.extend_from_slice(&l.to_le_bytes());
            bytes.extend_from_slice(&r.to_le_bytes());
        }
    } else {
        let dst_len = ((frames.len() as u64 * 48_000) / u64::from(src_rate.max(1))) as usize;
        let dst_len = dst_len.max(1);
        for i in 0..dst_len {
            let src_idx = (i * frames.len() / dst_len).min(frames.len() - 1);
            let (l, r) = frames[src_idx];
            bytes.extend_from_slice(&l.to_le_bytes());
            bytes.extend_from_slice(&r.to_le_bytes());
        }
    }
    cb(&bytes);
}

#[cfg(windows)]
fn run_loopback_worker(stop_flag: Arc<AtomicBool>, cb: CaptureCallback) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, StreamConfig};

    let host = cpal::default_host();
    // On WASAPI, capturing from an output device provides system loopback audio.
    let target = host
        .default_output_device()
        .and_then(|dev| dev.default_output_config().ok().map(|cfg| (dev, cfg)))
        .or_else(|| {
            host.default_input_device()
                .and_then(|dev| dev.default_input_config().ok().map(|cfg| (dev, cfg)))
        });

    if let Some((device, default_cfg)) = target {
        let format = default_cfg.sample_format();
        let config: StreamConfig = default_cfg.into();
        let ch = (config.channels as usize).max(1);
        let rate = config.sample_rate.0.max(8_000);

        let stream_opt = match format {
            SampleFormat::F32 => {
                let err_flag = Arc::clone(&stop_flag);
                let cb_c = Arc::clone(&cb);
                device
                    .build_input_stream(
                        &config,
                        move |data: &[f32], _| {
                            let mut pairs = Vec::with_capacity(data.len() / ch);
                            for frame in data.chunks_exact(ch) {
                                let l = (frame[0].clamp(-1.0, 1.0) * 32767.0) as i16;
                                let r = if ch > 1 {
                                    (frame[1].clamp(-1.0, 1.0) * 32767.0) as i16
                                } else {
                                    l
                                };
                                pairs.push((l, r));
                            }
                            emit_captured_s16le(&pairs, rate, &cb_c);
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            SampleFormat::I16 => {
                let err_flag = Arc::clone(&stop_flag);
                let cb_c = Arc::clone(&cb);
                device
                    .build_input_stream(
                        &config,
                        move |data: &[i16], _| {
                            let mut pairs = Vec::with_capacity(data.len() / ch);
                            for frame in data.chunks_exact(ch) {
                                let l = frame[0];
                                let r = if ch > 1 { frame[1] } else { l };
                                pairs.push((l, r));
                            }
                            emit_captured_s16le(&pairs, rate, &cb_c);
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            SampleFormat::U16 => {
                let err_flag = Arc::clone(&stop_flag);
                let cb_c = Arc::clone(&cb);
                device
                    .build_input_stream(
                        &config,
                        move |data: &[u16], _| {
                            let mut pairs = Vec::with_capacity(data.len() / ch);
                            for frame in data.chunks_exact(ch) {
                                let l = (i32::from(frame[0]) - 32768) as i16;
                                let r = if ch > 1 {
                                    (i32::from(frame[1]) - 32768) as i16
                                } else {
                                    l
                                };
                                pairs.push((l, r));
                            }
                            emit_captured_s16le(&pairs, rate, &cb_c);
                        },
                        move |_| err_flag.store(true, Ordering::SeqCst),
                        None,
                    )
                    .ok()
            }
            _ => None,
        };

        if let Some(stream) = stream_opt {
            if stream.play().is_ok() {
                while !stop_flag.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(20));
                }
                drop(stream);
                return;
            }
        }
    }

    let silence = vec![0u8; 4096];
    while !stop_flag.load(Ordering::SeqCst) {
        cb(&silence);
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(windows))]
fn run_loopback_worker(stop_flag: Arc<AtomicBool>, cb: CaptureCallback) {
    let silence = vec![0u8; 4096];
    while !stop_flag.load(Ordering::SeqCst) {
        cb(&silence);
        thread::sleep(Duration::from_millis(20));
    }
}
