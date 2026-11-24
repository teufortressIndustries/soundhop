//! Linux PulseAudio/PipeWire PCM capture (`parec` / `pacat --record`) and playback (`pacat`)
//! bridges for the SHT3 audio transport pipeline.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
use crate::transport::dsp::apply_gain_i16;
use crate::transport::dsp::{
    crossfade_stereo_in_place, fade_in_stereo_in_place, fade_out_stereo_in_place, GainRamp,
};
use crate::transport::pcm::{PcmCapture, PcmPlayback, PCM_CHANNELS, PCM_SAMPLE_RATE};
use crate::transport::{TransportError, TransportResult};

const DEFAULT_LATENCY_MS: u32 = 20;
const DEFAULT_PLAYBACK_LATENCY_MS: u32 = 30;
const DEFAULT_SINK_TARGET_MS: f64 = 40.0;
const STEREO_FRAMES_PER_MS: usize = (PCM_SAMPLE_RATE as usize) / 1000;
const BYTES_PER_STEREO_FRAME: usize = (PCM_CHANNELS as usize) * 2;
const BYTES_PER_MS: f64 = (STEREO_FRAMES_PER_MS * BYTES_PER_STEREO_FRAME) as f64;
const RING_CAPACITY_FRAMES: usize = 160 * STEREO_FRAMES_PER_MS;
const RING_CAPACITY_SAMPLES: usize = RING_CAPACITY_FRAMES * 2;
const OVERRUN_CROSSFADE_FRAMES: usize = 96;
const UNDERRUN_RAMP_FRAMES: usize = 96;
const CAPTURE_DECLICK_RAMP_FRAMES: usize = 96;
const PLAYBACK_PIPE_SIZE_BYTES: libc::c_int = 8192;
const WRITER_CHUNK_SAMPLES: usize = 1024;
const WRITER_POLL_SLICE_MS: i32 = 10;
const WRITER_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(3);
const PIPE_UNDERRUN_THRESHOLD_BYTES: usize = UNDERRUN_RAMP_FRAMES * BYTES_PER_STEREO_FRAME;
const MAX_PLAYBACK_RESPAWN_ATTEMPTS: usize = 3;
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
    need_fade_in: bool,
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
            need_fade_in: false,
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
            if samples > 0 && samples < buf.len() {
                if self.need_fade_in {
                    fade_in_stereo_in_place(&mut buf[..samples], CAPTURE_DECLICK_RAMP_FRAMES);
                }
                fade_out_stereo_in_place(&mut buf[..samples], CAPTURE_DECLICK_RAMP_FRAMES);
                self.need_fade_in = true;
            } else if samples == 0 {
                self.need_fade_in = true;
            } else if self.need_fade_in {
                fade_in_stereo_in_place(&mut buf[..samples], CAPTURE_DECLICK_RAMP_FRAMES);
                self.need_fade_in = false;
            }
            buf[samples..].fill(0);
            return Ok(buf.len());
        }

        let samples = filled / 2;
        if !filled.is_multiple_of(2) {
            self.leftover[0] = self.byte_buf[filled - 1];
            self.leftover_len = 1;
        }
        decode_le_samples(&self.byte_buf[..samples * 2], &mut buf[..samples]);
        if self.need_fade_in && samples > 0 {
            fade_in_stereo_in_place(&mut buf[..samples], CAPTURE_DECLICK_RAMP_FRAMES);
            self.need_fade_in = false;
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

#[cfg(test)]
struct SyncPlayback {
    spawner: PlaybackSpawner,
    child: Child,
    stdin: Option<ChildStdin>,
    current_gain: f32,
    byte_buf: Vec<u8>,
}

struct PlaybackQueueState {
    ring: VecDeque<i16>,
    primed: bool,
    in_underrun: bool,
    need_fade_in: bool,
    overruns: u64,
    underruns: u64,
    stop: bool,
    drain_before_stop: bool,
    failed: bool,
}

struct PlaybackAtomics {
    ring_samples: AtomicUsize,
    staging_bytes: AtomicUsize,
    underruns: AtomicU64,
    overruns: AtomicU64,
}

struct AsyncPlayback {
    queue: Arc<(Mutex<PlaybackQueueState>, Condvar)>,
    atomics: Arc<PlaybackAtomics>,
    pipe_fd: Arc<Mutex<Option<RawFd>>>,
    stop_flag: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
    gain_ramp: GainRamp,
    ramp_buf: Vec<i16>,
}

enum PlaybackBackend {
    #[cfg(test)]
    Sync(SyncPlayback),
    Async(AsyncPlayback),
}

/// Plays 48 kHz stereo S16LE PCM to PulseAudio/PipeWire via `pacat --playback`,
/// applying smooth software gain ramping and automatic `BrokenPipe` recovery.
///
/// For `pacat` playback, a bounded stereo ring buffer and background non-blocking
/// pipe writer thread (`O_NONBLOCK` + `poll(POLLOUT)` + bounded `F_SETPIPE_SZ`)
/// prevent kernel pipe bloat and caller blocking while exposing live fill depth
/// via [`Self::buffered_ms`].
///
/// The child `pacat` process is terminated and reaped automatically on [`Drop`].
pub struct LinuxPcmPlayback {
    sink: Option<String>,
    latency_ms: u32,
    volume_pct: Arc<AtomicU8>,
    muted: Arc<AtomicBool>,
    backend: PlaybackBackend,
}

impl LinuxPcmPlayback {
    /// Spawn `pacat --playback --raw --format=s16le --rate=48000 --channels=2 --latency-msec=30`.
    pub fn spawn() -> TransportResult<Self> {
        Self::spawn_for_sink(None, DEFAULT_PLAYBACK_LATENCY_MS)
    }

    /// Spawn `pacat --playback` targeting an optional PulseAudio/PipeWire sink and latency.
    pub fn spawn_for_sink(sink: Option<&str>, latency_ms: u32) -> TransportResult<Self> {
        let sink_owned = sink.map(str::to_string);
        let cmd = build_pacat_command(sink_owned.as_deref(), latency_ms);
        Self::spawn_async_internal(
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
        Self::spawn_sync_internal(
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
        Self::spawn_sync_internal(
            None,
            DEFAULT_PLAYBACK_LATENCY_MS,
            PlaybackSpawner::Custom(Box::new(make_cmd)),
            cmd,
        )
    }

    #[cfg(test)]
    fn spawn_async_command(cmd: Command) -> TransportResult<Self> {
        Self::spawn_async_internal(
            None,
            DEFAULT_PLAYBACK_LATENCY_MS,
            PlaybackSpawner::None,
            cmd,
        )
    }

    #[cfg(test)]
    fn spawn_async_with_spawner(
        mut make_cmd: impl FnMut() -> Command + Send + 'static,
    ) -> TransportResult<Self> {
        let cmd = make_cmd();
        Self::spawn_async_internal(
            None,
            DEFAULT_PLAYBACK_LATENCY_MS,
            PlaybackSpawner::Custom(Box::new(make_cmd)),
            cmd,
        )
    }

    #[cfg(test)]
    fn spawn_sync_internal(
        sink: Option<String>,
        latency_ms: u32,
        spawner: PlaybackSpawner,
        cmd: Command,
    ) -> TransportResult<Self> {
        let (child, stdin) = spawn_playback_child(cmd)?;
        Ok(Self {
            sink,
            latency_ms,
            volume_pct: Arc::new(AtomicU8::new(100)),
            muted: Arc::new(AtomicBool::new(false)),
            backend: PlaybackBackend::Sync(SyncPlayback {
                spawner,
                child,
                stdin: Some(stdin),
                current_gain: 1.0,
                byte_buf: Vec::new(),
            }),
        })
    }

    fn spawn_async_internal(
        sink: Option<String>,
        latency_ms: u32,
        spawner: PlaybackSpawner,
        cmd: Command,
    ) -> TransportResult<Self> {
        let (mut child, stdin) = spawn_playback_child(cmd)?;
        if let Err(err) = configure_playback_pipe(stdin.as_raw_fd()) {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(TransportError::Io(err));
        }

        let pipe_fd = Arc::new(Mutex::new(Some(stdin.as_raw_fd())));
        let queue = Arc::new((
            Mutex::new(PlaybackQueueState {
                ring: VecDeque::with_capacity(RING_CAPACITY_SAMPLES),
                primed: false,
                in_underrun: false,
                need_fade_in: false,
                overruns: 0,
                underruns: 0,
                stop: false,
                drain_before_stop: false,
                failed: false,
            }),
            Condvar::new(),
        ));
        let atomics = Arc::new(PlaybackAtomics {
            ring_samples: AtomicUsize::new(0),
            staging_bytes: AtomicUsize::new(0),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
        });
        let stop_flag = Arc::new(AtomicBool::new(false));

        let q_c = Arc::clone(&queue);
        let a_c = Arc::clone(&atomics);
        let fd_c = Arc::clone(&pipe_fd);
        let stop_c = Arc::clone(&stop_flag);
        let writer = thread::Builder::new()
            .name("soundhop-linux-pcm-writer".to_string())
            .spawn(move || {
                playback_writer_loop(spawner, child, stdin, q_c, a_c, fd_c, stop_c);
            })
            .map_err(|err| {
                // If thread creation fails, ensure fd/child are cleaned up cleanly.
                *pipe_fd.lock().unwrap_or_else(|e| e.into_inner()) = None;
                TransportError::Io(err)
            })?;

        Ok(Self {
            sink,
            latency_ms,
            volume_pct: Arc::new(AtomicU8::new(100)),
            muted: Arc::new(AtomicBool::new(false)),
            backend: PlaybackBackend::Async(AsyncPlayback {
                queue,
                atomics,
                pipe_fd,
                stop_flag,
                writer: Some(writer),
                gain_ramp: GainRamp::with_initial(100, false)
                    .with_ramp_frames(GAIN_RAMP_MAX_FRAMES),
                ramp_buf: Vec::new(),
            }),
        })
    }

    /// Configured target PulseAudio/PipeWire sink name, if any.
    pub fn sink(&self) -> Option<&str> {
        self.sink.as_deref()
    }

    /// Configured playback latency in milliseconds.
    pub fn latency_ms(&self) -> u32 {
        self.latency_ms
    }

    /// Estimated total audio buffered across the user ring and unread kernel pipe bytes (ms).
    ///
    /// Returns `None` for synchronous test spawners (`None` / `Custom`).
    pub fn buffered_ms(&self) -> Option<f64> {
        match &self.backend {
            #[cfg(test)]
            PlaybackBackend::Sync(_) => None,
            PlaybackBackend::Async(async_pb) => {
                let ring_bytes = async_pb.atomics.ring_samples.load(Ordering::Acquire) * 2;
                let staging_bytes = async_pb.atomics.staging_bytes.load(Ordering::Acquire);
                let pipe_unread = query_pipe_unread_locked(&async_pb.pipe_fd);
                let total_bytes = ring_bytes + staging_bytes + pipe_unread;
                Some((total_bytes as f64) / BYTES_PER_MS)
            }
        }
    }

    /// Target prebuffer depth in milliseconds (`40.0 ms`).
    pub fn sink_target_ms(&self) -> f64 {
        DEFAULT_SINK_TARGET_MS
    }

    /// Cumulative playback underrun events (ring + pipe drained dry after priming).
    pub fn sink_underruns(&self) -> u64 {
        match &self.backend {
            #[cfg(test)]
            PlaybackBackend::Sync(_) => 0,
            PlaybackBackend::Async(async_pb) => async_pb.atomics.underruns.load(Ordering::Acquire),
        }
    }

    /// Cumulative playback ring overrun drops (with 2 ms stereo crossfade).
    pub fn sink_overruns(&self) -> u64 {
        match &self.backend {
            #[cfg(test)]
            PlaybackBackend::Sync(_) => 0,
            PlaybackBackend::Async(async_pb) => async_pb.atomics.overruns.load(Ordering::Acquire),
        }
    }

    #[cfg(test)]
    fn finish_for_test(&mut self) {
        match &mut self.backend {
            PlaybackBackend::Sync(sync_pb) => {
                drop(sync_pb.stdin.take());
                let _ = sync_pb.child.wait();
            }
            PlaybackBackend::Async(async_pb) => {
                let (lock, cv) = &*async_pb.queue;
                {
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    state.primed = true;
                    state.drain_before_stop = true;
                    state.stop = true;
                    cv.notify_all();
                }
                if let Some(writer) = async_pb.writer.take() {
                    let _ = writer.join();
                }
            }
        }
    }
}

impl PcmPlayback for LinuxPcmPlayback {
    fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let vol = u32::from(self.volume_pct.load(Ordering::Acquire)).min(150);
        let muted = self.muted.load(Ordering::Acquire);

        match &mut self.backend {
            #[cfg(test)]
            PlaybackBackend::Sync(sync_pb) => sync_pb.write_frame(samples, vol, muted),
            PlaybackBackend::Async(async_pb) => async_pb.write_frame(samples, vol, muted),
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

    fn buffered_ms(&self) -> Option<f64> {
        Self::buffered_ms(self)
    }

    fn sink_target_ms(&self) -> f64 {
        Self::sink_target_ms(self)
    }

    fn sink_underruns(&self) -> u64 {
        Self::sink_underruns(self)
    }

    fn sink_overruns(&self) -> u64 {
        Self::sink_overruns(self)
    }
}

impl Drop for LinuxPcmPlayback {
    fn drop(&mut self) {
        match &mut self.backend {
            #[cfg(test)]
            PlaybackBackend::Sync(sync_pb) => {
                // Close stdin first so child observes EOF, then force-kill and reap.
                drop(sync_pb.stdin.take());
                let _ = sync_pb.child.kill();
                let _ = sync_pb.child.wait();
            }
            PlaybackBackend::Async(async_pb) => {
                async_pb.stop_flag.store(true, Ordering::Release);
                let (lock, cv) = &*async_pb.queue;
                {
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    state.stop = true;
                    cv.notify_all();
                }
                if let Some(writer) = async_pb.writer.take() {
                    let _ = writer.join();
                }
            }
        }
    }
}

#[cfg(test)]
impl SyncPlayback {
    fn try_respawn(&mut self) -> TransportResult<bool> {
        let Some(cmd) = build_respawn_command(&mut self.spawner) else {
            return Ok(false);
        };

        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();

        let (child, stdin) = spawn_playback_child(cmd)?;
        self.child = child;
        self.stdin = Some(stdin);
        Ok(true)
    }

    fn write_frame(&mut self, samples: &[i16], vol: u32, muted: bool) -> TransportResult<()> {
        if self.stdin.is_none() {
            return Err(TransportError::Closed);
        }

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
}

impl AsyncPlayback {
    fn write_frame(&mut self, samples: &[i16], vol: u32, muted: bool) -> TransportResult<()> {
        let aligned_len = samples.len() & !1;
        if aligned_len == 0 {
            return Ok(());
        }

        self.ramp_buf.clear();
        self.ramp_buf.extend_from_slice(&samples[..aligned_len]);
        self.gain_ramp.apply(&mut self.ramp_buf, 2, vol, muted);

        let (lock, cv) = &*self.queue;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed || state.stop {
            return Err(TransportError::Closed);
        }

        push_to_ring(&mut state, &mut self.ramp_buf, &self.atomics);

        if !state.primed {
            let ring_bytes = state.ring.len() * 2;
            let staging_bytes = self.atomics.staging_bytes.load(Ordering::Acquire);
            let pipe_unread = query_pipe_unread_locked(&self.pipe_fd);
            let target_bytes = (DEFAULT_SINK_TARGET_MS * BYTES_PER_MS).round() as usize;
            if ring_bytes + staging_bytes + pipe_unread >= target_bytes {
                state.primed = true;
            }
        }

        cv.notify_one();
        Ok(())
    }
}

fn push_to_ring(
    state: &mut PlaybackQueueState,
    new_samples: &mut [i16],
    atomics: &PlaybackAtomics,
) {
    if new_samples.is_empty() {
        return;
    }
    if state.need_fade_in {
        fade_in_stereo_in_place(new_samples, UNDERRUN_RAMP_FRAMES);
        state.need_fade_in = false;
        state.in_underrun = false;
    }

    let total = state.ring.len() + new_samples.len();
    if total > RING_CAPACITY_SAMPLES {
        let overflow = (total - RING_CAPACITY_SAMPLES) & !1;
        let max_fade_samples = OVERRUN_CROSSFADE_FRAMES * 2;
        let mut from_tail = [0i16; OVERRUN_CROSSFADE_FRAMES * 2];

        if overflow < state.ring.len() {
            let remaining = state.ring.len() - overflow;
            let tail_len = max_fade_samples.min(overflow).min(remaining) & !1;
            // Blend from the head of the dropped span (continuous with audio already sent
            // to the pipe) into the new front of the ring buffer.
            for (slot, src) in from_tail[..tail_len]
                .iter_mut()
                .zip(state.ring.range(..tail_len))
            {
                *slot = *src;
            }
            state.ring.drain(..overflow);

            if tail_len >= 2 {
                let mut head_buf = [0i16; OVERRUN_CROSSFADE_FRAMES * 2];
                for (slot, src) in head_buf[..tail_len]
                    .iter_mut()
                    .zip(state.ring.range(..tail_len))
                {
                    *slot = *src;
                }
                crossfade_stereo_in_place(
                    &from_tail[..tail_len],
                    &mut head_buf[..tail_len],
                    tail_len / 2,
                );
                for (dst, &val) in state
                    .ring
                    .iter_mut()
                    .take(tail_len)
                    .zip(&head_buf[..tail_len])
                {
                    *dst = val;
                }
            }
            state.ring.extend(new_samples.iter().copied());
        } else {
            let tail_len = max_fade_samples.min(state.ring.len()) & !1;
            for (slot, src) in from_tail[..tail_len]
                .iter_mut()
                .zip(state.ring.range(..tail_len))
            {
                *slot = *src;
            }
            state.ring.clear();
            let keep_start = (new_samples.len() - RING_CAPACITY_SAMPLES) & !1;
            if tail_len >= 2 {
                let blend_len = tail_len.min(new_samples.len() - keep_start) & !1;
                if blend_len >= 2 {
                    crossfade_stereo_in_place(
                        &from_tail[..tail_len],
                        &mut new_samples[keep_start..keep_start + blend_len],
                        blend_len / 2,
                    );
                }
            }
            state.ring.extend(new_samples[keep_start..].iter().copied());
        }

        state.overruns += 1;
        atomics.overruns.store(state.overruns, Ordering::Release);
    } else {
        state.ring.extend(new_samples.iter().copied());
    }

    atomics
        .ring_samples
        .store(state.ring.len(), Ordering::Release);
}

fn build_respawn_command(spawner: &mut PlaybackSpawner) -> Option<Command> {
    match spawner {
        #[cfg(test)]
        PlaybackSpawner::None => None,
        PlaybackSpawner::Pacat { sink, latency_ms } => {
            Some(build_pacat_command(sink.as_deref(), *latency_ms))
        }
        #[cfg(test)]
        PlaybackSpawner::Custom(f) => Some(f()),
    }
}

fn configure_playback_pipe(fd: RawFd) -> std::io::Result<()> {
    set_nonblocking(fd)?;
    // Tighten kernel pipe size (~42.7 ms at 48 kHz stereo s16le) to avoid 64 KB bloat.
    let _ = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, PLAYBACK_PIPE_SIZE_BYTES) };
    Ok(())
}

fn query_pipe_unread_bytes(fd: RawFd) -> usize {
    let mut unread: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(fd, libc::FIONREAD as _, &mut unread) };
    if rc == 0 && unread > 0 {
        unread as usize
    } else {
        0
    }
}

fn query_pipe_unread_locked(pipe_fd: &Mutex<Option<RawFd>>) -> usize {
    let guard = pipe_fd.lock().unwrap_or_else(|e| e.into_inner());
    match *guard {
        Some(fd) => query_pipe_unread_bytes(fd),
        None => 0,
    }
}

fn encode_i16_slice_to_le_bytes(samples: &[i16], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

fn drain_ring_samples_into_bytes(
    ring: &mut VecDeque<i16>,
    count: usize,
    scratch: &mut Vec<i16>,
    out: &mut Vec<u8>,
) {
    scratch.clear();
    scratch.reserve(count);
    for _ in 0..count {
        if let Some(s) = ring.pop_front() {
            scratch.push(s);
        }
    }
    encode_i16_slice_to_le_bytes(scratch, out);
}

#[allow(clippy::too_many_arguments)]
fn playback_writer_loop(
    mut spawner: PlaybackSpawner,
    mut child: Child,
    stdin: ChildStdin,
    queue: Arc<(Mutex<PlaybackQueueState>, Condvar)>,
    atomics: Arc<PlaybackAtomics>,
    pipe_fd: Arc<Mutex<Option<RawFd>>>,
    stop_flag: Arc<AtomicBool>,
) {
    let mut stdin = Some(stdin);
    let mut staging: Vec<u8> = Vec::with_capacity(WRITER_CHUNK_SAMPLES * 2);
    let mut staging_pos: usize = 0;
    let mut scratch_i16: Vec<i16> = Vec::with_capacity(WRITER_CHUNK_SAMPLES);
    let mut consecutive_respawns: usize = 0;
    let mut drain_on_exit = false;

    'outer: loop {
        if staging_pos >= staging.len() {
            staging.clear();
            staging_pos = 0;

            let (lock, cv) = &*queue;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());

            loop {
                if state.stop {
                    drain_on_exit = state.drain_before_stop;
                    if drain_on_exit && !state.ring.is_empty() {
                        let count = state.ring.len().min(WRITER_CHUNK_SAMPLES);
                        drain_ring_samples_into_bytes(
                            &mut state.ring,
                            count,
                            &mut scratch_i16,
                            &mut staging,
                        );
                        atomics
                            .ring_samples
                            .store(state.ring.len(), Ordering::Release);
                        atomics
                            .staging_bytes
                            .store(staging.len(), Ordering::Release);
                        break;
                    }
                    break 'outer;
                }

                if !state.primed {
                    state = cv.wait(state).unwrap_or_else(|e| e.into_inner());
                    continue;
                }

                let lookahead_samples = if state.in_underrun {
                    0
                } else {
                    UNDERRUN_RAMP_FRAMES * 2
                };

                if state.ring.len() > lookahead_samples {
                    let count = (state.ring.len() - lookahead_samples).min(WRITER_CHUNK_SAMPLES);
                    drain_ring_samples_into_bytes(
                        &mut state.ring,
                        count,
                        &mut scratch_i16,
                        &mut staging,
                    );
                    atomics
                        .ring_samples
                        .store(state.ring.len(), Ordering::Release);
                    atomics
                        .staging_bytes
                        .store(staging.len(), Ordering::Release);
                    break;
                }

                if state.in_underrun {
                    state = cv.wait(state).unwrap_or_else(|e| e.into_inner());
                    continue;
                }

                let pipe_unread = query_pipe_unread_locked(&pipe_fd);
                if pipe_unread <= PIPE_UNDERRUN_THRESHOLD_BYTES {
                    // Ring + pipe drained dry after priming: fade out remaining lookahead tail.
                    let remaining = state.ring.len();
                    if remaining >= 2 {
                        scratch_i16.clear();
                        scratch_i16.extend(state.ring.drain(..));
                        fade_out_stereo_in_place(&mut scratch_i16, UNDERRUN_RAMP_FRAMES);
                        encode_i16_slice_to_le_bytes(&scratch_i16, &mut staging);
                    } else {
                        state.ring.clear();
                    }
                    state.in_underrun = true;
                    state.need_fade_in = true;
                    state.underruns += 1;
                    atomics.ring_samples.store(0, Ordering::Release);
                    atomics
                        .staging_bytes
                        .store(staging.len(), Ordering::Release);
                    atomics.underruns.store(state.underruns, Ordering::Release);
                    if !staging.is_empty() {
                        break;
                    }
                } else {
                    let (guard, _) = cv
                        .wait_timeout(state, WRITER_IDLE_POLL_INTERVAL)
                        .unwrap_or_else(|e| e.into_inner());
                    state = guard;
                }
            }
        }

        while staging_pos < staging.len() {
            if stop_flag.load(Ordering::Acquire) {
                break 'outer;
            }

            let write_res = {
                let _fd_guard = pipe_fd.lock().unwrap_or_else(|e| e.into_inner());
                let res = match stdin.as_mut() {
                    Some(pipe_in) => pipe_in.write(&staging[staging_pos..]),
                    None => break 'outer,
                };
                if let Ok(n) = res {
                    if n > 0 {
                        staging_pos += n;
                        atomics.staging_bytes.fetch_sub(n, Ordering::Release);
                        consecutive_respawns = 0;
                    }
                }
                res
            };

            match write_res {
                Ok(0) => {
                    // Treat zero-byte write on pipe as broken pipe.
                    if !respawn_writer_child(
                        &mut spawner,
                        &mut child,
                        &mut stdin,
                        &pipe_fd,
                        &stop_flag,
                        &mut consecutive_respawns,
                        &mut staging,
                        &mut staging_pos,
                        &atomics,
                    ) {
                        mark_writer_failed(&queue);
                        break 'outer;
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    let Some(fd) = stdin.as_ref().map(AsRawFd::as_raw_fd) else {
                        break 'outer;
                    };
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let rc = unsafe { libc::poll(&mut pfd, 1, WRITER_POLL_SLICE_MS) };
                    if rc < 0 {
                        let err = std::io::Error::last_os_error();
                        if err.kind() == ErrorKind::Interrupted {
                            continue;
                        }
                        mark_writer_failed(&queue);
                        break 'outer;
                    }
                }
                Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                    if !respawn_writer_child(
                        &mut spawner,
                        &mut child,
                        &mut stdin,
                        &pipe_fd,
                        &stop_flag,
                        &mut consecutive_respawns,
                        &mut staging,
                        &mut staging_pos,
                        &atomics,
                    ) {
                        mark_writer_failed(&queue);
                        break 'outer;
                    }
                }
                Err(_) => {
                    mark_writer_failed(&queue);
                    break 'outer;
                }
            }
        }
    }

    *pipe_fd.lock().unwrap_or_else(|e| e.into_inner()) = None;
    atomics.staging_bytes.store(0, Ordering::Release);
    drop(stdin.take());
    if !drain_on_exit {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[allow(clippy::too_many_arguments)]
fn respawn_writer_child(
    spawner: &mut PlaybackSpawner,
    child: &mut Child,
    stdin: &mut Option<ChildStdin>,
    pipe_fd: &Mutex<Option<RawFd>>,
    stop_flag: &AtomicBool,
    consecutive_respawns: &mut usize,
    staging: &mut [u8],
    staging_pos: &mut usize,
    atomics: &PlaybackAtomics,
) -> bool {
    if *consecutive_respawns >= MAX_PLAYBACK_RESPAWN_ATTEMPTS || stop_flag.load(Ordering::Acquire) {
        return false;
    }
    let Some(cmd) = build_respawn_command(spawner) else {
        return false;
    };

    *pipe_fd.lock().unwrap_or_else(|e| e.into_inner()) = None;
    drop(stdin.take());
    let _ = child.kill();
    let _ = child.wait();

    if *consecutive_respawns > 0 {
        let deadline = Instant::now() + RESPAWN_BACKOFF;
        while Instant::now() < deadline {
            if stop_flag.load(Ordering::Acquire) {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    let Ok((mut new_child, new_stdin)) = spawn_playback_child(cmd) else {
        return false;
    };
    if configure_playback_pipe(new_stdin.as_raw_fd()).is_err() {
        drop(new_stdin);
        let _ = new_child.kill();
        let _ = new_child.wait();
        return false;
    }

    *pipe_fd.lock().unwrap_or_else(|e| e.into_inner()) = Some(new_stdin.as_raw_fd());
    *child = new_child;
    *stdin = Some(new_stdin);
    *consecutive_respawns += 1;

    // Re-align remaining staging bytes to a stereo frame boundary for the fresh child.
    let rem = staging.len().saturating_sub(*staging_pos);
    let misaligned = rem % BYTES_PER_STEREO_FRAME;
    if misaligned != 0 {
        *staging_pos += misaligned;
        atomics.staging_bytes.store(
            staging.len().saturating_sub(*staging_pos),
            Ordering::Release,
        );
    }

    true
}

fn mark_writer_failed(queue: &(Mutex<PlaybackQueueState>, Condvar)) {
    let (lock, cv) = queue;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    state.failed = true;
    cv.notify_all();
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

#[cfg(test)]
#[inline]
fn target_gain_factor(volume_pct: u32, muted: bool) -> f32 {
    if muted || volume_pct == 0 {
        0.0
    } else {
        (volume_pct.min(150) as f32) / 100.0
    }
}

#[cfg(test)]
#[inline]
fn scale_sample_f32(sample: i16, gain: f32) -> i16 {
    if gain <= 0.0 {
        return 0;
    }
    (f32::from(sample) * gain)
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

#[cfg(test)]
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
        play.finish_for_test();

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

        play.finish_for_test();

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

        play.finish_for_test();

        let bytes = std::fs::read(&out_path).unwrap();
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        assert_eq!(samples, payload);
    }

    #[test]
    fn linux_playback_sync_spawner_reports_none_buffered_ms() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("cat >/dev/null");
        let play = LinuxPcmPlayback::spawn_command(cmd).unwrap();
        assert_eq!(play.buffered_ms(), None);
        assert_eq!(play.sink_target_ms(), 40.0);
        assert_eq!(play.sink_underruns(), 0);
        assert_eq!(play.sink_overruns(), 0);
    }

    #[test]
    fn linux_playback_async_ring_buffer_nonblocking_overrun_crossfades_step_waveform() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("overrun.raw");

        // Delay reading for 180 ms so the 8 KB pipe and 160 ms ring buffer saturate and overflow,
        // then drain everything cleanly into out_path.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("sleep 0.18 && cat > '{}'", out_path.display()));

        let mut play = LinuxPcmPlayback::spawn_async_command(cmd).unwrap();
        assert_eq!(play.sink_target_ms(), 40.0);
        assert_eq!(play.buffered_ms(), Some(0.0));

        // 20 ms frame = 1920 interleaved stereo samples (960 stereo frames).
        let pos_frame = vec![10_000i16; 1920];
        let neg_ring_frame = vec![-10_000i16; RING_CAPACITY_SAMPLES];

        // Fill pipe (~42 ms) + ring (160 ms) completely with +10_000 while reader sleeps.
        let t0 = Instant::now();
        for _ in 0..4 {
            play.write_frame(&pos_frame).unwrap();
        }
        // Let the writer thread saturate the 8 KB kernel pipe with +10_000 first.
        std::thread::sleep(Duration::from_millis(15));
        for _ in 0..8 {
            play.write_frame(&pos_frame).unwrap();
        }

        // Push a full 160 ms (-10_000) frame while reader is still sleeping:
        // write_frame must remain non-blocking, drop the entire +10_000 ring, and crossfade
        // +10_000 into -10_000 across 96 stereo frames (2 ms).
        play.write_frame(&neg_ring_frame).unwrap();
        let push_elapsed = t0.elapsed();
        assert!(
            push_elapsed < Duration::from_millis(80),
            "write_frame blocked caller thread: {push_elapsed:?}"
        );
        assert!(
            play.sink_overruns() > 0,
            "expected overrun count > 0, got {}",
            play.sink_overruns()
        );
        assert!(
            play.buffered_ms().unwrap() > 160.0,
            "expected ring + pipe buffered_ms > 160 ms, got {:?}",
            play.buffered_ms()
        );

        play.finish_for_test();

        let bytes = std::fs::read(&out_path).unwrap();
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();

        // Find the transition between +10_000 and -10_000 in left channel and verify
        // there is no abrupt 20_000 single-frame step discontinuity.
        let left: Vec<i16> = samples.iter().step_by(2).copied().collect();
        assert!(left.contains(&10_000));
        assert!(left.contains(&-10_000));
        let max_step = left
            .windows(2)
            .map(|w| (i32::from(w[0]) - i32::from(w[1])).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_step < 1_000,
            "expected smooth overrun crossfade (< 1000 delta/sample), got max_step={max_step}"
        );
    }

    #[test]
    fn linux_playback_buffered_ms_tracks_paced_reader_and_underruns() {
        // 1. Paced slow reader subprocess (reads 3840 bytes every 45 ms while sender writes
        // every 20 ms): buffered_ms() rises past sink_target_ms (40 ms).
        let mut slow_cmd = Command::new("python3");
        slow_cmd.arg("-c").arg(
            "import sys,time\nwhile True:\n d=sys.stdin.buffer.read(3840)\n if not d: break\n time.sleep(0.045)",
        );
        let mut slow_play = LinuxPcmPlayback::spawn_async_command(slow_cmd).unwrap();
        let frame = vec![4_000i16; 1920]; // 20 ms

        slow_play.write_frame(&frame).unwrap();
        let b1 = slow_play.buffered_ms().unwrap();
        assert!(
            (b1 - 20.0).abs() < 0.5,
            "after 1 frame expected ~20 ms, got {b1}"
        );

        slow_play.write_frame(&frame).unwrap();
        let b2 = slow_play.buffered_ms().unwrap();
        assert!(
            (b2 - 40.0).abs() < 0.5,
            "after 2 frames expected ~40 ms, got {b2}"
        );

        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(20));
            slow_play.write_frame(&frame).unwrap();
        }
        let b_after = slow_play.buffered_ms().unwrap();
        assert!(
            b_after > 55.0,
            "expected buffered_ms to rise above target 40 ms against slow reader, got {b_after}"
        );
        drop(slow_play);

        // 2. Fast reader: drains pipe dry after priming, fading out tail, counting underrun,
        // and fading in head on recovery.
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("underrun.raw");
        let mut fast_cmd = Command::new("/bin/sh");
        fast_cmd
            .arg("-c")
            .arg(format!("cat > '{}'", out_path.display()));
        let mut fast_play = LinuxPcmPlayback::spawn_async_command(fast_cmd).unwrap();

        let tone = vec![8_000i16; 1920];
        fast_play.write_frame(&tone).unwrap();
        fast_play.write_frame(&tone).unwrap();

        // Wait briefly for cat to drain the pipe dry and writer to detect underrun.
        let deadline = Instant::now() + Duration::from_millis(300);
        while fast_play.sink_underruns() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            fast_play.sink_underruns() >= 1,
            "expected underrun after draining dry"
        );
        assert!(
            fast_play.buffered_ms().unwrap() < 1.0,
            "expected buffered_ms ~0 after underrun, got {:?}",
            fast_play.buffered_ms()
        );

        // Recovery frame should fade in smoothly from 0.
        fast_play.write_frame(&tone).unwrap();
        fast_play.finish_for_test();

        let bytes = std::fs::read(&out_path).unwrap();
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        // 3 frames = 5760 samples. Around sample 3840 (boundary between frame 2 tail and frame 3 head),
        // samples should fade down to 0 then fade back up from 0.
        assert_eq!(samples.len(), 5760);
        assert_eq!(samples[3838], 0);
        assert_eq!(samples[3839], 0);
        assert_eq!(samples[3840], 0);
        assert_eq!(samples[3841], 0);
        assert_eq!(samples[5758], 8_000);
    }

    #[test]
    fn linux_playback_async_recovers_from_broken_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("async_recovered.raw");
        let out_path_c = out_path.clone();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_c = Arc::clone(&count);

        let mut play = LinuxPcmPlayback::spawn_async_with_spawner(move || {
            let n = count_c.fetch_add(1, Ordering::SeqCst);
            let mut cmd = Command::new("/bin/sh");
            if n == 0 {
                cmd.arg("-c").arg("exit 0");
            } else {
                cmd.arg("-c")
                    .arg(format!("cat > '{}'", out_path_c.display()));
            }
            cmd
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(30));

        // Write 2 x 20 ms frames to prime and trigger write -> BrokenPipe -> respawn.
        let frame = vec![1234i16; 1920];
        play.write_frame(&frame).unwrap();
        play.write_frame(&frame).unwrap();
        play.finish_for_test();

        assert_eq!(count.load(Ordering::SeqCst), 2);
        let bytes = std::fs::read(&out_path).unwrap();
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect();
        assert_eq!(samples.len(), 3840);
        assert!(samples.iter().all(|&s| s == 1234));
    }

    #[test]
    fn linux_capture_partial_timeout_fades_out_tail_and_fades_in_recovery() {
        // Emit 400 samples (200 stereo frames = 800 bytes) of +10_000/-10_000, pause 60 ms
        // (triggering the 45 ms read timeout mid-frame), then emit 1920 samples (3840 bytes)
        // of +10_000/-10_000.
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg(
            "import sys,time; f=b'\\x10\\x27\\xf0\\xd8'; sys.stdout.buffer.write(f*200); sys.stdout.buffer.flush(); time.sleep(0.06); sys.stdout.buffer.write(f*960); sys.stdout.buffer.flush()",
        );
        let mut cap = LinuxPcmCapture::spawn_command("partial.monitor".into(), cmd).unwrap();

        let mut buf1 = vec![999i16; 1920];
        let mut n1 = cap.read_frame(&mut buf1).unwrap();
        // Skip initial zero-fill if python3 interpreter startup itself exceeded 45 ms.
        while n1 > 0 && buf1.iter().all(|&s| s == 0) {
            n1 = cap.read_frame(&mut buf1).unwrap();
        }
        assert_eq!(n1, 1920);
        // Leading partial samples retain original amplitude.
        assert_eq!(buf1[0], 10_000);
        assert_eq!(buf1[1], -10_000);
        // Trailing partial samples fade smoothly down to zero before the zero-filled tail.
        assert!(buf1[210].abs() < 10_000);
        assert_eq!(buf1[398], 0);
        assert_eq!(buf1[399], 0);
        assert!(buf1[400..].iter().all(|&s| s == 0));

        // Next live frame after timeout must fade in smoothly from 0 back to full scale.
        let mut buf2 = vec![0i16; 1920];
        let mut n2 = cap.read_frame(&mut buf2).unwrap();
        while n2 > 0 && buf2.iter().all(|&s| s == 0) {
            n2 = cap.read_frame(&mut buf2).unwrap();
        }
        assert_eq!(n2, 1920);
        assert_eq!(buf2[0], 0);
        assert_eq!(buf2[1], 0);
        assert!(
            buf2[96] > 4_000 && buf2[96] < 6_000,
            "midpoint fade-in got {}",
            buf2[96]
        );
        assert_eq!(buf2[192], 10_000);
        assert_eq!(buf2[193], -10_000);
    }
}
