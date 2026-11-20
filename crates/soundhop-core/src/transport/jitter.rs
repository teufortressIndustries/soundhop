//! Adaptive jitter buffer for SHT3 audio playback with configurable target depth,
//! PLC on short underruns (up to 3 frames), and oldest-frame drop on overrun.

use std::collections::VecDeque;

use crate::transport::codec::AudioCodec;
use crate::transport::TransportResult;

/// Configuration for [`JitterBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterConfig {
    /// Target prebuffer depth in milliseconds (default: 40 ms = 2 frames @ 20 ms).
    pub target_ms: u16,
    /// Duration of each audio frame in milliseconds (10 or 20 ms).
    pub frame_ms: u8,
    /// Maximum consecutive underrun frames concealed via PLC before emitting silence (default: 3).
    pub max_plc_frames: usize,
    /// Extra headroom above `target_ms` before dropping oldest frames on overrun (default: 40 ms).
    pub overrun_margin_ms: u16,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            target_ms: 40,
            frame_ms: 20,
            max_plc_frames: 3,
            overrun_margin_ms: 40,
        }
    }
}

/// Telemetry counters reported by [`JitterBuffer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub buffered_frames: usize,
    pub buffered_ms: u16,
    pub dropped_overrun: u64,
    pub plc_frames: u64,
    pub silence_frames: u64,
    pub underruns: u64,
}

/// Outcome of popping one frame interval from [`JitterBuffer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopOutcome {
    /// A buffered PCM frame is ready for playback.
    Frame(Vec<i16>),
    /// Buffer underrun within PLC budget: caller should run `codec.decode_plc`.
    Plc,
    /// Buffer is prebuffering or exceeded PLC budget: caller should output silence.
    Silence,
}

/// Frame-oriented adaptive jitter buffer for SHT3 audio receivers.
#[derive(Debug)]
pub struct JitterBuffer {
    config: JitterConfig,
    target_frames: usize,
    max_frames: usize,
    queue: VecDeque<Vec<i16>>,
    primed: bool,
    consecutive_underruns: usize,
    dropped_overrun: u64,
    plc_frames: u64,
    silence_frames: u64,
    underruns: u64,
}

impl JitterBuffer {
    pub fn new(mut config: JitterConfig) -> Self {
        if config.frame_ms == 0 {
            config.frame_ms = 20;
        }
        let frame_ms = usize::from(config.frame_ms);
        let target_frames = (usize::from(config.target_ms) / frame_ms).max(1);
        let overrun_frames = (usize::from(config.overrun_margin_ms) / frame_ms).max(1);
        let max_frames = target_frames + overrun_frames;

        Self {
            config,
            target_frames,
            max_frames,
            queue: VecDeque::with_capacity(max_frames + 1),
            primed: false,
            consecutive_underruns: 0,
            dropped_overrun: 0,
            plc_frames: 0,
            silence_frames: 0,
            underruns: 0,
        }
    }

    /// Push a decoded PCM frame into the buffer, dropping the oldest frame(s) on overrun.
    pub fn push(&mut self, pcm_frame: Vec<i16>) {
        self.queue.push_back(pcm_frame);
        while self.queue.len() > self.max_frames {
            self.queue.pop_front();
            self.dropped_overrun += 1;
        }
        if !self.primed && self.queue.len() >= self.target_frames {
            self.primed = true;
        }
    }

    /// Pop one frame interval from the buffer.
    pub fn pop(&mut self) -> PopOutcome {
        if !self.primed {
            self.silence_frames += 1;
            return PopOutcome::Silence;
        }
        if let Some(frame) = self.queue.pop_front() {
            self.consecutive_underruns = 0;
            return PopOutcome::Frame(frame);
        }

        self.underruns += 1;
        self.consecutive_underruns += 1;
        if self.consecutive_underruns <= self.config.max_plc_frames {
            self.plc_frames += 1;
            PopOutcome::Plc
        } else {
            self.silence_frames += 1;
            PopOutcome::Silence
        }
    }

    /// Pop one frame interval and materialize interleaved PCM into `out_pcm`,
    /// automatically invoking `codec.decode_plc` on short underruns or writing silence.
    pub fn pop_with_codec(
        &mut self,
        codec: &mut dyn AudioCodec,
        out_pcm: &mut Vec<i16>,
    ) -> TransportResult<usize> {
        match self.pop() {
            PopOutcome::Frame(frame) => {
                let n = frame.len();
                out_pcm.extend_from_slice(&frame);
                Ok(n)
            }
            PopOutcome::Plc => codec.decode_plc(out_pcm),
            PopOutcome::Silence => {
                let n = codec.interleaved_samples_per_frame();
                out_pcm.resize(out_pcm.len() + n, 0);
                Ok(n)
            }
        }
    }

    /// Clear queued audio and reset prebuffering state (e.g. on `FLAG_DISCONTINUITY`).
    pub fn reset(&mut self) {
        self.queue.clear();
        self.primed = false;
        self.consecutive_underruns = 0;
    }

    pub fn is_primed(&self) -> bool {
        self.primed
    }

    pub fn stats(&self) -> JitterStats {
        let buffered_frames = self.queue.len();
        let buffered_ms =
            (buffered_frames * usize::from(self.config.frame_ms)).min(usize::from(u16::MAX)) as u16;
        JitterStats {
            buffered_frames,
            buffered_ms,
            dropped_overrun: self.dropped_overrun,
            plc_frames: self.plc_frames,
            silence_frames: self.silence_frames,
            underruns: self.underruns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prebuffers_then_plays_then_plc_up_to_3_then_silence() {
        let mut jb = JitterBuffer::new(JitterConfig {
            target_ms: 40,
            frame_ms: 20,
            max_plc_frames: 3,
            overrun_margin_ms: 40,
        });

        // Not primed after 1 frame (< 40ms target).
        jb.push(vec![1, 2]);
        assert!(!jb.is_primed());
        assert_eq!(jb.pop(), PopOutcome::Silence);

        // Primed after 2nd frame (40ms).
        jb.push(vec![3, 4]);
        assert!(jb.is_primed());
        assert_eq!(jb.pop(), PopOutcome::Frame(vec![1, 2]));
        assert_eq!(jb.pop(), PopOutcome::Frame(vec![3, 4]));

        // Next 3 pops trigger PLC.
        assert_eq!(jb.pop(), PopOutcome::Plc);
        assert_eq!(jb.pop(), PopOutcome::Plc);
        assert_eq!(jb.pop(), PopOutcome::Plc);

        // 4th consecutive underrun falls back to Silence.
        assert_eq!(jb.pop(), PopOutcome::Silence);

        let stats = jb.stats();
        assert_eq!(stats.plc_frames, 3);
        assert_eq!(stats.underruns, 4);
    }

    #[test]
    fn overrun_drops_oldest_frames() {
        let mut jb = JitterBuffer::new(JitterConfig {
            target_ms: 40,         // 2 frames
            frame_ms: 20,          // 20 ms
            max_plc_frames: 3,     //
            overrun_margin_ms: 40, // +2 frames => max 4 frames
        });

        for i in 0..6i16 {
            jb.push(vec![i]);
        }

        let stats = jb.stats();
        assert_eq!(stats.buffered_frames, 4);
        assert_eq!(stats.buffered_ms, 80);
        assert_eq!(stats.dropped_overrun, 2);

        // Oldest frames (0, 1) were dropped; next pop is 2.
        assert_eq!(jb.pop(), PopOutcome::Frame(vec![2]));
    }
}
