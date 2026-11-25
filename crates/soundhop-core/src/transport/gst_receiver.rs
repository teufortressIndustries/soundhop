//! GStreamer inbound SRTP audio receiver pipeline: receives UDP SRTP packets, authenticates and
//! decrypts via `srtpdec` (enforcing exact SSRC match with zero mutex locks in `request-key`),
//! absorbs Wi-Fi jitter via `rtpjitterbuffer`, decodes Opus (with in-band FEC + PLC) or L16 PCM,
//! applies live volume/mute, and plays back to PulseAudio/PipeWire, WASAPI2, or a test `appsink`.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::transport::gst_runtime::{ensure_gst_init, make_element};
use crate::transport::wire::{AudioParams, CodecId};
use crate::transport::{TransportError, TransportResult};

const BUS_POLL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_mseconds(100);

/// Audio playback endpoint for a [`GstAudioReceiver`] pipeline.
#[derive(Clone)]
pub enum PlaybackEndpoint {
    /// Linux PulseAudio/PipeWire playback (`pulsesink`).
    SystemPulse,
    /// Windows WASAPI2 playback (`wasapi2sink`).
    SystemWasapi,
    /// Headless test sink capturing decoded 48 kHz stereo S16LE interleaved samples.
    TestAppSink(Arc<Mutex<Vec<i16>>>),
}

impl std::fmt::Debug for PlaybackEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemPulse => f.write_str("PlaybackEndpoint::SystemPulse"),
            Self::SystemWasapi => f.write_str("PlaybackEndpoint::SystemWasapi"),
            Self::TestAppSink(_) => f.write_str("PlaybackEndpoint::TestAppSink(..)"),
        }
    }
}

/// Factory returning a [`PlaybackEndpoint`] for a newly authenticated inbound peer.
pub type PlaybackEndpointFactory =
    Arc<dyn Fn(&str) -> TransportResult<PlaybackEndpoint> + Send + Sync>;

/// Snapshot of GStreamer `rtpjitterbuffer` telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReceiverJitterStats {
    pub num_pushed: u64,
    pub num_lost: u64,
    pub num_late: u64,
    pub avg_jitter_ns: u64,
    pub percent: u8,
    pub buffered_ms: u16,
    pub dropped_frames: u32,
}

/// Prepared SRTP receiver whose UDP port has been bound, awaiting SRTP key activation.
///
/// Two-phase startup (`bind` -> `start`) allows the server handshake to embed the actual
/// OS-bound `rtp_port` in `ServerHello` (bound into `mac_s` and HKDF), then attach the derived
/// SRTP key and transition to `Playing` **before** flushing `ServerHello` to the client.
pub struct PreparedGstReceiver {
    pipeline: gst::Pipeline,
    srtpdec: gst::Element,
    jitterbuffer: gst::Element,
    volume_elem: gst::Element,
    bound_rtp_port: u16,
    configured_jitter_ms: u16,
    volume_pct: Arc<AtomicU8>,
    muted: Arc<AtomicBool>,
    activated: bool,
}

impl PreparedGstReceiver {
    /// Build the receiver pipeline and bind `udpsrc` (trying `preferred_rtp_port` if `> 0`,
    /// falling back to ephemeral port `0` if busy).
    pub fn bind(
        preferred_rtp_port: u16,
        audio_params: AudioParams,
        endpoint: PlaybackEndpoint,
        initial_volume: u8,
        initial_muted: bool,
    ) -> TransportResult<Self> {
        ensure_gst_init()?;

        let pipeline = gst::Pipeline::new();
        let udpsrc = make_element("udpsrc", None)?;
        udpsrc.set_property("buffer-size", 524_288i32);

        let srtp_caps = match audio_params.codec {
            CodecId::Opus => gst::Caps::builder("application/x-srtp")
                .field("media", "audio")
                .field("clock-rate", audio_params.sample_rate as i32)
                .field("encoding-name", "OPUS")
                .field("payload", 96i32)
                .build(),
            CodecId::PcmS16Le => gst::Caps::builder("application/x-srtp")
                .field("media", "audio")
                .field("clock-rate", audio_params.sample_rate as i32)
                .field("encoding-name", "L16")
                .field("channels", i32::from(audio_params.channels))
                .field("payload", 97i32)
                .build(),
        };
        udpsrc.set_property("caps", &srtp_caps);

        // Bind UDP port cleanly by transitioning udpsrc to Ready (preferred -> ephemeral fallback).
        let bound_rtp_port = bind_udpsrc_port(&udpsrc, preferred_rtp_port)?;

        let srtpdec = make_element("srtpdec", None)?;
        srtpdec.set_property("replay-window-size", 256u32);

        let configured_jitter_ms = audio_params.jitter_ms.clamp(10, 500);
        let jitterbuffer = make_element("rtpjitterbuffer", None)?;
        jitterbuffer.set_property("latency", u32::from(configured_jitter_ms));
        jitterbuffer.set_property_from_str("mode", "slave");
        jitterbuffer.set_property("do-lost", true);

        let audioconvert = make_element("audioconvert", None)?;
        let audioresample = make_element("audioresample", None)?;

        let clamped_vol = initial_volume.min(150);
        let volume_elem = make_element("volume", Some("sh_vol"))?;
        volume_elem.set_property("volume", f64::from(clamped_vol) / 100.0);
        volume_elem.set_property("mute", initial_muted);

        let (sink_caps_opt, sink) = build_playback_sink(endpoint)?;

        let link_result: TransportResult<()> = (|| {
            match audio_params.codec {
                CodecId::Opus => {
                    let depay = make_element("rtpopusdepay", None)?;
                    let dec = make_element("opusdec", None)?;
                    dec.set_property("plc", true);
                    dec.set_property("use-inband-fec", true);

                    pipeline
                        .add_many([
                            &udpsrc,
                            &srtpdec,
                            &jitterbuffer,
                            &depay,
                            &dec,
                            &audioconvert,
                            &audioresample,
                            &volume_elem,
                        ])
                        .map_err(|e| {
                            TransportError::Pipeline(format!(
                                "failed adding Opus receiver elements: {e}"
                            ))
                        })?;
                    link_srtp_frontend(&udpsrc, &srtpdec, &jitterbuffer)?;
                    gst::Element::link_many([
                        &jitterbuffer,
                        &depay,
                        &dec,
                        &audioconvert,
                        &audioresample,
                        &volume_elem,
                    ])
                    .map_err(|e| {
                        TransportError::Pipeline(format!("failed linking Opus receiver chain: {e}"))
                    })?;
                }
                CodecId::PcmS16Le => {
                    let depay = make_element("rtpL16depay", None)?;
                    pipeline
                        .add_many([
                            &udpsrc,
                            &srtpdec,
                            &jitterbuffer,
                            &depay,
                            &audioconvert,
                            &audioresample,
                            &volume_elem,
                        ])
                        .map_err(|e| {
                            TransportError::Pipeline(format!(
                                "failed adding PCM receiver elements: {e}"
                            ))
                        })?;
                    link_srtp_frontend(&udpsrc, &srtpdec, &jitterbuffer)?;
                    gst::Element::link_many([
                        &jitterbuffer,
                        &depay,
                        &audioconvert,
                        &audioresample,
                        &volume_elem,
                    ])
                    .map_err(|e| {
                        TransportError::Pipeline(format!("failed linking PCM receiver chain: {e}"))
                    })?;
                }
            }

            if let Some(sink_caps) = sink_caps_opt {
                pipeline.add_many([&sink_caps, &sink]).map_err(|e| {
                    TransportError::Pipeline(format!("failed adding sink elements: {e}"))
                })?;
                gst::Element::link_many([&volume_elem, &sink_caps, &sink]).map_err(|e| {
                    TransportError::Pipeline(format!("failed linking receiver sink chain: {e}"))
                })?;
            } else {
                pipeline.add(&sink).map_err(|e| {
                    TransportError::Pipeline(format!("failed adding sink element: {e}"))
                })?;
                volume_elem.link(&sink).map_err(|e| {
                    TransportError::Pipeline(format!("failed linking volume to sink: {e}"))
                })?;
            }
            Ok(())
        })();

        if let Err(err) = link_result {
            if let Some(bus) = pipeline.bus() {
                bus.set_flushing(true);
            }
            let _ = pipeline.set_state(gst::State::Null);
            let _ = udpsrc.set_state(gst::State::Null);
            return Err(err);
        }

        Ok(Self {
            pipeline,
            srtpdec,
            jitterbuffer,
            volume_elem,
            bound_rtp_port,
            configured_jitter_ms,
            volume_pct: Arc::new(AtomicU8::new(clamped_vol)),
            muted: Arc::new(AtomicBool::new(initial_muted)),
            activated: false,
        })
    }

    /// The actual UDP port bound by `udpsrc`.
    pub fn rtp_port(&self) -> u16 {
        self.bound_rtp_port
    }

    /// Attach the lock-free `srtpdec::request-key` handler for `(expected_ssrc, srtp_key)`
    /// and transition the pipeline to `Playing`.
    pub fn start(
        mut self,
        expected_ssrc: u32,
        srtp_key: [u8; 30],
    ) -> TransportResult<GstAudioReceiver> {
        // Signal callback captures ONLY `(expected_ssrc, srtp_key)` — zero mutex locks!
        self.srtpdec.connect("request-key", false, move |values| {
            let ssrc = match values.get(1).and_then(|v| v.get::<u32>().ok()) {
                Some(s) => s,
                None => return Some(None::<gst::Caps>.to_value()),
            };
            if ssrc != expected_ssrc {
                return Some(None::<gst::Caps>.to_value());
            }
            let key_buf = gst::Buffer::from_slice(srtp_key);
            let caps = gst::Caps::builder("application/x-srtp")
                .field("srtp-key", key_buf)
                .field("srtp-cipher", "aes-128-icm")
                .field("srtp-auth", "hmac-sha1-80")
                .field("srtcp-cipher", "aes-128-icm")
                .field("srtcp-auth", "hmac-sha1-80")
                .field("roc", 0u32)
                .build();
            Some(Some(caps).to_value())
        });

        if let Err(err) = self.pipeline.set_state(gst::State::Playing) {
            if let Some(bus) = self.pipeline.bus() {
                bus.set_flushing(true);
            }
            let _ = self.pipeline.set_state(gst::State::Null);
            self.activated = true;
            return Err(TransportError::Pipeline(format!(
                "failed starting GStreamer receiver pipeline: {err:?}"
            )));
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let last_error = Arc::new(Mutex::new(None));

        let bus_thread = if let Some(bus) = self.pipeline.bus() {
            let stop_c = Arc::clone(&stop_flag);
            let healthy_c = Arc::clone(&healthy);
            let last_err_c = Arc::clone(&last_error);
            Some(thread::spawn(move || {
                while !stop_c.load(Ordering::Acquire) {
                    let Some(msg) = bus.timed_pop(BUS_POLL_TIMEOUT) else {
                        continue;
                    };
                    match msg.view() {
                        gst::MessageView::Error(err) => {
                            let detail = format!(
                                "GStreamer receiver error from {}: {} ({:?})",
                                err.src()
                                    .map(|s| s.path_string().to_string())
                                    .unwrap_or_else(|| "unknown".into()),
                                err.error(),
                                err.debug()
                            );
                            *last_err_c.lock().unwrap_or_else(|e| e.into_inner()) = Some(detail);
                            healthy_c.store(false, Ordering::Release);
                            break;
                        }
                        gst::MessageView::Eos(_) => {
                            healthy_c.store(false, Ordering::Release);
                            break;
                        }
                        _ => {}
                    }
                }
            }))
        } else {
            None
        };

        self.activated = true;
        Ok(GstAudioReceiver {
            pipeline: self.pipeline.clone(),
            jitterbuffer: self.jitterbuffer.clone(),
            volume_elem: self.volume_elem.clone(),
            bound_rtp_port: self.bound_rtp_port,
            configured_jitter_ms: self.configured_jitter_ms,
            volume_pct: Arc::clone(&self.volume_pct),
            muted: Arc::clone(&self.muted),
            stop_flag,
            healthy,
            last_error,
            bus_thread: Mutex::new(bus_thread),
        })
    }
}

impl Drop for PreparedGstReceiver {
    fn drop(&mut self) {
        if !self.activated {
            if let Some(bus) = self.pipeline.bus() {
                bus.set_flushing(true);
            }
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }
}

/// Live GStreamer SRTP inbound audio receiver pipeline.
pub struct GstAudioReceiver {
    pipeline: gst::Pipeline,
    jitterbuffer: gst::Element,
    volume_elem: gst::Element,
    bound_rtp_port: u16,
    configured_jitter_ms: u16,
    volume_pct: Arc<AtomicU8>,
    muted: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    bus_thread: Mutex<Option<JoinHandle<()>>>,
}

impl GstAudioReceiver {
    pub fn rtp_port(&self) -> u16 {
        self.bound_rtp_port
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Update receiver output volume (`0..=150` percent) thread-safely.
    pub fn set_volume(&self, pct: u8) {
        let clamped = pct.min(150);
        self.volume_pct.store(clamped, Ordering::Release);
        self.volume_elem
            .set_property("volume", f64::from(clamped) / 100.0);
    }

    /// Update receiver mute state thread-safely.
    pub fn set_mute(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
        self.volume_elem.set_property("mute", muted);
    }

    pub fn volume(&self) -> u8 {
        self.volume_pct.load(Ordering::Acquire)
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    /// Read live `rtpjitterbuffer` `stats` and `percent` telemetry.
    pub fn jitter_stats(&self) -> ReceiverJitterStats {
        let stats = self.jitterbuffer.property::<gst::Structure>("stats");
        let num_pushed = stats.get::<u64>("num-pushed").unwrap_or(0);
        let num_lost = stats.get::<u64>("num-lost").unwrap_or(0);
        let num_late = stats.get::<u64>("num-late").unwrap_or(0);
        let avg_jitter_ns = stats.get::<u64>("avg-jitter").unwrap_or(0);
        let percent = self.jitterbuffer.property::<i32>("percent").clamp(0, 100) as u8;
        let buffered_ms =
            ((u32::from(self.configured_jitter_ms) * u32::from(percent)) / 100) as u16;
        let dropped_frames = num_lost.saturating_add(num_late).min(u64::from(u32::MAX)) as u32;
        ReceiverJitterStats {
            num_pushed,
            num_lost,
            num_late,
            avg_jitter_ns,
            percent,
            buffered_ms,
            dropped_frames,
        }
    }

    /// Deadlock-free teardown: flush bus, join watchdog thread, transition pipeline to `Null`.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        self.healthy.store(false, Ordering::Release);
        if let Some(bus) = self.pipeline.bus() {
            bus.set_flushing(true);
        }
        if let Some(handle) = self
            .bus_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        let _ = self
            .pipeline
            .state(Some(gst::ClockTime::from_mseconds(500)));
    }
}

impl Drop for GstAudioReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

fn bind_udpsrc_port(udpsrc: &gst::Element, preferred_rtp_port: u16) -> TransportResult<u16> {
    if preferred_rtp_port > 0 {
        udpsrc.set_property("port", i32::from(preferred_rtp_port));
        if udpsrc.set_state(gst::State::Ready).is_ok() {
            let actual = udpsrc.property::<i32>("port");
            if actual > 0 && actual <= i32::from(u16::MAX) {
                return Ok(actual as u16);
            }
        }
        let _ = udpsrc.set_state(gst::State::Null);
    }

    // Fall back to OS-assigned ephemeral UDP port.
    udpsrc.set_property("port", 0i32);
    udpsrc.set_state(gst::State::Ready).map_err(|e| {
        TransportError::Pipeline(format!("failed binding udpsrc ephemeral port: {e:?}"))
    })?;
    let actual = udpsrc.property::<i32>("port");
    if actual <= 0 || actual > i32::from(u16::MAX) {
        let _ = udpsrc.set_state(gst::State::Null);
        return Err(TransportError::Pipeline(format!(
            "udpsrc reported invalid bound port {actual}"
        )));
    }
    Ok(actual as u16)
}

fn link_srtp_frontend(
    udpsrc: &gst::Element,
    srtpdec: &gst::Element,
    jitterbuffer: &gst::Element,
) -> TransportResult<()> {
    udpsrc
        .link_pads(Some("src"), srtpdec, Some("rtp_sink"))
        .map_err(|e| TransportError::Pipeline(format!("failed linking udpsrc to srtpdec: {e}")))?;
    srtpdec
        .link_pads(Some("rtp_src"), jitterbuffer, Some("sink"))
        .map_err(|e| {
            TransportError::Pipeline(format!("failed linking srtpdec to rtpjitterbuffer: {e}"))
        })?;
    Ok(())
}

fn build_playback_sink(
    endpoint: PlaybackEndpoint,
) -> TransportResult<(Option<gst::Element>, gst::Element)> {
    match endpoint {
        PlaybackEndpoint::SystemPulse => {
            let sink = make_element("pulsesink", None)?;
            sink.set_property("buffer-time", 40_000i64);
            sink.set_property("latency-time", 10_000i64);
            sink.set_property("provide-clock", true);
            sink.set_property_from_str("slave-method", "skew");
            Ok((None, sink))
        }
        PlaybackEndpoint::SystemWasapi => {
            let sink = make_element("wasapi2sink", None)?;
            sink.set_property("low-latency", true);
            sink.set_property("provide-clock", true);
            sink.set_property_from_str("slave-method", "skew");
            Ok((None, sink))
        }
        PlaybackEndpoint::TestAppSink(samples_buf) => {
            let caps = gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("rate", 48_000i32)
                .field("channels", 2i32)
                .field("layout", "interleaved")
                .build();
            let capsfilter = make_element("capsfilter", None)?;
            capsfilter.set_property("caps", &caps);

            let elem = make_element("appsink", None)?;
            let appsink = elem.downcast::<gst_app::AppSink>().map_err(|_| {
                TransportError::Pipeline("failed downcasting appsink element".into())
            })?;
            appsink.set_sync(true);
            appsink.set_caps(Some(&caps));
            appsink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                        let bytes = map.as_slice();
                        let mut guard = samples_buf.lock().unwrap_or_else(|e| e.into_inner());
                        let (chunks, _) = bytes.as_chunks::<2>();
                        guard.reserve(chunks.len());
                        for &chunk in chunks {
                            guard.push(i16::from_le_bytes(chunk));
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
            Ok((Some(capsfilter), appsink.upcast()))
        }
    }
}
