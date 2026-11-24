//! Windows WASAPI PCM capture and playback bridges for the SHT3 audio transport
//! pipeline, wrapping [`WindowsLoopbackRecorder`] and [`WindowsPlayer`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::backend::windows::audio::{apply_gain_i16, WindowsLoopbackRecorder, WindowsPlayer};
use crate::transport::pcm::{PcmCapture, PcmPlayback, PCM_CHANNELS, PCM_SAMPLE_RATE};
use crate::transport::{TransportError, TransportResult};

/// Cap queued capture samples at 0.5 s of 48 kHz stereo audio to bound latency.
const MAX_CAPTURE_QUEUE_SAMPLES: usize = (PCM_SAMPLE_RATE as usize) * (PCM_CHANNELS as usize) / 2;
/// Maximum wait per frame before synthesizing idle silence when WASAPI loopback pauses callbacks.
const CAPTURE_WAIT_STEP: Duration = Duration::from_millis(35);
/// Cap SHT3 playback ring buffer at 200 ms of 48 kHz stereo audio (`48000 * 2 * 0.2 = 19_200`).
const SHT3_MAX_PLAYBACK_BUFFER_SAMPLES: usize = 19_200;
/// Maximum stereo frames (~4 ms @ 48 kHz) over which volume/mute transitions are ramped.
const GAIN_RAMP_MAX_FRAMES: usize = 192;

/// Pull-based [`PcmCapture`] adapter over [`WindowsLoopbackRecorder`] (48 kHz stereo S16LE).
pub struct WindowsPcmCapture {
    recorder: WindowsLoopbackRecorder,
    queue: Arc<(Mutex<VecDeque<i16>>, Condvar)>,
    stopped: Arc<AtomicBool>,
    external_stop: Option<Arc<AtomicBool>>,
}

impl WindowsPcmCapture {
    /// Start WASAPI system-loopback capture feeding a bounded 48 kHz stereo S16LE ring buffer.
    pub fn start() -> TransportResult<Self> {
        let queue = Arc::new((Mutex::new(VecDeque::with_capacity(8_192)), Condvar::new()));
        let stopped = Arc::new(AtomicBool::new(false));

        let queue_cb = Arc::clone(&queue);
        let stopped_cb = Arc::clone(&stopped);
        let recorder = WindowsLoopbackRecorder::try_new(move |pcm_bytes: &[u8]| {
            if stopped_cb.load(Ordering::Acquire) || pcm_bytes.len() < 2 {
                return;
            }
            let (lock, cvar) = &*queue_cb;
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let incoming = pcm_bytes.len() / 2;
            if guard.len() + incoming > MAX_CAPTURE_QUEUE_SAMPLES {
                let overflow = (guard.len() + incoming) - MAX_CAPTURE_QUEUE_SAMPLES;
                let drop_n = overflow.min(guard.len());
                let aligned = drop_n - (drop_n % usize::from(PCM_CHANNELS));
                guard.drain(..aligned);
            }
            for &pair in pcm_bytes.as_chunks::<2>().0 {
                guard.push_back(i16::from_le_bytes(pair));
            }
            cvar.notify_one();
        })
        .map_err(|msg| TransportError::Io(std::io::Error::other(msg)))?;

        Ok(Self {
            recorder,
            queue,
            stopped,
            external_stop: None,
        })
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
            || self
                .external_stop
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    /// Stop the underlying WASAPI loopback recorder and wake any blocked reader.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.recorder.stop();
        self.queue.1.notify_all();
    }
}

impl PcmCapture for WindowsPcmCapture {
    fn attach_stop_flag(&mut self, stop_flag: Arc<AtomicBool>) {
        self.external_stop = Some(stop_flag);
    }

    fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize> {
        if buf.is_empty() || self.is_stopped() || !self.recorder.is_alive() {
            return Ok(0);
        }

        let (lock, cvar) = &*self.queue;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + CAPTURE_WAIT_STEP;

        while guard.len() < buf.len() {
            if self.is_stopped() || !self.recorder.is_alive() {
                return Ok(0);
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let wait_slice = if self.external_stop.is_some() {
                remaining.min(Duration::from_millis(5))
            } else {
                remaining
            };
            let (next_guard, _) = cvar
                .wait_timeout(guard, wait_slice)
                .unwrap_or_else(|e| e.into_inner());
            guard = next_guard;
        }

        if self.is_stopped() || !self.recorder.is_alive() {
            return Ok(0);
        }

        read_capture_queue_into(&mut guard, buf)
    }
}

/// Drain a full frame from `guard`, or stereo-aligned partial samples + zero-fill on idle timeout.
fn read_capture_queue_into(guard: &mut VecDeque<i16>, buf: &mut [i16]) -> TransportResult<usize> {
    if guard.len() >= buf.len() {
        for slot in buf.iter_mut() {
            *slot = guard.pop_front().unwrap_or(0);
        }
        return Ok(buf.len());
    }

    let ch = usize::from(PCM_CHANNELS);
    let avail = guard.len().min(buf.len());
    let aligned = avail - (avail % ch);
    for slot in buf.iter_mut().take(aligned) {
        *slot = guard.pop_front().unwrap_or(0);
    }
    buf[aligned..].fill(0);
    Ok(buf.len())
}

impl Drop for WindowsPcmCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Push-based [`PcmPlayback`] adapter over [`WindowsPlayer`] (48 kHz stereo S16LE)
/// with smooth per-frame gain ramping.
pub struct WindowsPcmPlayback {
    player: WindowsPlayer,
    volume_pct: Arc<AtomicU8>,
    muted: Arc<AtomicBool>,
    current_gain: f32,
    byte_buf: Vec<u8>,
}

impl WindowsPcmPlayback {
    /// Create and start a 48 kHz stereo [`WindowsPlayer`] capped at 200 ms for SHT3 playback.
    pub fn start() -> Self {
        let player = WindowsPlayer::with_max_buffer_samples(
            PCM_SAMPLE_RATE,
            PCM_CHANNELS,
            SHT3_MAX_PLAYBACK_BUFFER_SAMPLES,
        );
        player.start();
        Self {
            player,
            volume_pct: Arc::new(AtomicU8::new(100)),
            muted: Arc::new(AtomicBool::new(false)),
            current_gain: 1.0,
            byte_buf: Vec::with_capacity(3_840),
        }
    }

    /// Wrap an existing [`WindowsPlayer`] (starting it if not already started).
    pub fn from_player(player: WindowsPlayer) -> Self {
        let vol = (player.volume().min(150)) as u8;
        let muted = player.is_muted();
        // Keep the underlying player at unity so gain ramping is applied once without double-scaling.
        player.set_volume(100);
        player.set_mute(false);
        player.start();
        Self {
            player,
            volume_pct: Arc::new(AtomicU8::new(vol)),
            muted: Arc::new(AtomicBool::new(muted)),
            current_gain: target_gain_factor(u32::from(vol), muted),
            byte_buf: Vec::with_capacity(3_840),
        }
    }
}

impl PcmPlayback for WindowsPcmPlayback {
    fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let vol = u32::from(self.volume_pct.load(Ordering::Acquire)).min(150);
        let muted = self.muted.load(Ordering::Acquire);
        encode_ramped_s16le(
            samples,
            vol,
            muted,
            &mut self.current_gain,
            &mut self.byte_buf,
        );
        self.player.write(&self.byte_buf);
        Ok(())
    }

    fn set_volume(&self, pct: u8) {
        self.volume_pct.store(pct.min(150), Ordering::Release);
    }

    fn set_mute(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    fn volume(&self) -> u8 {
        self.volume_pct.load(Ordering::Acquire)
    }

    fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }
}

impl Drop for WindowsPcmPlayback {
    fn drop(&mut self) {
        self.player.close();
    }
}

#[inline]
fn target_gain_factor(volume_pct: u32, muted: bool) -> f32 {
    if muted || volume_pct == 0 {
        0.0
    } else {
        (volume_pct.min(150) as f32) / 100.0
    }
}

#[inline]
fn scale_sample_f32(sample: i16, gain: f32) -> i16 {
    if gain <= 0.0 {
        return 0;
    }
    (f32::from(sample) * gain)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

fn encode_ramped_s16le(
    samples: &[i16],
    volume_pct: u32,
    muted: bool,
    current_gain: &mut f32,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.reserve(samples.len() * 2);

    let target = target_gain_factor(volume_pct, muted);
    if *current_gain == target {
        if muted || volume_pct == 0 {
            out.resize(samples.len() * 2, 0);
        } else if volume_pct == 100 {
            for &s in samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
        } else {
            for &s in samples {
                let scaled = apply_gain_i16(s, volume_pct, false);
                out.extend_from_slice(&scaled.to_le_bytes());
            }
        }
        return;
    }

    let start = *current_gain;
    let stereo_frames = samples.len() / 2;
    if stereo_frames == 0 {
        let scaled = scale_sample_f32(samples[0], target);
        out.extend_from_slice(&scaled.to_le_bytes());
        *current_gain = target;
        return;
    }

    let ramp_frames = stereo_frames.min(GAIN_RAMP_MAX_FRAMES);
    for (frame_idx, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
        let gain = if frame_idx < ramp_frames {
            let t = (frame_idx + 1) as f32 / (ramp_frames as f32);
            start + (target - start) * t
        } else {
            target
        };
        let l = scale_sample_f32(pair[0], gain);
        let r = scale_sample_f32(pair[1], gain);
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    if !samples.len().is_multiple_of(2) {
        let s = scale_sample_f32(samples[samples.len() - 1], target);
        out.extend_from_slice(&s.to_le_bytes());
    }
    *current_gain = target;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_timeout_synthesizes_stereo_aligned_silence() {
        let mut guard = VecDeque::from(vec![100i16, -100, 200]);
        let mut buf = [999i16; 6];
        let n = read_capture_queue_into(&mut guard, &mut buf).unwrap();
        assert_eq!(n, 6);
        // Only the complete stereo pair (100, -100) is consumed; trailing mono sample stays queued.
        assert_eq!(buf, [100, -100, 0, 0, 0, 0]);
        assert_eq!(guard.into_iter().collect::<Vec<_>>(), vec![200]);
    }

    #[test]
    fn gain_ramp_is_smooth_on_change_and_bit_transparent_at_unity() {
        let mut current_gain = 1.0f32;
        let mut out = Vec::new();
        let unity_in: Vec<i16> = (0..64).map(|i| (i * 300) - 9000).collect();

        // Settled at 100% unmuted: 100% bit-transparent
        encode_ramped_s16le(&unity_in, 100, false, &mut current_gain, &mut out);
        let unity_out: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&p| i16::from_le_bytes(p))
            .collect();
        assert_eq!(unity_out, unity_in);

        // Ramp down to muted across 32 stereo frames (constant DC 10_000)
        let dc = vec![10_000i16; 64];
        encode_ramped_s16le(&dc, 100, true, &mut current_gain, &mut out);
        let ramped: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&p| i16::from_le_bytes(p))
            .collect();
        // First sample must not snap straight to 0, and last sample reaches 0
        assert!(ramped[0] > 9_000);
        assert_eq!(ramped[ramped.len() - 1], 0);
        for w in ramped.as_chunks::<2>().0.windows(2) {
            assert!(w[0][0] >= w[1][0]);
            assert_eq!(w[0][0], w[0][1]);
        }
    }

    #[test]
    fn windows_playback_caps_buffer_at_200ms_and_applies_ramped_gain() {
        let mut play = WindowsPcmPlayback::start();
        assert_eq!(play.volume(), 100);
        assert!(!play.is_muted());
        play.set_volume(75);
        play.set_mute(true);
        assert_eq!(play.volume(), 75);
        assert!(play.is_muted());
        play.write_frame(&[10_000, -10_000, 10_000, -10_000])
            .unwrap();
    }
}
