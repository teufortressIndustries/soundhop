#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    soundhop_core::transport::init_bundled_gst_env();
    std::process::exit(soundhop_cli::run_gui_binary());
}
