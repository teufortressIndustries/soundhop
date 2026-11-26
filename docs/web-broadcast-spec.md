# SoundHop Web Broadcast Architecture & Implementation Specification

> **Status**: Implementation-Ready Design Specification  
> **Target Crate Version**: SoundHop `2.7.0+`  
> **Scope**: `soundhop-core`, `soundhop-cli`, `soundhop-gui`, and `packaging/`

---

## 1. Goals, Non-Goals & Why Web Broadcast over `scrcpy` `app_process`

### 1.1 Goals

1. **Zero-install phone listening**: Stream PC system or routed application audio to any smartphone, tablet, or secondary browser on the LAN (or over USB via `adb reverse`) using only the mobile web browser—no SoundHop Android APK, no `scrcpy` download, and no USB debugging required for Wi-Fi use.
2. **Low-latency primary transport with universal fallback**:
   - **Primary (`WebRTC`)**: Pure-Rust ICE-Lite WebRTC (`str0m`) over a single UDP port (`4718/udp`) carrying 20 ms 48 kHz stereo Opus frames (`~50–90 ms` end-to-end latency on LAN Wi-Fi).
   - **Fallback (`WebSocket PCM16LE`)**: Automatic fallback over the same HTTP/WS TCP port (`4717/tcp`) when UDP is blocked, mDNS/ICE fails, or traffic runs over USB `adb reverse` (`tcp:4717`), with an automatic `AudioWorklet` upgrade in secure contexts (`http://localhost:4717`).
3. **Single shared capture & encode pipeline**: Capture and encode once inside `soundhop-core` (`GstBroadcaster`) and fan out immutable frames to $N$ simultaneous browser listeners without duplicating `pulsesrc` / `wasapi2src` captures or `opusenc` instances.
4. **Full SoundHop integration**: Reuse SoundHop's existing Linux virtual sink + app-routing (`route_app`, `always_local`, `always_remote`, `keep_local`), volume/telemetry, firewall management (`FirewallPorts`), daemon IPC, CLI/TUI, `egui` Control Center (with live QR code), and system tray.
5. **Zero-build embedded frontend**: Embed a self-contained HTML/CSS/JS single-page app (`include_str!`) in `soundhop-core` with zero Node/npm/TypeScript/WASM build dependencies, preserving SoundHop's single-binary Rust build and AnyLinux/Windows packaging.

### 1.2 Non-Goals

- **Bidirectional microphone audio from browser to PC**: Web Broadcast is PC-to-browser (`recvonly`) playback only. Browser microphone capture (`getUserMedia`) requires HTTPS on non-localhost origins and is out of scope; PC-Android mic passthrough continues to use `AndroidCaptureSession` (`scrcpy`).
- **Self-signed HTTPS / local CA provisioning**: Installing custom root CAs on Android/iOS is high-friction and brittle. Serving plain HTTP over LAN (and `http://localhost` over `adb reverse`, which browsers treat as a `SecureContext`) avoids certificate warnings while still supporting WebRTC `recvonly` and WS fallback.
- **Public internet / WAN relay (TURN)**: SoundHop targets trusted LANs and USB `adb reverse`. No STUN/TURN servers or NAT hole-punching across the public internet are included.
- **Replacing existing peer-to-peer SoundHop protocols**: `PA-TCP` (`4713/tcp`), `SHT3` (`4714/tcp` + `4716/udp`), and `Pairing v2` (`4715/tcp`) remain unchanged for PC-to-PC and multi-room audio sharing.

### 1.3 Why Web Broadcast over `scrcpy` `app_process` for PC-to-Phone Audio

SoundHop already manages `adb` and `scrcpy` in `crates/soundhop-core/src/android.rs` (`AndroidManager`) for capturing **Android -> PC** audio (`scrcpy` utilizes Android's `AudioPlaybackCapture` / `app_process` server). Pushing **PC -> Android** audio via a custom `app_process` binary over `adb` is a poor fit compared to Web Broadcast:

| Dimension | `scrcpy` / custom `app_process` over ADB | Web Broadcast (HTTP + WebRTC / WS) |
| :--- | :--- | :--- |
| **Android setup** | Requires Developer Options, USB Debugging, `adb` trust prompt, and managing a Dalvik/ART `app_process` DEX | Zero setup on Wi-Fi (scan QR or open URL); optional 1-click `adb reverse` on USB |
| **Android OS stability** | `AudioTrack` via hidden/internal `app_process` surfaces varies across OEMs (MIUI/OneUI/ColorOS) and Android API levels, lacks `MediaSession` lock-screen controls, and gets killed aggressively in background | Standard Chrome/Firefox `<audio>` + `MediaSession` API integrates natively with Android media notifications, lock-screen controls, and Bluetooth headset routing |
| **Multi-listener & cross-OS** | Tightly coupled to one ADB-connected Android device | Supports $N$ simultaneous listeners (Android Chrome/Firefox, iOS Safari, laptops, tablets) |
| **Maintenance & supply chain** | Requires compiling/shipping/signing Android `.dex` or APK payloads and tracking AOSP private APIs | 100% pure Rust (`str0m` + `tungstenite` + `qrcode`) + standard W3C browser APIs |
| **USB low-latency mode** | Custom socket framing | Standard `adb reverse tcp:4717 tcp:4717` + `http://localhost:4717` (`isSecureContext === true` -> `AudioWorklet`) |

---

## 2. Architecture Overview & Module Layout

### 2.1 High-Level Data & Control Flow

```text
  Linux (PipeWire/PA)                            Windows (WASAPI)
  ┌──────────────────────────────────────┐       ┌───────────────────────────────┐
  │ soundhop_web_broadcast (null-sink)   │       │ Default Output Device         │
  │   ▲                                  │       │   │                           │
  │   ├─ standalone: default / routed    │       │   ▼                           │
  │   └─ keep_local: soundhop_multiroom  │       │ wasapi2src (loopback=true)    │
  └───────────────┬──────────────────────┘       └───────────────┬───────────────┘
                  │ .monitor                                     │
                  └──────────────────────┬───────────────────────┘
                                         ▼
                     ┌───────────────────────────────────────┐
                     │ GstBroadcaster (single pipeline)      │
                     │  audioconvert -> audioresample        │
                     │  -> S16LE 48kHz 2ch -> tee ("fanout") │
                     │     ├─ queue -> opusenc -> appsink    │
                     │     └─ queue ------------> appsink    │
                     └─────────┬──────────────────┬──────────┘
           Arc<BroadcastFrame> │                  │ Arc<BroadcastFrame>
           (Opus 20ms)         ▼                  ▼ (PCM16LE 20ms, lazy copy)
                ┌──────────────────┐        ┌──────────────────┐
                │ web-rtc-mux      │        │ web-ws-fanout    │
                │ (str0m ice-lite) │        │ (tungstenite)    │
                │ 0.0.0.0:4718/udp │        │ /ws on :4717/tcp │
                └────────┬─────────┘        └────────┬─────────┘
                         │                           │
                         └─────────────┬─────────────┘
                                       ▼
                        ┌─────────────────────────────┐
                        │ web-http-accept (0.0.0.0:4717/tcp)
                        │ GET /, /api/info, POST /webrtc/offer,
                        │ DELETE /webrtc/peer/:id, GET /ws
                        └──────────────┬──────────────┘
                                       │
          ┌────────────────────────────┴────────────────────────────┐
          ▼ (Wi-Fi LAN)                                             ▼ (USB ADB)
  http://<lan-ip>:4717/#t=<tok>                             adb reverse tcp:4717 tcp:4717
  1. WebRTC recvonly (4718/udp)                             http://localhost:4717/#t=<tok>
  2. Auto-fallback: WS AudioBufferSourceNode                WS AudioWorklet (SecureContext)
```

### 2.2 Workspace & Module Layout

#### `crates/soundhop-core/`
- `Cargo.toml`: Add workspace dependencies `str0m`, `tungstenite`, `qrcode`.
- `src/constants.rs`: Add `WEB_BROADCAST_SINK_NAME`, `DEFAULT_WEB_PORT`, `DEFAULT_WEBRTC_PORT`, `DEFAULT_WEB_MAX_CLIENTS`.
- `src/config.rs`: Extend `SharingConfig` with `web_*` fields and update `Config::normalize()`.
- `src/state.rs`: Add `web_broadcast_active`, `web_port`, `web_adb_reverse_serials`, and `State::clear_web_broadcast()`.
- `src/firewall.rs`: Extend `FirewallPorts` with `web_tcp: Option<u16>` and `webrtc_udp: Option<u16>`, updating `from_config()`, `specs()`, `windows_close_specs()`, and unit tests.
- `src/transport/gst_runtime.rs`: Add `"tee"`, `"queue"`, `"appsink"` to `REQUIRED_CORE_ELEMENTS`.
- `src/transport/gst_sender.rs`: Promote `build_capture_source` to `pub(crate)`.
- `src/transport/gst_broadcaster.rs` *(new)*: `GstBroadcaster` (gated behind `#[cfg(feature = "gstreamer")]`) + stub implementation under `#[cfg(not(feature = "gstreamer"))]`, `BroadcastFrame`, subscriber registry, slow-subscriber eviction, and Windows WASAPI hotplug restart.
- `src/web/` *(new module, exported in `src/lib.rs` as `pub mod web;`)*:
  - `src/web/mod.rs`: `WebBroadcastManager`, `WebBroadcastConfig`, `WebBroadcastStatus`, `WebClientInfo`, token generation/rotation, start/stop orchestration, client caps & eviction.
  - `src/web/http.rs`: Sync HTTP/1.1 listener (`TcpListener` + `BufReader<TcpStream>`), route dispatch, constant-time bearer auth, `Origin`/`Host`/`allowed_networks` validation, rate limiting, WebSocket upgrade handoff.
  - `src/web/rtc.rs`: Dedicated `web-rtc-mux` thread owning `UdpSocket` + `HashMap<PeerId, PeerRtc>` (`str0m` ICE-Lite), STUN `USERNAME` `ufrag` demux, loopback UDP poke, `MediaAdded`/`Writer` Opus packet injection, and `poll_output()` drain loop.
  - `src/web/ws.rs`: `web-ws-fanout` thread(s), `tungstenite` auth (`Sec-WebSocket-Protocol` or first JSON text frame), 16-byte binary header encoder (`SH` + PCM16LE), ping/pong RTT tracking, bounded per-client queue.
  - `src/web/qr.rs`: `qrcode` helpers for UTF-8 terminal rendering (CLI/TUI) and RGBA/bool matrix export (for `egui`).
  - `src/web/web_player.html`: Zero-build embedded browser SPA loaded via `include_str!("web_player.html")`.
- `src/android.rs`: Add `ToolStatus::adb_ready()`, `ToolStatus::scrcpy_ready()`, and `AndroidManager` methods `setup_reverse_tunnel`, `remove_reverse_tunnel`, `list_reverse_tunnels`, `open_browser_url`.
- `src/controller/mod.rs` & `src/controller/web.rs` *(new)* & `src/controller/android.rs` & `src/controller/session.rs`: Integrate `WebBroadcastManager`, Linux virtual null/combine sink setup/teardown, `reconcile()` checks, and `status()` `"web_broadcast"` payload.
- `src/daemon.rs`: Add `handle_ipc()` match arms (`web_start`, `web_stop`, `web_status`, `web_rotate_token`, `web_kick`, `web_adb_reverse`), `shutdown()` cleanup, `run_android_supervisor_tick()` stale reverse tunnel pruning, and `config_patch`/`reload` rebind handling.

#### `crates/soundhop-cli/`
- `src/parser.rs`: Add `Commands::Web(WebArgs)` (`visible_alias = "broadcast"`) and `WebAction` subcommands.
- `src/core_cmds/mod.rs`, `src/core_cmds/connect.rs`, `src/core_cmds/web.rs` *(new)*: Handle `soundhop web ...` commands and enrich `soundhop status` output.
- `src/tui.rs`: Add Web Broadcast panel / controls (`TuiTab::WebBroadcast` or integrated section with live QR, URL, listener list, toggle/rotate/ADB reverse hotkeys).
- `src/doctor.rs`: Update `FirewallPorts` struct literal in unit test (`L824`).

#### `crates/soundhop-gui/`
- `src/backend/snapshot.rs`: Add `WebClientView` and `web_*` fields on `GuiSnapshot` hydrated from `status["web_broadcast"]`.
- `src/backend/commands.rs` & `src/backend/optimistic.rs`: Add `GuiCommand` variants (`StartWebBroadcast`, `StopWebBroadcast`, `RotateWebToken`, `KickWebClient`, `WebAdbReverse`) and optimistic state updates.
- `src/views/home.rs`: Add Web Broadcast card with enable/keep-local toggles, cached `TextureHandle` QR code (`TextureOptions::NEAREST`), copyable URL, Rotate Token button, and live listener list with WebRTC/WS badges and Kick button.
- `src/views/phone.rs`: Gate USB `adb reverse` + `Open in Phone Browser` controls on `snap.android_tools.adb_ready()`.
- `src/app.rs` & `src/tray.rs`: Add `"Web Broadcast Live (N)"` nav rail badge, tray toggle/copy-URL menu items, and tray tooltip summary.

#### `packaging/`
- `packaging/windows/soundhop.iss`: Delete `"SoundHop Web (4717/tcp)"` and `"SoundHop WebRTC (4718/udp)"` Windows Defender rules in `CleanupFirewallRules()`.

---

## 3. Shared Audio Capture & Fan-Out Pipeline

### 3.1 Bundled GStreamer Elements & Runtime Verification

All required runtime GStreamer elements are already bundled across SoundHop's platform packaging (`packaging/verify-appimage.sh`, `packaging/windows/build-windows.ps1`, and `.github/workflows/ci.yml`):
- **Both Linux & Windows**: `gstcoreelements` (`capsfilter`, `tee`, `queue`), `gstaudioconvert`, `gstaudioresample`, `gstopus` (`opusenc`), `gstapp` (`appsink`)
- **Linux (`verify-appimage.sh`)**: `gstpulseaudio` (`pulsesrc`), plus `gstaudiotestsrc` (`audiotestsrc`, used in tests)
- **Windows (`build-windows.ps1`)**: `gstwasapi2` (`wasapi2src`); `audiotestsrc` is test-only and not required in Windows runtime bundles

Extend `REQUIRED_CORE_ELEMENTS` in `crates/soundhop-core/src/transport/gst_runtime.rs` with `"tee"`, `"queue"`, and `"appsink"` so `check_gstreamer_runtime()` validates them upfront.

### 3.2 `GstBroadcaster` Pipeline & `Arc<BroadcastFrame>` Fan-Out

Promote `build_capture_source(endpoint: &CaptureEndpoint, audio_params: &AudioParams)` in `crates/soundhop-core/src/transport/gst_sender.rs` to `pub(crate)`. `GstBroadcaster` constructs a single live capture pipeline feeding two leaky branches off a `tee` (with `bitrate_bps = (i32::from(audio_params.bitrate_kbps) * 1000).clamp(16_000, 510_000)` derived from `cfg.audio.profile`, matching `gst_sender.rs:192-194`):

```text
source (pulsesrc | wasapi2src loopback=true | audiotestsrc is-live=true)
  -> audioconvert -> audioresample
  -> capsfilter("audio/x-raw", format="S16LE", rate=48000, channels=2, layout="interleaved")
  -> tee (name="fanout", allow-not-linked=true)
     ├─> queue(leaky="downstream", max-size-buffers=4)
     │     -> opusenc(audio-type="generic", bandwidth="fullband",
     │                bitrate-type="constrained-vbr", complexity=10,
     │                dtx=false, inband-fec=true, packet-loss-percentage=10,
     │                bitrate=bitrate_bps, frame-size="20")
     │     -> appsink("opus_sink", sync=false, drop=true, max-buffers=4)
     └─> queue(leaky="downstream", max-size-buffers=4)
           -> appsink("pcm_sink", sync=false, drop=true, max-buffers=4)
```

Key design rules:
- **Live clocking**: Both `appsink`s set `sync=false`, `drop=true`, `max-buffers=4` because `pulsesrc` / `wasapi2src` (`provide-clock=true`) or `audiotestsrc` (`is-live=true`) already paces the pipeline in real time.
- **Zero-copy shared frame**:
  ```rust
  #[derive(Debug, Clone)]
  pub struct BroadcastFrame {
      pub seq: u64,
      pub pts_samples: u64,
      pub duration_samples: u32, // 960 samples @ 48 kHz (20 ms)
      pub captured_at: Instant,
      pub data: Arc<[u8]>,
  }
  ```
- **Lazy PCM allocation**: `pcm_sink`'s `new_sample` callback checks `pcm_sub_count.load(Ordering::Acquire) > 0` *before* calling `buffer.map_readable()` and framing into exact `3840`-byte (960 frames $\times$ 2 channels $\times$ 2 bytes) S16LE chunks. When all listeners use WebRTC, the PCM branch performs zero per-frame allocations. Similarly, `opus_sink` checks `opus_sub_count.load(Ordering::Acquire) > 0`.
- **Opus fan-out wakeups**: When pushing an `Arc<BroadcastFrame>` to the `web-rtc-mux` channel, `opus_sink` sends a 1-byte UDP loopback poke to the mux socket so `web-rtc-mux` wakes immediately (`<10 µs`) without polling.

### 3.3 Slow-Subscriber Backpressure & Eviction

The `appsink` streaming thread **must never block** on any network socket or slow mobile listener:

- Each subscriber registers a bounded `mpsc::SyncSender<Arc<BroadcastFrame>>` (capacity `32`, ~640 ms max buffer) or a 16-slot `Mutex<VecDeque<Arc<BroadcastFrame>>>` ring that drops the oldest packet on overflow (preferred for `ws://` fallback so lagging clients stay near live edge).
- **WebRTC (`web-rtc-mux`)**: The single mux channel carries `Arc<BroadcastFrame>` with capacity `32`. If full, drop the newest frame and increment a dropped-frame counter. Per-peer RTCP/DTLS backpressure is handled inside `str0m` + non-blocking `UdpSocket`.
- **WebSocket (`web-ws-fanout`)**: Iterating `pcm_subs` under a brief lock (or `Arc` snapshot), call `try_send(Arc::clone(&frame))`:
  - `TrySendError::Full(_)`: Drop frame for that subscriber, increment `sub.dropped_frames` (`AtomicU32`) and `sub.consecutive_drops` (`AtomicU32`). Once `consecutive_drops >= 250` (~5 s of continuous saturation @ 20 ms/frame), evict and close the stalled WebSocket client.
  - `TrySendError::Disconnected(_)`: Prune the dead subscriber immediately and decrement `pcm_sub_count`.

### 3.4 Linux Virtual Sinks, `keep_local`, App Routing & Multi-Room Compatibility

Define `pub const WEB_BROADCAST_SINK_NAME: &str = "soundhop_web_broadcast";` in `crates/soundhop-core/src/constants.rs`. Because it starts with `SINK_PREFIX` (`"soundhop_"`), existing helpers (`Sink::is_soundhop()`, `remember_previous_default()`, `local_default_candidate()`, and `LinuxAudioBackend::detailed_sink_inputs()`) automatically classify it as a SoundHop managed sink and never mistake it for the user's physical speakers.

1. **Standalone Web Broadcast (`keep_local = false`, Linux only)**:
   - Call `self.remember_previous_default()`.
   - Load `module-null-sink` via `backend.load_module("module-null-sink", &[format!("sink_name={WEB_BROADCAST_SINK_NAME}"), "sink_properties=device.description=SoundHop_Web_Broadcast".to_string(), "rate=48000".to_string(), "channels=2".to_string()])` (`&[String]`, matching `backend/mod.rs:184` and `controller/session.rs:301-309`).
   - Capture `CaptureEndpoint::SystemPulseMonitor { device: format!("{WEB_BROADCAST_SINK_NAME}.monitor") }`.
   - On Linux, populate `State`: `connected_peer = "web-broadcast"`, `connected_name = "Web Broadcast"`, `connected_sink = WEB_BROADCAST_SINK_NAME`, `connected_module = web_null_module`, `web_null_module`, `keep_local = false`. (On Windows, do **not** set `st.connected_peer` / `st.connected_sink`, since Windows captures `SystemWasapiLoopback` directly without a virtual sink and `WindowsAudioBackend::available()` is `true` while `sink_exists(...)` would return `false`.)
   - Apply `cfg.routing.default_mode`, `cfg.connection.switch_default`, and `cfg.connection.move_streams` (`make_default` + `enforce_routed_apps`).
   - Because `State::is_connected()` and `st.connected_sink` are set on Linux, `route_app()`, `unroute_app()`, `enforce_routed_apps()`, `Daemon::audio_monitor_loop()`, `status_volume_json()`, `set_volume()`, and `desired_monitor_source()` work out of the box.

2. **Standalone Web Broadcast (`keep_local = true`, default, Linux)**:
   - Do **not** capture `local_sink.monitor` directly—doing so would leak `always_local` apps into the broadcast.
   - Follow `connect_multi_internal` (`controller/session.rs` `L585-688`): load `module-null-sink` (`soundhop_web_broadcast`) **and** `LinuxAudioBackend::create_combined_sink(MULTIROOM_SINK_NAME, "WebBroadcast+Local", &[&local_sink, WEB_BROADCAST_SINK_NAME])`.
   - Set `st.connected_peer = "web-broadcast"`, `st.connected_sink = MULTIROOM_SINK_NAME`, `st.combine_module = combine_id`, `st.connected_module = web_null_module`, `st.web_null_module = web_null_module`, `st.keep_local = true`.
   - `pulsesrc` captures `soundhop_web_broadcast.monitor`: `always_local` apps stay solely on `local_sink`, while default/`always_remote` apps play to both PC speakers and phone browsers.

3. **Simultaneous Peer Session, `connection_dict()`, `disconnect`, and `reconcile()`**:
   - **Rule**: Starting or stopping Web Broadcast while a peer/multi-room session is already active must **not** overwrite or tear down the peer session (`st.connected_peer != "web-broadcast"`). When `st.is_connected()` is already true for another peer/group at `start_web_broadcast` time, leave `st.connected_*` untouched and capture `<st.connected_sink>.monitor` directly (or append `soundhop_web_broadcast` when rebuilding a multiroom combine-sink). On `stop_web_broadcast()`, unload only `web_null_module` (if distinct) and leave the peer session intact.
   - **`Controller::connection_dict()` (`controller/mod.rs:528-558`)**: When `st.connected_peer == "web-broadcast"`, emit `serde_json::Value::Null` (rather than `"transport": "pa-tcp"`), so `GuiSnapshot::connected` stays `false` in the GUI Hero card (`views/home.rs:125`) and Web Broadcast status is presented cleanly via `"web_broadcast"`.
   - **`connect_internal()` / `connect_multi_internal()` (`controller/session.rs:167, 575`)**: If `st.connected_peer == "web-broadcast"` when connecting to a PC peer, tear down the standalone `web-broadcast` virtual sink cleanly first (or seamlessly re-target `WebBroadcastManager` to `<new_peer_sink>.monitor`) before setting up the peer session.
   - **`disconnect_internal()` (`controller/session.rs:819`)**: When `st.connected_peer == "web-broadcast"`, do **not** rip out `web_null_module` while `web_broadcast.is_active()`; peer disconnect is a no-op (users stop Web Broadcast via `web_stop` / GUI toggle / `Daemon::shutdown()`).
   - **`Controller::reconcile()` (`controller/mod.rs:667-690`)**: Gate `st_snap.connected_peer == "web-broadcast"` explicitly: if `!cfg!(target_os = "linux")` or `!self.web_broadcast.is_active()`, unload any leftover `combine_module`/`web_null_module`, restore `previous_default_sink`, and call `st.clear_connection()`. When `cfg!(target_os = "linux") && self.web_broadcast.is_active()`, verify `sink_exists(&st.connected_sink)` and `backend.module(st.connected_module)` as usual.

### 3.5 Windows `wasapi2src` Loopback & Hotplug Reconnect

- `CaptureEndpoint::SystemWasapiLoopback` uses `wasapi2src loopback=true low-latency=true provide-clock=true` on the default Windows render endpoint, so local speakers always keep playing (`keep_local` is inherent on Windows, matching `controller/session.rs` `L560-563`; `WindowsAudioBackend::move_app_streams` remains unsupported).
- All $N$ browser listeners share the single `wasapi2src` + `opusenc` pipeline.
- **Device hotplug / stream error recovery**: When the user plugs in headphones or changes the Windows default output device, `wasapi2src` can emit `gst::MessageView::Error` or `Eos`. `GstBroadcaster`'s `bus_thread` catches `Error`/`Eos` (when `!stop_flag`), tears down only the GStreamer pipeline, and runs a bounded restart loop (`250 ms -> 500 ms -> 1 s -> 2 s` cap, matching `ReconnectBackoff` in `backend/windows/audio.rs`) while keeping the HTTP/WebRTC/WS server and connected browser sockets alive. On restart, emit a `WS_FLAG_DISCONTINUITY` frame on WS and reset `sample_pos` continuity cleanly.

### 3.6 Lifecycle & `--no-default-features` Behavior

- **Lazy capture start / idle stop**: Binding `0.0.0.0:4717/tcp` and `0.0.0.0:4718/udp` (plus Linux virtual sink setup) happens immediately on `start_web_broadcast()`. The `GstBroadcaster` GStreamer pipeline starts lazily on the first listener (`subscriber_count: 0 -> 1`) and stops after a **5-second grace period** once `subscriber_count == 0` (preventing idle CPU/WASAPI usage when the user only has the QR card open).
- **Deadlock-free stop sequence** (matching `GstAudioSender::stop` in `gst_sender.rs` `L424-445`):
  1. `stop_flag.store(true, Ordering::Release)` and clear subscriber maps (waking subscriber threads).
  2. `bus.set_flushing(true)` *before* joining `bus_thread`.
  3. `bus_thread.join()`.
  4. `pipeline.set_state(gst::State::Null)` + `pipeline.state(Some(500 ms))`.
  5. Unload `combine_module` and `web_null_module` on Linux (`unload_module(..., true)`), restore `previous_default_sink` if standalone, and call `st.clear_web_broadcast()`.
- **`--no-default-features` (no GStreamer)**:
  - Gate `gstreamer` / `gstreamer_app` imports and `GstBroadcaster` behind `#[cfg(feature = "gstreamer")]`.
  - Under `#[cfg(not(feature = "gstreamer"))]`, provide a stub `GstBroadcaster::start(...)` returning `Err(TransportError::Pipeline("GStreamer transport support is disabled in this build".into()))`. `start_web_broadcast()` still binds/reports status or fails closed clearly depending on call path; `/webrtc/offer` returns `503 Service Unavailable` with `{"error":"GStreamer/Opus support disabled in this build"}`. (Optional future enhancement: WS-PCM could use `cpal`/`parec` without GStreamer, left out of Phase 1).

---

## 4. Config, State & `FirewallPorts`

### 4.1 Constants (`crates/soundhop-core/src/constants.rs`)

```rust
/// Virtual PulseAudio/PipeWire null-sink name for Web Broadcast audio capture.
pub const WEB_BROADCAST_SINK_NAME: &str = "soundhop_web_broadcast";

/// Default HTTP + WebSocket TCP port for Web Broadcast.
pub const DEFAULT_WEB_PORT: u16 = 4717;

/// Default single multiplexed UDP port for Web Broadcast WebRTC (ICE-Lite/DTLS/SRTP).
pub const DEFAULT_WEBRTC_PORT: u16 = 4718;

/// Default maximum concurrent browser listeners.
pub const DEFAULT_WEB_MAX_CLIENTS: u16 = 8;
```

### 4.2 `SharingConfig` (`crates/soundhop-core/src/config.rs`)

Ports `4713..4716` are currently used (`4713` PA-TCP, `4714` SHT3, `4715` Pairing, `4716` RTP). Web Broadcast allocates `4717/tcp` and `4718/udp`. Extend `SharingConfig` with backwards-compatible `#[serde(default = "...")]` fields so `Config::get_dotted`, `set_dotted`, and `apply_patch` work automatically:

```rust
fn default_web_port() -> u16 { DEFAULT_WEB_PORT }
fn default_webrtc_port() -> u16 { DEFAULT_WEBRTC_PORT }
fn default_web_require_token() -> bool { true }
fn default_web_keep_local() -> bool { true }
fn default_web_max_clients() -> u16 { DEFAULT_WEB_MAX_CLIENTS }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SharingConfig {
    pub enabled: bool,
    pub port: u16,
    pub v3_port: u16,
    pub pair_port: u16,
    #[serde(default = "default_rtp_port")]
    pub rtp_port: u16,
    pub security: String,
    pub allowed_networks: Vec<String>,
    pub psk_hash: String,

    // --- Web Broadcast ---
    /// Auto-start Web Broadcast when the daemon starts.
    #[serde(default)]
    pub web_enabled: bool,
    /// HTTP + WebSocket TCP port (default `4717`).
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    /// Single multiplexed WebRTC ICE-Lite UDP port (default `4718`).
    #[serde(default = "default_webrtc_port")]
    pub webrtc_port: u16,
    /// Require per-session bearer token (`#t=<token>`) for Web Broadcast access (default `true`).
    #[serde(default = "default_web_require_token")]
    pub web_require_token: bool,
    /// Keep playing audio on local PC speakers while Web Broadcast is active (default `true`).
    #[serde(default = "default_web_keep_local")]
    pub web_keep_local: bool,
    /// Automatically establish `adb reverse` when a USB Android device is ready (default `false`).
    #[serde(default)]
    pub web_adb_reverse: bool,
    /// Maximum simultaneous browser listeners (default `8`, clamped `[1..=32]`).
    #[serde(default = "default_web_max_clients")]
    pub web_max_clients: u16,
}
```

In `SharingConfig::default()`, initialize the new fields to their defaults above. In `Config::normalize()`:
- `if self.sharing.web_port == 0 { self.sharing.web_port = DEFAULT_WEB_PORT; }`
- `if self.sharing.webrtc_port == 0 { self.sharing.webrtc_port = DEFAULT_WEBRTC_PORT; }`
- `self.sharing.web_max_clients = self.sharing.web_max_clients.clamp(1, 32);`

### 4.3 `State` (`crates/soundhop-core/src/state.rs`)

Add runtime recovery fields to `State` (keeping the session auth token **ephemeral in memory** on `WebBroadcastManager` rather than writing plaintext secrets to `state.json`):

```rust
    #[serde(default)]
    pub web_broadcast_active: bool,
    #[serde(default)]
    pub web_port: u16,
    #[serde(default)]
    pub web_null_module: u32,
    #[serde(default)]
    pub web_adb_reverse_serials: Vec<String>,
```

Add helper:
```rust
    pub fn clear_web_broadcast(&mut self) {
        self.web_broadcast_active = false;
        self.web_port = 0;
        self.web_null_module = 0;
        self.web_adb_reverse_serials.clear();
    }
```

### 4.4 `FirewallPorts` & Packaging Rules (`crates/soundhop-core/src/firewall.rs`)

Extend `FirewallPorts` with optional fields so existing non-Web default assertions remain minimal while open/close and doctor handle Web Broadcast cleanly:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallPorts {
    pub legacy_audio_tcp: Option<u16>,
    pub control_tcp: u16,
    pub pair_tcp: u16,
    pub rtp_udp: u16,
    pub discovery_udp: u16,
    pub include_mdns: bool,
    /// Web Broadcast HTTP/WS TCP port (`None` when Web Broadcast is disabled).
    #[serde(default)]
    pub web_tcp: Option<u16>,
    /// Web Broadcast WebRTC ICE-Lite UDP port (`None` when Web Broadcast is disabled).
    #[serde(default)]
    pub webrtc_udp: Option<u16>,
}
```

- `FirewallPorts::default()`: `web_tcp: None`, `webrtc_udp: None`.
- `FirewallPorts::from_config(cfg: &Config)`:
  - `web_tcp: cfg.sharing.web_enabled.then_some(cfg.sharing.web_port)`
  - `webrtc_udp: cfg.sharing.web_enabled.then_some(cfg.sharing.webrtc_port)`
  - Also provide `FirewallPorts::from_config_with_web(cfg: &Config, web_active: bool, web_port: u16, webrtc_port: u16)` so GUI/CLI firewall actions can open ports when Web Broadcast is started ad-hoc even if `sharing.web_enabled == false`.
- `FirewallPorts::specs(&self)`:
  - `if let Some(port) = self.web_tcp { push_unique(FirewallPortSpec::new(port, PortProtocol::Tcp, "Web")); }` -> `4717/tcp`, Windows rule `"SoundHop Web (4717/tcp)"`.
  - `if let Some(port) = self.webrtc_udp { push_unique(FirewallPortSpec::new(port, PortProtocol::Udp, "WebRTC")); }` -> `4718/udp`, Windows rule `"SoundHop WebRTC (4718/udp)"`.
- `FirewallPorts::windows_close_specs(&self)`:
  - Populate `copy.web_tcp.get_or_insert(DEFAULT_WEB_PORT)` and `copy.webrtc_udp.get_or_insert(DEFAULT_WEBRTC_PORT)` (matching `copy.include_mdns = true`) so `close_ports` always cleans up created rules.
- **Call sites & tests to update**:
  - `crates/soundhop-core/src/firewall.rs` unit tests (`L2062-2108`).
  - `crates/soundhop-cli/src/doctor.rs` (`L824` struct literal).
  - `packaging/windows/soundhop.iss` (`CleanupFirewallRules()` around `L201-206`): delete `"SoundHop Web (4717/tcp)"` and `"SoundHop WebRTC (4718/udp)"`.

---

## 5. Web / WebRTC Server & Wire Protocol

### 5.1 Cargo Dependencies

Add to root `Cargo.toml` (`[workspace.dependencies]`) and `crates/soundhop-core/Cargo.toml`:

```toml
str0m = { version = "0.25", default-features = false, features = ["rust-crypto"] }
tungstenite = { version = "0.30", default-features = false }
qrcode = { version = "0.15", default-features = false }
```

Rationale:
- `str0m` (`0.25`) with `rust-crypto` selects pure-Rust crypto (`str0m-rust-crypto` + `dimpl`), avoiding OpenSSL or `aws-lc-rs`/CMake build requirements in AppImage/Windows CI.
- `tungstenite` (`0.30`) with `default-features = false` provides synchronous `Read + Write` WebSocket handling with zero `tokio` runtime dependency.
- `qrcode` (`0.15`) with `default-features = false` avoids pulling duplicate `image` features into `soundhop-core` while exposing `QrCode::new()`, `.render::<char>()` (CLI/TUI), and `.to_colors()` (GUI).
- **HTTP server**: Use raw `std::net::TcpListener` + bounded worker pool + small sync HTTP/1.1 parser (~120 LOC matching `PairingServer::start` in `crates/soundhop-core/src/pairing.rs`). **Important WS upgrade detail**: Do not consume request bytes into a `BufReader<TcpStream>` and then pass the bare `TcpStream` to `tungstenite::accept_hdr(stream, cb)` (that loses buffered/read header bytes and hangs the handshake). Instead, either:
  1. Wrap a `PrefixedStream { head: std::io::Cursor<Vec<u8>>, inner: TcpStream }` (`impl Read + Write`) preserving the already-read request bytes and pass `PrefixedStream` to `tungstenite::accept_hdr`, **or**
  2. Read headers via `BufReader`, inspect `Upgrade: websocket` + `Sec-WebSocket-Key`, write the `101 Switching Protocols` response using `tungstenite::handshake::derive_accept_key`, and wrap `buf_reader.into_inner()` via `WebSocket::from_raw_socket(stream, Role::Server, None)` (preferred when `buf_reader.buffer()` is empty after `\r\n\r\n`).

### 5.2 Synchronous Threading Model (No `tokio`)

`str0m::Rtc` is Sans-I/O and `!Sync`, requiring single-thread ownership of all `Rtc` state:

1. **`web-http-accept` thread**:
   - Binds `TcpListener::bind(("0.0.0.0", http_port))` (non-blocking accept loop checking `stop_flag`, matching `PairingServer::start`).
   - Dispatches each accepted `TcpStream` to a bounded worker pool (max `16` concurrent HTTP workers, `5 s` read/write timeouts) for `GET /`, `GET /api/info`, `POST /webrtc/offer`, `DELETE /webrtc/peer/:id`, or hands off upgraded `/ws` streams to `web-ws-fanout`.
2. **`web-rtc-mux` thread** (single dedicated `std::thread`):
   - Owns the single `UdpSocket::bind(("0.0.0.0", webrtc_udp_port))` and `HashMap<PeerId, PeerRtc>`.
   - Receives control messages (`AddOffer { peer_id, offer_sdp, client_info, reply_tx }`, `RemovePeer { peer_id }`, `Stop`) and Opus `Arc<BroadcastFrame>` packets via `std::sync::mpsc`.
   - **Loopback UDP poke & timeout safety**: Whenever a sender pushes to the control or Opus channel, it sends a 1-byte UDP datagram to `127.0.0.1:<webrtc_udp_port>` from a dedicated `poke_socket` (with known `poke_addr`). Before blocking in `udp_socket.recv_from(&mut buf)`, compute the earliest `next_timeout` across all peers: if `next_timeout <= now`, drive `rtc.handle_input(Input::Timeout(now))` and drain `poll_output()` **before** calling `recv_from`. Never pass `Some(Duration::ZERO)` to `set_read_timeout` (which disables the timeout and blocks indefinitely on Linux/Windows); clamp any positive timeout to `>= Duration::from_millis(1)` (or `Some(Duration::from_millis(250))` when idle so `stop_flag` and handshake deadlines are still checked). Packets from `poke_addr` are discarded immediately while draining all pending channel commands/frames.
3. **`web-ws-fanout` thread(s)**:
   - Maintains active `WebSocket<TcpStream>` fallback clients with a shared WS-writer loop (non-blocking / short write timeout `500 ms` with `TCP_NODELAY = true`) and a lightweight reader/heartbeat thread per client (or `poll` / `SO_RCVTIMEO`) for `ping`/`pong` and clean close detection.

### 5.3 `str0m` ICE-Lite Single-UDP-Port Multiplexing & Media Injection

#### `PeerRtc` Creation & Explicit LAN Candidates
```rust
let mut rtc = RtcConfig::new()
    .set_ice_lite(true)
    .set_stats_interval(Some(Duration::from_secs(2)))
    .build(Instant::now());

for ip_str in crate::util::local_addresses(false) {
    if let Ok(IpAddr::V4(v4)) = ip_str.parse::<IpAddr>() {
        let cand = Candidate::host(SocketAddr::new(IpAddr::V4(v4), webrtc_udp_port), "udp")?;
        let _ = rtc.add_local_candidate(cand);
    }
}
```
- Using `local_addresses(false)` (already in `util.rs`) filters out loopback and virtual bridge interfaces (`docker0`, `virbr0`, `veth*`, etc.) and prioritizes `192.168.*` / `10.*` LAN IPv4 addresses.
- ICE-Lite ensures the PC responds to browser STUN connectivity checks without sending outbound STUN checks or needing external STUN/TURN servers—making Chrome's `.local` mDNS candidate obfuscation harmless.

#### Single-Port UDP Demultiplexing & Peer Cleanup
Because all browser peers share `0.0.0.0:<webrtc_udp_port>`:
1. **Fast path**: Look up `source: SocketAddr` in `addr_to_peer: HashMap<SocketAddr, PeerId>`.
2. **Cold path / NAT rebind**: On unknown `source`, inspect if the UDP packet is a STUN Binding Request (first 2 bits `00`, magic cookie `0x2112A442` at bytes `[4..8]`). Parse the STUN `USERNAME` attribute (`0x0006`) formatted as `"<local_ufrag>:<remote_ufrag>"`, split on `':'`, and look up `local_ufrag` in `ufrag_to_peer: HashMap<String, PeerId>` (extracted from the SDP Answer's `a=ice-ufrag:` when `accept_offer` completes). Record `addr_to_peer.insert(source, peer_id)` and forward the datagram.
3. Unrecognized non-STUN packets from unknown `SocketAddr`s are dropped silently.
4. **Peer eviction & index purging**: Whenever a peer is removed via `DELETE /webrtc/peer/:id`, `web_kick`, `IceConnectionState::Disconnected`, or fails to reach `IceConnectionState::Connected` within a `10 s` ICE/DTLS handshake deadline, remove the peer from `peers` **and** purge all matching entries in both `addr_to_peer` and `ufrag_to_peer`.

#### SDP Negotiation, Media Injection & Poll/Drain Contract
1. Parse incoming offer SDP via `SdpOffer::from_sdp_string(&offer_sdp)` and call `let answer = rtc.sdp_api().accept_offer(offer)?;`. Validate that the offer contains a compatible audio media section (reject oversized/invalid SDPs with HTTP `400`).
2. When `rtc.poll_output()` emits `Event::MediaAdded(m)`, store `peer.audio_mid = Some(m.mid)`.
3. For each 20 ms (`960`-sample) Opus `Arc<BroadcastFrame>`:
   - Advance `peer.sample_idx += u64::from(frame.duration_samples)` (`960`).
   - Only when `peer.ice_connected` (`Event::IceConnectionStateChange(IceConnectionState::Connected)`) and `peer.audio_mid` is `Some(mid)`:
     - Obtain `let mut writer = rtc.writer(mid)?;`
     - Select the negotiated Opus `Pt` (`payload_params()` matching Opus `48000/2`, typically PT `111`).
     - Call `writer.write(pt, frame.captured_at, MediaTime::new(peer.sample_idx, Frequency::FORTY_EIGHT_KHZ), &frame.data)`.
4. **Mandatory poll/drain contract**: After *every* `rtc.handle_input(Input::Receive(...))`, `Input::Timeout(now)`, or `writer.write(...)`, loop `rtc.poll_output()` until `Output::Timeout(t)`:
   - `Output::Transmit(tx)` -> `udp_socket.send_to(&tx.contents, tx.destination)` (updating `bytes_sent`).
   - `Output::Event(ev)` -> handle `IceConnectionStateChange`, `MediaAdded`, `PeerStats` (updating `WebClientInfo.rtt_ms` from `stats.rtt`), etc.
   - `Output::Timeout(t)` -> update `peer.next_timeout = t` and break.

### 5.4 HTTP Routes & Authentication Flow

#### URL & Auth Token Flow
- **Token format**: 128-bit CSPRNG token encoded as 32-character lowercase hex (`hex::encode(random_16_bytes)` via `rand::thread_rng()` + already-present `hex` crate, adding zero extra encoding crates), held in memory on `WebBroadcastManager` (rotated on `web_rotate_token` or restart).
- **Share URL**:
  - When `web_require_token == true`: `http://<lan-ip>:<web_port>/#t=<token>` (or `http://localhost:<web_port>/#t=<token>` for `adb reverse`). Using URL fragment `#t=` prevents the token from appearing in HTTP request lines, server logs, or `Referer` headers on `GET /`.
  - When `web_require_token == false`: `http://<lan-ip>:<web_port>/`.
- **SPA extraction & forwarding**:
  - On load, JS extracts `#t=` (also accepting `#token=` as a lenient alias) from `window.location.hash`, stores it in memory, and immediately strips the hash via `history.replaceState(null, "", location.pathname + location.search)`.
  - If no token is present and `/api/info` returns `authRequired: true`, the SPA shows a manual token prompt box with a **Scan QR** button.
  - **QR re-pairing**: The header scan button (and the auth-panel button) opens a scanner overlay. Live camera preview uses `getUserMedia` where available; a `BarcodeDetector` and the vendored jsQR decoder (`GET /jsqr.js`) decode camera frames or a user-selected image file. Only a 32-hex lowercase token, or a same-scheme/same-port URL with `#t=`/`#token=` carrying one, is accepted — the scanned URL is never navigated to. Applying a scanned token re-validates it via `/api/info`; if the device had been listening or kicked, playback resumes automatically. This is the recovery path after a host token rotation or a host-initiated kick.
  - All HTTP API requests (`GET /api/info`, `POST /webrtc/offer`, `DELETE /webrtc/peer/:id`) carry `Authorization: Bearer <token>`.
  - Browser `WebSocket` constructors cannot set custom `Authorization` headers; `/ws` authenticates via **`Sec-WebSocket-Protocol: soundhop.v1, auth.<token>`** (server echoes `Sec-WebSocket-Protocol: soundhop.v1`) OR, if `auth.<token>` subprotocol is absent, awaits a single JSON text frame `{"type":"auth","token":"<token>"}` within `2 s` before sending audio frames. If the `2 s` deadline expires, a non-auth frame arrives first, or the token is invalid, the server sends a WebSocket Close frame (`1008` Policy Violation) and closes the socket. A **host-initiated kick** instead uses close code `4003` (reason `kicked by host`), which the SPA treats as terminal.
  - All token comparisons use `crate::security::constant_time_eq` (`crates/soundhop-core/src/security.rs:59`).

#### HTTP Endpoints

| Method & Path | Auth | Request / Response |
| :--- | :--- | :--- |
| `GET /` | Public | `200 OK`, `Content-Type: text/html; charset=utf-8`, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff` -> embedded `web_player.html` |
| `GET /jsqr.js` | Public | `200 OK`, `Content-Type: application/javascript; charset=utf-8` -> vendored jsQR decoder used by the QR scanner |
| `GET /api/info` | Bearer (if `authRequired`, returns minimal `{"authRequired":true}` when header absent—see note below) | `200 OK` JSON: `{"version": VERSION /* crate::constants::VERSION */,"deviceName":"...","sampleRate":48000,"channels":2,"frameSamples":960,"codec":"opus","wsCodec":"pcm16le","webrtc":true,"ws":true,"authRequired":bool}` |
| `POST /webrtc/offer` | Bearer | Req (`<= 64 KiB`): `{"type":"offer","sdp":"...","clientName":"Android Chrome"}` <br> `200 OK`: `{"type":"answer","sdp":"...","peerId":"<id>","iceLite":true}` <br> `400` invalid SDP, `401` bad token, `429` rate limit, `503` max clients or GStreamer disabled |
| `DELETE /webrtc/peer/<id>` | Bearer | `204 No Content` (called via `fetch(..., { keepalive: true })` on `pagehide` / Stop) |
| `GET /ws` | Subprotocol / first WS frame | `101 Switching Protocols` (`Sec-WebSocket-Protocol: soundhop.v1`) -> binary PCM16LE stream |

*Note on `GET /api/info`*: To let the SPA detect whether to display a token input box when opened without `#t=`, `GET /api/info` returns `200` with `{"authRequired":true}` (omitting `deviceName` and capabilities) when unauthenticated, or full metadata once `Authorization: Bearer <token>` succeeds (returning `401` only if a *wrong* token was supplied).

### 5.5 Binary WebSocket Frame Format (`/ws`)

Every 20 ms audio chunk is sent as a single WebSocket **Binary** frame (`16`-byte little-endian header + `3840`-byte PCM16LE payload = `3856` bytes/frame $\approx 192.8\text{ KB/s}$):

```text
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|   Magic 0x53  |   Magic 0x48  |  Version 0x01 |     Flags     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                        seq (u32 LE)                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
+                      sample_pos (u64 LE)                      +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|         Interleaved PCM16LE samples (3840 bytes = 960*2*2)    |
~                                                               ~
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

- **Magic (`[0..2]`)**: `[0x53, 0x48]` (`"SH"`).
- **Version (`[2]`)**: `0x01`.
- **Flags (`[3]`)**: `bit 0` (`0x01`) = `PCM16LE 48 kHz stereo`; `bit 1` (`0x02`) = `discontinuity / stream reset` (re-anchor client jitter buffer).
- **Heartbeat / RTT Text Frames**: Client sends `{"type":"ping","t":<ms>}`; server replies `{"type":"pong","t":<ms>}` and updates `WebClientInfo.rtt_ms` if client reports `{"type":"stats","rttMs":<f64>}`.

---

## 6. Zero-Build Embedded Browser SPA (`web_player.html`)

Delivered as a single inline HTML document (`<style>` + `<script>`) compiled into `soundhop-core` with `include_str!`.

### 6.1 Tap-to-Listen UX & `MediaSession` Integration

- Mobile browsers block unmuted WebAudio/HTMLMediaElement autoplay without a synchronous user gesture.
- Clicking the single prominent **Listen** button synchronously inside the `click` handler:
  1. Instantiates/resumes `new AudioContext({ latencyHint: 'interactive', sampleRate: 48000 })`.
  2. Creates/unlocks a hidden `<audio playsinline>` element (required on Android Chrome & iOS Safari for background playback and media notification integration).
  3. Registers `navigator.mediaSession`:
     - `metadata`: `new MediaMetadata({ title: info.deviceName, artist: 'SoundHop Web Broadcast', album: 'Live Audio' })`
     - `setActionHandler('play', ...)` / `'pause'` / `'stop'` wired to mute/resume or clean disconnect.

### 6.2 Primary Transport: WebRTC (`recvonly`)

1. Construct `pc = new RTCPeerConnection({ iceServers: [] })`.
2. Add `pc.addTransceiver('audio', { direction: 'recvonly' })` (requests **zero** microphone permissions).
3. On `pc.ontrack`: attach `event.streams[0]` to `<audio>.srcObject`, call `audio.play()`, and set `event.receiver.playoutDelayHint = 0.05` when supported.
4. Call `createOffer()` -> `setLocalDescription(offer)`. Wait for `iceGatheringState === 'complete'` OR a `400 ms` gathering cap (whichever comes first, avoiding slow `.local` mDNS stalls).
5. `POST /webrtc/offer` with `Authorization: Bearer <token>`, then `setRemoteDescription(answer)`.
6. **Automatic fallback triggers** (within `3.0 s` of offer):
   - `window.RTCPeerConnection` unavailable, OR
   - `POST /webrtc/offer` returns `503` (e.g. GStreamer disabled), OR
   - `iceConnectionState` / `connectionState` enters `failed` / `disconnected` or fails to reach `connected` within `3.0 s` (typical over `adb reverse` TCP-only USB or strict UDP firewalls).
   - On trigger: close `pc` cleanly (`DELETE /webrtc/peer/:id`), keep `AudioContext` alive, and immediately start the `ws://` fallback.

### 6.3 Automatic `ws://` Fallback & `localhost` / `adb reverse` `AudioWorklet` Upgrade

The SPA selects the WebSocket audio renderer based on `window.isSecureContext`:

1. **Plain HTTP over LAN (`!window.isSecureContext`, e.g. `http://192.168.1.10:4717`)**:
   - `AudioWorklet` is unavailable in insecure contexts in Chrome/Firefox.
   - Parse each binary WS frame (`DataView`), verify `0x53, 0x48` magic and version `0x01`, convert `Int16` interleaved PCM to planar `Float32Array` (`/ 32768.0`) in an `AudioBuffer(2, 960, 48000)`, schedule `AudioBufferSourceNode` -> `GainNode` -> `MediaStreamAudioDestinationNode` -> `<audio>.srcObject` (preserving Android Chrome `MediaSession` & screen-off playback).
   - Provide an optional **"Direct Output"** toggle in advanced settings routing `GainNode -> audioCtx.destination` if a specific mobile browser exhibits quirks with `MediaStreamAudioDestinationNode`.

2. **Secure Context (`window.isSecureContext === true`, e.g. `http://localhost:4717` via `adb reverse`)**:
   - Create an inline `AudioWorkletProcessor` script via `new Blob([WORKLET_JS], { type: 'application/javascript' })` and `URL.createObjectURL(blob)` -> `await audioCtx.audioWorklet.addModule(blobUrl)`.
   - Maintain a `Float32Array` SPSC ring buffer inside the worklet (`128`-frame WebAudio quantum, capacity `~16,384` stereo frames) fed via `workletNode.port.postMessage(pcmFloat32, [pcmFloat32.buffer])` (zero-copy `Transferable`).
   - Route `workletNode -> GainNode -> MediaStreamAudioDestinationNode -> <audio>` (or direct `audioCtx.destination` for minimal USB latency).

### 6.4 Jitter / Drift Control & Reconnect UX

- **Target buffer**: `60 ms` (`3` frames), operating window `[40 ms .. 160 ms]`.
- **`AudioBufferSourceNode` scheduling**:
  - Maintain `nextTime` anchored to `audioCtx.currentTime + 0.06`.
  - **Underrun** (`nextTime < audioCtx.currentTime + 0.015` or `WS_FLAG_DISCONTINUITY`): re-anchor `nextTime = audioCtx.currentTime + 0.06`.
  - **Drift / bloat** (`nextTime - audioCtx.currentTime > 0.16`): drop incoming frame(s) or re-anchor with a `5 ms` `GainNode` micro-fade to avoid clicks.
- **`AudioWorklet` ring control**:
  - Track ring fill in samples; drop/skip oldest frames or apply `1.005x` linear resampling when fill exceeds `140 ms`; output smooth `5 ms` faded silence on underrun and re-prime to `60 ms` before unmuting.
- **Reconnect & Controls UX**:
  - Exponential backoff on unexpected drop: `0.5 s -> 1 s -> 2 s -> 5 s` (capped at `5 s`), with immediate retry on `document.visibilitychange` (`visible`) and `window.online`.
  - **Kick is terminal**: WS close `4003` and the in-band WebRTC control-channel message `{"type":"kicked"}` (sent on the browser-created `sh-control` data channel before the host closes the peer) both call `stopListening(..., true)`. That clears `shouldListen` and sets `kickedByHost`, so no reconnect path — backoff, `visibilitychange`, `online`, or transport fallback — can resurrect the session. The user must tap Listen (or scan a fresh QR token) to resume.
  - Live transport pill badge: `WebRTC · Opus`, `WS · PCM`, or `WS Worklet · USB`.
  - Live buffer/RTT readout, volume slider + mute button, manual transport switcher (`Auto` / `Force WS`), and `pagehide` cleanup (`fetch('/webrtc/peer/' + peerId, { method: 'DELETE', keepalive: true, headers })`).

---

## 7. Daemon IPC, `AndroidManager` `adb reverse`, CLI, TUI & `egui`/Tray UI

### 7.1 `Controller` & Status Payload (`crates/soundhop-core/src/controller/`)

Add `pub web_broadcast: Arc<WebBroadcastManager>` to `Controller` (`controller/mod.rs`) and typed status structs:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebClientInfo {
    pub id: String,
    pub remote_addr: String,
    pub transport: String, // "webrtc" | "ws"
    pub user_agent: String,
    pub connected_since: f64,
    pub bytes_sent: u64,
    pub rtt_ms: Option<f64>,
    pub dropped_frames: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebBroadcastStatus {
    pub active: bool,
    pub http_port: u16,
    pub webrtc_udp_port: u16,
    pub urls: Vec<String>,
    pub localhost_url: String,
    pub token_required: bool,
    pub token: Option<String>,
    pub clients: Vec<WebClientInfo>,
    pub adb_reversed_serials: Vec<String>,
    pub keep_local: bool,
    pub pipeline_running: bool,
    pub capture_source: String,
}
```

- Expose on `Controller` (`controller/web.rs`):
  - `start_web_broadcast(port: Option<u16>, keep_local: Option<bool>, require_token: Option<bool>) -> Result<WebBroadcastStatus, ControllerError>`
  - `stop_web_broadcast() -> Result<WebBroadcastStatus, ControllerError>`
  - `rotate_web_token() -> Result<WebBroadcastStatus, ControllerError>`
  - `kick_web_client(client_id: &str) -> Result<bool, ControllerError>`
  - `web_broadcast_status() -> WebBroadcastStatus`
  - `list_web_clients() -> Vec<WebClientInfo>`
  - `web_adb_reverse(serial: Option<&str>, port: Option<u16>, enable: bool, open_browser: bool) -> Result<WebBroadcastStatus, ControllerError>`
- Include `"web_broadcast": self.web_broadcast_status()` in `Controller::status()` (`controller/mod.rs:572-630`).

### 7.2 Daemon & IPC (`crates/soundhop-core/src/daemon.rs`)

Add `Daemon::handle_ipc()` (`daemon.rs:1061`) match arms (canonical `web_*` names plus `broadcast_*` aliases for `start`/`stop`/`status`):

| IPC `"cmd"` | Request Fields | Response Payload |
| :--- | :--- | :--- |
| `"web_start"` / `"broadcast_start"` | `{ port?: u16, keep_local?: bool, require_token?: bool, adb_reverse?: bool, serial?: String, open_browser?: bool }` | `{ ok: true, web_broadcast: WebBroadcastStatus }` |
| `"web_stop"` / `"broadcast_stop"` | `{}` (removes any tracked `adb reverse` tunnels and stops server + pipeline) | `{ ok: true, web_broadcast: WebBroadcastStatus }` |
| `"web_status"` / `"broadcast_status"` | `{}` | `{ ok: true, web_broadcast: WebBroadcastStatus }` |
| `"web_rotate_token"` | `{}` | `{ ok: true, token: Option<String>, urls: Vec<String>, web_broadcast: WebBroadcastStatus }` |
| `"web_kick"` | `{ client_id: String }` | `{ ok: true, kicked: bool, web_broadcast: WebBroadcastStatus }` |
| `"web_adb_reverse"` | `{ serial?: String, port?: u16, enable: bool, open_browser?: bool }` | `{ ok: true, web_broadcast: WebBroadcastStatus }` |

Additional daemon hooks:
- `Daemon::startup()` / `setup_sharing()`: If `cfg.sharing.web_enabled == true`, auto-start Web Broadcast; also clean up any leftover `state.web_adb_reverse_serials` from an unclean prior shutdown if `web_enabled == false`.
- `Daemon::shutdown()` (`daemon.rs:261-299`): Remove active `adb reverse` tunnels and call `self.controller.stop_web_broadcast()`.
- `Daemon::handle_ipc()` `"config_patch"` / `"reload"` (`daemon.rs:1514-1536`): Restart/rebind `web_broadcast` if `sharing.web_port`, `sharing.webrtc_port`, or `sharing.web_require_token` changed while active.
- `Daemon::run_android_supervisor_tick()` (`daemon.rs:562`): Prune `web_adb_reverse_serials` if a tracked USB serial disappears from `list_devices()`.

### 7.3 `AndroidManager` `adb reverse` (Decoupled from `scrcpy`)

In `crates/soundhop-core/src/android.rs`:
- **Decouple `ToolStatus::ready()`**:
  - Add `pub fn adb_ready(&self) -> bool { self.adb_path.is_some() }`.
  - Add `pub fn scrcpy_ready(&self) -> bool { self.scrcpy_path.is_some() && self.adb_path.is_some() }`.
  - Keep `pub fn ready(&self) -> bool { self.scrcpy_ready() }` for backward compatibility.
  - Update `Controller::android_tool_status()` (`controller/android.rs:8-18`) to emit `"adb_ready": st.adb_ready()` and `"scrcpy_ready": st.scrcpy_ready()` alongside `"ready": st.ready()`.
- **Reverse tunnel & browser launch APIs on `AndroidManager`**:
  - Track active tunnels in `Mutex<BTreeSet<(String, u16)>>` (`(serial, remote_port)`).
  - `setup_reverse_tunnel(&self, serial: &str, remote_port: u16, local_port: u16) -> Result<(), AndroidError>`: validates `validate_adb_target(serial)`, requires `remote_port > 0 && local_port > 0`, runs `adb -s <serial> reverse tcp:<remote_port> tcp:<local_port>`, records `(serial, remote_port)`.
  - `remove_reverse_tunnel(&self, serial: &str, remote_port: Option<u16>) -> Result<(), AndroidError>`: runs `adb -s <serial> reverse --remove tcp:<port>` (or `--remove-all` if `None`), ignoring device-offline / not-found errors cleanly during teardown.
  - `list_reverse_tunnels(&self, serial: &str) -> Result<Vec<String>, AndroidError>`: runs `adb -s <serial> reverse --list`.
  - `open_browser_url(&self, serial: &str, url: &str) -> Result<(), AndroidError>`: runs `adb -s <serial> shell am start -a android.intent.action.VIEW -d <url>` after strict allowlist validation (Section 8.4).
  - Call `remove_reverse_tunnel` during `web_stop`, `Daemon::shutdown()`, and `AndroidManager::disconnect_wifi()`.

### 7.4 CLI & TUI (`crates/soundhop-cli/`)

- **Parser (`src/parser.rs`)**: Add `Commands::Web(WebArgs)` with `#[command(visible_alias = "broadcast")]`:
  - `soundhop web start [--port <u16>] [--keep-local | --no-keep-local] [--no-token] [--adb] [--serial <id>] [--open]`
  - `soundhop web stop`
  - `soundhop web status` (prints active state, HTTP/WebRTC ports, LAN & localhost URLs, token, and listener table with transport/RTT/bytes/drops)
  - `soundhop web qr [--adb]` (renders UTF-8 half-block QR code using `qrcode::render::unicode::Dense1x2` + printable URL)
  - `soundhop web rotate-token`
  - `soundhop web kick <client_id>`
- **`soundhop status` (`src/core_cmds/connect.rs`)**: Print active Web Broadcast URL & listener count when `status["web_broadcast"]["active"] == true`.
- **TUI (`src/tui.rs`)**: Add a Web Broadcast panel (either as a 4th tab `TuiTab::WebBroadcast` or dedicated section in `TuiTab::MicAndAndroid`) displaying On/Off status, compact half-block QR + URL, listener list (`remote_addr`, `[WebRTC]` / `[WS]`, RTT/bytes), and keybindings (`b` toggle broadcast, `t` rotate token, `r` toggle `adb reverse` + open browser).

### 7.5 `egui` Control Center & Tray (`crates/soundhop-gui/`)

- **Backend (`backend/snapshot.rs`, `backend/commands.rs`, `backend/optimistic.rs`)**:
  - Add `WebClientView` and `GuiSnapshot` fields: `web_broadcast_active: bool`, `web_http_port: u16`, `web_webrtc_port: u16`, `web_urls: Vec<String>`, `web_localhost_url: String`, `web_token: Option<String>`, `web_token_required: bool`, `web_clients: Vec<WebClientView>`, `web_adb_reversed: Vec<String>`, `web_keep_local: bool`.
  - Hydrate in `RefreshUpdate::from_status()` from `status_json["web_broadcast"]` and update `apply_to()` diff checks.
  - Add `GuiCommand` variants: `StartWebBroadcast { keep_local: bool }`, `StopWebBroadcast`, `RotateWebToken`, `KickWebClient { client_id: String }`, `WebAdbReverse { serial: String, enable: bool, open_browser: bool }`.
- **Home View (`views/home.rs`)**:
  - Add a **Web Broadcast (Listen on Phone)** card below the speaker-sharing section (`L509-577`):
    - `toggle_switch` for Web Broadcast On/Off + `"Keep playing on PC"` checkbox/toggle.
    - Crisp QR code rendered via `egui::ColorImage` + `ctx.load_texture(..., TextureOptions::NEAREST)` cached in `HomeViewState` keyed by `(url, token)`.
    - Clickable/copyable URL (`ui.ctx().copy_text(url)`) + `"Rotate Token"` `secondary_button`.
    - Live listener list with `pill_badge` (`"WebRTC"` in `pal.success`/`pal.success_soft` vs `"WebSocket"` in `pal.warning`/`pal.warning_soft`), remote IP, User-Agent summary, RTT/dropped frames, and `"Kick"` `compact_button`.
- **Phone View (`views/phone.rs`)**:
  - Gate `"adb reverse (USB Low-Latency)"` toggle + `"Open in Phone Browser"` button on `snap.android_tools.adb_ready()` (NOT `ready()`, so `scrcpy` is not required).
- **Navigation Rail (`app.rs`) & System Tray (`tray.rs`)**:
  - Show `"Web Broadcast Live (N)"` `pill_badge` in `app.rs` bottom status rail when active.
  - Extend `TrayMenuSnapshot`, `build_tray_menu_from_snapshot()`, and `format_tray_tooltip()` with `"Web Broadcast (Listen on Phone)"` toggle, `"Copy Listen URL"`, and active listener count.

---

## 8. Security & Threat Model

| Threat Vector | Mitigation |
| :--- | :--- |
| **Unauthorized LAN listener eavesdropping** | Session bearer token enabled by default (`sharing.web_require_token = true`). 128-bit CSPRNG URL-safe token checked via `constant_time_eq`. Delivered in URL fragment (`#t=`) so it stays out of HTTP request lines and `Referer` headers. One-click rotation (`web_rotate_token`) invalidates old links and optionally drops active unauthenticated/revoked sessions. |
| **Subnet / non-LAN exposure** | Peer IP on TCP accept (`http`, `ws`) and UDP (`str0m` / `/webrtc/offer`) must belong to `cfg.sharing.allowed_networks` (same `IpNet` LAN allowlist used by `SHT3` / `PairingServer`, plus `127.0.0.1` / `::1` for `adb reverse`). Requests from outside `allowed_networks` receive `403 Forbidden`. |
| **Cross-Origin / DNS rebinding attacks** | Reject HTTP/WS requests whose `Origin` header (when present) is not `http://<local-ip>:<web_port>`, `http://localhost:<web_port>`, or `http://127.0.0.1:<web_port>`, and validate `Host` header against local listen port/addresses. |
| **Resource exhaustion / DoS** | Enforce `sharing.web_max_clients` (default `8`, max `32`) across WebRTC + WS combined (`503` when full); cap HTTP header size (`8 KiB`) and `/webrtc/offer` body size (`64 KiB`); rate-limit `POST /webrtc/offer` to `5 req / 10 s` per remote IP (`429`); bound HTTP worker threads (`16`) and socket timeouts (`5 s`); evict slow WS subscribers after `250` consecutive dropped frames (`~5 s`). |
| **`adb shell am start` command injection** | `adb shell` passes arguments through Android `/system/bin/sh`. `AndroidManager::open_browser_url()` enforces an exact prefix allowlist (`http://localhost:<port>/` or `http://127.0.0.1:<port>/`) AND restricts the entire URL string to `[A-Za-z0-9_.-~:/?=#&]` before calling `run_adb`, rejecting any spaces, quotes, `;`, `$`, backticks, or control characters. Device serials pass `validate_adb_target()`. |
| **Token leakage to disk** | Keep the ephemeral Web Broadcast token in memory only (never written to `state.json` or `config.toml`), and never log the token string in `tracing` or `append_log_line()`. |

---

## 9. Phased Implementation Plan & Verification Checklist

### Phase 1: Core Config, State, Firewall & `GstBroadcaster`
- Add constants (`constants.rs`), `SharingConfig` fields + `normalize()` (`config.rs`), `State` fields + `clear_web_broadcast()` (`state.rs`), and `FirewallPorts` fields + tests (`firewall.rs`, `doctor.rs`, `packaging/windows/soundhop.iss`).
- Update `REQUIRED_CORE_ELEMENTS` (`gst_runtime.rs`), make `build_capture_source` `pub(crate)` (`gst_sender.rs`), and implement `GstBroadcaster` + `#[cfg(not(feature = "gstreamer"))]` stub (`gst_broadcaster.rs`).
- **Unit tests**: Config TOML backward compatibility + `normalize()`, `FirewallPorts` specs/cleanup, `GstBroadcaster` `TestSineWave` dual-`appsink` frame emission and slow-subscriber eviction.

### Phase 2: `soundhop-core::web` Server, `str0m` Mux, WS Fan-Out & Embedded SPA
- Add `str0m`, `tungstenite`, `qrcode` to `Cargo.toml` and `crates/soundhop-core/Cargo.toml`.
- Implement `web/http.rs`, `web/rtc.rs`, `web/ws.rs`, `web/qr.rs`, `web/mod.rs`, and `web/web_player.html`.
- **Integration tests**: HTTP auth (`401`/`200`), `POST /webrtc/offer` SDP negotiation with `str0m` headless client peer, STUN `USERNAME` demux, binary WS frame framing/header verification, rate/client caps, `Origin`/subnet rejection.

### Phase 3: Controller, Daemon IPC & `AndroidManager` `adb reverse`
- Implement `controller/web.rs`, Linux `soundhop_web_broadcast` + `keep_local` combine-sink routing, `reconcile()`, and `Daemon::handle_ipc()` / `shutdown()` / supervisor hooks.
- Decouple `ToolStatus::adb_ready()` and add `setup_reverse_tunnel`, `remove_reverse_tunnel`, `list_reverse_tunnels`, `open_browser_url` with fake-runner unit tests in `android.rs`.
- Extend `crates/soundhop-core/tests/daemon_ipc_integration.rs` with `web_start`, `web_status`, `web_rotate_token`, `web_stop` round-trips.

### Phase 4: CLI, TUI, `egui` Control Center & Tray
- Add `soundhop web` subcommands (`parser.rs`, `core_cmds/web.rs`, `connect.rs`) and TUI controls (`tui.rs`).
- Wire `GuiSnapshot`, `GuiCommand`, `optimistic.rs`, `views/home.rs` QR card, `views/phone.rs` `adb_ready()` USB reverse controls, `app.rs` badge, and `tray.rs` menu items.

### Automated CI Verification Checklist (from `AGENTS.md`)
Run after each phase and before final completion:
- `cargo fmt --all -- --check`
- `cargo xtask bump-version --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test --workspace --locked`
- `cargo check -p soundhop-core --no-default-features` and `cargo check -p soundhop-cli --no-default-features` (verifies `--no-default-features` compiles cleanly without GStreamer)

### Manual Cross-Platform & Mobile Test Matrix

| Scenario | Client / OS | Expected Transport & Behavior |
| :--- | :--- | :--- |
| **LAN Wi-Fi (Default)** | Android Chrome (LAN IP `http://<ip>:4717/#t=...`) | Tap **Listen** -> WebRTC `recvonly` Opus (`4718/udp`) connects `< 1s`, `WebRTC · Opus` badge, lock-screen `MediaSession` controls work, screen-off playback continues |
| **LAN Wi-Fi (Firefox)** | Android Firefox (LAN IP) | WebRTC `recvonly` Opus connects cleanly; `MediaSession` play/pause works |
| **Blocked UDP / Strict Firewall** | Android Chrome (`4718/udp` blocked) | WebRTC times out at `3.0 s` -> seamless auto-fallback to `ws://` `AudioBufferSourceNode` (`WS · PCM`), audio resumes within ~1 s without extra tap |
| **USB `adb reverse`** | Android Chrome (`http://localhost:4717/#t=...`) | `adb_ready()` works without `scrcpy` installed; WebRTC UDP unavailable over USB -> auto-fallback upgrades to `AudioWorklet` (`WS Worklet · USB`), `< 80 ms` latency; unplugging USB prunes `adb reverse` cleanly |
| **Multi-listener fan-out** | 2 phones + 1 laptop browser simultaneously | Single `GstBroadcaster` pipeline serves all 3 (`1` WebRTC + `2` WS); kicking 1 client or throttling 1 socket does not glitch the other 2 |
| **Linux `keep_local` & Routing** | Linux PipeWire/PulseAudio | `keep_local = true`: `always_local` app stays on PC speakers only; default/`always_remote` app plays on both PC and phone. `keep_local = false`: PC speakers silent; stopping restores `previous_default_sink` |
| **Windows Hotplug** | Windows 10/11 (`wasapi2src loopback`) | Unplugging/switching default audio device triggers bounded pipeline restart; connected browser auto-resumes audio without reload |
