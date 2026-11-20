#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    std::process::exit(soundhop_cli::run_gui_binary());
}
