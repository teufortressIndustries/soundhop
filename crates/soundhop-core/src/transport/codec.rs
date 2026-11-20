//! Audio codec abstraction (`AudioCodec`) with zero-dependency `PcmCodec` and
//! pure-Rust `OpusCodec` (via `opus2` `backend-mousiki`, gated behind default feature `opus`).

use crate::transport::wire::{AudioParams, CodecId, FrameType};
use crate::transport::{TransportError, TransportResult, MAX_PAYLOAD_LEN};

/// Standard audio quality/latency profiles for SHT3 streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioProfile {
    /// Default: Opus 48 kHz stereo, 20 ms frames (960 samples/ch), 128 kbps VBR.
    #[default]
    OpusBalanced,
    /// Low-latency: Opus 48 kHz stereo, 10 ms frames (480 samples/ch), 128 kbps VBR.
    OpusLowLatency,
    /// Low-bandwidth: Opus 48 kHz stereo, 20 ms frames (960 samples/ch), 64 kbps VBR.
    OpusLowBandwidth,
    /// Uncompressed lossless PCM: 48 kHz stereo S16LE, 20 ms frames (3,840 B/frame).
    PcmLossless,
}

impl AudioProfile {
    pub fn codec_id(self) -> CodecId {
        match self {
            Self::OpusBalanced | Self::OpusLowLatency | Self::OpusLowBandwidth => CodecId::Opus,
            Self::PcmLossless => CodecId::PcmS16Le,
        }
    }

    pub fn frame_type(self) -> FrameType {
        match self.codec_id() {
            CodecId::Opus => FrameType::AudioOpus,
            CodecId::PcmS16Le => FrameType::AudioPcm,
        }
    }

    pub fn sample_rate(self) -> u32 {
        48_000
    }

    pub fn channels(self) -> u8 {
        2
    }

    pub fn frame_ms(self) -> u8 {
        match self {
            Self::OpusLowLatency => 10,
            Self::OpusBalanced | Self::OpusLowBandwidth | Self::PcmLossless => 20,
        }
    }

    pub fn bitrate_kbps(self) -> u16 {
        match self {
            Self::OpusBalanced | Self::OpusLowLatency => 128,
            Self::OpusLowBandwidth => 64,
            Self::PcmLossless => 1536,
        }
    }

    /// Number of samples per channel in one frame (e.g. 960 for 20ms @ 48kHz, 480 for 10ms).
    pub fn frame_samples_per_channel(self) -> usize {
        (self.sample_rate() as usize * usize::from(self.frame_ms())) / 1000
    }

    /// Total interleaved `i16` samples in one frame (`frame_samples_per_channel * channels`).
    pub fn interleaved_samples_per_frame(self) -> usize {
        self.frame_samples_per_channel() * usize::from(self.channels())
    }

    pub fn to_audio_params(self, jitter_ms: u16) -> AudioParams {
        AudioParams {
            codec: self.codec_id(),
            sample_rate: self.sample_rate(),
            channels: self.channels(),
            frame_ms: self.frame_ms(),
            bitrate_kbps: self.bitrate_kbps(),
            jitter_ms,
        }
    }

    pub fn from_audio_params(params: &AudioParams) -> TransportResult<Self> {
        if params.sample_rate != 48_000 || params.channels != 2 {
            return Err(TransportError::UnsupportedProfile(format!(
                "unsupported format: {}Hz {}ch (expected 48000Hz 2ch)",
                params.sample_rate, params.channels
            )));
        }
        match (params.codec, params.frame_ms, params.bitrate_kbps) {
            (CodecId::PcmS16Le, 20, _) => Ok(Self::PcmLossless),
            (CodecId::Opus, 10, _) => Ok(Self::OpusLowLatency),
            (CodecId::Opus, 20, kbps) if kbps <= 80 => Ok(Self::OpusLowBandwidth),
            (CodecId::Opus, 20, _) => Ok(Self::OpusBalanced),
            _ => Err(TransportError::UnsupportedProfile(format!(
                "unsupported AudioParams: {params:?}"
            ))),
        }
    }
}

/// Common codec interface for SHT3 audio framing.
pub trait AudioCodec: Send {
    fn codec_id(&self) -> CodecId;
    fn frame_samples_per_channel(&self) -> usize;
    fn channels(&self) -> usize;

    fn interleaved_samples_per_frame(&self) -> usize {
        self.frame_samples_per_channel() * self.channels()
    }

    /// Encode one frame of interleaved `i16` PCM samples into `out` (appending encoded bytes).
    /// Returns the number of bytes appended.
    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> TransportResult<usize>;

    /// Decode one encoded packet into `out_pcm` (appending interleaved `i16` samples).
    /// Returns the number of interleaved `i16` samples appended.
    fn decode(&mut self, packet: &[u8], out_pcm: &mut Vec<i16>) -> TransportResult<usize>;

    /// Conceal one missing/lost frame using Packet Loss Concealment (PLC).
    /// Appends one frame of interleaved `i16` samples to `out_pcm` and returns sample count.
    fn decode_plc(&mut self, out_pcm: &mut Vec<i16>) -> TransportResult<usize>;
}

/// Uncompressed 16-bit little-endian PCM codec (always available, zero external deps).
#[derive(Debug, Clone)]
pub struct PcmCodec {
    frame_samples_per_channel: usize,
    channels: usize,
}

impl PcmCodec {
    pub fn new(sample_rate: u32, channels: u8, frame_ms: u8) -> TransportResult<Self> {
        if sample_rate == 0 || channels == 0 || frame_ms == 0 {
            return Err(TransportError::Codec(
                "sample_rate, channels, and frame_ms must be non-zero".into(),
            ));
        }
        let frame_samples_per_channel = (sample_rate as usize * usize::from(frame_ms)) / 1000;
        let frame_bytes = frame_samples_per_channel * usize::from(channels) * 2;
        if frame_bytes == 0 || frame_bytes > MAX_PAYLOAD_LEN {
            return Err(TransportError::Codec(format!(
                "PCM frame byte size {frame_bytes} out of range (1..={MAX_PAYLOAD_LEN})"
            )));
        }
        Ok(Self {
            frame_samples_per_channel,
            channels: usize::from(channels),
        })
    }

    pub fn from_profile(profile: AudioProfile) -> TransportResult<Self> {
        Self::new(
            profile.sample_rate(),
            profile.channels(),
            profile.frame_ms(),
        )
    }
}

impl AudioCodec for PcmCodec {
    fn codec_id(&self) -> CodecId {
        CodecId::PcmS16Le
    }

    fn frame_samples_per_channel(&self) -> usize {
        self.frame_samples_per_channel
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> TransportResult<usize> {
        let expected = self.interleaved_samples_per_frame();
        if pcm.len() != expected {
            return Err(TransportError::Codec(format!(
                "PCM encode expected {expected} samples, got {}",
                pcm.len()
            )));
        }
        let bytes = expected * 2;
        out.reserve(bytes);
        for &s in pcm {
            out.extend_from_slice(&s.to_le_bytes());
        }
        Ok(bytes)
    }

    fn decode(&mut self, packet: &[u8], out_pcm: &mut Vec<i16>) -> TransportResult<usize> {
        let expected_samples = self.interleaved_samples_per_frame();
        let expected_bytes = expected_samples * 2;
        if packet.len() != expected_bytes {
            return Err(TransportError::Codec(format!(
                "PCM decode expected {expected_bytes} bytes, got {}",
                packet.len()
            )));
        }
        out_pcm.reserve(expected_samples);
        for i in (0..expected_bytes).step_by(2) {
            out_pcm.push(i16::from_le_bytes([packet[i], packet[i + 1]]));
        }
        Ok(expected_samples)
    }

    fn decode_plc(&mut self, out_pcm: &mut Vec<i16>) -> TransportResult<usize> {
        let samples = self.interleaved_samples_per_frame();
        out_pcm.resize(out_pcm.len() + samples, 0);
        Ok(samples)
    }
}

/// Pure-Rust Opus codec backed by `opus2` (`backend-mousiki`, zero C/CMake dependencies).
#[cfg(feature = "opus")]
pub struct OpusCodec {
    encoder: opus2::Encoder,
    decoder: opus2::Decoder,
    frame_samples_per_channel: usize,
    channels: usize,
    scratch_enc: Vec<u8>,
    scratch_dec: Vec<i16>,
}

#[cfg(feature = "opus")]
impl OpusCodec {
    /// Create a new `OpusCodec` configured from [`AudioParams`].
    pub fn from_params(params: &AudioParams) -> TransportResult<Self> {
        if params.codec != CodecId::Opus {
            return Err(TransportError::Codec(
                "OpusCodec requires CodecId::Opus".into(),
            ));
        }
        let opus_channels = match params.channels {
            1 => opus2::Channels::Mono,
            2 => opus2::Channels::Stereo,
            other => {
                return Err(TransportError::Codec(format!(
                    "unsupported Opus channel count: {other}"
                )))
            }
        };
        let frame_samples_per_channel =
            (params.sample_rate as usize * usize::from(params.frame_ms)) / 1000;
        let channels = usize::from(params.channels);
        let interleaved = frame_samples_per_channel * channels;
        if interleaved == 0 {
            return Err(TransportError::Codec("zero Opus frame size".into()));
        }

        let mut encoder = opus2::Encoder::new(
            params.sample_rate,
            opus_channels,
            opus2::Application::LowDelay,
        )
        .map_err(|e| TransportError::Codec(format!("Opus encoder init failed: {e}")))?;

        let target_bps = i32::from(params.bitrate_kbps.clamp(16, 510)) * 1000;
        encoder
            .set_bitrate(opus2::Bitrate::Bits(target_bps))
            .map_err(|e| TransportError::Codec(format!("Opus set_bitrate failed: {e}")))?;
        let _ = encoder.set_signal(opus2::Signal::Music);
        let _ = encoder.set_vbr(true);
        let _ = encoder.set_inband_fec(false);

        let decoder = opus2::Decoder::new(params.sample_rate, opus_channels)
            .map_err(|e| TransportError::Codec(format!("Opus decoder init failed: {e}")))?;

        Ok(Self {
            encoder,
            decoder,
            frame_samples_per_channel,
            channels,
            scratch_enc: vec![0u8; MAX_PAYLOAD_LEN],
            scratch_dec: vec![0i16; interleaved],
        })
    }

    /// Create a new `OpusCodec` from an [`AudioProfile`].
    pub fn from_profile(profile: AudioProfile) -> TransportResult<Self> {
        if profile.codec_id() != CodecId::Opus {
            return Err(TransportError::UnsupportedProfile(format!(
                "{profile:?} is not an Opus profile"
            )));
        }
        Self::from_params(&profile.to_audio_params(40))
    }

    /// Dynamically adjust target Opus encoder bitrate (e.g. from `ControlMessage::BitrateHint`).
    pub fn set_bitrate_kbps(&mut self, kbps: u16) -> TransportResult<()> {
        let bps = i32::from(kbps.clamp(16, 510)) * 1000;
        self.encoder
            .set_bitrate(opus2::Bitrate::Bits(bps))
            .map_err(|e| TransportError::Codec(format!("Opus set_bitrate failed: {e}")))
    }
}

#[cfg(feature = "opus")]
impl AudioCodec for OpusCodec {
    fn codec_id(&self) -> CodecId {
        CodecId::Opus
    }

    fn frame_samples_per_channel(&self) -> usize {
        self.frame_samples_per_channel
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> TransportResult<usize> {
        let expected = self.interleaved_samples_per_frame();
        if pcm.len() != expected {
            return Err(TransportError::Codec(format!(
                "Opus encode expected {expected} samples, got {}",
                pcm.len()
            )));
        }
        let written = self
            .encoder
            .encode(pcm, &mut self.scratch_enc)
            .map_err(|e| TransportError::Codec(format!("Opus encode failed: {e}")))?;
        out.extend_from_slice(&self.scratch_enc[..written]);
        Ok(written)
    }

    fn decode(&mut self, packet: &[u8], out_pcm: &mut Vec<i16>) -> TransportResult<usize> {
        if packet.is_empty() {
            return self.decode_plc(out_pcm);
        }
        let decoded_per_ch = self
            .decoder
            .decode(packet, &mut self.scratch_dec, false)
            .map_err(|e| TransportError::Codec(format!("Opus decode failed: {e}")))?;
        let total_samples = decoded_per_ch * self.channels;
        out_pcm.extend_from_slice(&self.scratch_dec[..total_samples]);
        Ok(total_samples)
    }

    fn decode_plc(&mut self, out_pcm: &mut Vec<i16>) -> TransportResult<usize> {
        let decoded_per_ch = self
            .decoder
            .decode(&[], &mut self.scratch_dec, false)
            .map_err(|e| TransportError::Codec(format!("Opus PLC failed: {e}")))?;
        let total_samples = decoded_per_ch * self.channels;
        out_pcm.extend_from_slice(&self.scratch_dec[..total_samples]);
        Ok(total_samples)
    }
}

/// Construct a boxed [`AudioCodec`] for the negotiated [`AudioParams`].
pub fn create_codec(params: &AudioParams) -> TransportResult<Box<dyn AudioCodec>> {
    match params.codec {
        CodecId::PcmS16Le => Ok(Box::new(PcmCodec::new(
            params.sample_rate,
            params.channels,
            params.frame_ms,
        )?)),
        CodecId::Opus => {
            #[cfg(feature = "opus")]
            {
                Ok(Box::new(OpusCodec::from_params(params)?))
            }
            #[cfg(not(feature = "opus"))]
            {
                Err(TransportError::UnsupportedProfile(
                    "Opus codec requested, but soundhop-core was built without the `opus` feature"
                        .into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_stereo_frame(samples_per_ch: usize, freq_hz: f64, frame_idx: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(samples_per_ch * 2);
        let base = frame_idx * samples_per_ch;
        for i in 0..samples_per_ch {
            let t = (base + i) as f64 / 48_000.0;
            let l = ((2.0 * std::f64::consts::PI * freq_hz * t).sin() * 14_000.0) as i16;
            let r = ((2.0 * std::f64::consts::PI * (freq_hz * 1.25) * t).sin() * 12_000.0) as i16;
            out.push(l);
            out.push(r);
        }
        out
    }

    #[test]
    fn pcm_codec_lossless_roundtrip_and_plc() {
        let mut codec = PcmCodec::from_profile(AudioProfile::PcmLossless).unwrap();
        let pcm = sine_stereo_frame(960, 440.0, 0);
        let mut encoded = Vec::new();
        let n_bytes = codec.encode(&pcm, &mut encoded).unwrap();
        assert_eq!(n_bytes, 960 * 2 * 2);

        let mut decoded = Vec::new();
        let n_samples = codec.decode(&encoded, &mut decoded).unwrap();
        assert_eq!(n_samples, pcm.len());
        assert_eq!(decoded, pcm);

        let mut plc = Vec::new();
        let n_plc = codec.decode_plc(&mut plc).unwrap();
        assert_eq!(n_plc, 1920);
        assert!(plc.iter().all(|&s| s == 0));
    }

    #[cfg(feature = "opus")]
    #[test]
    fn opus_codec_balanced_and_low_latency_roundtrip_and_plc() {
        for profile in [
            AudioProfile::OpusBalanced,
            AudioProfile::OpusLowLatency,
            AudioProfile::OpusLowBandwidth,
        ] {
            let mut codec = OpusCodec::from_profile(profile).unwrap();
            let spc = profile.frame_samples_per_channel();
            let mut last_energy = 0i64;
            for frame_idx in 0..5 {
                let pcm = sine_stereo_frame(spc, 440.0, frame_idx);
                let mut pkt = Vec::new();
                let pkt_len = codec.encode(&pcm, &mut pkt).unwrap();
                assert!(pkt_len > 0 && pkt_len < pcm.len() * 2);

                let mut decoded = Vec::new();
                let dec_samples = codec.decode(&pkt, &mut decoded).unwrap();
                assert_eq!(dec_samples, spc * 2);
                last_energy = decoded.iter().map(|&s| i64::from(s).abs()).sum();
            }
            // After 5 frames of 440Hz sine, decoded output must contain strong signal energy.
            assert!(
                last_energy > 100_000,
                "expected non-silent decoded Opus audio for {profile:?}, energy={last_energy}"
            );

            // Measure actual waveform correlation & SNR (accounting for Opus 312-sample lookahead)
            let mut codec_snr = OpusCodec::from_profile(profile).unwrap();
            let total_frames = 60usize;
            let total_spc = spc * total_frames;
            let mut in_l = Vec::with_capacity(total_spc);
            let mut in_r = Vec::with_capacity(total_spc);
            let mut out_l = Vec::with_capacity(total_spc);
            let mut out_r = Vec::with_capacity(total_spc);

            for f_idx in 0..total_frames {
                let mut pcm = Vec::with_capacity(spc * 2);
                for i in 0..spc {
                    let s_idx = f_idx * spc + i;
                    let t = s_idx as f64 / 48_000.0;
                    // Realistic dynamic stereo signal: bass (110Hz) + vocal mid (660Hz) + treble (3520Hz) with envelope modulation
                    let env = 0.25 + 0.75 * ((t * 3.0 * std::f64::consts::PI).sin().abs());
                    let l_val = env
                        * (8000.0 * (2.0 * std::f64::consts::PI * 110.0 * t).sin()
                            + 6000.0 * (2.0 * std::f64::consts::PI * 660.0 * t).sin()
                            + 3000.0 * (2.0 * std::f64::consts::PI * 3520.0 * t).cos());
                    let r_val = env
                        * (8000.0 * (2.0 * std::f64::consts::PI * 110.0 * t).sin()
                            + 5500.0 * (2.0 * std::f64::consts::PI * 880.0 * t).cos()
                            + 3000.0 * (2.0 * std::f64::consts::PI * 2640.0 * t).sin());
                    let l_i16 = l_val.clamp(-32000.0, 32000.0) as i16;
                    let r_i16 = r_val.clamp(-32000.0, 32000.0) as i16;
                    in_l.push(l_i16);
                    in_r.push(r_i16);
                    pcm.push(l_i16);
                    pcm.push(r_i16);
                }
                let mut pkt = Vec::new();
                codec_snr.encode(&pcm, &mut pkt).unwrap();
                let mut dec = Vec::new();
                codec_snr.decode(&pkt, &mut dec).unwrap();
                for &[l, r] in dec.as_chunks::<2>().0 {
                    out_l.push(l);
                    out_r.push(r);
                }
            }

            // Find best lag (Opus standard lookahead @ 48kHz is 312 samples = 6.5ms)
            let mut best_lag = 0usize;
            let mut best_corr = -1.0f64;
            let search_start = spc * 5;
            let search_end = total_spc - spc * 2;
            for lag in 0..800usize {
                let mut dot = 0.0f64;
                let mut e_in = 0.0f64;
                let mut e_out = 0.0f64;
                for idx in search_start..search_end {
                    let a = f64::from(in_l[idx]);
                    let b = f64::from(out_l[idx + lag]);
                    dot += a * b;
                    e_in += a * a;
                    e_out += b * b;
                }
                let corr = dot / (e_in.sqrt() * e_out.sqrt() + 1e-9);
                if corr > best_corr {
                    best_corr = corr;
                    best_lag = lag;
                }
            }
            let mut sig_pow = 0.0f64;
            let mut err_pow = 0.0f64;
            for idx in search_start..search_end {
                let a = f64::from(in_l[idx]);
                let b = f64::from(out_l[idx + best_lag]);
                sig_pow += a * a;
                err_pow += (a - b) * (a - b);
            }
            let snr_db = 10.0 * (sig_pow / (err_pow + 1e-9)).log10();
            assert!(
                best_corr > 0.90 && snr_db > 8.0,
                "expected high-fidelity Opus reconstruction for {profile:?}, got corr={best_corr:.4}, snr={snr_db:.2} dB"
            );

            // PLC must produce a valid full frame of samples.
            let mut plc_out = Vec::new();
            let plc_samples = codec.decode_plc(&mut plc_out).unwrap();
            assert_eq!(plc_samples, spc * 2);
            assert_eq!(plc_out.len(), spc * 2);
        }
    }
}
