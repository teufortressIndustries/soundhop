//! GStreamer outbound audio sender pipeline: captures system audio (or synthetic test wave),
//! encodes to Opus or L16 PCM, packetizes to RTP with explicit SSRC, encrypts via SRTP
//! (`AES-128-ICM` + `HMAC-SHA1-80`), and transmits over UDP.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use zeroize::Zeroizing;

use crate::transport::gst_runtime::{ensure_gst_init, make_element};
use crate::transport::wire::{AudioParams, CodecId};
use crate::transport::{TransportError, TransportResult};

const BUS_POLL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_mseconds(100);

/// Owned `AsRef<[u8]>` wrapper so `gst::Buffer::from_slice` zeroizes key material on buffer drop.
struct ZeroizingKeyBuf(Zeroizing<[u8; 30]>);

impl AsRef<[u8]> for ZeroizingKeyBuf {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// Thread-safe handle allowing tests to trigger live packet drops or probability changes
/// on a `netsim` element embedded in a [`CaptureEndpoint::TestSineWave`] sender pipeline.
#[derive(Debug, Default)]
pub struct NetsimController {
    element: Mutex<Option<gst::Element>>,
}

impl NetsimController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn attach(&self, elem: gst::Element) {
        *self.element.lock().unwrap_or_else(|e| e.into_inner()) = Some(elem);
    }

    pub(crate) fn detach(&self) {
        *self.element.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Instruct `netsim` to drop the next `count` RTP/SRTP packets immediately.
    pub fn drop_next_packets(&self, count: u32) -> bool {
        if let Some(elem) = self
            .element
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            elem.set_property("drop-packets", count);
            true
        } else {
            false
        }
    }

    /// Update `netsim` random packet drop probability (`0.0..=1.0`) on the fly.
    pub fn set_drop_probability(&self, prob: f32) -> bool {
        if let Some(elem) = self
            .element
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            elem.set_property("drop-probability", prob.clamp(0.0, 1.0));
            true
        } else {
            false
        }
    }
}

/// Network impairment parameters applied via GStreamer `netsim` in headless tests.
#[derive(Debug, Clone)]
pub struct NetsimConfig {
    pub drop_probability: f32,
    pub delay_probability: f32,
    pub min_delay_ms: i32,
    pub max_delay_ms: i32,
    pub allow_reordering: bool,
    pub drop_packets: u32,
    pub controller: Option<Arc<NetsimController>>,
}

impl Default for NetsimConfig {
    fn default() -> Self {
        Self {
            drop_probability: 0.0,
            delay_probability: 0.0,
            min_delay_ms: 0,
            max_delay_ms: 0,
            allow_reordering: true,
            drop_packets: 0,
            controller: None,
        }
    }
}

/// Audio capture endpoint for a [`GstAudioSender`] pipeline.
#[derive(Debug, Clone)]
pub enum CaptureEndpoint {
    /// Linux PulseAudio/PipeWire monitor source (`<sink_name>.monitor`).
    SystemPulseMonitor { device: String },
    /// Windows WASAPI2 loopback capture (`wasapi2src loopback=true`).
    SystemWasapiLoopback,
    /// Headless live sine-wave generator (`audiotestsrc`) with optional `netsim` impairment.
    TestSineWave {
        freq_hz: f64,
        netsim: Option<NetsimConfig>,
    },
}

/// Live GStreamer SRTP outbound audio pipeline.
pub struct GstAudioSender {
    pipeline: gst::Pipeline,
    stop_flag: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    frames_sent: Arc<AtomicU64>,
    bytes_sent: Arc<AtomicU64>,
    last_error: Arc<Mutex<Option<String>>>,
    netsim_controller: Option<Arc<NetsimController>>,
    bus_thread: Mutex<Option<JoinHandle<()>>>,
}

impl GstAudioSender {
    /// Build and start a live SRTP audio sender pipeline targeting `dest_ip:rtp_port`.
    pub fn start(
        endpoint: CaptureEndpoint,
        audio_params: AudioParams,
        dest_ip: IpAddr,
        rtp_port: u16,
        sender_ssrc: u32,
        srtp_key: Zeroizing<[u8; 30]>,
    ) -> TransportResult<Self> {
        ensure_gst_init()?;
        if rtp_port == 0 {
            return Err(TransportError::Pipeline(
                "cannot start SRTP sender with rtp_port 0".into(),
            ));
        }

        let pipeline = gst::Pipeline::new();
        let (source, netsim_cfg) = build_capture_source(&endpoint, &audio_params)?;
        let audioconvert = make_element("audioconvert", None)?;
        let audioresample = make_element("audioresample", None)?;

        let srtpenc = make_element("srtpenc", None)?;
        let key_buf = gst::Buffer::from_slice(ZeroizingKeyBuf(srtp_key));
        srtpenc.set_property("key", &key_buf);
        srtpenc.set_property_from_str("rtp-cipher", "aes-128-icm");
        srtpenc.set_property_from_str("rtp-auth", "hmac-sha1-80");
        srtpenc.set_property_from_str("rtcp-cipher", "aes-128-icm");
        srtpenc.set_property_from_str("rtcp-auth", "hmac-sha1-80");
        srtpenc.set_property("replay-window-size", 256u32);

        let udpsink = make_element("udpsink", None)?;
        udpsink.set_property("host", dest_ip.to_string());
        udpsink.set_property("port", i32::from(rtp_port));
        udpsink.set_property("sync", false);
        udpsink.set_property("async", false);

        let frames_sent = Arc::new(AtomicU64::new(0));
        let bytes_sent = Arc::new(AtomicU64::new(0));

        // Build codec + RTP payloader branch.
        let payloader = match audio_params.codec {
            CodecId::Opus => {
                let raw_caps = gst::Caps::builder("audio/x-raw")
                    .field("rate", audio_params.sample_rate as i32)
                    .field("channels", i32::from(audio_params.channels))
                    .field("layout", "interleaved")
                    .build();
                let capsfilter = make_element("capsfilter", None)?;
                capsfilter.set_property("caps", &raw_caps);

                let opusenc = make_element("opusenc", None)?;
                opusenc.set_property_from_str("audio-type", "generic");
                opusenc.set_property_from_str("bandwidth", "fullband");
                opusenc.set_property_from_str("bitrate-type", "constrained-vbr");
                opusenc.set_property("complexity", 10i32);
                opusenc.set_property("dtx", false);
                opusenc.set_property("inband-fec", true);
                opusenc.set_property("packet-loss-percentage", 10i32);
                let bitrate_bps =
                    (i32::from(audio_params.bitrate_kbps) * 1000).clamp(16_000, 510_000);
                opusenc.set_property("bitrate", bitrate_bps);
                let frame_size = if audio_params.frame_ms <= 10 {
                    "10"
                } else {
                    "20"
                };
                opusenc.set_property_from_str("frame-size", frame_size);

                let rtpopuspay = make_element("rtpopuspay", None)?;
                rtpopuspay.set_property("pt", 96u32);
                rtpopuspay.set_property("mtu", 1400u32);
                rtpopuspay.set_property("perfect-rtptime", true);
                rtpopuspay.set_property("ssrc", sender_ssrc);

                pipeline
                    .add_many([
                        &source,
                        &audioconvert,
                        &audioresample,
                        &capsfilter,
                        &opusenc,
                        &rtpopuspay,
                        &srtpenc,
                        &udpsink,
                    ])
                    .map_err(|e| {
                        TransportError::Pipeline(format!("failed adding Opus sender elements: {e}"))
                    })?;
                gst::Element::link_many([
                    &source,
                    &audioconvert,
                    &audioresample,
                    &capsfilter,
                    &opusenc,
                    &rtpopuspay,
                ])
                .map_err(|e| {
                    TransportError::Pipeline(format!("failed linking Opus sender chain: {e}"))
                })?;
                rtpopuspay
            }
            CodecId::PcmS16Le => {
                let raw_caps = gst::Caps::builder("audio/x-raw")
                    .field("format", "S16BE")
                    .field("rate", audio_params.sample_rate as i32)
                    .field("channels", i32::from(audio_params.channels))
                    .field("layout", "interleaved")
                    .build();
                let capsfilter = make_element("capsfilter", None)?;
                capsfilter.set_property("caps", &raw_caps);

                let rtp_l16_pay = make_element("rtpL16pay", None)?;
                rtp_l16_pay.set_property("pt", 97u32);
                rtp_l16_pay.set_property("mtu", 1400u32);
                rtp_l16_pay.set_property("perfect-rtptime", true);
                rtp_l16_pay.set_property("ssrc", sender_ssrc);

                pipeline
                    .add_many([
                        &source,
                        &audioconvert,
                        &audioresample,
                        &capsfilter,
                        &rtp_l16_pay,
                        &srtpenc,
                        &udpsink,
                    ])
                    .map_err(|e| {
                        TransportError::Pipeline(format!("failed adding PCM sender elements: {e}"))
                    })?;
                gst::Element::link_many([
                    &source,
                    &audioconvert,
                    &audioresample,
                    &capsfilter,
                    &rtp_l16_pay,
                ])
                .map_err(|e| {
                    TransportError::Pipeline(format!("failed linking PCM sender chain: {e}"))
                })?;
                rtp_l16_pay
            }
        };

        // Request srtpenc's `rtp_sink_0` pad and link payloader -> srtpenc -> [netsim] -> udpsink.
        let srtp_sink_pad = srtpenc.request_pad_simple("rtp_sink_0").ok_or_else(|| {
            TransportError::Pipeline("srtpenc did not provide rtp_sink_0 pad".into())
        })?;
        let pay_src_pad = payloader.static_pad("src").ok_or_else(|| {
            TransportError::Pipeline("RTP payloader missing static src pad".into())
        })?;
        pay_src_pad.link(&srtp_sink_pad).map_err(|e| {
            TransportError::Pipeline(format!("failed linking payloader to srtpenc: {e:?}"))
        })?;

        let srtp_src_pad = srtpenc.static_pad("rtp_src_0").ok_or_else(|| {
            TransportError::Pipeline("srtpenc did not create rtp_src_0 pad".into())
        })?;

        // Track transmitted SRTP frames and bytes atomically on the srtpenc output pad.
        let frames_probe = Arc::clone(&frames_sent);
        let bytes_probe = Arc::clone(&bytes_sent);
        srtp_src_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            if let Some(gst::PadProbeData::Buffer(ref buf)) = info.data {
                frames_probe.fetch_add(1, Ordering::Relaxed);
                bytes_probe.fetch_add(buf.size() as u64, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });

        let mut netsim_controller = None;
        if let Some(cfg) = netsim_cfg {
            let netsim = make_element("netsim", None)?;
            netsim.set_property("drop-probability", cfg.drop_probability.clamp(0.0, 1.0));
            netsim.set_property("delay-probability", cfg.delay_probability.clamp(0.0, 1.0));
            netsim.set_property("min-delay", cfg.min_delay_ms);
            netsim.set_property("max-delay", cfg.max_delay_ms.max(cfg.min_delay_ms));
            netsim.set_property("allow-reordering", cfg.allow_reordering);
            if cfg.drop_packets > 0 {
                netsim.set_property("drop-packets", cfg.drop_packets);
            }
            if let Some(ctrl) = cfg.controller {
                ctrl.attach(netsim.clone());
                netsim_controller = Some(ctrl);
            }
            pipeline.add(&netsim).map_err(|e| {
                TransportError::Pipeline(format!("failed adding netsim element: {e}"))
            })?;
            let netsim_sink = netsim
                .static_pad("sink")
                .ok_or_else(|| TransportError::Pipeline("netsim missing sink pad".into()))?;
            srtp_src_pad.link(&netsim_sink).map_err(|e| {
                TransportError::Pipeline(format!("failed linking srtpenc to netsim: {e:?}"))
            })?;
            netsim.link(&udpsink).map_err(|e| {
                TransportError::Pipeline(format!("failed linking netsim to udpsink: {e}"))
            })?;
        } else {
            let udp_sink_pad = udpsink
                .static_pad("sink")
                .ok_or_else(|| TransportError::Pipeline("udpsink missing sink pad".into()))?;
            srtp_src_pad.link(&udp_sink_pad).map_err(|e| {
                TransportError::Pipeline(format!("failed linking srtpenc to udpsink: {e:?}"))
            })?;
        }

        if let Err(err) = pipeline.set_state(gst::State::Playing) {
            if let Some(bus) = pipeline.bus() {
                bus.set_flushing(true);
            }
            let _ = pipeline.set_state(gst::State::Null);
            if let Some(ref ctrl) = netsim_controller {
                ctrl.detach();
            }
            return Err(TransportError::Pipeline(format!(
                "failed starting GStreamer sender pipeline: {err:?}"
            )));
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let last_error = Arc::new(Mutex::new(None));

        let bus_thread = if let Some(bus) = pipeline.bus() {
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
                                "GStreamer sender error from {}: {} ({:?})",
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

        Ok(Self {
            pipeline,
            stop_flag,
            healthy,
            frames_sent,
            bytes_sent,
            last_error,
            netsim_controller,
            bus_thread: Mutex::new(bus_thread),
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn frames_sent(&self) -> u64 {
        self.frames_sent.load(Ordering::Relaxed)
    }

    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stop the pipeline cleanly and join the bus watchdog thread without deadlocking.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        self.healthy.store(false, Ordering::Release);
        if let Some(ref ctrl) = self.netsim_controller {
            ctrl.detach();
        }
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

impl Drop for GstAudioSender {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_capture_source(
    endpoint: &CaptureEndpoint,
    audio_params: &AudioParams,
) -> TransportResult<(gst::Element, Option<NetsimConfig>)> {
    match endpoint {
        CaptureEndpoint::SystemPulseMonitor { device } => {
            let pulsesrc = make_element("pulsesrc", None)?;
            pulsesrc.set_property("device", device.as_str());
            let (buffer_time, latency_time): (i64, i64) = if audio_params.frame_ms <= 10 {
                (10_000, 5_000)
            } else {
                (20_000, 10_000)
            };
            pulsesrc.set_property("buffer-time", buffer_time);
            pulsesrc.set_property("latency-time", latency_time);
            pulsesrc.set_property("provide-clock", true);
            Ok((pulsesrc, None))
        }
        CaptureEndpoint::SystemWasapiLoopback => {
            let wasapisrc = make_element("wasapi2src", None)?;
            wasapisrc.set_property("loopback", true);
            wasapisrc.set_property("low-latency", true);
            wasapisrc.set_property("provide-clock", true);
            Ok((wasapisrc, None))
        }
        CaptureEndpoint::TestSineWave { freq_hz, netsim } => {
            let src = make_element("audiotestsrc", None)?;
            src.set_property("is-live", true);
            src.set_property_from_str("wave", "sine");
            src.set_property("freq", *freq_hz);
            src.set_property("volume", 0.5f64);
            let spc = ((audio_params.sample_rate as usize * usize::from(audio_params.frame_ms))
                / 1000)
                .clamp(240, 1920) as i32;
            src.set_property("samplesperbuffer", spc);
            Ok((src, netsim.clone()))
        }
    }
}

/// Helper to keep unused Duration import lint-free if not referenced directly.
#[allow(dead_code)]
const _DEFAULT_TEARDOWN_HINT: Duration = Duration::from_millis(500);
