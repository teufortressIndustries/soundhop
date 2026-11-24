//! Linux PulseAudio/PipeWire PCM capture (`parec` / `pacat --record`) and playback (`pacat`)
//! bridges for the SHT3 audio transport pipeline.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::transport::dsp::apply_gain_i16;
use crate::transport::pcm::{PcmCapture, PcmPlayback, PCM_CHANNELS, PCM_SAMPLE_RATE};
use crate::transport::{TransportError, TransportResult};

const DEFAULT_LATENCY_MS: u32 = 20;
const DEFAULT_PLAYBACK_LATENCY_MS: u32 = 60;
const CAPTURE_POLL_TIMEOUT: Duration = Duration::from_millis(45);
const CAPTURE_POLL_SLICE_MS: i32 = 10;
const MAX_CAPTURE_RESPAWN_ATTEMPTS: usize = 3;
const RESPAWN_BACKOFF: Duration = Duration::from_millis(100);
const GAIN_RAMP_MAX_FRAMES: usize = 192;

enum CaptureSpawner {
    #[cfg(test)]
    None,
    Pulse {
        latency_ms: u32,
    },
    #[cfg(test)]
    Custom(Box<dyn FnMut() -> Command + Send>),
}

/// Captures 48 kHz stereo S16LE PCM from a PulseAudio/PipeWire monitor source via `parec`
/// (with automatic fallback to `pacat --record` when `parec` is absent).
///
/// The child process is terminated and reaped automatically on [`Drop`].
pub struct LinuxPcmCapture {
    monitor_source: String,
    spawner: CaptureSpawner,
    child: Child,
    stdout: ChildStdout,
    byte_buf: Vec<u8>,
    leftover: [u8; 4],
    leftover_len: usize,
    stop_flag: Option<Arc<AtomicBool>>,
    discontinuity: bool,
}

impl LinuxPcmCapture {
    /// Spawn `parec` (or `pacat --record` fallback) at default 20 ms latency.
    pub fn spawn(monitor_source: impl Into<String>) -> TransportResult<Self> {
        Self::spawn_with_latency(monitor_source, DEFAULT_LATENCY_MS)
    }

    /// Spawn `parec` (or `pacat --record` fallback) with a custom `--latency-msec` value.
    pub fn spawn_with_latency(
        monitor_source: impl Into<String>,
        latency_ms: u32,
    ) -> TransportResult<Self> {
        let monitor_source = monitor_source.into();
        let cmd = build_pulse_capture_command(&monitor_source, latency_ms);
        Self::spawn_internal(monitor_source, CaptureSpawner::Pulse { latency_ms }, cmd)
    }

    #[cfg(test)]
    fn spawn_command(monitor_source: String, cmd: Command) -> TransportResult<Self> {
        Self::spawn_internal(monitor_source, CaptureSpawner::None, cmd)
    }

    #[cfg(test)]
    fn spawn_with_spawner(
        monitor_source: String,
        mut make_cmd: impl FnMut() -> Command + Send + 'static,
    ) -> TransportResult<Self> {
        let cmd = make_cmd();
        Self::spawn_internal(
            monitor_source,
            CaptureSpawner::Custom(Box::new(make_cmd)),
            cmd,
        )
    }

    fn spawn_internal(
        monitor_source: String,
        spawner: CaptureSpawner,
        cmd: Command,
    ) -> TransportResult<Self> {
        let (child, stdout) = spawn_capture_child(cmd)?;
        Ok(Self {
            monitor_source,
            spawner,
            child,
            stdout,
            byte_buf: Vec::new(),
            leftover: [0; 4],
            leftover_len: 0,
            stop_flag: None,
            discontinuity: false,
        })
    }

    /// Name of the monitor source being captured.
    pub fn monitor_source(&self) -> &str {
        &self.monitor_source
    }

    fn is_stopped(&self) -> bool {
        self.stop_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    fn try_respawn(&mut self) -> bool {
        #[cfg(test)]
        if matches!(self.spawner, CaptureSpawner::None) {
            return false;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();

        for _ in 0..MAX_CAPTURE_RESPAWN_ATTEMPTS {
            if self.sleep_interruptible(RESPAWN_BACKOFF) {
                return false;
            }
            let cmd = match &mut self.spawner {
                #[cfg(test)]
                CaptureSpawner::None => return false,
                CaptureSpawner::Pulse { latency_ms } => {
                    build_pulse_capture_command(&self.monitor_source, *latency_ms)
                }
                #[cfg(test)]
                CaptureSpawner::Custom(f) => f(),
            };
            if let Ok((child, stdout)) = spawn_capture_child(cmd) {
                self.child = child;
                self.stdout = stdout;
                self.leftover_len = 0;
                self.discontinuity = true;
                return true;
            }
        }
        false
    }

    fn sleep_interruptible(&self, delay: Duration) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            if self.is_stopped() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(10)),
            );
        }
    }
}

impl PcmCapture for LinuxPcmCapture {
    fn attach_stop_flag(&mut self, stop_flag: Arc<AtomicBool>) {
        self.stop_flag = Some(stop_flag);
    }

    fn take_discontinuity(&mut self) -> bool {
        std::mem::replace(&mut self.discontinuity, false)
    }

    fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize> {
        if buf.is_empty() || self.is_stopped() {
            return Ok(0);
        }
        let target_bytes = buf.len() * 2;
        if self.byte_buf.len() != target_bytes {
            self.byte_buf.resize(target_bytes, 0);
        }

        let mut filled = self.leftover_len.min(target_bytes);
        if filled > 0 {
            self.byte_buf[..filled].copy_from_slice(&self.leftover[..filled]);
            self.leftover_len = 0;
        }

        let mut deadline = Instant::now() + CAPTURE_POLL_TIMEOUT;
        let mut timed_out = false;

        while filled < target_bytes {
            if self.is_stopped() {
                return Ok(0);
            }
            match self.stdout.read(&mut self.byte_buf[filled..]) {
                Ok(0) => {
                    if filled >= 2 {
                        break;
                    }
                    if self.try_respawn() {
                        filled = 0;
                        deadline = Instant::now() + CAPTURE_POLL_TIMEOUT;
                        continue;
                    }
                    return Ok(0);
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    let now = Instant::now();
                    if now >= deadline {
                        if let Ok(Some(_)) = self.child.try_wait() {
                            if filled >= 2 {
                                break;
                            }
                            if self.try_respawn() {
                                filled = 0;
                                deadline = Instant::now() + CAPTURE_POLL_TIMEOUT;
                                continue;
                            }
                            return Ok(0);
                        }
                        timed_out = true;
                        break;
                    }
                    let rem_ms = deadline
                        .saturating_duration_since(now)
                        .as_millis()
                        .clamp(1, i32::MAX as u128) as i32;
                    let wait_ms = rem_ms.min(CAPTURE_POLL_SLICE_MS);
                    let mut pfd = libc::pollfd {
                        fd: self.stdout.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let rc = unsafe { libc::poll(&mut pfd, 1, wait_ms) };
                    if rc < 0 {
                        let err = std::io::Error::last_os_error();
                        if err.kind() == ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(TransportError::Io(err));
                    }
                }
                Err(e) => return Err(TransportError::Io(e)),
            }
        }

        if timed_out {
            let align = if buf.len() >= 2 && buf.len().is_multiple_of(2) {
                4
            } else {
                2
            };
            let usable = filled - (filled % align);
            let rem = filled - usable;
            if rem > 0 {
                self.leftover[..rem].copy_from_slice(&self.byte_buf[usable..filled]);
                self.leftover_len = rem;
            }
            let samples = usable / 2;
            decode_le_samples(&self.byte_buf[..usable], &mut buf[..samples]);
            buf[samples..].fill(0);
            return Ok(buf.len());
        }

        let samples = filled / 2;
        if !filled.is_multiple_of(2) {
            self.leftover[0] = self.byte_buf[filled - 1];
            self.leftover_len = 1;
        }
        decode_le_samples(&self.byte_buf[..samples * 2], &mut buf[..samples]);
        Ok(samples)
    }
}

impl Drop for LinuxPcmCapture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn decode_le_samples(bytes: &[u8], out: &mut [i16]) {
    for (&pair, slot) in bytes.as_chunks::<2>().0.iter().zip(out.iter_mut()) {
        *slot = i16::from_le_bytes(pair);
    }
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if (flags & libc::O_NONBLOCK) == 0 {
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn spawn_capture_child(mut cmd: Command) -> TransportResult<(Child, ChildStdout)> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    crate::util::close_fds_pre_exec(&mut cmd);

    let mut child = cmd.spawn()?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(TransportError::Io(std::io::Error::other(
            "failed to capture parec stdout pipe",
        )));
    };
    if let Err(err) = set_nonblocking(stdout.as_raw_fd()) {
        drop(stdout);
        let _ = child.kill();
        let _ = child.wait();
        return Err(TransportError::Io(err));
    }
    Ok((child, stdout))
}

enum PlaybackSpawner {
    #[cfg(test)]
    None,
    Pacat {
        sink: Option<String>,
        latency_ms: u32,
    },
    #[cfg(test)]
    Custom(Box<dyn FnMut() -> Command + Send>),
}

/// Plays 48 kHz stereo S16LE PCM to PulseAudio/PipeWire via `pacat --playback`,
/// applying smooth software gain ramping and automatic `BrokenPipe` recovery.
///
/// The child `pacat` process is terminated and reaped automatically on [`Drop`].
pub struct LinuxPcmPlayback {
    sink: Option<String>,
    latency_ms: u32,
    spawner: PlaybackSpawner,
    child: Child,
    stdin: Option<ChildStdin>,
    volume_pct: Arc<AtomicU8>,
    muted: Arc<AtomicBool>,
    current_gain: f32,
    byte_buf: Vec<u8>,
}

impl LinuxPcmPlayback {
    /// Spawn `pacat --playback --raw --format=s16le --rate=48000 --channels=2 --latency-msec=60`.
    pub fn spawn() -> TransportResult<Self> {
        Self::spawn_for_sink(None, DEFAULT_PLAYBACK_LATENCY_MS)
    }

    /// Spawn `pacat --playback` targeting an optional PulseAudio/PipeWire sink and latency.
    pub fn spawn_for_sink(sink: Option<&str>, latency_ms: u32) -> TransportResult<Self> {
        let sink_owned = sink.map(str::to_string);
        let cmd = build_pacat_command(sink_owned.as_deref(), latency_ms);
        Self::spawn_internal(
            sink_owned.clone(),
            latency_ms,
            PlaybackSpawner::Pacat {
                sink: sink_owned,
                latency_ms,
            },
            cmd,
        )
    }

    #[cfg(test)]
    fn spawn_command(cmd: Command) -> TransportResult<Self> {
        Self::spawn_internal(
            None,
            DEFAULT_PLAYBACK_LATENCY_MS,
            PlaybackSpawner::None,
            cmd,
        )
    }

    #[cfg(test)]
    fn spawn_with_spawner(
        mut make_cmd: impl FnMut() -> Command + Send + 'static,
    ) -> TransportResult<Self> {
        let cmd = make_cmd();
        Self::spawn_internal(
            None,
            DEFAULT_PLAYBACK_LATENCY_MS,
            PlaybackSpawner::Custom(Box::new(make_cmd)),
            cmd,
        )
    }

    fn spawn_internal(
        sink: Option<String>,
        latency_ms: u32,
        spawner: PlaybackSpawner,
        cmd: Command,
    ) -> TransportResult<Self> {
        let (child, stdin) = spawn_playback_child(cmd)?;
        Ok(Self {
            sink,
            latency_ms,
            spawner,
            child,
            stdin: Some(stdin),
            volume_pct: Arc::new(AtomicU8::new(100)),
            muted: Arc::new(AtomicBool::new(false)),
            current_gain: 1.0,
            byte_buf: Vec::new(),
        })
    }

    fn try_respawn(&mut self) -> TransportResult<bool> {
        let cmd = match &mut self.spawner {
            #[cfg(test)]
            PlaybackSpawner::None => return Ok(false),
            PlaybackSpawner::Pacat { sink, latency_ms } => {
                build_pacat_command(sink.as_deref(), *latency_ms)
            }
            #[cfg(test)]
            PlaybackSpawner::Custom(f) => f(),
        };

        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();

        let (child, stdin) = spawn_playback_child(cmd)?;
        self.child = child;
        self.stdin = Some(stdin);
        Ok(true)
    }

    /// Configured target PulseAudio/PipeWire sink name, if any.
    pub fn sink(&self) -> Option<&str> {
        self.sink.as_deref()
    }

    /// Configured playback latency in milliseconds.
    pub fn latency_ms(&self) -> u32 {
        self.latency_ms
    }
}

impl PcmPlayback for LinuxPcmPlayback {
    fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()> {
        if samples.is_empty() {
            return Ok(());
        }
        if self.stdin.is_none() {
            return Err(TransportError::Closed);
        }

        let vol = u32::from(self.volume_pct.load(Ordering::Acquire)).min(150);
        let muted = self.muted.load(Ordering::Acquire);
        encode_ramped_s16le(
            samples,
            vol,
            muted,
            &mut self.current_gain,
            &mut self.byte_buf,
        );

        let write_res = match self.stdin.as_mut() {
            Some(stdin) => stdin.write_all(&self.byte_buf),
            None => return Err(TransportError::Closed),
        };
        match write_res {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                if self.try_respawn()? {
                    let Some(stdin) = self.stdin.as_mut() else {
                        return Err(TransportError::Closed);
                    };
                    stdin.write_all(&self.byte_buf)?;
                    Ok(())
                } else {
                    Err(TransportError::Io(e))
                }
            }
            Err(e) => Err(TransportError::Io(e)),
        }
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

fn spawn_playback_child(mut cmd: Command) -> TransportResult<(Child, ChildStdin)> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    crate::util::close_fds_pre_exec(&mut cmd);

    let mut child = cmd.spawn()?;
    let Some(stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(TransportError::Io(std::io::Error::other(
            "failed to open pacat stdin pipe",
        )));
    };
    Ok((child, stdin))
}

fn build_pulse_capture_command(monitor_source: &str, latency_ms: u32) -> Command {
    let has_parec = crate::util::command_exists("parec");
    let has_pacat = !has_parec && crate::util::command_exists("pacat");
    let (program, args) = select_capture_program(has_parec, has_pacat, monitor_source, latency_ms);
    let mut cmd = Command::new(program);
    cmd.args(&args);
    cmd
}

fn build_pacat_command(sink: Option<&str>, latency_ms: u32) -> Command {
    let args = pacat_args(sink, latency_ms);
    let mut cmd = Command::new("pacat");
    cmd.args(&args);
    cmd
}

fn select_capture_program(
    has_parec: bool,
    has_pacat: bool,
    monitor_source: &str,
    latency_ms: u32,
) -> (&'static str, Vec<String>) {
    if has_parec || !has_pacat {
        ("parec", parec_args(monitor_source, latency_ms))
    } else {
        ("pacat", pacat_record_args(monitor_source, latency_ms))
    }
}

#[inline]
fn target_gain_factor(volume_pct: u32, muted: bool) -> f32 {
    if muted || volume_pct == 0 {
        0.0
    } else {
        (volume_pct.min(150) as f32) / 100.0
    }
}

#[inline]
fn scale_sample_f32(sample: i16, gain: f32) -> i16 {
    if gain <= 0.0 {
        return 0;
    }
    (f32::from(sample) * gain)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

fn encode_ramped_s16le(
    samples: &[i16],
    volume_pct: u32,
    muted: bool,
    current_gain: &mut f32,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.reserve(samples.len() * 2);

    let target = target_gain_factor(volume_pct, muted);
    if *current_gain == target {
        if muted || volume_pct == 0 {
            out.resize(samples.len() * 2, 0);
        } else if volume_pct == 100 {
            for &s in samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
        } else {
            for &s in samples {
                let scaled = apply_gain_i16(s, volume_pct, false);
                out.extend_from_slice(&scaled.to_le_bytes());
            }
        }
        return;
    }

    let start = *current_gain;
    let stereo_frames = samples.len() / 2;
    if stereo_frames == 0 {
        let scaled = scale_sample_f32(samples[0], target);
        out.extend_from_slice(&scaled.to_le_bytes());
        *current_gain = target;
        return;
    }

    let ramp_frames = stereo_frames.min(GAIN_RAMP_MAX_FRAMES);
    for (frame_idx, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
        let gain = if frame_idx < ramp_frames {
            let t = (frame_idx + 1) as f32 / (ramp_frames as f32);
            start + (target - start) * t
        } else {
            target
        };
        let l = scale_sample_f32(pair[0], gain);
        let r = scale_sample_f32(pair[1], gain);
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    if !samples.len().is_multiple_of(2) {
        let s = scale_sample_f32(samples[samples.len() - 1], target);
        out.extend_from_slice(&s.to_le_bytes());
    }
    *current_gain = target;
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

fn pacat_record_args(monitor_source: &str, latency_ms: u32) -> Vec<String> {
    vec![
        "--record".to_string(),
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

    #[test]
    fn select_capture_program_falls_back_to_pacat_record() {
        let (prog, args) = select_capture_program(true, true, "src.monitor", 20);
        assert_eq!(prog, "parec");
        assert_eq!(args, parec_args("src.monitor", 20));

        let (prog, args) = select_capture_program(false, true, "src.monitor", 20);
        assert_eq!(prog, "pacat");
        assert_eq!(
            args,
            vec![
                "--record",
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=20",
                "-d",
                "src.monitor",
            ]
        );
    }

    #[test]
    fn linux_capture_synthesizes_silence_when_pipe_is_idle_or_suspended() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 5");
        let mut cap = LinuxPcmCapture::spawn_command("idle.monitor".into(), cmd).unwrap();

        let mut buf = [999i16; 16];
        let start = Instant::now();
        let n = cap.read_frame(&mut buf).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(n, 16);
        assert!(buf.iter().all(|&s| s == 0));
        assert!(
            elapsed < Duration::from_millis(500),
            "idle silence synthesis took too long: {elapsed:?}"
        );
    }

    #[test]
    fn linux_capture_wakes_immediately_when_stop_flag_is_set() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 5");
        let mut cap = LinuxPcmCapture::spawn_command("stop.monitor".into(), cmd).unwrap();

        let stop_flag = Arc::new(AtomicBool::new(false));
        cap.attach_stop_flag(Arc::clone(&stop_flag));

        // Pre-set stop_flag returns Ok(0) immediately without waiting for poll timeout.
        stop_flag.store(true, Ordering::Release);
        let mut buf = [0i16; 16];
        let start = Instant::now();
        let n = cap.read_frame(&mut buf).unwrap();
        assert_eq!(n, 0);
        assert!(start.elapsed() < Duration::from_millis(20));

        // Reset flag, spawn reader thread on sleeping pipe, and flip stop_flag asynchronously.
        let mut cmd2 = Command::new("/bin/sh");
        cmd2.arg("-c").arg("sleep 5");
        let mut cap2 = LinuxPcmCapture::spawn_command("stop2.monitor".into(), cmd2).unwrap();
        let stop_flag2 = Arc::new(AtomicBool::new(false));
        cap2.attach_stop_flag(Arc::clone(&stop_flag2));

        let flag_setter = Arc::clone(&stop_flag2);
        let handle = std::thread::spawn(move || {
            let mut b = [0i16; 64];
            let t0 = Instant::now();
            let res = cap2.read_frame(&mut b).unwrap();
            (res, t0.elapsed())
        });
        std::thread::sleep(Duration::from_millis(5));
        flag_setter.store(true, Ordering::Release);
        let (res, elapsed) = handle.join().unwrap();
        assert_eq!(res, 0);
        assert!(
            elapsed < Duration::from_millis(100),
            "stop_flag wakeup took too long: {elapsed:?}"
        );
    }

    #[test]
    fn linux_capture_respawns_on_eof_and_sets_discontinuity() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_c = Arc::clone(&count);
        let mut cap = LinuxPcmCapture::spawn_with_spawner("respawn.monitor".into(), move || {
            let n = count_c.fetch_add(1, Ordering::SeqCst);
            let mut cmd = Command::new("/bin/sh");
            if n == 0 {
                // First invocation exits immediately with EOF.
                cmd.arg("-c").arg("exit 0");
            } else {
                // Respawned invocation emits two stereo samples (1, -1).
                cmd.arg("-c").arg(r"printf '\001\000\377\377'");
            }
            cmd
        })
        .unwrap();

        let mut buf = [0i16; 2];
        let n = cap.read_frame(&mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, [1, -1]);
        assert!(cap.take_discontinuity());
        assert!(!cap.take_discontinuity());
    }

    #[test]
    fn linux_playback_smoothly_ramps_gain_and_preserves_unity_transparency() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("ramped.raw");

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!("cat > '{}'", out_path.display()));

        let mut play = LinuxPcmPlayback::spawn_command(cmd).unwrap();
        let unity_frame: Vec<i16> = (0..64).map(|i| (i * 250) - 8000).collect();
        play.write_frame(&unity_frame).unwrap();

        // Change to mute across a 64-sample (32-stereo-frame) constant DC frame.
        play.set_mute(true);
        let dc_frame = vec![10_000i16; 64];
        play.write_frame(&dc_frame).unwrap();

        drop(play.stdin.take());
        let _ = play.child.wait();

        let bytes = std::fs::read(&out_path).unwrap();
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        assert_eq!(samples.len(), 128);
        assert_eq!(&samples[..64], unity_frame.as_slice());

        let ramped = &samples[64..];
        assert!(
            ramped[0] > 9_000,
            "first ramped sample should start near previous unity gain, got {}",
            ramped[0]
        );
        assert_eq!(ramped[ramped.len() - 1], 0);
        for w in ramped.as_chunks::<2>().0.windows(2) {
            assert!(w[0][0] >= w[1][0]);
            assert_eq!(w[0][0], w[0][1]);
        }
    }

    #[test]
    fn linux_playback_recovers_once_from_broken_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("recovered.raw");
        let out_path_c = out_path.clone();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_c = Arc::clone(&count);

        let mut play = LinuxPcmPlayback::spawn_with_spawner(move || {
            let n = count_c.fetch_add(1, Ordering::SeqCst);
            let mut cmd = Command::new("/bin/sh");
            if n == 0 {
                // First child exits immediately so writing triggers BrokenPipe.
                cmd.arg("-c").arg("exit 0");
            } else {
                cmd.arg("-c")
                    .arg(format!("cat > '{}'", out_path_c.display()));
            }
            cmd
        })
        .unwrap();

        // Give the initial child a moment to exit and close its pipe read end.
        std::thread::sleep(Duration::from_millis(30));

        let payload = vec![1234i16, -5678, 42, -42];
        play.write_frame(&payload).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);

        drop(play.stdin.take());
        let _ = play.child.wait();

        let bytes = std::fs::read(&out_path).unwrap();
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        assert_eq!(samples, payload);
    }
}
