//! Android tool status, device discovery, and connection history helpers.

use crate::util::{lock_or_recover, now_secs};

use super::Controller;

impl Controller {
    pub fn android_tool_status(&self) -> serde_json::Value {
        let st = self.android_manager.get_tool_status();
        serde_json::json!({
            "scrcpy_path": st.scrcpy_path,
            "scrcpy_version": st.scrcpy_version,
            "adb_path": st.adb_path,
            "adb_version": st.adb_version,
            "is_managed": st.is_managed,
            "ready": st.ready(),
        })
    }

    pub fn android_list_devices(&self) -> Vec<serde_json::Value> {
        let devs = self.android_manager.list_devices();
        let now = now_secs();
        {
            let mut st = lock_or_recover(&self.state);
            for d in &devs {
                st.remember_android_device(
                    &d.serial,
                    &d.model,
                    &d.manufacturer,
                    if d.is_wifi { &d.ip } else { "" },
                    if d.is_wifi { d.port } else { 5555 },
                    d.is_wifi,
                    false,
                    Some(now),
                );
            }
        }
        self.save();
        devs.into_iter()
            .filter_map(|d| serde_json::to_value(d).ok())
            .collect()
    }

    pub fn android_history(&self) -> Vec<serde_json::Value> {
        let st = lock_or_recover(&self.state);
        let mut devs: Vec<_> = st.known_android_devices.values().cloned().collect();
        devs.sort_by(|a, b| {
            b.last_connected
                .max(b.last_seen)
                .partial_cmp(&a.last_connected.max(a.last_seen))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        devs.into_iter()
            .filter_map(|d| serde_json::to_value(d).ok())
            .collect()
    }
}
