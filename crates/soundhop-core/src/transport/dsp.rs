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
}
