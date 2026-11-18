//! `soundhop-gui` — Native OS system tray (`tray-icon` + `muda`) and unified NUI Control Center (`eframe`/`egui`).

pub mod android_wizard;
pub mod app;
pub mod backend;
pub mod settings;
pub mod theme;
pub mod tray;
pub mod views;
pub mod widgets;

use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use soundhop_core::controller::Controller;

pub(crate) fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn read_or_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn write_or_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

pub use android_wizard::open_android_wizard;
pub use app::{open_control_center, open_control_center_with_toast, GuiTab};
pub use settings::open_settings_dialog;
pub use tray::run_tray;

/// Launch the unified SoundHop Control Center window on the given tab.
pub fn run_standalone_control_center(initial_tab: GuiTab) -> Result<i32, String> {
    run_standalone_control_center_with_toast(initial_tab, None)
}

/// Launch the unified SoundHop Control Center window with an optional startup toast.
pub fn run_standalone_control_center_with_toast(
    initial_tab: GuiTab,
    initial_toast: Option<String>,
) -> Result<i32, String> {
    let ctrl = Arc::new(Controller::new(None, None, None, None, None, true));
    open_control_center_with_toast(ctrl, initial_tab, initial_toast)?;
    Ok(0)
}

/// Helper to launch the Settings & Routing Rules window using a default Controller instance.
pub fn run_standalone_settings() -> Result<i32, String> {
    run_standalone_control_center(GuiTab::Settings)
}

/// Helper to launch the Android Audio & Wi-Fi ADB Wizard window.
pub fn run_standalone_android_wizard() -> Result<i32, String> {
    run_standalone_control_center(GuiTab::Phone)
}
