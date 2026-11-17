//! Compatibility wrapper forwarding legacy Android Wizard entrypoints to the unified Control Center.

use std::sync::Arc;

use soundhop_core::controller::Controller;

use crate::app::{open_control_center, GuiTab};

pub fn open_android_wizard(controller: Arc<Controller>) -> Result<(), String> {
    open_control_center(controller, GuiTab::Phone)
}
