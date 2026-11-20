//! Windows WASAPI PCM capture and playback bridges for the SHT3 audio transport
//! pipeline, wrapping [`WindowsLoopbackRecorder`] and [`WindowsPlayer`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::backend::windows::audio::{WindowsLoopbackRecorder, WindowsPlayer};
use crate::transport::pcm::{PcmCapture, PcmPlayback, PCM_CHANNELS, PCM_SAMPLE_RATE};
use crate::transport::{TransportError, TransportResult};

/// Cap queued capture samples at 0.5 s of 48 kHz stereo audio to bound latency.
const MAX_CAPTURE_QUEUE_SAMPLES: usize = (PCM_SAMPLE_RATE as usize) * (PCM_CHANNELS as usize) / 2;
const CAPTURE_WAIT_STEP: Duration = Duration::from_millis(25);

/// Pull-based [`PcmCapture`] adapter over [`WindowsLoopbackRecorder`] (48 kHz stereo S16LE).
pub struct WindowsPcmCapture {
    recorder: WindowsLoopbackRecorder,
    queue: Arc<(Mutex<VecDeque<i16>>, Condvar)>,
    stopped: Arc<AtomicBool>,
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
        })
    }

    /// Stop the underlying WASAPI loopback recorder and wake any blocked reader.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.recorder.stop();
        self.queue.1.notify_all();
    }
}

impl PcmCapture for WindowsPcmCapture {
    fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let (lock, cvar) = &*self.queue;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        while guard.len() < buf.len() {
            if self.stopped.load(Ordering::Acquire) || !self.recorder.is_alive() {
                break;
            }
            let (next_guard, _) = cvar
                .wait_timeout(guard, CAPTURE_WAIT_STEP)
                .unwrap_or_else(|e| e.into_inner());
            guard = next_guard;
        }

        let avail = guard.len().min(buf.len());
        if avail == 0 {
            return Ok(0);
        }
        for slot in buf.iter_mut().take(avail) {
            *slot = guard.pop_front().unwrap_or(0);
        }
        Ok(avail)
    }
}

impl Drop for WindowsPcmCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Push-based [`PcmPlayback`] adapter over [`WindowsPlayer`] (48 kHz stereo S16LE).
pub struct WindowsPcmPlayback {
    player: WindowsPlayer,
    byte_buf: Vec<u8>,
}

impl WindowsPcmPlayback {
    /// Create and start a 48 kHz stereo [`WindowsPlayer`] for SHT3 audio playback.
    pub fn start() -> Self {
        let player = WindowsPlayer::new(PCM_SAMPLE_RATE, PCM_CHANNELS);
        player.start();
        Self {
            player,
            byte_buf: Vec::with_capacity(3_840),
        }
    }

    /// Wrap an existing [`WindowsPlayer`] (starting it if not already started).
    pub fn from_player(player: WindowsPlayer) -> Self {
        player.start();
        Self {
            player,
            byte_buf: Vec::with_capacity(3_840),
        }
    }
}

impl PcmPlayback for WindowsPcmPlayback {
    fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()> {
        if samples.is_empty() {
            return Ok(());
        }
        self.byte_buf.clear();
        self.byte_buf.reserve(samples.len() * 2);
        for &s in samples {
            self.byte_buf.extend_from_slice(&s.to_le_bytes());
        }
        self.player.write(&self.byte_buf);
        Ok(())
    }

    fn set_volume(&self, pct: u8) {
        self.player.set_volume(u32::from(pct.min(150)));
    }

    fn set_mute(&self, muted: bool) {
        self.player.set_mute(muted);
    }

    fn volume(&self) -> u8 {
        self.player.volume().min(150) as u8
    }

    fn is_muted(&self) -> bool {
        self.player.is_muted()
    }
}

impl Drop for WindowsPcmPlayback {
    fn drop(&mut self) {
        self.player.close();
    }
}
