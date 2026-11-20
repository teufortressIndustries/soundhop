//! Cross-platform 48 kHz stereo S16LE PCM capture/playback boundary traits and
//! headless mock implementations for SHT3 transport pipelines and tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
}
