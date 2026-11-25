//! Standalone audio DSP helpers for the SHT3 transport pipeline:
//! gain/mute scaling, persistent-phase resamplers, and adaptive clock-drift compensation.

pub use crate::audio_dsp::{
    apply_gain_buffer, apply_gain_i16, LinearResampler, PlaybackResampler, RESUME_FADE_IN_FRAMES,
    UNDERRUN_TAIL_FADE_FRAMES,
};

/// Default ramp duration in stereo frames (~4 ms at 48 kHz) for [`GainRamp`].
pub const DEFAULT_GAIN_RAMP_FRAMES: usize = 192;

/// Compute linear gain multiplier for `volume_pct` (`0..=150`) and `muted` state.
#[inline]
pub fn target_gain_factor(volume_pct: u32, muted: bool) -> f32 {
    if muted || volume_pct == 0 {
        0.0
    } else {
        (volume_pct.min(150) as f32) / 100.0
    }
}

/// Scale a signed 16-bit PCM sample by a linear `gain` multiplier, clamping to `i16`.
#[inline]
pub fn scale_sample_f32(sample: i16, gain: f32) -> i16 {
    if gain <= 0.0 {
        return 0;
    }
    (f32::from(sample) * gain)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// Encode interleaved `i16` PCM samples as S16LE bytes, smoothly ramping gain from
/// `*current_gain` to the target `(volume_pct, muted)` over up to [`DEFAULT_GAIN_RAMP_FRAMES`]
/// stereo frames.
pub fn encode_ramped_s16le(
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

    if samples.is_empty() {
        *current_gain = target;
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

    let ramp_frames = stereo_frames.min(DEFAULT_GAIN_RAMP_FRAMES);
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

/// Default EMA smoothing factor for buffer fill error in [`DriftController`].
pub const DEFAULT_DRIFT_EMA_ALPHA: f64 = 0.05;
/// Default proportional gain (`ratio` offset per ms of smoothed fill error).
pub const DEFAULT_DRIFT_KP: f64 = 2.5e-5;
/// Default integral gain (`ratio` offset per `ms * sec` of smoothed fill error).
pub const DEFAULT_DRIFT_KI: f64 = 2.0e-6;
/// Anti-windup clamp on the integral term (`±2000 ppm`).
pub const DEFAULT_DRIFT_MAX_INTEGRAL: f64 = 0.002;
/// Maximum allowed resampling ratio deviation from `1.0` (`±5000 ppm`).
pub const DEFAULT_DRIFT_MAX_OFFSET: f64 = 0.005;
/// Maximum ratio slew rate (`50e-6` per `20 ms` step = `2.5e-3` per second).
pub const DEFAULT_DRIFT_MAX_SLEW_PER_SEC: f64 = 50e-6 / 0.020;

/// Adaptive PI clock-drift controller for SHT3 audio playback buffering.
///
/// Tracks `err_ms = smoothed_fill_ms - target_ms` and outputs a slew-rate-limited
/// resampling ratio around `1.0`:
/// - `measured_ms > target_ms` (sender faster than receiver DAC): `ratio > 1.0`,
///   advancing input phase faster to drain excess buffer.
/// - `measured_ms < target_ms` (sender slower than receiver DAC): `ratio < 1.0`,
///   advancing input phase slower to replenish the buffer.
#[derive(Debug, Clone)]
pub struct DriftController {
    target_ms: f64,
    ema_alpha: f64,
    kp: f64,
    ki: f64,
    max_integral: f64,
    max_offset: f64,
    max_slew_per_sec: f64,
    smoothed_fill_ms: f64,
    has_measurement: bool,
    integral: f64,
    ratio: f64,
}

impl DriftController {
    /// Create a [`DriftController`] targeting `target_ms` with default PI/EMA/slew parameters.
    pub fn new(target_ms: f64) -> Self {
        Self::with_params(
            target_ms,
            DEFAULT_DRIFT_EMA_ALPHA,
            DEFAULT_DRIFT_KP,
            DEFAULT_DRIFT_KI,
            DEFAULT_DRIFT_MAX_SLEW_PER_SEC,
        )
    }

    /// Create a [`DriftController`] with explicit EMA, PI, and slew-rate parameters.
    pub fn with_params(
        target_ms: f64,
        ema_alpha: f64,
        kp: f64,
        ki: f64,
        max_slew_per_sec: f64,
    ) -> Self {
        let target = if target_ms.is_finite() {
            target_ms.max(0.0)
        } else {
            40.0
        };
        let alpha = if ema_alpha.is_finite() {
            ema_alpha.clamp(1e-6, 1.0)
        } else {
            DEFAULT_DRIFT_EMA_ALPHA
        };
        Self {
            target_ms: target,
            ema_alpha: alpha,
            kp: if kp.is_finite() {
                kp.max(0.0)
            } else {
                DEFAULT_DRIFT_KP
            },
            ki: if ki.is_finite() {
                ki.max(0.0)
            } else {
                DEFAULT_DRIFT_KI
            },
            max_integral: DEFAULT_DRIFT_MAX_INTEGRAL,
            max_offset: DEFAULT_DRIFT_MAX_OFFSET,
            max_slew_per_sec: if max_slew_per_sec.is_finite() {
                max_slew_per_sec.max(0.0)
            } else {
                DEFAULT_DRIFT_MAX_SLEW_PER_SEC
            },
            smoothed_fill_ms: target,
            has_measurement: false,
            integral: 0.0,
            ratio: 1.0,
        }
    }

    /// Feed a new buffer fill measurement (`measured_ms`) after `dt_secs` elapsed
    /// and return the updated slew-rate-limited resampling `ratio`.
    pub fn update(&mut self, measured_ms: f64, dt_secs: f64) -> f64 {
        if !measured_ms.is_finite() {
            return self.ratio;
        }
        let measured = measured_ms.max(0.0);
        if !self.has_measurement {
            self.smoothed_fill_ms = measured;
            self.has_measurement = true;
        } else {
            self.smoothed_fill_ms += self.ema_alpha * (measured - self.smoothed_fill_ms);
        }

        if !dt_secs.is_finite() || dt_secs <= 0.0 {
            return self.ratio;
        }

        let err_ms = self.smoothed_fill_ms - self.target_ms;
        self.integral = (self.integral + self.ki * err_ms * dt_secs)
            .clamp(-self.max_integral, self.max_integral);

        let min_ratio = 1.0 - self.max_offset;
        let max_ratio = 1.0 + self.max_offset;
        let target_ratio = (1.0 + self.kp * err_ms + self.integral).clamp(min_ratio, max_ratio);

        let max_delta = self.max_slew_per_sec * dt_secs;
        let delta = (target_ratio - self.ratio).clamp(-max_delta, max_delta);
        self.ratio = (self.ratio + delta).clamp(min_ratio, max_ratio);
        self.ratio
    }

    /// Current resampling ratio (`1.0` = nominal).
    #[inline]
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Current clock-drift estimate in parts per million (`(ratio - 1.0) * 1e6`).
    #[inline]
    pub fn ppm(&self) -> f64 {
        (self.ratio - 1.0) * 1_000_000.0
    }

    /// Current EMA-smoothed buffer fill level in milliseconds.
    #[inline]
    pub fn smoothed_fill_ms(&self) -> f64 {
        self.smoothed_fill_ms
    }

    /// Configured target buffer fill level in milliseconds.
    #[inline]
    pub fn target_ms(&self) -> f64 {
        self.target_ms
    }

    /// Update the target buffer fill level in milliseconds.
    pub fn set_target_ms(&mut self, target_ms: f64) {
        if target_ms.is_finite() {
            self.target_ms = target_ms.max(0.0);
        }
    }

    /// Reset smoothed fill, PI integrator, and output ratio back to `1.0`.
    pub fn reset(&mut self) {
        self.smoothed_fill_ms = self.target_ms;
        self.has_measurement = false;
        self.integral = 0.0;
        self.ratio = 1.0;
    }
}

#[inline]
fn catmull_rom_i16(y0: i16, y1: i16, y2: i16, y3: i16, t: f64) -> i16 {
    let p0 = f64::from(y0);
    let p1 = f64::from(y1);
    let p2 = f64::from(y2);
    let p3 = f64::from(y3);
    let t2 = t * t;
    let t3 = t2 * t;
    let val = 0.5
        * ((2.0 * p1)
            + (-p0 + p2) * t
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3);
    val.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// Stateful 4-point Catmull-Rom cubic stereo resampler (`i16` interleaved stereo in/out).
///
/// Maintains a 4-frame stereo history and fractional `phase` wrapped into `[0.0, 1.0)`
/// on every output frame. Performs bit-transparent passthrough when
/// `!initialized && (ratio - 1.0).abs() <= 1e-9`, while updating its tail history
/// so transitions to active resampling are seamless and click-free.
#[derive(Debug, Clone)]
pub struct AdaptiveResampler {
    history: [(i16, i16); 4],
    has_history: bool,
    frame_pos: i64,
    phase: f64,
    initialized: bool,
}

impl Default for AdaptiveResampler {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveResampler {
    /// Create a new [`AdaptiveResampler`] in bit-transparent passthrough state.
    pub fn new() -> Self {
        Self {
            history: [(0, 0); 4],
            has_history: false,
            frame_pos: 0,
            phase: 0.0,
            initialized: false,
        }
    }

    /// Reset history, fractional phase, and initialization state.
    pub fn reset(&mut self) {
        self.history = [(0, 0); 4];
        self.has_history = false;
        self.frame_pos = 0;
        self.phase = 0.0;
        self.initialized = false;
    }

    /// Current fractional sample phase in `[0.0, 1.0)`.
    #[inline]
    pub fn phase(&self) -> f64 {
        self.phase
    }

    #[inline]
    fn frame_at(&self, usable_input: &[i16], idx: i64) -> (i16, i16) {
        if idx < 0 {
            let hist_idx = (4 + idx).clamp(0, 3) as usize;
            self.history[hist_idx]
        } else {
            let i = (idx as usize) * 2;
            (usable_input[i], usable_input[i + 1])
        }
    }

    fn update_history(&mut self, usable_input: &[i16]) {
        let n_frames = usable_input.len() / 2;
        if n_frames >= 4 {
            let start = (n_frames - 4) * 2;
            for i in 0..4 {
                self.history[i] = (usable_input[start + i * 2], usable_input[start + i * 2 + 1]);
            }
            self.has_history = true;
        } else {
            for f in 0..n_frames {
                let frame = (usable_input[f * 2], usable_input[f * 2 + 1]);
                if !self.has_history {
                    self.history = [frame; 4];
                    self.has_history = true;
                } else {
                    self.history.rotate_left(1);
                    self.history[3] = frame;
                }
            }
        }
    }

    /// Process interleaved stereo `i16` `input` at the given `ratio` and append output
    /// interleaved stereo `i16` samples to `out`.
    pub fn process_stereo(&mut self, input: &[i16], ratio: f64, out: &mut Vec<i16>) {
        let n_frames = input.len() / 2;
        if n_frames == 0 {
            return;
        }
        let usable = n_frames * 2;
        let step = if ratio.is_finite() {
            ratio.clamp(0.25, 4.0)
        } else {
            1.0
        };

        if !self.initialized && (step - 1.0).abs() <= 1e-9 {
            out.extend_from_slice(&input[..usable]);
            self.update_history(&input[..usable]);
            return;
        }

        if !self.has_history {
            self.history = [(input[0], input[1]); 4];
        }
        self.initialized = true;

        let est_frames = ((n_frames as f64) / step).ceil() as usize + 4;
        out.reserve(est_frames * 2);

        let limit = n_frames as i64;
        while self.frame_pos + 2 < limit {
            let idx = self.frame_pos;
            let f0 = self.frame_at(&input[..usable], idx - 1);
            let f1 = self.frame_at(&input[..usable], idx);
            let f2 = self.frame_at(&input[..usable], idx + 1);
            let f3 = self.frame_at(&input[..usable], idx + 2);

            out.push(catmull_rom_i16(f0.0, f1.0, f2.0, f3.0, self.phase));
            out.push(catmull_rom_i16(f0.1, f1.1, f2.1, f3.1, self.phase));

            let next_phase = self.phase + step;
            let advance = next_phase.floor() as i64;
            let mut rem = next_phase - (advance as f64);
            let mut extra = 0i64;
            if rem >= 1.0 {
                rem -= 1.0;
                extra = 1;
            } else if rem < 0.0 {
                rem = 0.0;
            }
            self.phase = rem;
            self.frame_pos += advance + extra;
        }

        self.update_history(&input[..usable]);
        self.frame_pos -= limit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn drift_controller_sign_slew_clamps_and_reset() {
        let mut ctrl = DriftController::new(40.0);
        assert!((ctrl.target_ms() - 40.0).abs() <= f64::EPSILON);
        assert!((ctrl.ratio() - 1.0).abs() <= f64::EPSILON);
        assert!((ctrl.ppm()).abs() <= f64::EPSILON);

        // Excess fill (measured > target) => ratio > 1.0; slew-limited to <= 50e-6 per 20 ms.
        let r1 = ctrl.update(100.0, 0.020);
        assert!(r1 > 1.0, "expected ratio > 1.0, got {r1}");
        assert!(
            (r1 - 1.0) <= 50e-6 + 1e-12,
            "slew exceeded 50e-6 per 20ms: {}",
            r1 - 1.0
        );

        // Large sustained positive error clamps ratio at 1.005 and integral at +0.002.
        for _ in 0..2_000 {
            ctrl.update(500.0, 0.020);
        }
        assert!(
            (ctrl.ratio() - 1.005).abs() <= 1e-12,
            "got {}",
            ctrl.ratio()
        );
        assert!(
            (ctrl.integral - 0.002).abs() <= 1e-12,
            "got {}",
            ctrl.integral
        );

        // Reset restores unity ratio and target_ms.
        ctrl.reset();
        assert!((ctrl.ratio() - 1.0).abs() <= f64::EPSILON);
        assert!((ctrl.smoothed_fill_ms() - 40.0).abs() <= f64::EPSILON);

        // Deficit fill (measured < target) => ratio < 1.0; integral clamps at -0.002.
        for _ in 0..2_000 {
            ctrl.update(0.0, 0.020);
        }
        assert!(ctrl.ratio() < 1.0);
        assert!(
            (ctrl.integral + 0.002).abs() <= 1e-12,
            "got {}",
            ctrl.integral
        );
        // With target_ms = 40, P = -0.001 and I = -0.002 => ratio = 0.997.
        assert!(
            (ctrl.ratio() - 0.997).abs() <= 1e-12,
            "got {}",
            ctrl.ratio()
        );

        // With target_ms = 200, err_ms = -200 saturates output ratio at 0.995.
        ctrl.set_target_ms(200.0);
        assert!((ctrl.target_ms() - 200.0).abs() <= f64::EPSILON);
        for _ in 0..2_000 {
            ctrl.update(0.0, 0.020);
        }
        assert!(
            (ctrl.ratio() - 0.995).abs() <= 1e-12,
            "got {}",
            ctrl.ratio()
        );

        // Custom parameters via with_params.
        let mut custom = DriftController::with_params(30.0, 0.1, 1e-4, 1e-5, 0.01);
        assert!(custom.update(50.0, 0.020) > 1.0);
    }

    #[test]
    fn drift_controller_closed_loop_converges_plus_minus_200_ppm() {
        for &skew_ppm in &[200.0_f64, -200.0_f64] {
            let target_ms = 40.0_f64;
            let max_ms = 160.0_f64;
            let dt_secs = 0.020_f64;
            let frames_per_step = 960_usize; // 20 ms @ 48 kHz
            let sample_rate = 48_000.0_f64;
            let skew = skew_ppm * 1e-6;
            // Sender delivers 960 frames per 20 ms sender tick; DAC consumes at 48k / (1 + skew).
            let dac_frames_per_step = (frames_per_step as f64) / (1.0 + skew);

            let mut ctrl = DriftController::new(target_ms);
            let mut resampler = AdaptiveResampler::new();
            let mut buffered_frames: f64 = target_ms * 48.0; // 1920 frames
            let mut underruns = 0u64;
            let mut overruns = 0u64;

            let mut input = vec![0i16; frames_per_step * 2];
            let mut out = Vec::with_capacity(frames_per_step * 2 + 16);

            let mut sample_idx = 0usize;
            for _step in 0..30_000 {
                for f in 0..frames_per_step {
                    let t = (sample_idx as f64) / sample_rate;
                    let l = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 12_000.0) as i16;
                    let r = ((2.0 * std::f64::consts::PI * 660.0 * t).sin() * 12_000.0) as i16;
                    input[f * 2] = l;
                    input[f * 2 + 1] = r;
                    sample_idx += 1;
                }

                let measured_ms = buffered_frames / 48.0;
                let ratio = ctrl.update(measured_ms, dt_secs);

                out.clear();
                resampler.process_stereo(&input, ratio, &mut out);
                let produced_frames = (out.len() / 2) as f64;

                buffered_frames += produced_frames;
                if buffered_frames > max_ms * 48.0 {
                    overruns += 1;
                    buffered_frames = max_ms * 48.0;
                }

                buffered_frames -= dac_frames_per_step;
                if buffered_frames < 0.0 {
                    underruns += 1;
                    buffered_frames = 0.0;
                }
            }

            let final_fill_ms = buffered_frames / 48.0;
            let final_ppm = ctrl.ppm();
            assert_eq!(underruns, 0, "skew {skew_ppm} ppm caused underruns");
            assert_eq!(overruns, 0, "skew {skew_ppm} ppm caused overruns");
            assert!(
                (final_fill_ms - target_ms).abs() < 0.5,
                "skew {skew_ppm} ppm final fill {final_fill_ms:.3} ms did not converge to {target_ms} ms"
            );
            assert!(
                (final_ppm - skew_ppm).abs() <= 10.0,
                "skew {skew_ppm} ppm converged to {final_ppm:.3} ppm (outside ±10 ppm)"
            );
        }
    }

    #[test]
    fn adaptive_resampler_passthrough_and_multitone_snr_and_sweep_continuity() {
        // 1. Bit-transparent passthrough when !initialized && ratio == 1.0.
        let mut r = AdaptiveResampler::new();
        let passthrough_in: Vec<i16> = (0..960 * 2)
            .map(|i| {
                if i % 2 == 0 {
                    ((i * 19) % 15_000) as i16
                } else {
                    -(((i * 23) % 15_000) as i16)
                }
            })
            .collect();
        let mut out = Vec::new();
        r.process_stereo(&passthrough_in, 1.0, &mut out);
        assert_eq!(out, passthrough_in);

        // 2. Multi-tone stereo SNR > 65 dB.
        let mut r_snr = AdaptiveResampler::new();
        let sr = 48_000.0_f64;
        let total_in_frames = 4_800usize;
        let ratio = 1.0005_f64;
        let tone_l = |t: f64| -> f64 {
            10_000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                + 6_000.0 * (2.0 * std::f64::consts::PI * 1320.0 * t).sin()
        };
        let tone_r = |t: f64| -> f64 {
            10_000.0 * (2.0 * std::f64::consts::PI * 660.0 * t).sin()
                + 6_000.0 * (2.0 * std::f64::consts::PI * 1980.0 * t).sin()
        };

        let mut stereo_in = Vec::with_capacity(total_in_frames * 2);
        for n in 0..total_in_frames {
            let t = (n as f64) / sr;
            stereo_in.push(tone_l(t).round() as i16);
            stereo_in.push(tone_r(t).round() as i16);
        }

        let mut snr_out = Vec::new();
        for chunk in stereo_in.chunks(480 * 2) {
            r_snr.process_stereo(chunk, ratio, &mut snr_out);
        }

        let out_frames = snr_out.len() / 2;
        assert!(out_frames > 4_000);
        let mut sig_pow_l = 0.0_f64;
        let mut err_pow_l = 0.0_f64;
        let mut sig_pow_r = 0.0_f64;
        let mut err_pow_r = 0.0_f64;
        for k in 10..(out_frames - 10) {
            let t = ((k as f64) * ratio) / sr;
            let ref_l = tone_l(t);
            let ref_r = tone_r(t);
            let dl = f64::from(snr_out[k * 2]) - ref_l;
            let dr = f64::from(snr_out[k * 2 + 1]) - ref_r;
            sig_pow_l += ref_l * ref_l;
            err_pow_l += dl * dl;
            sig_pow_r += ref_r * ref_r;
            err_pow_r += dr * dr;
        }
        let snr_l = 10.0 * (sig_pow_l / err_pow_l).log10();
        let snr_r = 10.0 * (sig_pow_r / err_pow_r).log10();
        assert!(snr_l > 65.0, "Left SNR {snr_l:.2} dB <= 65 dB");
        assert!(snr_r > 65.0, "Right SNR {snr_r:.2} dB <= 65 dB");

        // 3. Click-free continuity across passthrough -> active -> ratio sweep across 1.0.
        let mut r_sweep = AdaptiveResampler::new();
        let mut sweep_out = Vec::new();
        let ratios = [
            1.0, 1.0, 1.0002, 1.0005, 1.001, 1.0, 0.999, 0.9985, 1.0, 1.0003,
        ];
        let chunk_frames = 960usize;
        let mut chunk = vec![0i16; chunk_frames * 2];
        let mut idx = 0usize;
        for &cur_ratio in &ratios {
            for f in 0..chunk_frames {
                let t = (idx as f64) / sr;
                chunk[f * 2] =
                    (12_000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()).round() as i16;
                chunk[f * 2 + 1] =
                    (12_000.0 * (2.0 * std::f64::consts::PI * 660.0 * t).sin()).round() as i16;
                idx += 1;
            }
            r_sweep.process_stereo(&chunk, cur_ratio, &mut sweep_out);
        }

        let sweep_frames = sweep_out.len() / 2;
        let max_step_l = (2.0 * std::f64::consts::PI * 440.0 / sr) * 12_000.0 * 1.05 + 10.0;
        let max_step_r = (2.0 * std::f64::consts::PI * 660.0 / sr) * 12_000.0 * 1.05 + 10.0;
        for f in 1..sweep_frames {
            let dl = (i32::from(sweep_out[f * 2]) - i32::from(sweep_out[(f - 1) * 2])).abs() as f64;
            let dr = (i32::from(sweep_out[f * 2 + 1]) - i32::from(sweep_out[(f - 1) * 2 + 1])).abs()
                as f64;
            assert!(
                dl <= max_step_l,
                "click detected on L at frame {f}: delta={dl:.1} > {max_step_l:.1}"
            );
            assert!(
                dr <= max_step_r,
                "click detected on R at frame {f}: delta={dr:.1} > {max_step_r:.1}"
            );
        }
    }

    #[test]
    fn adaptive_resampler_5m_frames_precision_and_no_lr_swap() {
        let mut r = AdaptiveResampler::new();
        let ratio = 1.000_197_f64;
        let chunk_frames = 1_000usize;
        let num_chunks = 5_000usize; // 5_000_000 stereo frames total
        let mut chunk = vec![0i16; chunk_frames * 2];
        for f in 0..chunk_frames {
            chunk[f * 2] = 10_000;
            chunk[f * 2 + 1] = -10_000;
        }

        let mut out = Vec::with_capacity(chunk_frames * 2 + 16);
        let mut total_out_frames = 0usize;
        for _ in 0..num_chunks {
            out.clear();
            r.process_stereo(&chunk, ratio, &mut out);
            assert!(
                (0.0..1.0).contains(&r.phase()),
                "phase out of [0.0, 1.0): {}",
                r.phase()
            );
            let (pairs, rem) = out.as_chunks::<2>();
            assert!(rem.is_empty());
            for &[l, r] in pairs {
                assert_eq!(l, 10_000, "L channel corrupted or swapped");
                assert_eq!(r, -10_000, "R channel corrupted or swapped");
            }
            total_out_frames += out.len() / 2;
        }

        let expected_frames = (5_000_000.0_f64 / ratio).round() as isize;
        let diff = (total_out_frames as isize - expected_frames).abs();
        assert!(
            diff <= 3,
            "5M-frame drift too large: produced {total_out_frames}, expected ~{expected_frames}"
        );
    }
}
