//! Headless GStreamer SRTP/Opus end-to-end, `netsim` Wi-Fi impairment, volume/telemetry,
//! and lifecycle test suite (zero audio hardware required).

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::transport::{
    check_gstreamer_runtime, AudioProfile, CaptureEndpoint, InMemoryTrustStore, NetsimConfig,
    NetsimController, OutboundConfig, PlaybackEndpoint, Sht3ServerConfig, TransportManager,
    TrustLookup,
};

/// Compute RMS amplitude of interleaved stereo `i16` samples (normalized `0.0..=1.0`).
fn rms_normalized(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples
        .iter()
        .map(|&s| {
            let v = f64::from(s) / 32768.0;
            v * v
        })
        .sum();
    (sum_sq / samples.len() as f64).sqrt()
}

/// Measure how strongly the left-channel signal locks onto `target_hz` (0.0..=1.0 power ratio,
/// using a Hann-windowed DFT bin at `target_hz` vs total signal energy).
fn tone_power_ratio(interleaved_stereo: &[i16], sample_rate: f64, target_hz: f64) -> f64 {
    let mono: Vec<f64> = interleaved_stereo
        .as_chunks::<2>()
        .0
        .iter()
        .map(|lr| f64::from(lr[0]) / 32768.0)
        .collect();
    let n = mono.len();
    if n < 256 {
        return 0.0;
    }
    let two_pi = 2.0 * std::f64::consts::PI;
    let omega = two_pi * target_hz / sample_rate;
    let mut cos_sum = 0.0;
    let mut sin_sum = 0.0;
    let mut total_energy = 0.0;
    let mut win_sq_sum = 0.0;
    for (i, &x) in mono.iter().enumerate() {
        let w = 0.5 * (1.0 - (two_pi * i as f64 / (n - 1) as f64).cos());
        let wx = x * w;
        total_energy += wx * wx;
        win_sq_sum += w * w;
        let phase = omega * i as f64;
        cos_sum += wx * phase.cos();
        sin_sum += wx * phase.sin();
    }
    if total_energy <= 1e-12 || win_sq_sum <= 1e-12 {
        return 0.0;
    }
    // Single real sinusoid power reconstructed from windowed projection.
    let tone_energy = 2.0 * (cos_sum * cos_sum + sin_sum * sin_sum) / win_sq_sum;
    (tone_energy / total_energy).clamp(0.0, 1.0)
}

/// Convert a tone power ratio in `(0, 1)` to approximate SNR in dB.
fn ratio_to_snr_db(ratio: f64) -> f64 {
    if ratio >= 0.9999 {
        40.0
    } else if ratio <= 1e-9 {
        -60.0
    } else {
        10.0 * (ratio / (1.0 - ratio)).log10()
    }
}

fn wait_for_samples(buf: &Arc<Mutex<Vec<i16>>>, min_samples: usize, timeout: Duration) -> Vec<i16> {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let guard = buf.lock().unwrap();
            if guard.len() >= min_samples {
                return guard.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {min_samples} decoded samples (got {})",
            buf.lock().unwrap().len()
        );
        thread::sleep(Duration::from_millis(15));
    }
}

#[test]
fn gstreamer_runtime_check_reports_all_required_elements() {
    let status = check_gstreamer_runtime().expect("GStreamer runtime check must succeed");
    assert!(
        status.available,
        "missing GStreamer elements: {:?}",
        status.missing_elements
    );
    assert!(!status.version.is_empty());
    assert!(status.missing_elements.is_empty());
}

#[test]
fn clean_e2e_srtp_loopback_all_profiles_and_forged_udp_rejection() {
    let k_pair = [0x42u8; 32];
    let trust = Arc::new(InMemoryTrustStore::new());
    trust.insert("sender-e2e", k_pair);

    for profile in [
        AudioProfile::OpusBalanced,
        AudioProfile::OpusLowLatency,
        AudioProfile::OpusLowBandwidth,
        AudioProfile::PcmLossless,
    ] {
        let samples = Arc::new(Mutex::new(Vec::<i16>::new()));
        let samples_c = Arc::clone(&samples);

        let manager = TransportManager::new();
        let srv_cfg = Sht3ServerConfig::new("receiver-e2e").with_rtp_port(0);
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                srv_cfg,
                Arc::clone(&trust) as Arc<dyn TrustLookup>,
                Arc::new(move |_| Ok(PlaybackEndpoint::TestAppSink(Arc::clone(&samples_c)))),
            )
            .unwrap();

        let out_cfg = OutboundConfig::new("sender-e2e", addr, k_pair)
            .with_profile(profile)
            .with_jitter_ms(40)
            .with_ping_interval(Duration::from_millis(150));
        let handle = manager
            .start_outbound(
                out_cfg,
                CaptureEndpoint::TestSineWave {
                    freq_hz: 440.0,
                    netsim: None,
                },
            )
            .unwrap();

        // Wait for initial decoded audio to settle (~400ms stereo = 38_400 samples).
        let _ = wait_for_samples(&samples, 38_400, Duration::from_secs(4));

        // Inject forged / wrong-SSRC / corrupted SRTP UDP packets directly to bound rtp_port.
        let rtp_port = handle.rtp_port();
        let real_ssrc = handle.sender_ssrc();
        let attacker = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target: SocketAddr = format!("127.0.0.1:{rtp_port}").parse().unwrap();

        // 1) Pure garbage packet
        let _ = attacker.send_to(&[0xFF; 96], target);
        // 2) Valid-looking RTP header with WRONG SSRC
        let mut wrong_ssrc_pkt = vec![0u8; 64];
        wrong_ssrc_pkt[0] = 0x80; // V=2
        wrong_ssrc_pkt[1] = 96; // PT=96
        wrong_ssrc_pkt[8..12].copy_from_slice(&(real_ssrc ^ 0x1234_5678).to_be_bytes());
        for seq in 0..5u16 {
            wrong_ssrc_pkt[2..4].copy_from_slice(&seq.to_be_bytes());
            let _ = attacker.send_to(&wrong_ssrc_pkt, target);
        }
        // 3) Valid-looking RTP header with REAL SSRC but forged ciphertext & auth tag
        let mut forged_pkt = vec![0xA5u8; 64];
        forged_pkt[0] = 0x80;
        forged_pkt[1] = 96;
        forged_pkt[8..12].copy_from_slice(&real_ssrc.to_be_bytes());
        for seq in 100..105u16 {
            forged_pkt[2..4].copy_from_slice(&seq.to_be_bytes());
            let _ = attacker.send_to(&forged_pkt, target);
        }

        // Collect a fresh 400ms window after attack packets and verify clean 440 Hz tone SNR.
        samples.lock().unwrap().clear();
        let decoded = wait_for_samples(&samples, 38_400, Duration::from_secs(4));
        let rms = rms_normalized(&decoded);
        let ratio = tone_power_ratio(&decoded, 48_000.0, 440.0);
        let snr_db = ratio_to_snr_db(ratio);

        assert!(
            rms > 0.15,
            "profile {profile:?}: decoded audio too quiet (rms={rms:.4})"
        );
        assert!(
            snr_db > 15.0,
            "profile {profile:?}: poor 440 Hz tone fidelity after forged UDP injection (ratio={ratio:.4}, snr={snr_db:.1} dB)"
        );

        handle.stop();
        manager.stop_listener();
    }
}

#[test]
fn wifi_bump_burst_drop_recovers_440hz_with_fec_and_plc() {
    let k_pair = [0x53u8; 32];
    let trust = Arc::new(InMemoryTrustStore::new());
    trust.insert("sender-burst", k_pair);

    let samples = Arc::new(Mutex::new(Vec::<i16>::new()));
    let samples_c = Arc::clone(&samples);
    let netsim_ctrl = NetsimController::new();

    let manager = TransportManager::new();
    let srv_cfg = Sht3ServerConfig::new("receiver-burst").with_rtp_port(0);
    let addr = manager
        .start_listener(
            "127.0.0.1:0".parse().unwrap(),
            srv_cfg,
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
            Arc::new(move |_| Ok(PlaybackEndpoint::TestAppSink(Arc::clone(&samples_c)))),
        )
        .unwrap();

    let out_cfg = OutboundConfig::new("sender-burst", addr, k_pair)
        .with_profile(AudioProfile::OpusBalanced)
        .with_jitter_ms(50)
        .with_ping_interval(Duration::from_millis(150));
    let handle = manager
        .start_outbound(
            out_cfg,
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: Some(NetsimConfig {
                    controller: Some(Arc::clone(&netsim_ctrl)),
                    ..NetsimConfig::default()
                }),
            },
        )
        .unwrap();

    // 1. Wait for clean baseline lock.
    let _ = wait_for_samples(&samples, 38_400, Duration::from_secs(4));
    samples.lock().unwrap().clear();
    let pre = wait_for_samples(&samples, 28_800, Duration::from_secs(3));
    let pre_snr = ratio_to_snr_db(tone_power_ratio(&pre, 48_000.0, 440.0));
    assert!(
        pre_snr > 15.0,
        "pre-burst baseline SNR too low: {pre_snr:.1} dB"
    );

    // 2. Inject a 5-packet burst drop (~100ms Wi-Fi bump) and collect recovery audio.
    samples.lock().unwrap().clear();
    assert!(netsim_ctrl.drop_next_packets(5));
    // Wait ~500ms across the burst + recovery.
    let during_and_after = wait_for_samples(&samples, 48_000, Duration::from_secs(4));
    let burst_ratio = tone_power_ratio(&during_and_after, 48_000.0, 440.0);
    assert!(
        burst_ratio > 0.65,
        "burst drop broke 440 Hz pitch lock (ratio={burst_ratio:.3})"
    );

    // 3. Verify fast full SNR recovery immediately after the burst.
    samples.lock().unwrap().clear();
    let recovered = wait_for_samples(&samples, 28_800, Duration::from_secs(3));
    let post_snr = ratio_to_snr_db(tone_power_ratio(&recovered, 48_000.0, 440.0));
    assert!(
        post_snr > 15.0,
        "post-burst SNR did not recover cleanly: {post_snr:.1} dB"
    );

    handle.stop();
    manager.stop_listener();
}

#[test]
fn wifi_sustained_loss_jitter_reordering_reports_bounded_telemetry() {
    let k_pair = [0x64u8; 32];
    let trust = Arc::new(InMemoryTrustStore::new());
    trust.insert("sender-wifi", k_pair);

    let samples = Arc::new(Mutex::new(Vec::<i16>::new()));
    let samples_c = Arc::clone(&samples);

    let manager = TransportManager::new();
    let srv_cfg = Sht3ServerConfig::new("receiver-wifi").with_rtp_port(0);
    let addr = manager
        .start_listener(
            "127.0.0.1:0".parse().unwrap(),
            srv_cfg,
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
            Arc::new(move |_| Ok(PlaybackEndpoint::TestAppSink(Arc::clone(&samples_c)))),
        )
        .unwrap();

    let configured_jitter_ms = 60u16;
    let out_cfg = OutboundConfig::new("sender-wifi", addr, k_pair)
        .with_profile(AudioProfile::OpusBalanced)
        .with_jitter_ms(configured_jitter_ms)
        .with_ping_interval(Duration::from_millis(200));
    let handle = manager
        .start_outbound(
            out_cfg,
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: Some(NetsimConfig {
                    drop_probability: 0.03,
                    delay_probability: 0.5,
                    min_delay_ms: 5,
                    max_delay_ms: 35,
                    allow_reordering: true,
                    drop_packets: 2, // Guarantee at least 2 deterministic drops alongside 3% random loss
                    controller: None,
                }),
            },
        )
        .unwrap();

    // Stream ~1.8s (~172_800 interleaved stereo samples) under sustained Wi-Fi impairment.
    let decoded = wait_for_samples(&samples, 144_000, Duration::from_secs(6));
    let _ = handle.request_ping();

    // Wait for a Pong reflecting dropped packets and bounded jitter buffer depth.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut final_tel = handle.telemetry();
    while Instant::now() < deadline {
        final_tel = handle.telemetry();
        if final_tel.pongs_received >= 2 && final_tel.dropped_frames > 0 {
            break;
        }
        let _ = handle.request_ping();
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        final_tel.pongs_received >= 1,
        "expected Pong telemetry under Wi-Fi impairment"
    );
    assert!(
        final_tel.dropped_frames > 0,
        "expected Pong dropped_frames > 0 under sustained loss, got {final_tel:?}"
    );
    assert!(
        final_tel.jitter_buf_ms <= configured_jitter_ms,
        "jitter buffer depth {} exceeded configured {} ms",
        final_tel.jitter_buf_ms,
        configured_jitter_ms
    );

    // Check the tail 500ms of decoded audio stays locked to 440 Hz.
    let tail = &decoded[decoded.len().saturating_sub(48_000)..];
    let ratio = tone_power_ratio(tail, 48_000.0, 440.0);
    assert!(
        ratio > 0.70,
        "440 Hz tone degraded excessively under 3% loss + jitter + reordering (ratio={ratio:.3})"
    );

    handle.stop();
    manager.stop_listener();
}

#[test]
fn live_volume_mute_and_ping_telemetry() {
    let k_pair = [0x75u8; 32];
    let trust = Arc::new(InMemoryTrustStore::new());
    trust.insert("sender-vol", k_pair);

    let samples = Arc::new(Mutex::new(Vec::<i16>::new()));
    let samples_c = Arc::clone(&samples);

    let manager = TransportManager::new();
    let srv_cfg = Sht3ServerConfig::new("receiver-vol").with_rtp_port(0);
    let addr = manager
        .start_listener(
            "127.0.0.1:0".parse().unwrap(),
            srv_cfg,
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
            Arc::new(move |_| Ok(PlaybackEndpoint::TestAppSink(Arc::clone(&samples_c)))),
        )
        .unwrap();

    let out_cfg = OutboundConfig::new("sender-vol", addr, k_pair)
        .with_profile(AudioProfile::OpusBalanced)
        .with_jitter_ms(40)
        .with_ping_interval(Duration::from_millis(500));
    let handle = manager
        .start_outbound(
            out_cfg,
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            },
        )
        .unwrap();

    // 1. Baseline RMS at 100% volume.
    let _ = wait_for_samples(&samples, 38_400, Duration::from_secs(4));
    samples.lock().unwrap().clear();
    let full_vol_samples = wait_for_samples(&samples, 28_800, Duration::from_secs(3));
    let rms_100 = rms_normalized(&full_vol_samples);
    assert!(rms_100 > 0.20, "baseline 100% RMS too low: {rms_100:.4}");

    // 2. Live set_volume(50) -> remote_volume == 50 and RMS scales ~0.5x.
    handle.set_volume(50).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while handle.telemetry().remote_volume != 50 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for remote_volume == 50"
        );
        thread::sleep(Duration::from_millis(15));
    }
    // Drain briefly so buffered pre-change frames clear, then measure 50% RMS.
    thread::sleep(Duration::from_millis(120));
    samples.lock().unwrap().clear();
    let half_vol_samples = wait_for_samples(&samples, 28_800, Duration::from_secs(3));
    let rms_50 = rms_normalized(&half_vol_samples);
    let ratio = rms_50 / rms_100;
    assert!(
        (0.35..=0.65).contains(&ratio),
        "expected ~0.5x RMS after set_volume(50), got rms_100={rms_100:.4}, rms_50={rms_50:.4}, ratio={ratio:.3}"
    );

    // 3. Live set_mute(true) -> remote_muted == true and output drops to digital silence.
    handle.set_mute(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.telemetry().remote_muted {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for remote_muted == true"
        );
        thread::sleep(Duration::from_millis(15));
    }
    thread::sleep(Duration::from_millis(120));
    samples.lock().unwrap().clear();
    let muted_samples = wait_for_samples(&samples, 19_200, Duration::from_secs(3));
    let max_abs_muted = muted_samples
        .iter()
        .map(|&s| s.unsigned_abs())
        .max()
        .unwrap_or(0);
    assert!(
        max_abs_muted <= 4,
        "expected silence when muted, got max_abs={max_abs_muted}"
    );

    // 4. Explicit request_ping() updates telemetry cleanly.
    let pongs_before = handle.telemetry().pongs_received;
    handle.request_ping().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let tel = handle.telemetry();
        if tel.pongs_received > pongs_before && tel.frames_sent > 0 && tel.bytes_sent > 0 {
            assert!(tel.rtt_us > 0);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for request_ping() Pong"
        );
        thread::sleep(Duration::from_millis(15));
    }

    handle.stop();
    manager.stop_listener();
}

#[test]
fn lifecycle_disconnect_peer_abrupt_reconnect_and_rapid_stress() {
    let k_pair = [0x86u8; 32];
    let trust = Arc::new(InMemoryTrustStore::new());
    trust.insert("sender-life", k_pair);

    // --- 1. disconnect_peer immediate teardown ---
    let manager = TransportManager::new();
    let addr = manager
        .start_listener(
            "127.0.0.1:0".parse().unwrap(),
            Sht3ServerConfig::new("receiver-life").with_rtp_port(0),
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
            Arc::new(|_| {
                Ok(PlaybackEndpoint::TestAppSink(Arc::new(Mutex::new(
                    Vec::new(),
                ))))
            }),
        )
        .unwrap();

    let handle = manager
        .start_outbound(
            OutboundConfig::new("sender-life", addr, k_pair)
                .with_profile(AudioProfile::OpusLowLatency),
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            },
        )
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while !manager
        .active_inbound_peers()
        .iter()
        .any(|id| id == "sender-life")
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }

    let t0 = Instant::now();
    assert!(manager.disconnect_peer("sender-life"));
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "disconnect_peer took too long: {:?}",
        t0.elapsed()
    );
    assert!(manager.active_inbound_peers().is_empty());

    let stop_deadline = Instant::now() + Duration::from_secs(2);
    while handle.is_running() {
        assert!(
            Instant::now() < stop_deadline,
            "outbound handle did not exit after remote disconnect_peer"
        );
        thread::sleep(Duration::from_millis(15));
    }
    handle.stop();
    manager.stop_listener();

    // --- 2. Abrupt server restart reconnect with fresh SSRC, fresh SRTP keys, and volume replay ---
    let samples_after = Arc::new(Mutex::new(Vec::<i16>::new()));
    let samples_after_c = Arc::clone(&samples_after);

    let srv_a = TransportManager::new();
    let fixed_addr = srv_a
        .start_listener(
            "127.0.0.1:0".parse().unwrap(),
            Sht3ServerConfig::new("receiver-restart").with_rtp_port(0),
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
            Arc::new(|_| {
                Ok(PlaybackEndpoint::TestAppSink(Arc::new(Mutex::new(
                    Vec::new(),
                ))))
            }),
        )
        .unwrap();

    let reconnecting = srv_a
        .start_outbound(
            OutboundConfig::new("sender-life", fixed_addr, k_pair)
                .with_profile(AudioProfile::OpusBalanced)
                .with_ping_interval(Duration::from_millis(120))
                .with_reconnect_window(Duration::from_secs(5)),
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            },
        )
        .unwrap();

    let ssrc_before = reconnecting.sender_ssrc();
    reconnecting.set_volume(73).unwrap();
    let vol_deadline = Instant::now() + Duration::from_secs(2);
    while reconnecting.telemetry().remote_volume != 73 {
        assert!(Instant::now() < vol_deadline);
        thread::sleep(Duration::from_millis(15));
    }

    // Abruptly crash server A (drops socket without sending Close), then restart server B on same port.
    srv_a.simulate_abrupt_crash();

    let srv_b = TransportManager::new();
    srv_b
        .start_listener(
            fixed_addr,
            Sht3ServerConfig::new("receiver-restart").with_rtp_port(0),
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
            Arc::new(move |_| Ok(PlaybackEndpoint::TestAppSink(Arc::clone(&samples_after_c)))),
        )
        .unwrap();

    // Wait for automatic reconnect with fresh SSRC, volume replay (73), and live audio on server B.
    let reconn_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let diag = srv_b.inbound_peer_diagnostics("sender-life");
        let ssrc_now = reconnecting.sender_ssrc();
        if let Some(d) = diag {
            if ssrc_now != ssrc_before && d.sender_ssrc == ssrc_now && d.volume == 73 {
                break;
            }
        }
        assert!(
            Instant::now() < reconn_deadline,
            "timed out waiting for reconnect with fresh SSRC & volume replay (running={})",
            reconnecting.is_running()
        );
        thread::sleep(Duration::from_millis(25));
    }

    // Confirm SRTP audio actually flows and decodes cleanly on the restarted server with fresh keys.
    let post_reconnect = wait_for_samples(&samples_after, 28_800, Duration::from_secs(4));
    assert!(
        tone_power_ratio(&post_reconnect, 48_000.0, 440.0) > 0.75,
        "reconnected audio did not lock onto 440 Hz"
    );

    reconnecting.stop();
    srv_b.stop_listener();

    // --- 3. 5x rapid start/stop stress test ---
    for i in 0..5 {
        let mgr = TransportManager::new();
        let samples_i = Arc::new(Mutex::new(Vec::<i16>::new()));
        let samples_ic = Arc::clone(&samples_i);
        let a = mgr
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new(format!("stress-srv-{i}")).with_rtp_port(0),
                Arc::clone(&trust) as Arc<dyn TrustLookup>,
                Arc::new(move |_| Ok(PlaybackEndpoint::TestAppSink(Arc::clone(&samples_ic)))),
            )
            .unwrap();
        let h = mgr
            .start_outbound(
                OutboundConfig::new("sender-life", a, k_pair)
                    .with_profile(AudioProfile::OpusLowLatency),
                CaptureEndpoint::TestSineWave {
                    freq_hz: 440.0,
                    netsim: None,
                },
            )
            .unwrap();
        let _ = wait_for_samples(&samples_i, 4_800, Duration::from_secs(3));
        h.stop();
        mgr.stop_listener();
        assert!(!h.is_running());
        assert!(mgr.active_inbound_peers().is_empty());
    }
}
