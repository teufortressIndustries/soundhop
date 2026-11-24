//! Adaptive jitter buffer for SHT3 audio playback with configurable target depth,
//! PLC on short underruns (up to 3 frames), and oldest-frame drop on overrun.

use std::collections::VecDeque;

use crate::transport::codec::AudioCodec;
use crate::transport::dsp::{
    crossfade_stereo_in_place, fade_in_stereo_in_place, fade_out_stereo_in_place,
};
use crate::transport::TransportResult;

/// Stereo frame count (~2 ms @ 48 kHz) used for de-clicking overrun/underrun transitions.
const TRANSITION_RAMP_FRAMES: usize = 96;
/// Number of consecutive clean popped frames before adaptive cushion steps down by 1 frame.
const CLEAN_FRAMES_DECAY_INTERVAL: usize = 250;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastPopKind {
    None,
    Frame,
    Plc,
    Silence,
}

/// Frame-oriented adaptive jitter buffer for SHT3 audio receivers.
#[derive(Debug)]
pub struct JitterBuffer {
    config: JitterConfig,
    target_frames: usize,
    max_frames: usize,
    cushion_frames: usize,
    clean_frames_streak: usize,
    concealed_debt: usize,
    queue: VecDeque<Vec<i16>>,
    primed: bool,
    consecutive_underruns: usize,
    last_pop_kind: LastPopKind,
    last_tail: (i16, i16),
    had_underrun_silence: bool,
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
            cushion_frames: 0,
            clean_frames_streak: 0,
            concealed_debt: 0,
            queue: VecDeque::with_capacity(max_frames + 1),
            primed: false,
            consecutive_underruns: 0,
            last_pop_kind: LastPopKind::None,
            last_tail: (0, 0),
            had_underrun_silence: false,
            dropped_overrun: 0,
            plc_frames: 0,
            silence_frames: 0,
            underruns: 0,
        }
    }

    /// Current adaptive target depth in frames (starts at base `target_frames` and steps
    /// up on underrun bursts up to `max_frames - 1`, decaying back after clean runs).
    pub fn effective_target_frames(&self) -> usize {
        (self.target_frames + self.cushion_frames)
            .min(self.max_frames.saturating_sub(1))
            .max(self.target_frames)
    }

    /// Current extra adaptive cushion frames above the configured `target_frames`.
    pub fn cushion_frames(&self) -> usize {
        self.cushion_frames
    }

    /// Number of concealed/underrun frames waiting to be caught up on the next burst.
    pub fn concealed_debt(&self) -> usize {
        self.concealed_debt
    }

    fn push_inner(&mut self, pcm_frame: Vec<i16>, catchup_debt: bool) {
        self.queue.push_back(pcm_frame);
        let mut last_dropped: Option<Vec<i16>> = None;
        while self.queue.len() > self.max_frames {
            if let Some(dropped) = self.queue.pop_front() {
                last_dropped = Some(dropped);
                self.dropped_overrun += 1;
            }
        }
        let target_depth = self.effective_target_frames();
        if catchup_debt {
            while self.concealed_debt > 0 && self.queue.len() > target_depth {
                if let Some(dropped) = self.queue.pop_front() {
                    last_dropped = Some(dropped);
                    self.dropped_overrun += 1;
                    self.concealed_debt -= 1;
                }
            }
        }
        if let (Some(dropped), Some(front)) = (last_dropped.as_deref(), self.queue.front_mut()) {
            crossfade_stereo_in_place(dropped, front, TRANSITION_RAMP_FRAMES);
        }
        if !self.primed && self.queue.len() >= target_depth {
            self.primed = true;
        }
    }

    /// Push a decoded PCM frame into the buffer, dropping the oldest frame(s) on overrun.
    pub fn push(&mut self, pcm_frame: Vec<i16>) {
        self.push_inner(pcm_frame, false);
    }

    /// Push a decoded PCM frame into a real-time playback buffer, dropping excess delayed
    /// burst frames down toward [`Self::effective_target_frames`] when `concealed_debt > 0`.
    pub fn push_realtime(&mut self, pcm_frame: Vec<i16>) {
        self.push_inner(pcm_frame, true);
    }

    /// Drop one delayed frame from the front of the queue against `concealed_debt`
    /// (with 2 ms stereo crossfade into the new front) when catching up after a stall.
    pub fn catchup_concealed_frame(&mut self) -> bool {
        if self.concealed_debt == 0 || self.queue.len() <= 1 {
            return false;
        }
        if let Some(dropped) = self.queue.pop_front() {
            self.dropped_overrun += 1;
            self.concealed_debt -= 1;
            if let Some(front) = self.queue.front_mut() {
                crossfade_stereo_in_place(&dropped, front, TRANSITION_RAMP_FRAMES);
            }
            true
        } else {
            false
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
            self.clean_frames_streak += 1;
            if self.clean_frames_streak >= CLEAN_FRAMES_DECAY_INTERVAL {
                self.clean_frames_streak = 0;
                if self.cushion_frames > 0 {
                    self.cushion_frames -= 1;
                }
            }
            return PopOutcome::Frame(frame);
        }

        self.underruns += 1;
        self.consecutive_underruns += 1;
        self.clean_frames_streak = 0;
        self.concealed_debt += 1;
        if self.consecutive_underruns == 1 {
            let max_cushion = self
                .max_frames
                .saturating_sub(1)
                .saturating_sub(self.target_frames);
            if self.cushion_frames < max_cushion {
                self.cushion_frames += 1;
            }
        }
        if self.consecutive_underruns <= self.config.max_plc_frames {
            self.plc_frames += 1;
            PopOutcome::Plc
        } else {
            self.silence_frames += 1;
            PopOutcome::Silence
        }
    }

    /// Pop one frame interval and materialize interleaved PCM into `out_pcm`,
    /// automatically invoking `codec.decode_plc` on short underruns or writing silence,
    /// with click-free tail fade-out (`Plc -> Silence`) and fade-in (`Silence -> Frame` after underrun).
    pub fn pop_with_codec(
        &mut self,
        codec: &mut dyn AudioCodec,
        out_pcm: &mut Vec<i16>,
    ) -> TransportResult<usize> {
        let was_primed = self.primed;
        match self.pop() {
            PopOutcome::Frame(frame) => {
                let start = out_pcm.len();
                let n = frame.len();
                if n >= 2 {
                    self.last_tail = (frame[n - 2], frame[n - 1]);
                } else {
                    self.last_tail = (0, 0);
                }
                out_pcm.extend_from_slice(&frame);
                if self.had_underrun_silence {
                    fade_in_stereo_in_place(&mut out_pcm[start..], TRANSITION_RAMP_FRAMES);
                    self.had_underrun_silence = false;
                }
                self.last_pop_kind = LastPopKind::Frame;
                Ok(n)
            }
            PopOutcome::Plc => {
                let start = out_pcm.len();
                let n = codec.decode_plc(out_pcm)?;
                if n >= 2 && out_pcm.len() >= start + n {
                    let end = start + n;
                    self.last_tail = (out_pcm[end - 2], out_pcm[end - 1]);
                } else {
                    self.last_tail = (0, 0);
                }
                self.had_underrun_silence = false;
                self.last_pop_kind = LastPopKind::Plc;
                Ok(n)
            }
            PopOutcome::Silence => {
                let n = codec.interleaved_samples_per_frame();
                let start = out_pcm.len();
                out_pcm.resize(start + n, 0);
                if was_primed {
                    self.had_underrun_silence = true;
                }
                if self.last_pop_kind == LastPopKind::Plc
                    && (self.last_tail.0 != 0 || self.last_tail.1 != 0)
                {
                    let ramp_frames = TRANSITION_RAMP_FRAMES.min(n / 2);
                    for f in 0..ramp_frames {
                        out_pcm[start + f * 2] = self.last_tail.0;
                        out_pcm[start + f * 2 + 1] = self.last_tail.1;
                    }
                    if ramp_frames > 0 {
                        fade_out_stereo_in_place(
                            &mut out_pcm[start..start + ramp_frames * 2],
                            ramp_frames,
                        );
                    }
                }
                self.last_tail = (0, 0);
                self.last_pop_kind = LastPopKind::Silence;
                Ok(n)
            }
        }
    }

    /// Clear queued audio and reset prebuffering state (e.g. on `FLAG_DISCONTINUITY`).
    pub fn reset(&mut self) {
        self.queue.clear();
        self.primed = false;
        self.consecutive_underruns = 0;
        self.concealed_debt = 0;
        self.cushion_frames = 0;
        self.clean_frames_streak = 0;
        self.last_pop_kind = LastPopKind::None;
        self.last_tail = (0, 0);
        self.had_underrun_silence = false;
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

    #[test]
    fn overrun_crossfades_dropped_frame_into_new_front() {
        let mut jb = JitterBuffer::new(JitterConfig {
            target_ms: 40,
            frame_ms: 20,
            max_plc_frames: 3,
            overrun_margin_ms: 20, // max 3 frames
        });

        let frame_samples = 960 * 2;
        jb.push(vec![1_000i16; frame_samples]);
        jb.push(vec![10_000i16; frame_samples]); // last dropped frame
        jb.push(vec![2_000i16; frame_samples]);
        jb.push(vec![2_000i16; frame_samples]);
        jb.push(vec![2_000i16; frame_samples]);

        assert_eq!(jb.stats().dropped_overrun, 2);
        let PopOutcome::Frame(front) = jb.pop() else {
            panic!("expected front frame");
        };
        // Head of new front frame smoothly transitions from dropped tail (10_000) to 2_000.
        assert_eq!(front[0], 10_000);
        assert!(front[TRANSITION_RAMP_FRAMES] > 5_500 && front[TRANSITION_RAMP_FRAMES] < 6_500);
        assert_eq!(front[TRANSITION_RAMP_FRAMES * 2], 2_000);
    }

    #[test]
    fn concealed_debt_and_push_realtime_catches_up_burst() {
        let mut jb = JitterBuffer::new(JitterConfig {
            target_ms: 40,
            frame_ms: 20,
            max_plc_frames: 3,
            overrun_margin_ms: 60, // max 5 frames
        });
        let frame_samples = 960 * 2;
        jb.push_realtime(vec![1_000; frame_samples]);
        jb.push_realtime(vec![1_000; frame_samples]);
        assert!(jb.is_primed());
        assert!(matches!(jb.pop(), PopOutcome::Frame(_)));
        assert!(matches!(jb.pop(), PopOutcome::Frame(_)));

        // Induce 3 PLC + 1 Silence underrun => concealed_debt == 4.
        assert_eq!(jb.pop(), PopOutcome::Plc);
        assert_eq!(jb.pop(), PopOutcome::Plc);
        assert_eq!(jb.pop(), PopOutcome::Plc);
        assert_eq!(jb.pop(), PopOutcome::Silence);
        assert_eq!(jb.concealed_debt(), 4);

        // Delayed burst of 6 frames arrives; push_realtime caps latency to effective_target_frames.
        for v in 1..=6i16 {
            jb.push_realtime(vec![v * 1_000; frame_samples]);
        }
        assert_eq!(jb.stats().buffered_frames, jb.effective_target_frames());
        assert!(jb.concealed_debt() < 4);

        jb.reset();
        assert_eq!(jb.concealed_debt(), 0);
        assert_eq!(jb.cushion_frames(), 0);
    }

    struct NonZeroPlcCodec {
        samples: usize,
        plc_value: i16,
    }

    impl AudioCodec for NonZeroPlcCodec {
        fn codec_id(&self) -> crate::transport::wire::CodecId {
            crate::transport::wire::CodecId::PcmS16Le
        }
        fn frame_samples_per_channel(&self) -> usize {
            self.samples / 2
        }
        fn channels(&self) -> usize {
            2
        }
        fn encode(&mut self, _pcm: &[i16], _out: &mut Vec<u8>) -> TransportResult<usize> {
            Ok(0)
        }
        fn decode(&mut self, _packet: &[u8], _out_pcm: &mut Vec<i16>) -> TransportResult<usize> {
            Ok(0)
        }
        fn decode_plc(&mut self, out_pcm: &mut Vec<i16>) -> TransportResult<usize> {
            out_pcm.resize(out_pcm.len() + self.samples, self.plc_value);
            Ok(self.samples)
        }
    }

    #[test]
    fn pop_with_codec_preserves_pcm_lossless_and_smooths_underrun_transitions() {
        let mut codec = NonZeroPlcCodec {
            samples: 400,
            plc_value: 8_000,
        };
        let mut jb = JitterBuffer::new(JitterConfig {
            target_ms: 40,
            frame_ms: 20,
            max_plc_frames: 1,
            overrun_margin_ms: 40,
        });

        let frame_a: Vec<i16> = (0..400).map(|i| (i as i16) + 100).collect();
        let frame_b: Vec<i16> = (0..400).map(|i| (i as i16) + 500).collect();
        jb.push(frame_a.clone());
        jb.push(frame_b.clone());

        // Contiguous Frame -> Frame must be 100% bit-exact.
        let mut out = Vec::new();
        jb.pop_with_codec(&mut codec, &mut out).unwrap();
        assert_eq!(out, frame_a);
        out.clear();
        jb.pop_with_codec(&mut codec, &mut out).unwrap();
        assert_eq!(out, frame_b);

        // Next pop triggers Plc (value 8_000), then Silence (must fade out from 8_000 to 0).
        out.clear();
        jb.pop_with_codec(&mut codec, &mut out).unwrap();
        assert!(out.iter().all(|&s| s == 8_000));

        out.clear();
        jb.pop_with_codec(&mut codec, &mut out).unwrap();
        assert!(out[0] > 7_500);
        assert_eq!(out[TRANSITION_RAMP_FRAMES * 2 - 1], 0);
        assert!(out[TRANSITION_RAMP_FRAMES * 2..].iter().all(|&s| s == 0));

        // Subsequent Silence -> Frame after underrun must fade in smoothly from zero.
        let frame_c = vec![6_000i16; 400];
        jb.push(frame_c);
        out.clear();
        jb.pop_with_codec(&mut codec, &mut out).unwrap();
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 0);
        assert!(out[TRANSITION_RAMP_FRAMES] > 2_500 && out[TRANSITION_RAMP_FRAMES] < 3_500);
        assert_eq!(out[TRANSITION_RAMP_FRAMES * 2], 6_000);
    }

    #[test]
    fn adaptive_target_depth_steps_up_on_underrun_and_decays_after_clean_run() {
        let mut jb = JitterBuffer::new(JitterConfig {
            target_ms: 40,         // 2 frames
            frame_ms: 20,          //
            max_plc_frames: 3,     //
            overrun_margin_ms: 40, // max 4 frames => cap at max_frames - 1 = 3
        });
        assert_eq!(jb.effective_target_frames(), 2);
        assert_eq!(jb.cushion_frames(), 0);

        jb.push(vec![1, 2]);
        jb.push(vec![3, 4]);
        assert!(matches!(jb.pop(), PopOutcome::Frame(_)));
        assert!(matches!(jb.pop(), PopOutcome::Frame(_)));

        // Underrun burst steps cushion up by +1 (capped at max_frames - 1 = 3).
        assert_eq!(jb.pop(), PopOutcome::Plc);
        assert_eq!(jb.pop(), PopOutcome::Plc);
        assert_eq!(jb.cushion_frames(), 1);
        assert_eq!(jb.effective_target_frames(), 3);

        // Even a second burst cannot push effective_target_frames beyond max_frames - 1.
        jb.push(vec![5, 6]);
        assert!(matches!(jb.pop(), PopOutcome::Frame(_)));
        assert_eq!(jb.pop(), PopOutcome::Plc);
        assert_eq!(jb.effective_target_frames(), 3);

        // 250 consecutive clean frames decays cushion back down to 0.
        for _ in 0..CLEAN_FRAMES_DECAY_INTERVAL {
            jb.push(vec![7, 8]);
            assert!(matches!(jb.pop(), PopOutcome::Frame(_)));
        }
        assert_eq!(jb.cushion_frames(), 0);
        assert_eq!(jb.effective_target_frames(), 2);
    }
}
