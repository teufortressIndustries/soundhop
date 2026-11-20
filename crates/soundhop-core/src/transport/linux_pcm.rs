//! Linux PulseAudio/PipeWire PCM capture (`parec`) and playback (`pacat`) bridges
//! for the SHT3 audio transport pipeline.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use crate::transport::dsp::apply_gain_i16;
use crate::transport::pcm::{PcmCapture, PcmPlayback, PCM_CHANNELS, PCM_SAMPLE_RATE};
use crate::transport::{TransportError, TransportResult};

const DEFAULT_LATENCY_MS: u32 = 20;
const DEFAULT_PLAYBACK_LATENCY_MS: u32 = 60;

/// Captures 48 kHz stereo S16LE PCM from a PulseAudio/PipeWire monitor source via `parec`.
///
/// The child `parec` process is terminated and reaped automatically on [`Drop`].
pub struct LinuxPcmCapture {
    monitor_source: String,
    child: Child,
    stdout: ChildStdout,
    byte_buf: Vec<u8>,
}

impl LinuxPcmCapture {
    /// Spawn `parec --raw --format=s16le --rate=48000 --channels=2 --latency-msec=20 -d <monitor_source>`.
    pub fn spawn(monitor_source: impl Into<String>) -> TransportResult<Self> {
        Self::spawn_with_latency(monitor_source, DEFAULT_LATENCY_MS)
    }

    /// Spawn `parec` with a custom `--latency-msec` value.
    pub fn spawn_with_latency(
        monitor_source: impl Into<String>,
        latency_ms: u32,
    ) -> TransportResult<Self> {
        let monitor_source = monitor_source.into();
        let args = parec_args(&monitor_source, latency_ms);
        let mut cmd = Command::new("parec");
        cmd.args(&args);
        Self::spawn_command(monitor_source, cmd)
    }

    fn spawn_command(monitor_source: String, mut cmd: Command) -> TransportResult<Self> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TransportError::Io(std::io::Error::other(
                "failed to capture parec stdout pipe",
            )));
        };

        Ok(Self {
            monitor_source,
            child,
            stdout,
            byte_buf: Vec::new(),
        })
    }

    /// Name of the monitor source being captured.
    pub fn monitor_source(&self) -> &str {
        &self.monitor_source
    }
}

impl PcmCapture for LinuxPcmCapture {
    fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let target_bytes = buf.len() * 2;
        if self.byte_buf.len() != target_bytes {
            self.byte_buf.resize(target_bytes, 0);
        }

        let mut filled = 0;
        while filled < target_bytes {
            match self.stdout.read(&mut self.byte_buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(TransportError::Io(e)),
            }
        }

        let samples = filled / 2;
        for (i, sample) in buf.iter_mut().take(samples).enumerate() {
            let idx = i * 2;
            *sample = i16::from_le_bytes([self.byte_buf[idx], self.byte_buf[idx + 1]]);
        }
        Ok(samples)
    }
}

impl Drop for LinuxPcmCapture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Plays 48 kHz stereo S16LE PCM to PulseAudio/PipeWire via `pacat --playback`,
/// applying live software volume and mute via [`apply_gain_i16`].
///
/// The child `pacat` process is terminated and reaped automatically on [`Drop`].
pub struct LinuxPcmPlayback {
    child: Child,
    stdin: Option<ChildStdin>,
    volume_pct: Arc<AtomicU8>,
    muted: Arc<AtomicBool>,
    byte_buf: Vec<u8>,
}

impl LinuxPcmPlayback {
    /// Spawn `pacat --playback --raw --format=s16le --rate=48000 --channels=2 --latency-msec=60`.
    pub fn spawn() -> TransportResult<Self> {
        Self::spawn_for_sink(None, DEFAULT_PLAYBACK_LATENCY_MS)
    }

    /// Spawn `pacat --playback` targeting an optional PulseAudio/PipeWire sink and latency.
    pub fn spawn_for_sink(sink: Option<&str>, latency_ms: u32) -> TransportResult<Self> {
        let args = pacat_args(sink, latency_ms);
        let mut cmd = Command::new("pacat");
        cmd.args(&args);
        Self::spawn_command(cmd)
    }

    fn spawn_command(mut cmd: Command) -> TransportResult<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TransportError::Io(std::io::Error::other(
                "failed to open pacat stdin pipe",
            )));
        };

        Ok(Self {
            child,
            stdin: Some(stdin),
            volume_pct: Arc::new(AtomicU8::new(100)),
            muted: Arc::new(AtomicBool::new(false)),
            byte_buf: Vec::new(),
        })
    }
}

impl PcmPlayback for LinuxPcmPlayback {
    fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(TransportError::Closed);
        };

        let vol = u32::from(self.volume_pct.load(Ordering::Acquire));
        let muted = self.muted.load(Ordering::Acquire);

        self.byte_buf.clear();
        self.byte_buf.reserve(samples.len() * 2);
        for &s in samples {
            let scaled = apply_gain_i16(s, vol, muted);
            self.byte_buf.extend_from_slice(&scaled.to_le_bytes());
        }

        stdin.write_all(&self.byte_buf)?;
        Ok(())
    }

    fn set_volume(&self, pct: u8) {
        self.volume_pct.store(pct.min(150), Ordering::Release);
    }

    fn set_mute(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    fn volume(&self) -> u8 {
        self.volume_pct.load(Ordering::Acquire)
    }

    fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }
}

impl Drop for LinuxPcmPlayback {
    fn drop(&mut self) {
        // Close stdin first so pacat observes EOF, then force-kill and reap.
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parec_args(monitor_source: &str, latency_ms: u32) -> Vec<String> {
    vec![
        "--raw".to_string(),
        "--format=s16le".to_string(),
        format!("--rate={PCM_SAMPLE_RATE}"),
        format!("--channels={PCM_CHANNELS}"),
        format!("--latency-msec={}", latency_ms.max(1)),
        "-d".to_string(),
        monitor_source.to_string(),
    ]
}

fn pacat_args(sink: Option<&str>, latency_ms: u32) -> Vec<String> {
    let mut args = vec![
        "--playback".to_string(),
        "--raw".to_string(),
        "--format=s16le".to_string(),
        format!("--rate={PCM_SAMPLE_RATE}"),
        format!("--channels={PCM_CHANNELS}"),
        format!("--latency-msec={}", latency_ms.max(1)),
    ];
    if let Some(sink_name) = sink {
        args.push("-d".to_string());
        args.push(sink_name.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parec_and_pacat_cli_arguments_match_canonical_format() {
        assert_eq!(
            parec_args("soundhop_peer.monitor", 20),
            vec![
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=20",
                "-d",
                "soundhop_peer.monitor",
            ]
        );
        assert_eq!(
            pacat_args(None, 20),
            vec![
                "--playback",
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=20",
            ]
        );
        assert_eq!(
            pacat_args(Some("alsa_output.pci"), 10),
            vec![
                "--playback",
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=10",
                "-d",
                "alsa_output.pci",
            ]
        );
    }

    #[test]
    fn linux_capture_reads_s16le_from_subprocess_and_reaps_on_drop() {
        // Emit 4 little-endian i16 samples via printf (1, -1, 1000, -1000).
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(r"printf '\001\000\377\377\350\003\030\374'");
        let mut cap = LinuxPcmCapture::spawn_command("test.monitor".into(), cmd).unwrap();
        assert_eq!(cap.monitor_source(), "test.monitor");

        let mut buf = [0i16; 4];
        let n = cap.read_frame(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(buf, [1, -1, 1000, -1000]);

        // Subsequent read reaches EOF cleanly.
        assert_eq!(cap.read_frame(&mut buf).unwrap(), 0);
    }

    #[test]
    fn linux_playback_applies_gain_and_mute_to_subprocess_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("pcm.raw");

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!("cat > '{}'", out_path.display()));

        let mut play = LinuxPcmPlayback::spawn_command(cmd).unwrap();
        play.set_volume(50);
        play.write_frame(&[1000, -2000]).unwrap();
        play.set_mute(true);
        play.write_frame(&[1000, -2000]).unwrap();

        // Close stdin and wait for cat to flush before reading output file.
        drop(play.stdin.take());
        let _ = play.child.wait();

        let bytes = std::fs::read(&out_path).unwrap();
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        assert_eq!(samples, vec![500, -1000, 0, 0]);
    }
}
