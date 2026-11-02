# 🔊 SoundHop

> Hop audio between Linux boxes on your local network without manual `pactl` plumbing or broken streams.

[![CI](https://github.com/teufortressIndustries/soundhop/actions/workflows/ci.yml/badge.svg)](https://github.com/teufortressIndustries/soundhop/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[**Download Latest AppImage (Releases)**](https://github.com/teufortressIndustries/soundhop/releases) • [**Quickstart**](#quickstart) • [**Requirements**](#requirements) • [**CLI Commands**](#cli-command-reference) • [**Building from Source**](#building-from-source) • [**How It Works**](#how-it-works-under-the-hood)

---

Ever wanted to stream audio from your desktop gaming rig or workstation over to your laptop speakers across the room, or beam audio to an HTPC hooked up to your sound system?

PulseAudio and PipeWire already implement native TCP network streaming modules (`module-native-protocol-tcp` and `module-tunnel-sink`), but setting them up by hand is tedious:
- Manually looking up peer LAN IP addresses and remembering raw `pactl load-module` arguments.
- Handling firewall rules (`4713/tcp`, `47777/udp`).
- Live audio streams not automatically moving over when a tunnel connects.
- Ghost tunnels and frozen audio when a remote laptop suspends or Wi-Fi drops.

**SoundHop** fixes this. It wraps PipeWire and PulseAudio in an automatic, self-healing workflow with zero-configuration LAN discovery, an interactive CLI (`soundhop` or the short alias `shop`), and an optional Qt system tray for KDE Plasma and GNOME.

---

## Quickstart

### 1. Grab the executable
Either download the standalone **[SoundHop AppImage](https://github.com/teufortressIndustries/soundhop/releases)** (no installation required) or clone and run:
```bash
./install.sh
```

### 2. Receiver (e.g. your laptop with the good speakers)
Turn on audio sharing:
```bash
soundhop share on
```
*(If you have an active firewall like firewalld or ufw, run `soundhop firewall open`)*.

### 3. Sender (e.g. your desktop / workstation)
Connect and pick the target machine:
```bash
soundhop connect
```
```text
Discovered devices:
  1) thinkpad-cachy (192.168.1.105:4713) [LAN]
Select device [1]: 1
✓ Audio is now playing on thinkpad-cachy.
```
Your active playback streams (Spotify, Firefox, mpv, games) immediately switch to the remote machine.

### 4. Disconnect
```bash
soundhop disconnect
```
Audio falls straight back to your local speakers or headphones without missing a beat.

> **Prefer a GUI?** Run `soundhop tray` (or double-click the AppImage) to get a system tray widget with one-click toggles and audio output switching.

---

## Requirements

| Component | Minimum Version | Notes |
| :--- | :--- | :--- |
| **Linux Kernel** | 5.x+ | Any modern distro (Fedora, Arch, CachyOS, Ubuntu, Debian, openSUSE) |
| **Audio Server** | PipeWire 0.3+ / PulseAudio 14+ | Must provide `pipewire-pulse` compatibility or PulseAudio |
| **CLI Tools** | `pactl` | Standard tool from `pipewire-pulse` or `pulseaudio-utils` |
| **Python** | 3.10+ | Required only when building from source (bundled inside AppImage) |
| **Optional GUI** | `PySide6` (Qt 6) | Only needed for the desktop system tray icon |
| **Optional mDNS**| `python-zeroconf` | Fallback discovery if raw UDP broadcasts are blocked on your router |

---

## How It Works (Under the Hood)

```
┌────────────────────────────────┐         LAN UDP :47777         ┌────────────────────────────────┐
│         Sender Device          │ ◄────────────────────────────► │        Receiver Device         │
│   (Desktop / Workstation)      │       (Zero-Config Beacon)     │       (Laptop / Speakers)      │
│                                │                                │                                │
│  • soundhop daemon             │                                │  • soundhop daemon             │
│  • module-tunnel-sink          │ ═════════════════════════════► │  • module-native-protocol-tcp  │
│  • Auto-routes active streams  │      Raw TCP PCM Audio :4713   │  • Subnet ACL (192.168.x.x)    │
└────────────────────────────────┘                                └────────────────────────────────┘
```

1. **Discovery**: Each node runs a tiny background supervisor. When sharing is enabled, it broadcasts a JSON beacon over UDP port `47777` (with automatic mDNS `_soundhop._tcp` fallback). Peers discover each other in under two seconds.
2. **Audio Transport**: Native PCM audio travels over TCP port `4713`. Sender loads `module-tunnel-sink` specifying `rate=48000 channels=2 format=s16le` to eliminate software resampling degradation.
3. **Stream Handover**: SoundHop inspects active sink-inputs (`pactl list sink-inputs`) and moves running audio apps onto the remote sink, setting it as default.
4. **Resilience & Sleep Supervision**: If the receiving device goes to sleep, closes its lid, or drops off Wi-Fi, the supervisor senses TCP timeout, cleanly unloads the dead tunnel, and restores sound to your local output. Once the receiver wakes back up, SoundHop can automatically reconnect.
5. **DSP Awareness**: SoundHop checks for active DSP suites like **EasyEffects**, **JamesDSP**, and **NoiseTorch** to warn if virtual sink re-routing could add unintended latency.

---

## CLI Command Reference

Both `soundhop` and its single-syllable alias `shop` are available.

### Everyday Commands
```bash
soundhop status                 # Show current connection, sharing status, discovered peers, and sinks
soundhop status --json          # Raw JSON output for Waybar, Polybar, or scripts
soundhop discover               # Scan LAN for online SoundHop nodes
soundhop connect [name|ip]      # Connect interactively or directly by hostname / IP
soundhop connect --host 192.168.1.50 --port 4713
soundhop disconnect             # Drop remote tunnel and restore local sound output
soundhop share <on|off|status>  # Enable or disable receiving audio from other devices
soundhop tray                   # Launch Qt system tray icon (KDE Plasma / GNOME)
```

### Android Audio Capture (scrcpy)
Capture and stream audio from Android devices directly to your Linux speakers over Wi-Fi or USB.
```bash
soundhop android                # Interactive setup wizard & device pairing
soundhop android 192.168.1.99   # Connect directly to a specific phone IP or serial
soundhop android stop           # Stop running audio capture session
soundhop android status         # Show scrcpy / adb tool paths and active sessions
soundhop android devices        # List detected Android devices and Android versions
soundhop android encoders       # List hardware/software audio encoders supported by your phone
soundhop android wifi           # Switch a plugged-in USB phone to wireless Wi-Fi mode
soundhop android install        # Download & install standalone official scrcpy (zero-bloat, on-demand)
```

### Automation & Services
```bash
soundhop auto <on|off>          # Toggle automatic reconnection to your last paired machine
soundhop auto pause             # Temporarily pause auto-reconnect (e.g. while on public Wi-Fi)
soundhop auto resume            # Resume auto-reconnect
soundhop service install        # Install and enable systemd user unit (soundhop.service)
soundhop service status         # Check systemd service status
soundhop service restart        # Restart user daemon
```

### Diagnostics & Network
```bash
soundhop doctor                 # Self-test PipeWire/PulseAudio, sockets, pactl, and firewalls
soundhop bug-report             # Dump sanitized markdown diagnostics for GitHub issues
soundhop bug-report --open      # Generate report and open pre-filled GitHub issue in browser
soundhop firewall <open|close|status> # Manage rules for firewalld (Fedora) or ufw (Ubuntu)
```

### Shell Completions
Tab completion is available for Bash, Zsh, and Fish:
```bash
# Fish (CachyOS / Arch default):
soundhop completions fish > ~/.config/fish/completions/soundhop.fish

# Bash:
soundhop completions bash > ~/.local/share/bash-completion/completions/soundhop

# Zsh:
soundhop completions zsh > ~/.zsh/completion/_soundhop
```

---

## Building from Source

If you want to hack on SoundHop or run straight from git:

### 1. Clone repository
```bash
git clone https://github.com/teufortressIndustries/soundhop.git
cd soundhop
```

### 2. Set up virtual environment
```bash
python3 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip

# Install in editable mode with GUI (PySide6) and mDNS (zeroconf):
pip install -e ".[all]"
```

### 3. Run unit tests
SoundHop comes with comprehensive unit tests for discovery, packet codecs, CLI parsing, state, and firewall helpers:
```bash
PYTHONPATH=src python3 -m unittest discover -s tests -v
```

### 4. Build Universal AppImage locally
We build a universal, standalone AppImage using a clean Ubuntu 22.04 container (with Podman or Docker) so it runs on glibc 2.35+ across Fedora, Arch, CachyOS, Debian, and Ubuntu:
```bash
./build-appimage-container.sh
```
The finished binary will be placed at `dist/SoundHop-1.1.0-universal-x86_64.AppImage`.

---

## Configuration & Tuning

Configuration is stored in `~/.config/soundhop/config.toml` (and can be edited directly or through `soundhop config set <key> <val>`):

```toml
[device]
name = "Fedora-Desktop"          # Display name broadcasted to other devices

[sharing]
enabled = false                 # Whether this device accepts incoming audio
security = "trusted-lan"        # "trusted-lan" (RFC1918 subnets), "cookie" (auth cookie), or "open"
buffer_latency_ms = 0           # 0 = automatic default (or 50 for low-latency, 100 balanced, 200 Wi-Fi buffer)

[connection]
auto_reconnect = true           # Automatically reconnect to last paired device when seen
disconnect_on_quit = true       # Drop remote tunnel and restore local audio when tray app exits
fallback_sink = ""              # Specific sink to restore to (empty = restore previous default)

[discovery]
port = 47777                    # UDP broadcast discovery port
mdns = true                     # Enable mDNS fallback browsing
```

---

## Limitations & Honest Notes

- **Linux only**: We rely directly on Linux PipeWire / PulseAudio native protocol streaming. Windows and macOS are not supported.
- **LAN-oriented**: Designed for local Wi-Fi, Ethernet, and peer-to-peer networks. If you want to stream across the open Internet, run over [Tailscale](https://tailscale.com) or WireGuard.
- **ALSA-only systems**: If your setup does not run PipeWire or PulseAudio, SoundHop will not work.

---

## Roadmap

- [x] **v1.0**:
  - PipeWire & PulseAudio TCP audio transport
  - Fast UDP + mDNS LAN discovery
  - Native Wayland + X11 Qt system tray GUI
  - Full CLI (`soundhop` / `shop`) and shell autocompletions
  - Disconnect-on-quit toggle, buffer latency presets, and DSP detection (EasyEffects)
  - Portable AppImage & containerized multi-distro build pipeline
- [x] **v1.1 (Current)**:
  - **Android Support (scrcpy)**: Connect and listen your android device.
- [ ] **v1.2 (Upcoming)**:
  - **Per-Application Stream Routing**: Direct specific apps (e.g. Spotify or Firefox) to remote speakers without moving your whole desktop audio.
  - **Microphone Pass-Through**: Stream microphone input back to the sender for remote calls and Discord.
  - **Remote Volume Sync**: Control receiver volume directly from the sender's media keys.

---

## License

[MIT](LICENSE) © SoundHop Contributors.
