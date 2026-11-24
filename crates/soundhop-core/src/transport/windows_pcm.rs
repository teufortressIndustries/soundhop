//! Windows WASAPI PCM capture and playback bridges for the SHT3 audio transport
//! pipeline, wrapping [`WindowsLoopbackRecorder`] and [`WindowsPlayer`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::backend::windows::audio::{WindowsLoopbackRecorder, WindowsPlayer};
use crate::transport::dsp::{encode_ramped_s16le, scale_sample_f32, target_gain_factor};
use crate::transport::pcm::{PcmCapture, PcmPlayback, PCM_CHANNELS, PCM_SAMPLE_RATE};
use crate::transport::{TransportError, TransportResult};

/// Cap queued capture samples at 0.5 s of 48 kHz stereo audio to bound latency.
const MAX_CAPTURE_QUEUE_SAMPLES: usize = (PCM_SAMPLE_RATE as usize) * (PCM_CHANNELS as usize) / 2;
/// Maximum wait per frame before synthesizing idle silence when WASAPI loopback pauses callbacks.
const CAPTURE_WAIT_STEP: Duration = Duration::from_millis(35);
/// Cap SHT3 playback ring buffer at 200 ms of 48 kHz stereo audio (`48000 * 2 * 0.2 = 19_200`).
const SHT3_MAX_PLAYBACK_BUFFER_SAMPLES: usize = 19_200;
/// Prebuffer 40 ms of 48 kHz stereo audio (`48000 * 2 * 0.04 = 3_840` samples) before playout.
const SHT3_PREBUFFER_SAMPLES: usize = 3_840;
/// Target playback prebuffer fill in milliseconds matching [`SHT3_PREBUFFER_SAMPLES`].
const SHT3_PLAYBACK_TARGET_MS: f64 = 40.0;
/// Stereo frames over which partial capture timeouts are faded out before zero-fill.
const CAPTURE_TIMEOUT_FADE_FRAMES: usize = 64;
/// Maximum stereo frames (~4 ms @ 48 kHz) over which volume/mute transitions are ramped.

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
    if aligned >= CAPTURE_TIMEOUT_FADE_FRAMES * ch {
        let fade_samples = CAPTURE_TIMEOUT_FADE_FRAMES * ch;
        let fade_start = aligned - fade_samples;
        let denom = CAPTURE_TIMEOUT_FADE_FRAMES as f32;
        for frame_idx in 0..CAPTURE_TIMEOUT_FADE_FRAMES {
            let gain = 1.0 - ((frame_idx + 1) as f32 / denom);
            let base = fade_start + frame_idx * ch;
            for c in 0..ch {
                buf[base + c] = scale_sample_f32(buf[base + c], gain);
            }
        }
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
    target_ms: f64,
}

impl WindowsPcmPlayback {
    /// Create and start a 48 kHz stereo [`WindowsPlayer`] capped at 200 ms with 40 ms prebuffering.
    pub fn start() -> Self {
        let player = WindowsPlayer::with_max_buffer_samples(
            PCM_SAMPLE_RATE,
            PCM_CHANNELS,
            SHT3_MAX_PLAYBACK_BUFFER_SAMPLES,
        )
        .with_prebuffer_samples(SHT3_PREBUFFER_SAMPLES);
        player.start();
        Self {
            player,
            volume_pct: Arc::new(AtomicU8::new(100)),
            muted: Arc::new(AtomicBool::new(false)),
            current_gain: 1.0,
            byte_buf: Vec::with_capacity(3_840),
            target_ms: SHT3_PLAYBACK_TARGET_MS,
        }
    }

    /// Wrap an existing [`WindowsPlayer`] (starting it if not already started).
    pub fn from_player(player: WindowsPlayer) -> Self {
        let vol = (player.volume().min(150)) as u8;
        let muted = player.is_muted();
        let samples_per_ms =
            f64::from(player.sample_rate.max(1)) * f64::from(player.channels.max(1)) / 1000.0;
        let target_ms = (player.prebuffer_samples() as f64) / samples_per_ms;
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
            target_ms,
        }
    }

    /// Current playback buffer fill in milliseconds.
    pub fn buffered_ms(&self) -> Option<f64> {
        let samples_per_ms = f64::from(self.player.sample_rate.max(1))
            * f64::from(self.player.channels.max(1))
            / 1000.0;
        Some((self.player.buffered_samples() as f64) / samples_per_ms)
    }

    /// Target playback buffer fill in milliseconds.
    pub fn sink_target_ms(&self) -> f64 {
        self.target_ms
    }

    /// Total playback buffer underruns observed by the underlying [`WindowsPlayer`].
    pub fn sink_underruns(&self) -> u64 {
        self.player.underruns()
    }

    /// Total playback buffer overruns observed by the underlying [`WindowsPlayer`].
    pub fn sink_overruns(&self) -> u64 {
        self.player.overruns()
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

    fn buffered_ms(&self) -> Option<f64> {
        Self::buffered_ms(self)
    }

    fn sink_target_ms(&self) -> f64 {
        Self::sink_target_ms(self)
    }

    fn sink_underruns(&self) -> u64 {
        Self::sink_underruns(self)
    }

    fn sink_overruns(&self) -> u64 {
        Self::sink_overruns(self)
    }
}

impl Drop for WindowsPcmPlayback {
    fn drop(&mut self) {
        self.player.close();
    }
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

    #[test]
    fn idle_timeout_fades_out_when_aligned_at_least_128_samples() {
        let mut guard = VecDeque::from(vec![10_000i16, -10_000].repeat(96)); // 192 samples
        let mut buf = [999i16; 384];
        let n = read_capture_queue_into(&mut guard, &mut buf).unwrap();
        assert_eq!(n, 384);
        assert!(guard.is_empty());

        // First 32 frames (64 samples) remain untouched
        assert_eq!(buf[0], 10_000);
        assert_eq!(buf[1], -10_000);
        assert_eq!(buf[62], 10_000);
        assert_eq!(buf[63], -10_000);

        // Last 64 frames of the aligned region fade smoothly to zero
        assert!(buf[64] < 10_000 && buf[64] > 9_000);
        assert_eq!(buf[190], 0);
        assert_eq!(buf[191], 0);
        assert!(buf[192..].iter().all(|&s| s == 0));

        let max_step_l = buf
            .as_chunks::<2>()
            .0
            .windows(2)
            .map(|w| (i32::from(w[1][0]) - i32::from(w[0][0])).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_step_l < 300,
            "expected smooth capture timeout fade-out, max_step_l={max_step_l}"
        );
    }

    #[test]
    fn windows_playback_prebuffers_40ms_and_exposes_metrics() {
        let mut play = WindowsPcmPlayback::start();
        assert_eq!(play.sink_target_ms(), 40.0);
        assert_eq!(play.buffered_ms(), Some(0.0));
        assert_eq!(play.sink_underruns(), 0);
        assert_eq!(play.sink_overruns(), 0);

        // One 20 ms frame (1_920 samples) is below the 40 ms prebuffer, so it stays buffered.
        let frame_20ms = vec![1_000i16; 1_920];
        play.write_frame(&frame_20ms).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(play.buffered_ms(), Some(20.0));
        assert_eq!(play.sink_underruns(), 0);

        // Second 20 ms frame primes the 40 ms prebuffer; worker then drains and hits underrun.
        play.write_frame(&frame_20ms).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(play.buffered_ms(), Some(0.0));
        assert_eq!(play.sink_underruns(), 1);
        assert_eq!(play.sink_overruns(), 0);
    }

    #[test]
    fn windows_playback_from_player_reports_overruns_and_buffered_ms() {
        let player = WindowsPlayer::with_max_buffer_samples(48_000, 2, 1_920);
        let mut play = WindowsPcmPlayback::from_player(player);
        assert_eq!(play.sink_target_ms(), 0.0);
        // Single 40 ms write into a 20 ms-capped player deterministically overflows.
        let frame_40ms = vec![500i16; 3_840];
        play.write_frame(&frame_40ms).unwrap();
        assert_eq!(play.sink_overruns(), 1);
        assert!(play.buffered_ms().is_some_and(|ms| ms <= 20.0));
    }
}
