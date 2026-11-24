//! Standalone audio DSP helpers for the SHT3 transport pipeline:
//! gain/mute scaling and persistent-phase linear resamplers.

use std::collections::VecDeque;

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

/// Default ramp duration in stereo frames (~4 ms at 48 kHz) for [`GainRamp`].
pub const DEFAULT_GAIN_RAMP_FRAMES: usize = 192;

/// Stateful smooth gain/mute interpolator that linearly ramps gain over ~4 ms
/// (`192` stereo frames @ 48 kHz) on target changes while staying bit-transparent
/// when settled at unity (`volume_pct == 100 && !muted`) or exact zero when muted.
#[derive(Debug, Clone)]
pub struct GainRamp {
    ramp_frames: usize,
    current_gain: f32,
    start_gain: f32,
    target_gain: f32,
    target_volume_pct: u32,
    target_muted: bool,
    ramp_pos: usize,
}

impl Default for GainRamp {
    fn default() -> Self {
        Self::new()
    }
}

impl GainRamp {
    /// Create a [`GainRamp`] settled at unity gain (`100%`, unmuted).
    pub fn new() -> Self {
        Self::with_initial(100, false)
    }

    /// Create a [`GainRamp`] settled at the given initial volume and mute state.
    pub fn with_initial(volume_pct: u32, muted: bool) -> Self {
        let gain = Self::target_gain_for(volume_pct, muted);
        Self {
            ramp_frames: DEFAULT_GAIN_RAMP_FRAMES,
            current_gain: gain,
            start_gain: gain,
            target_gain: gain,
            target_volume_pct: volume_pct,
            target_muted: muted,
            ramp_pos: DEFAULT_GAIN_RAMP_FRAMES,
        }
    }

    /// Override the linear ramp length in audio frames (clamped to at least 1).
    pub fn with_ramp_frames(mut self, ramp_frames: usize) -> Self {
        let frames = ramp_frames.max(1);
        let was_settled = self.is_settled();
        self.ramp_frames = frames;
        if was_settled {
            self.ramp_pos = frames;
        }
        self
    }

    #[inline]
    fn target_gain_for(volume_pct: u32, muted: bool) -> f32 {
        if muted || volume_pct == 0 {
            0.0
        } else {
            (volume_pct.min(150) as f32) / 100.0
        }
    }

    /// Update target volume/mute; begins a linear ramp if the effective gain changed.
    pub fn set_target(&mut self, volume_pct: u32, muted: bool) {
        let new_target = Self::target_gain_for(volume_pct, muted);
        self.target_volume_pct = volume_pct;
        self.target_muted = muted;
        if (new_target - self.target_gain).abs() > f32::EPSILON {
            self.start_gain = self.current_gain;
            self.target_gain = new_target;
            self.ramp_pos = 0;
        }
    }

    /// Snap immediately to the specified volume/mute without ramping.
    pub fn reset(&mut self, volume_pct: u32, muted: bool) {
        let gain = Self::target_gain_for(volume_pct, muted);
        self.current_gain = gain;
        self.start_gain = gain;
        self.target_gain = gain;
        self.target_volume_pct = volume_pct;
        self.target_muted = muted;
        self.ramp_pos = self.ramp_frames;
    }

    /// Returns `true` when the ramp has reached its target gain.
    #[inline]
    pub fn is_settled(&self) -> bool {
        self.ramp_pos >= self.ramp_frames
    }

    /// Current instantaneous linear gain multiplier.
    #[inline]
    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    #[inline]
    fn next_frame_gain(&mut self) -> f32 {
        if self.ramp_pos >= self.ramp_frames {
            self.current_gain = self.target_gain;
            return self.target_gain;
        }
        self.ramp_pos += 1;
        if self.ramp_pos >= self.ramp_frames {
            self.current_gain = self.target_gain;
        } else {
            let t = (self.ramp_pos as f32) / (self.ramp_frames as f32);
            self.current_gain = self.start_gain + (self.target_gain - self.start_gain) * t;
        }
        self.current_gain
    }

    /// Process interleaved `i16` samples in-place for the given channel count.
    ///
    /// Bit-transparent passthrough when settled at `100%` unmuted, and exact zero fill
    /// when settled muted / `0%`.
    pub fn process_interleaved(&mut self, samples: &mut [i16], channels: usize) {
        if samples.is_empty() {
            return;
        }
        let ch = channels.max(1);
        if self.is_settled() {
            if self.target_muted || self.target_volume_pct == 0 {
                samples.fill(0);
                return;
            }
            if self.target_volume_pct == 100 {
                return;
            }
            for s in samples.iter_mut() {
                *s = apply_gain_i16(*s, self.target_volume_pct, false);
            }
            return;
        }

        for frame in samples.chunks_mut(ch) {
            let gain = self.next_frame_gain();
            for s in frame.iter_mut() {
                *s = (f32::from(*s) * gain)
                    .round()
                    .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
            }
        }
    }

    /// Convenience wrapper that updates target `(volume_pct, muted)` and processes `samples`.
    pub fn apply(&mut self, samples: &mut [i16], channels: usize, volume_pct: u32, muted: bool) {
        self.set_target(volume_pct, muted);
        self.process_interleaved(samples, channels);
    }
}

#[inline]
fn scale_i16(sample: i16, gain: f32) -> i16 {
    (f32::from(sample) * gain)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// Linearly crossfade the tail of `from_tail` into the head of `into_head` in-place,
/// aligned on stereo (2-channel) frame boundaries.
pub fn crossfade_stereo_in_place(from_tail: &[i16], into_head: &mut [i16], ramp_frames: usize) {
    let frames = ramp_frames
        .min(from_tail.len() / 2)
        .min(into_head.len() / 2);
    if frames == 0 {
        return;
    }
    let tail_start = ((from_tail.len() / 2) - frames) * 2;
    let denom = frames as f32;
    for f in 0..frames {
        let alpha = (f as f32) / denom;
        let inv = 1.0 - alpha;
        let head_idx = f * 2;
        let tail_idx = tail_start + head_idx;
        for ch in 0..2 {
            let blended = f32::from(from_tail[tail_idx + ch]) * inv
                + f32::from(into_head[head_idx + ch]) * alpha;
            into_head[head_idx + ch] = blended
                .round()
                .clamp(f32::from(i16::MIN), f32::from(i16::MAX))
                as i16;
        }
    }
}

/// Linearly fade out the trailing `ramp_frames` stereo frames of `samples` to zero in-place.
pub fn fade_out_stereo_in_place(samples: &mut [i16], ramp_frames: usize) {
    let total_frames = samples.len() / 2;
    let frames = ramp_frames.min(total_frames);
    if frames == 0 {
        return;
    }
    let start_frame = total_frames - frames;
    let denom = frames as f32;
    for f in 0..frames {
        let gain = ((frames - 1 - f) as f32) / denom;
        let idx = (start_frame + f) * 2;
        samples[idx] = scale_i16(samples[idx], gain);
        samples[idx + 1] = scale_i16(samples[idx + 1], gain);
    }
}

/// Linearly fade in the leading `ramp_frames` stereo frames of `samples` from zero in-place.
pub fn fade_in_stereo_in_place(samples: &mut [i16], ramp_frames: usize) {
    let total_frames = samples.len() / 2;
    let frames = ramp_frames.min(total_frames);
    if frames == 0 {
        return;
    }
    let denom = frames as f32;
    for f in 0..frames {
        let gain = (f as f32) / denom;
        let idx = f * 2;
        samples[idx] = scale_i16(samples[idx], gain);
        samples[idx + 1] = scale_i16(samples[idx + 1], gain);
    }
}

#[inline]
fn lerp_i16(a: i16, b: i16, t: f64) -> i16 {
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

    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.prev = (0, 0);
        self.next = (0, 0);
        self.initialized = false;
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
fn pop_frame_pair(buf: &mut VecDeque<i16>, src_channels: usize) -> (i16, i16) {
    if src_channels <= 1 {
        let s = buf.pop_front().unwrap_or(0);
        (s, s)
    } else {
        let l = buf.pop_front().unwrap_or(0);
        let r = buf.pop_front().unwrap_or(0);
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
    fn gain_ramp_bit_transparent_unity_exact_mute_and_linear_ramp() {
        let mut ramp = GainRamp::new();
        assert!(ramp.is_settled());

        // Settled at 100% unmuted => exact bit-transparent passthrough.
        let original = vec![10_000i16, -12_345, i16::MAX, i16::MIN, 1, -1];
        let mut buf = original.clone();
        ramp.process_interleaved(&mut buf, 2);
        assert_eq!(buf, original);

        // Ramp to muted over 192 stereo frames (384 interleaved samples).
        let mut ramp_buf = vec![10_000i16; DEFAULT_GAIN_RAMP_FRAMES * 2 + 20];
        ramp.apply(&mut ramp_buf, 2, 100, true);
        assert!(ramp.is_settled());
        // First stereo frame still near unity (~0.995 * 10_000), midpoint ~5_000, settled end exact 0.
        assert!(ramp_buf[0] > 9_800 && ramp_buf[0] < 10_000);
        let mid_idx = (DEFAULT_GAIN_RAMP_FRAMES / 2 - 1) * 2;
        assert!((ramp_buf[mid_idx] - 5_000).abs() <= 60);
        assert_eq!(ramp_buf[DEFAULT_GAIN_RAMP_FRAMES * 2 - 2], 0);
        assert!(ramp_buf[DEFAULT_GAIN_RAMP_FRAMES * 2..]
            .iter()
            .all(|&s| s == 0));

        // Changing volume while muted keeps gain settled at 0 without unmuting.
        ramp.set_target(50, true);
        assert!(ramp.is_settled());
        let mut muted_buf = vec![4_000i16; 16];
        ramp.process_interleaved(&mut muted_buf, 2);
        assert!(muted_buf.iter().all(|&s| s == 0));

        // Unmuting to 50% ramps linearly from 0.0 to 0.5.
        let mut unmute_buf = vec![10_000i16; DEFAULT_GAIN_RAMP_FRAMES * 2 + 4];
        ramp.apply(&mut unmute_buf, 2, 50, false);
        assert!(unmute_buf[0] < 100);
        assert_eq!(unmute_buf[DEFAULT_GAIN_RAMP_FRAMES * 2], 5_000);
    }

    #[test]
    fn stereo_declicking_crossfade_fade_in_and_fade_out() {
        let from_tail = [10_000i16, -10_000].repeat(8);
        let mut into_head = [2_000i16, 4_000].repeat(10);
        crossfade_stereo_in_place(&from_tail, &mut into_head, 4);
        assert_eq!(into_head[0], 10_000);
        assert_eq!(into_head[1], -10_000);
        assert_eq!(into_head[4], 6_000);
        assert_eq!(into_head[5], -3_000);
        // Samples past ramp_frames remain untouched.
        assert_eq!(
            &into_head[8..],
            &[2_000, 4_000, 2_000, 4_000, 2_000, 4_000, 2_000, 4_000, 2_000, 4_000, 2_000, 4_000]
        );

        let mut fade_in = vec![
            1_000i16, -2_000, 1_000, -2_000, 1_000, -2_000, 1_000, -2_000,
        ];
        fade_in_stereo_in_place(&mut fade_in, 2);
        assert_eq!(fade_in[0], 0);
        assert_eq!(fade_in[1], 0);
        assert_eq!(fade_in[2], 500);
        assert_eq!(fade_in[3], -1_000);
        assert_eq!(fade_in[4], 1_000);
        assert_eq!(fade_in[5], -2_000);

        let mut fade_out = vec![
            1_000i16, -2_000, 1_000, -2_000, 1_000, -2_000, 1_000, -2_000,
        ];
        fade_out_stereo_in_place(&mut fade_out, 2);
        assert_eq!(fade_out[0], 1_000);
        assert_eq!(fade_out[1], -2_000);
        assert_eq!(fade_out[4], 500);
        assert_eq!(fade_out[5], -1_000);
        assert_eq!(fade_out[6], 0);
        assert_eq!(fade_out[7], 0);
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
}
