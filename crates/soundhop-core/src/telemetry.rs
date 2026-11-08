//! Real-time audio level (Peak/RMS VU meter) and network latency/jitter telemetry.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::controller::Controller;
use crate::util::{
    command_exists, is_own_non_loopback_ip, is_private_address, now_secs, tcp_probe_rtt,
};

const MAX_RTT_SAMPLES: usize = 30;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PeerNetStats {
    pub peer_id: String,
    pub last_rtt_ms: f64,
    pub min_rtt_ms: f64,
    pub max_rtt_ms: f64,
    pub avg_rtt_ms: f64,
    pub jitter_ms: f64,
    pub samples: Vec<f64>,
    pub updated_at: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AudioLevelSnapshot {
    /// Peak amplitude in `[0.0, 1.0]`.
    pub peak_left: f32,
    pub peak_right: f32,
    /// RMS amplitude in `[0.0, 1.0]`.
    pub rms_left: f32,
    pub rms_right: f32,
    /// Approximate decibels relative to full scale (`-60.0..=0.0`).
    pub dbfs: f32,
    pub active: bool,
    pub updated_at: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TelemetrySnapshot {
    pub audio: AudioLevelSnapshot,
    pub peers: BTreeMap<String, PeerNetStats>,
}

pub struct TelemetryTracker {
    audio: RwLock<AudioLevelSnapshot>,
    rtt_history: RwLock<BTreeMap<String, VecDeque<f64>>>,
}

impl Default for TelemetryTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryTracker {
    pub fn new() -> Self {
        Self {
            audio: RwLock::new(AudioLevelSnapshot {
                dbfs: -60.0,
                ..Default::default()
            }),
            rtt_history: RwLock::new(BTreeMap::new()),
        }
    }

    /// Record a new RTT sample (in milliseconds) for a peer and compute rolling stats.
    pub fn record_rtt(&self, peer_id: &str, rtt_ms: f64) -> PeerNetStats {
        let clamped = rtt_ms.max(0.05);
        let mut map = self.rtt_history.write().unwrap();
        let queue = map.entry(peer_id.to_string()).or_default();
        if queue.len() >= MAX_RTT_SAMPLES {
            queue.pop_front();
        }
        queue.push_back(clamped);
        compute_stats(peer_id, queue)
    }

    pub fn peer_stats(&self, peer_id: &str) -> Option<PeerNetStats> {
        let map = self.rtt_history.read().unwrap();
        map.get(peer_id).map(|q| compute_stats(peer_id, q))
    }

    /// Update real-time audio levels from interleaved S16LE stereo PCM bytes with smooth VU ballistics.
    pub fn update_audio_from_s16le(&self, pcm_bytes: &[u8]) {
        let raw = compute_levels_s16le(pcm_bytes);
        let mut guard = self.audio.write().unwrap();
        let decayed_peak_l = (guard.peak_left * 0.82).clamp(0.0, 1.0);
        let decayed_peak_r = (guard.peak_right * 0.82).clamp(0.0, 1.0);
        let decayed_rms_l = (guard.rms_left * 0.82).clamp(0.0, 1.0);
        let decayed_rms_r = (guard.rms_right * 0.82).clamp(0.0, 1.0);
        let decayed_dbfs = (guard.dbfs - 2.2).clamp(-60.0, 0.0);

        let mut peak_left = raw.peak_left.max(decayed_peak_l);
        let mut peak_right = raw.peak_right.max(decayed_peak_r);
        let mut rms_left = raw.rms_left.max(decayed_rms_l);
        let mut rms_right = raw.rms_right.max(decayed_rms_r);
        let mut dbfs = raw.dbfs.max(decayed_dbfs);
        if peak_left < 0.004 && peak_right < 0.004 && dbfs <= -58.5 {
            peak_left = 0.0;
            peak_right = 0.0;
            rms_left = 0.0;
            rms_right = 0.0;
            dbfs = -60.0;
        }
        *guard = AudioLevelSnapshot {
            peak_left,
            peak_right,
            rms_left,
            rms_right,
            dbfs,
            active: peak_left > 0.004 || peak_right > 0.004,
            updated_at: raw.updated_at,
        };
    }

    /// Update audio levels explicitly (e.g., from volume/sink activity state on Linux).
    pub fn set_audio_levels(&self, snapshot: AudioLevelSnapshot) {
        *self.audio.write().unwrap() = snapshot;
    }

    pub fn snapshot(&self) -> TelemetrySnapshot {
        let audio = self.audio.read().unwrap().clone();
        let map = self.rtt_history.read().unwrap();
        let mut peers = BTreeMap::new();
        for (id, q) in map.iter() {
            if !q.is_empty() {
                peers.insert(id.clone(), compute_stats(id, q));
            }
        }
        TelemetrySnapshot { audio, peers }
    }
}

fn compute_stats(peer_id: &str, samples: &VecDeque<f64>) -> PeerNetStats {
    if samples.is_empty() {
        return PeerNetStats {
            peer_id: peer_id.to_string(),
            ..Default::default()
        };
    }
    let vec: Vec<f64> = samples.iter().copied().collect();
    let last = *vec.last().unwrap_or(&0.0);
    let min = vec.iter().copied().fold(f64::INFINITY, f64::min);
    let max = vec.iter().copied().fold(0.0_f64, f64::max);
    let sum: f64 = vec.iter().sum();
    let avg = sum / (vec.len() as f64);

    let jitter = if vec.len() >= 2 {
        let mut diff_sum = 0.0;
        for w in vec.windows(2) {
            diff_sum += (w[1] - w[0]).abs();
        }
        diff_sum / ((vec.len() - 1) as f64)
    } else {
        0.0
    };

    PeerNetStats {
        peer_id: peer_id.to_string(),
        last_rtt_ms: (last * 100.0).round() / 100.0,
        min_rtt_ms: (min * 100.0).round() / 100.0,
        max_rtt_ms: (max * 100.0).round() / 100.0,
        avg_rtt_ms: (avg * 100.0).round() / 100.0,
        jitter_ms: (jitter * 100.0).round() / 100.0,
        samples: vec,
        updated_at: now_secs(),
    }
}

/// Compute stereo Peak, RMS, and dBFS from interleaved 16-bit signed little-endian PCM.
pub fn compute_levels_s16le(pcm_bytes: &[u8]) -> AudioLevelSnapshot {
    let frame_bytes = 4; // 2 channels * 2 bytes
    let num_frames = pcm_bytes.len() / frame_bytes;
    if num_frames == 0 {
        return AudioLevelSnapshot {
            dbfs: -60.0,
            updated_at: now_secs(),
            ..Default::default()
        };
    }
    let mut peak_l = 0.0_f32;
    let mut peak_r = 0.0_f32;
    let mut sum_sq_l = 0.0_f64;
    let mut sum_sq_r = 0.0_f64;

    for chunk in pcm_bytes.chunks_exact(frame_bytes) {
        let l = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0;
        let r = i16::from_le_bytes([chunk[2], chunk[3]]) as f32 / 32768.0;
        let abs_l = l.abs();
        let abs_r = r.abs();
        if abs_l > peak_l {
            peak_l = abs_l;
        }
        if abs_r > peak_r {
            peak_r = abs_r;
        }
        sum_sq_l += (l as f64) * (l as f64);
        sum_sq_r += (r as f64) * (r as f64);
    }

    let rms_l = (sum_sq_l / (num_frames as f64)).sqrt() as f32;
    let rms_r = (sum_sq_r / (num_frames as f64)).sqrt() as f32;
    let combined_rms = ((rms_l + rms_r) * 0.5).max(1e-6);
    let dbfs = (20.0 * combined_rms.log10()).clamp(-60.0, 0.0);
    let active = peak_l > 0.002 || peak_r > 0.002;

    AudioLevelSnapshot {
        peak_left: peak_l.clamp(0.0, 1.0),
        peak_right: peak_r.clamp(0.0, 1.0),
        rms_left: rms_l.clamp(0.0, 1.0),
        rms_right: rms_r.clamp(0.0, 1.0),
        dbfs,
        active,
        updated_at: now_secs(),
    }
}

/// Live audio VU monitor (`parec` / `pacat --record` on Linux) and periodic TCP RTT/Jitter probe.
pub struct AudioTelemetryMonitor {
    stop_flag: Arc<AtomicBool>,
    active_child: Arc<Mutex<Option<Child>>>,
    audio_thread: Mutex<Option<JoinHandle<()>>>,
    net_thread: Mutex<Option<JoinHandle<()>>>,
}

impl AudioTelemetryMonitor {
    pub fn start(controller: Arc<Controller>) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let active_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

        if !controller.is_save_enabled() {
            return Self {
                stop_flag,
                active_child,
                audio_thread: Mutex::new(None),
                net_thread: Mutex::new(None),
            };
        }

        let stop_audio = Arc::clone(&stop_flag);
        let child_slot = Arc::clone(&active_child);
        let ctrl_audio = Arc::clone(&controller);
        let audio_thread = thread::spawn(move || {
            run_audio_capture_loop(ctrl_audio, stop_audio, child_slot);
        });

        let stop_net = Arc::clone(&stop_flag);
        let ctrl_net = Arc::clone(&controller);
        let net_thread = thread::spawn(move || {
            run_network_probe_loop(ctrl_net, stop_net);
        });

        Self {
            stop_flag,
            active_child,
            audio_thread: Mutex::new(Some(audio_thread)),
            net_thread: Mutex::new(Some(net_thread)),
        }
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.active_child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(h) = self.audio_thread.lock().unwrap().take() {
            let _ = h.join();
        }
        if let Some(h) = self.net_thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl Drop for AudioTelemetryMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn desired_monitor_source(controller: &Controller, force_default: bool) -> String {
    if !force_default {
        let st = controller.state.lock().unwrap();
        if st.is_connected() && !st.connected_sink.is_empty() {
            return format!("{}.monitor", st.connected_sink);
        }
    }
    "@DEFAULT_SINK@.monitor".to_string()
}

fn run_audio_capture_loop(
    controller: Arc<Controller>,
    stop_flag: Arc<AtomicBool>,
    active_child: Arc<Mutex<Option<Child>>>,
) {
    let use_parec = command_exists("parec");
    let use_pacat = !use_parec && command_exists("pacat");
    if !use_parec && !use_pacat {
        return;
    }

    let mut force_default = false;
    while !stop_flag.load(Ordering::SeqCst) {
        let monitor_source = desired_monitor_source(&controller, force_default);
        let mut cmd = if use_parec {
            Command::new("parec")
        } else {
            let mut c = Command::new("pacat");
            c.arg("--record");
            c
        };
        cmd.args([
            "--format=s16le",
            "--rate=16000",
            "--channels=2",
            "--latency-msec=30",
            "-d",
            &monitor_source,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

        let started = Instant::now();
        let Ok(mut child) = cmd.spawn() else {
            for _ in 0..10 {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            continue;
        };

        let stdout_opt = child.stdout.take();
        *active_child.lock().unwrap() = Some(child);

        if let Some(mut stdout) = stdout_opt {
            let mut buf = [0u8; 1600]; // ~25ms at 16kHz stereo s16le
            let mut chunks: u32 = 0;
            loop {
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        controller.telemetry.update_audio_from_s16le(&buf[..n]);
                        chunks = chunks.wrapping_add(1);
                        if chunks.is_multiple_of(20) {
                            let next_source = desired_monitor_source(&controller, false);
                            if next_source != monitor_source && !force_default {
                                break;
                            }
                        }
                    }
                }
            }
        }

        if let Some(mut c) = active_child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }

        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        if started.elapsed() < Duration::from_millis(300) {
            force_default = !force_default;
            for _ in 0..5 {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(80));
            }
        } else {
            force_default = false;
        }
    }
}

fn run_network_probe_loop(controller: Arc<Controller>, stop_flag: Arc<AtomicBool>) {
    while !stop_flag.load(Ordering::SeqCst) {
        controller.reload_from_disk();
        let st = controller.state.lock().unwrap().clone();
        let mut targets: Vec<(String, String, u16)> = Vec::new();
        let mut seen = HashSet::new();

        if st.is_connected()
            && !st.connected_host.is_empty()
            && st.connected_port > 0
            && is_private_address(&st.connected_host)
            && !is_own_non_loopback_ip(&st.connected_host)
        {
            let id = if st.connected_peer.is_empty() {
                st.connected_host.clone()
            } else {
                st.connected_peer.clone()
            };
            if seen.insert(id.clone()) {
                targets.push((id, st.connected_host.clone(), st.connected_port));
            }
        }

        for sess in &st.multiroom_peers {
            if !sess.host.is_empty()
                && sess.port > 0
                && is_private_address(&sess.host)
                && !is_own_non_loopback_ip(&sess.host)
                && seen.insert(sess.peer_id.clone())
            {
                targets.push((sess.peer_id.clone(), sess.host.clone(), sess.port));
            }
        }

        for kp in st.known_peers.values() {
            if targets.len() >= 4 {
                break;
            }
            if !kp.host.is_empty()
                && kp.port > 0
                && is_private_address(&kp.host)
                && !is_own_non_loopback_ip(&kp.host)
                && seen.insert(kp.id.clone())
            {
                targets.push((kp.id.clone(), kp.host.clone(), kp.port));
            }
        }

        for (peer_id, host, port) in targets {
            if stop_flag.load(Ordering::SeqCst) {
                return;
            }
            if let Some(rtt) = tcp_probe_rtt(&host, port, Duration::from_millis(500)) {
                controller.telemetry.record_rtt(&peer_id, rtt);
            }
        }

        for _ in 0..10 {
            if stop_flag.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rtt_and_jitter_calculation() {
        let tracker = TelemetryTracker::new();
        tracker.record_rtt("peer_a", 2.0);
        tracker.record_rtt("peer_a", 4.0);
        let stats = tracker.record_rtt("peer_a", 3.0);
        assert_eq!(stats.samples.len(), 3);
        assert_eq!(stats.min_rtt_ms, 2.0);
        assert_eq!(stats.max_rtt_ms, 4.0);
        assert_eq!(stats.avg_rtt_ms, 3.0);
        assert_eq!(stats.jitter_ms, 1.5);
    }

    #[test]
    fn test_s16le_level_meter() {
        let mut buf = Vec::new();
        for _ in 0..100 {
            buf.extend_from_slice(&16384_i16.to_le_bytes()); // 0.5 L
            buf.extend_from_slice(&(-16384_i16).to_le_bytes()); // 0.5 R
        }
        let snap = compute_levels_s16le(&buf);
        assert!(snap.active);
        assert!((snap.peak_left - 0.5).abs() < 0.01);
        assert!((snap.rms_right - 0.5).abs() < 0.01);
        assert!(snap.dbfs > -7.0 && snap.dbfs < -5.5);
    }

    #[test]
    fn test_audio_telemetry_monitor_lifecycle() {
        let ctrl = Arc::new(Controller::new(None, None, None, None, None, false));
        ctrl.set_save_enabled(false);
        let monitor = AudioTelemetryMonitor::start(ctrl);
        monitor.stop();
    }
}
