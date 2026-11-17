//! Compatibility wrapper forwarding legacy Settings dialog entrypoints to the unified Control Center.

use std::sync::Arc;

use soundhop_core::controller::Controller;
use soundhop_core::telemetry::TelemetryTracker;

use crate::app::{open_control_center, GuiTab};

pub fn open_settings_dialog(
    controller: Arc<Controller>,
    _telemetry: Option<Arc<TelemetryTracker>>,
) -> Result<(), String> {
    open_control_center(controller, GuiTab::Settings)
}
