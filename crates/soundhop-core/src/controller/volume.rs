//! Volume management, remote volume synchronization, and status volume telemetry.

use std::collections::HashSet;

use crate::state::State;
use crate::telemetry::AudioLevelSnapshot;
use crate::util::{lock_or_recover, now_secs};

use super::Controller;

impl Controller {
    fn broadcast_remote_volume(&self, st: &State, vol: u32, mute: bool) {
        let sht3_peer_ids: HashSet<String> = {
            let guard = lock_or_recover(&self.sht3_outbounds);
            for handle in guard.values() {
                if handle.is_running() {
                    let _ = handle.set_volume(vol.min(150) as u8);
                    let _ = handle.set_mute(mute);
                }
            }
            guard.keys().cloned().collect()
        };
        if !st.multiroom_peers.is_empty() {
            for sess in &st.multiroom_peers {
                if !sht3_peer_ids.contains(&sess.peer_id) {
                    self.backend
                        .remote_set_volume(&sess.host, sess.port, vol, Some(mute));
                }
            }
        } else if !sht3_peer_ids.contains(&st.connected_peer) {
            self.backend
                .remote_set_volume(&st.connected_host, st.connected_port, vol, Some(mute));
        }
    }
    pub(super) fn status_volume_json(&self, st: &State) -> serde_json::Value {
        if !self.backend.available() {
            return serde_json::Value::Null;
        }
        let active_sink = if !st.connected_sink.is_empty() {
            st.connected_sink.clone()
        } else {
            self.backend.default_sink().unwrap_or_default()
        };
        let lvl = self.backend.get_sink_volume(&active_sink);
        let mute = self.backend.get_sink_mute(&active_sink);
        // Update synthetic audio level meter only if parec telemetry has not written live PCM levels recently
        let now = now_secs();
        let existing_audio = self.telemetry.snapshot().audio;
        if now - existing_audio.updated_at > 1.5 {
            let any_stream = !self.backend.sink_inputs().unwrap_or_default().is_empty();
            let norm = if mute || !any_stream {
                0.0
            } else {
                (lvl as f32 / 100.0).clamp(0.0, 1.0) * 0.75
            };
            self.telemetry.set_audio_levels(AudioLevelSnapshot {
                peak_left: norm,
                peak_right: norm * 0.96,
                rms_left: norm * 0.7,
                rms_right: norm * 0.68,
                dbfs: if norm > 0.001 {
                    20.0 * norm.log10()
                } else {
                    -60.0
                },
                active: any_stream && !mute,
                updated_at: now,
            });
        }
        serde_json::json!({
            "level": lvl,
            "mute": mute,
            "sink": active_sink,
        })
    }

    pub fn get_volume(&self) -> serde_json::Value {
        let st = lock_or_recover(&self.state).clone();
        let sink = if !st.connected_sink.is_empty() {
            st.connected_sink.clone()
        } else {
            self.backend.default_sink().unwrap_or_default()
        };
        let vol = self.backend.get_sink_volume(&sink);
        let mute = self.backend.get_sink_mute(&sink);
        serde_json::json!({
            "volume": vol,
            "mute": mute,
            "sink": sink,
            "connected": st.is_connected(),
        })
    }

    pub fn set_volume(
        &self,
        volume: Option<u32>,
        mute: Option<bool>,
        sync_remote: bool,
    ) -> serde_json::Value {
        let st = lock_or_recover(&self.state).clone();
        let sink = if !st.connected_sink.is_empty() {
            st.connected_sink.clone()
        } else {
            self.backend.default_sink().unwrap_or_default()
        };
        if let Some(v) = volume {
            self.backend.set_sink_volume(&sink, v);
        }
        if let Some(m) = mute {
            self.backend.set_sink_mute(&sink, m);
        }
        if sync_remote && st.is_connected() && !st.connected_host.is_empty() {
            let cur_vol = volume.unwrap_or_else(|| self.backend.get_sink_volume(&sink));
            let cur_mute = mute.unwrap_or_else(|| self.backend.get_sink_mute(&sink));
            self.broadcast_remote_volume(&st, cur_vol, cur_mute);
            let mut guard = lock_or_recover(&self.state);
            guard.last_synced_volume = cur_vol as i32;
            guard.last_synced_mute = cur_mute;
        }
        self.get_volume()
    }

    pub fn sync_remote_volume(&self) -> bool {
        let st = lock_or_recover(&self.state).clone();
        if !st.is_connected() || st.connected_host.is_empty() {
            return false;
        }
        let sink = if !st.connected_sink.is_empty() {
            st.connected_sink.clone()
        } else {
            self.backend.default_sink().unwrap_or_default()
        };
        let vol = self.backend.get_sink_volume(&sink);
        let mute = self.backend.get_sink_mute(&sink);
        if vol as i32 == st.last_synced_volume && mute == st.last_synced_mute {
            return false;
        }
        self.broadcast_remote_volume(&st, vol, mute);
        let mut guard = lock_or_recover(&self.state);
        guard.last_synced_volume = vol as i32;
        guard.last_synced_mute = mute;
        true
    }
}
