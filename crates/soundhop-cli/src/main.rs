fn main() {
    soundhop_core::transport::init_bundled_gst_env();
    std::process::exit(soundhop_cli::run_cli());
}
