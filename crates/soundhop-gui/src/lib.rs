//! `soundhop-gui` — Native OS system tray (`tray-icon` + `muda`) and `eframe`/`egui` dialogs.

pub mod android_wizard;
pub mod settings;
pub mod tray;

use std::sync::Arc;

use soundhop_core::controller::Controller;

pub use android_wizard::open_android_wizard;
pub use settings::open_settings_dialog;
pub use tray::run_tray;

/// Helper to launch the Settings & Routing Rules window using a default Controller instance.
pub fn run_standalone_settings() -> Result<i32, String> {
    let ctrl = Arc::new(Controller::new(None, None, None, None, None, true));
    let telem = Arc::clone(&ctrl.telemetry);
    open_settings_dialog(ctrl, Some(telem))?;
    Ok(0)
}

/// Helper to launch the Android Audio & Wi-Fi ADB Wizard window.
pub fn run_standalone_android_wizard() -> Result<i32, String> {
    let ctrl = Arc::new(Controller::new(None, None, None, None, None, true));
    open_android_wizard(ctrl)?;
    Ok(0)
}
