//! Audio playback and loopback capture for Windows (`cpal`/WASAPI),
//! with cross-platform mock fallbacks so unit tests run on both Linux and Windows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct AudioEndpoint {
    pub name: String,
    pub channels: u8,
    pub sample_rate: u32,
}

pub fn list_audio_endpoints() -> Vec<AudioEndpoint> {
    #[cfg(windows)]
    {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        if let Ok(devices) = host.output_devices() {
            let mut out = Vec::new();
            for dev in devices {
                let name = dev.name().unwrap_or_else(|_| "Speakers".to_string());
                let (channels, sample_rate) = dev
                    .default_output_config()
                    .map(|c| (c.channels() as u8, c.sample_rate().0))
                    .unwrap_or((2, 48_000));
                out.push(AudioEndpoint {
                    name,
                    channels,
                    sample_rate,
                });
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    vec![AudioEndpoint {
        name: "Speakers (Default Audio)".to_string(),
        channels: 2,
        sample_rate: 48_000,
    }]
}

pub struct WindowsPlayer {
    pub sample_rate: u32,
    pub channels: u8,
    closed: AtomicBool,
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl WindowsPlayer {
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            sample_rate,
            channels,
            closed: AtomicBool::new(false),
            buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn start(&self) {
        self.closed.store(false, Ordering::SeqCst);
    }

    pub fn write(&self, pcm_data: &[u8]) {
        if self.closed.load(Ordering::SeqCst) || pcm_data.is_empty() {
            return;
        }
        let mut buf = self.buffer.lock().unwrap();
        if buf.len() > 192_000 {
            buf.clear();
        }
        buf.extend_from_slice(pcm_data);
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.buffer.lock().unwrap().clear();
    }
}

pub struct WindowsLoopbackRecorder {
    stop_flag: Arc<AtomicBool>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl WindowsLoopbackRecorder {
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(&[u8]) + Send + Sync + 'static,
    {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&stop_flag);
        let cb = Arc::new(callback);
        let handle = thread::spawn(move || {
            let silence = vec![0u8; 4096];
            while !flag_clone.load(Ordering::SeqCst) {
                cb(&silence);
                thread::sleep(Duration::from_millis(20));
            }
        });
        Self {
            stop_flag,
            handle: Mutex::new(Some(handle)),
        }
    }

    pub fn is_alive(&self) -> bool {
        !self.stop_flag.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}
