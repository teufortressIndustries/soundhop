//! Shared audio DSP helpers: gain/mute scaling and persistent-phase linear resamplers
//! used by both audio backends and network transports.

use std::collections::VecDeque;

/// Linear fade-out duration in stereo frames (~1.33 ms at 48 kHz) on playback underrun.
pub const UNDERRUN_TAIL_FADE_FRAMES: usize = 64;
/// Linear fade-in duration in stereo frames (~1.33 ms at 48 kHz) on playback resume.
pub const RESUME_FADE_IN_FRAMES: usize = 64;

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

/// Apply gain and mute in-place across a slice of signed 16-bit PCM samples.
pub fn apply_gain_buffer(samples: &mut [i16], volume_pct: u32, muted: bool) {
    if muted || volume_pct == 0 {
        samples.fill(0);
        return;
    }
    if volume_pct == 100 {
        return;
    }
    for s in samples.iter_mut() {
        *s = apply_gain_i16(*s, volume_pct, false);
    }
}

#[inline]
pub(crate) fn lerp_i16(a: i16, b: i16, t: f64) -> i16 {
    let v = f64::from(a) + (f64::from(b) - f64::from(a)) * t;
    v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// Persistent-phase rational linear resampler for push-based stereo frame streams.
///
/// Carries exact rational sample position (`pos_num / dst_rate`) and the previous boundary
/// frame across calls so sample-rate conversion (e.g. 44.1 kHz <-> 48 kHz) has zero phase
/// discontinuities or clicks.
#[derive(Debug, Clone)]
pub struct LinearResampler {
    src_rate: u32,
    dst_rate: u32,
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

    pub fn src_rate(&self) -> u32 {
        self.src_rate
    }

    pub fn dst_rate(&self) -> u32 {
        self.dst_rate
    }

    pub fn reset(&mut self) {
        self.pos_num = 0;
        self.prev = (0, 0);
        self.has_prev = false;
    }

    /// Resample stereo `frames` into interleaved `i16` samples appended to `out`.
    pub fn resample_to_i16(&mut self, frames: &[(i16, i16)], out: &mut Vec<i16>) {
        if frames.is_empty() {
            return;
        }
        if self.src_rate == self.dst_rate {
            out.reserve(frames.len() * 2);
            for &(l, r) in frames {
                out.push(l);
                out.push(r);
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
        out.reserve(est_out * 2);

        while self.pos_num < limit_num {
            let (a, b, t) = if self.pos_num < 0 {
                (self.prev, frames[0], (self.pos_num + dst) as f64 / dst_f)
            } else {
                let idx = (self.pos_num / dst) as usize;
                let rem = self.pos_num % dst;
                (frames[idx], frames[idx + 1], rem as f64 / dst_f)
            };
            out.push(lerp_i16(a.0, b.0, t));
            out.push(lerp_i16(a.1, b.1, t));
            self.pos_num += src;
        }

        self.prev = frames[frames.len() - 1];
        self.pos_num -= (frames.len() as i64) * dst;
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

        let mut tmp = Vec::new();
        self.resample_to_i16(frames, &mut tmp);
        out.reserve(tmp.len() * 2);
        for s in tmp {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
}

/// Persistent-phase linear resampler for pull-based playback callbacks reading from a `VecDeque<i16>`.
#[derive(Debug, Clone)]
pub struct PlaybackResampler {
    src_rate: u32,
    dst_rate: u32,
    rate_adjust: f64,
    phase: f64,
    prev: (i16, i16),
    next: (i16, i16),
    initialized: bool,
    prebuffer_samples: usize,
    smooth_underruns: bool,
    primed: bool,
    last_emitted: (i16, i16),
    tail_pos: usize,
    tail_total: usize,
    tail_start: (i16, i16),
    fade_in_pos: usize,
    fade_in_total: usize,
    fade_in_start: (i16, i16),
}

impl PlaybackResampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            src_rate: src_rate.max(1),
            dst_rate: dst_rate.max(1),
            rate_adjust: 1.0,
            phase: 0.0,
            prev: (0, 0),
            next: (0, 0),
            initialized: false,
            prebuffer_samples: 0,
            smooth_underruns: false,
            primed: false,
            last_emitted: (0, 0),
            tail_pos: 0,
            tail_total: 0,
            tail_start: (0, 0),
            fade_in_pos: 0,
            fade_in_total: 0,
            fade_in_start: (0, 0),
        }
    }

    /// Create a resampler with optional prebuffering and click-free underrun tail fade-out / resume fade-in.
    pub fn with_prebuffer(src_rate: u32, dst_rate: u32, prebuffer_samples: usize) -> Self {
        let mut resampler = Self::new(src_rate, dst_rate);
        resampler.prebuffer_samples = prebuffer_samples;
        resampler.smooth_underruns = true;
        resampler
    }

    /// Set a fine multiplicative rate adjustment factor around `1.0` for clock-drift compensation.
    pub fn set_rate_adjust(&mut self, ratio: f64) {
        if ratio.is_finite() {
            self.rate_adjust = ratio.clamp(0.5, 2.0);
        }
    }

    /// Current multiplicative rate adjustment factor.
    pub fn rate_adjust(&self) -> f64 {
        self.rate_adjust
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.prev = (0, 0);
        self.next = (0, 0);
        self.initialized = false;
        self.primed = false;
        self.last_emitted = (0, 0);
        self.tail_pos = 0;
        self.tail_total = 0;
        self.tail_start = (0, 0);
        self.fade_in_pos = 0;
        self.fade_in_total = 0;
        self.fade_in_start = (0, 0);
    }

    #[inline]
    fn is_unity_rate(&self) -> bool {
        self.src_rate == self.dst_rate && (self.rate_adjust - 1.0).abs() <= 1e-9
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
        write_sample: F,
    ) {
        let _ = self.write_frames_checked(
            buf,
            src_channels,
            dst_channels,
            out_frames,
            volume_pct,
            muted,
            write_sample,
        );
    }

    /// Fill `out_frames` from `buf`, returning `true` if a primed buffer ran empty (underrun).
    #[allow(clippy::too_many_arguments)]
    pub fn write_frames_checked<F: FnMut(usize, i16)>(
        &mut self,
        buf: &mut VecDeque<i16>,
        src_channels: usize,
        dst_channels: usize,
        out_frames: usize,
        volume_pct: u32,
        muted: bool,
        mut write_sample: F,
    ) -> bool {
        let dst_channels = dst_channels.max(1);
        let unity_rate = self.is_unity_rate();

        if !self.smooth_underruns {
            if unity_rate && !self.initialized {
                for f in 0..out_frames {
                    let (raw_l, raw_r) = pop_frame_pair(buf, src_channels);
                    let l = apply_gain_i16(raw_l, volume_pct, muted);
                    let r = apply_gain_i16(raw_r, volume_pct, muted);
                    write_stereo_to_channels(f, dst_channels, l, r, &mut write_sample);
                }
                return false;
            }

            if !self.initialized {
                self.prev = pop_frame_pair(buf, src_channels);
                self.next = pop_frame_pair(buf, src_channels);
                self.initialized = true;
            }

            let step = (f64::from(self.src_rate) / f64::from(self.dst_rate)) * self.rate_adjust;
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
            return false;
        }

        if !self.primed {
            let min_ready = if self.prebuffer_samples > 0 {
                self.prebuffer_samples
            } else {
                src_channels.max(1)
            };
            if buf.len() >= min_ready {
                self.primed = true;
                self.fade_in_start = self.last_emitted;
                self.fade_in_pos = 0;
                self.fade_in_total = RESUME_FADE_IN_FRAMES;
                self.tail_pos = 0;
                self.tail_total = 0;
            } else {
                for f in 0..out_frames {
                    let (raw_l, raw_r) = self.next_tail_or_silence();
                    let l = apply_gain_i16(raw_l, volume_pct, muted);
                    let r = apply_gain_i16(raw_r, volume_pct, muted);
                    write_stereo_to_channels(f, dst_channels, l, r, &mut write_sample);
                }
                return false;
            }
        }

        let mut underrun_occurred = false;
        if unity_rate && !self.initialized {
            for f in 0..out_frames {
                let (raw_l, raw_r) = if self.primed {
                    if buf.is_empty() {
                        self.trigger_underrun();
                        underrun_occurred = true;
                        self.next_tail_or_silence()
                    } else {
                        let raw = pop_frame_pair(buf, src_channels);
                        self.apply_resume_fade(raw)
                    }
                } else {
                    self.next_tail_or_silence()
                };
                let l = apply_gain_i16(raw_l, volume_pct, muted);
                let r = apply_gain_i16(raw_r, volume_pct, muted);
                write_stereo_to_channels(f, dst_channels, l, r, &mut write_sample);
            }
            return underrun_occurred;
        }

        if !self.initialized {
            if buf.is_empty() {
                self.trigger_underrun();
                underrun_occurred = true;
            } else {
                self.prev = pop_frame_pair(buf, src_channels);
                self.next = if buf.is_empty() {
                    self.prev
                } else {
                    pop_frame_pair(buf, src_channels)
                };
                self.phase = 0.0;
                self.initialized = true;
            }
        }

        let step = (f64::from(self.src_rate) / f64::from(self.dst_rate)) * self.rate_adjust;
        for f in 0..out_frames {
            let (raw_l, raw_r) = if self.primed {
                let interp_l = lerp_i16(self.prev.0, self.next.0, self.phase);
                let interp_r = lerp_i16(self.prev.1, self.next.1, self.phase);
                let pair = self.apply_resume_fade((interp_l, interp_r));

                self.phase += step;
                while self.phase >= 1.0 {
                    self.phase -= 1.0;
                    self.prev = self.next;
                    if buf.is_empty() {
                        if f + 1 < out_frames {
                            self.trigger_underrun();
                            underrun_occurred = true;
                        } else {
                            self.initialized = false;
                            self.phase = 0.0;
                        }
                        break;
                    }
                    self.next = pop_frame_pair(buf, src_channels);
                }
                pair
            } else {
                self.next_tail_or_silence()
            };
            let l = apply_gain_i16(raw_l, volume_pct, muted);
            let r = apply_gain_i16(raw_r, volume_pct, muted);
            write_stereo_to_channels(f, dst_channels, l, r, &mut write_sample);
        }
        underrun_occurred
    }

    fn trigger_underrun(&mut self) {
        self.primed = false;
        self.initialized = false;
        self.phase = 0.0;
        self.tail_start = self.last_emitted;
        self.tail_pos = 0;
        self.tail_total = UNDERRUN_TAIL_FADE_FRAMES;
        self.fade_in_pos = 0;
        self.fade_in_total = 0;
    }

    fn next_tail_or_silence(&mut self) -> (i16, i16) {
        if self.tail_pos < self.tail_total {
            self.tail_pos += 1;
            let t = (self.tail_pos as f64) / ((self.tail_total + 1) as f64);
            let l = lerp_i16(self.tail_start.0, 0, t);
            let r = lerp_i16(self.tail_start.1, 0, t);
            self.last_emitted = (l, r);
            (l, r)
        } else {
            self.last_emitted = (0, 0);
            (0, 0)
        }
    }

    fn apply_resume_fade(&mut self, raw: (i16, i16)) -> (i16, i16) {
        let out = if self.fade_in_pos < self.fade_in_total {
            self.fade_in_pos += 1;
            let t = (self.fade_in_pos as f64) / ((self.fade_in_total + 1) as f64);
            (
                lerp_i16(self.fade_in_start.0, raw.0, t),
                lerp_i16(self.fade_in_start.1, raw.1, t),
            )
        } else {
            raw
        };
        self.last_emitted = out;
        out
    }
}

#[inline]
pub(crate) fn pop_frame_pair(buf: &mut VecDeque<i16>, src_channels: usize) -> (i16, i16) {
    if src_channels <= 1 {
        let s = buf.pop_front().unwrap_or(0);
        (s, s)
    } else {
        let l = buf.pop_front().unwrap_or(0);
        let r = buf.pop_front().unwrap_or(l);
        for _ in 2..src_channels {
            let _ = buf.pop_front();
        }
        (l, r)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_gain_handles_mute_unity_and_clamp() {
        assert_eq!(apply_gain_i16(10_000, 100, true), 0);
        assert_eq!(apply_gain_i16(10_000, 0, false), 0);
        assert_eq!(apply_gain_i16(10_000, 100, false), 10_000);
        assert_eq!(apply_gain_i16(10_000, 50, false), 5_000);
        assert_eq!(apply_gain_i16(30_000, 150, false), i16::MAX);
        assert_eq!(apply_gain_i16(-30_000, 150, false), i16::MIN);

        let mut buf = [1000i16, -2000, 3000];
        apply_gain_buffer(&mut buf, 50, false);
        assert_eq!(buf, [500, -1000, 1500]);
        apply_gain_buffer(&mut buf, 100, true);
        assert_eq!(buf, [0, 0, 0]);
    }

    #[test]
    fn linear_resampler_44100_to_48000_preserves_ratio() {
        let mut r = LinearResampler::new(44_100, 48_000);
        let input: Vec<(i16, i16)> = (0..4410)
            .map(|i| {
                let s = ((i as f64 * 0.05).sin() * 12_000.0) as i16;
                (s, -s)
            })
            .collect();
        let mut out = Vec::new();
        // Push in chunks to exercise persistent boundary phase.
        for chunk in input.chunks(441) {
            r.resample_to_i16(chunk, &mut out);
        }
        let out_frames = out.len() / 2;
        // 4410 frames @ 44.1kHz = 0.1s => ~4800 frames @ 48kHz.
        assert!((4795..=4805).contains(&out_frames), "got {out_frames}");
    }

    #[test]
    fn playback_resampler_reset_clears_phase_and_history() {
        let mut r = PlaybackResampler::new(44_100, 48_000);
        let input: Vec<i16> = (0..200).map(|i| ((i * 37) % 10_000) as i16).collect();
        let mut q1 = VecDeque::from(input.clone());
        let mut out1 = vec![0i16; 160];
        r.write_frames(&mut q1, 2, 2, 80, 100, false, |idx, s| out1[idx] = s);

        r.reset();
        let mut q2 = VecDeque::from(input);
        let mut out2 = vec![0i16; 160];
        r.write_frames(&mut q2, 2, 2, 80, 100, false, |idx, s| out2[idx] = s);
        assert_eq!(out1, out2);
    }

    #[test]
    fn playback_resampler_rate_adjust_preserves_unity_and_adjusts_consumption() {
        // Bit-exact passthrough when src_rate == dst_rate && rate_adjust == 1.0.
        let mut exact = PlaybackResampler::new(48_000, 48_000);
        assert!((exact.rate_adjust() - 1.0).abs() <= f64::EPSILON);
        let input: Vec<i16> = (0..400)
            .map(|i| ((i * 53 - 10_000) % 20_000) as i16)
            .collect();
        let mut q = VecDeque::from(input.clone());
        let mut out = vec![0i16; 400];
        exact.write_frames(&mut q, 2, 2, 200, 100, false, |idx, s| out[idx] = s);
        assert_eq!(out, input);
        assert!(q.is_empty());

        // Fast rate_adjust (> 1.0) consumes more source samples per output frame.
        let mut fast = PlaybackResampler::new(48_000, 48_000);
        fast.set_rate_adjust(1.005);
        assert!((fast.rate_adjust() - 1.005).abs() <= 1e-12);
        let mut q_fast = VecDeque::from(vec![0i16; 4_000]);
        let mut out_fast = vec![0i16; 2_000];
        fast.write_frames(&mut q_fast, 2, 2, 1_000, 100, false, |idx, s| {
            out_fast[idx] = s
        });
        let consumed_fast_frames = (4_000 - q_fast.len()) / 2;
        assert!(
            consumed_fast_frames > 1_000,
            "expected > 1000 consumed frames, got {consumed_fast_frames}"
        );

        // Slow rate_adjust (< 1.0) consumes fewer source samples per output frame.
        let mut slow = PlaybackResampler::new(48_000, 48_000);
        slow.set_rate_adjust(0.995);
        let mut q_slow = VecDeque::from(vec![0i16; 4_000]);
        let mut out_slow = vec![0i16; 2_000];
        slow.write_frames(&mut q_slow, 2, 2, 1_000, 100, false, |idx, s| {
            out_slow[idx] = s
        });
        let consumed_slow_frames = (4_000 - q_slow.len()) / 2;
        assert!(
            consumed_slow_frames < 1_000,
            "expected < 1000 consumed frames, got {consumed_slow_frames}"
        );
    }
}
