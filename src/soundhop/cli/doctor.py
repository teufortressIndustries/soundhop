"""System diagnostics and bug-reporting utilities for SoundHop CLI."""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import socket
import subprocess
import sys
import urllib.parse
from pathlib import Path
from typing import Any

from ..android import AndroidManager
from ..backend import AudioBackend, BackendError
from ..config import Config
from ..constants import APP_NAME, ISSUES_URL, PROTOCOL_VERSION, SINK_PREFIX, VERSION
from ..discovery import mdns_available
from ..firewall import detect as detect_firewall
from ..ipc import DaemonNotRunning, IpcError, request
from ..service import (
    service_status,
    tray_dependency_available,
    tray_dependency_hint,
)
from ..util import (
    command_exists,
    log_path,
    process_running,
    state_path,
    style,
    tcp_reachable,
)
from .ui import _json_enabled, emit_json, error


def _daemon_snapshot() -> dict[str, Any] | None:
    cli = sys.modules.get("soundhop.cli")
    req = getattr(cli, "request", request) if cli else request
    try:
        return req("status", timeout=2.0)
    except (DaemonNotRunning, IpcError, OSError):
        return None


def _run_doctor() -> dict[str, Any]:
    checks: list[dict[str, Any]] = []
    config = Config.load()

    def add(name: str, ok_: bool, detail: str, hint: str = "", warn: bool = False) -> None:
        checks.append(
            {"name": name, "ok": ok_, "detail": detail, "hint": hint, "warn": warn}
        )

    add(
        "Python",
        sys.version_info >= (3, 10),
        f"{platform.python_version()} ({sys.executable})",
        "SoundHop needs Python 3.10 or newer.",
    )
    if sys.platform != "win32":
        add(
            "pactl",
            command_exists("pactl"),
            shutil.which("pactl") or "not found",
            "Install pipewire-pulse or pulseaudio-utils.",
        )
    else:
        try:
            import sounddevice  # noqa: F401

            has_sd = True
        except ImportError:
            has_sd = False
        add(
            "sounddevice",
            has_sd,
            "installed" if has_sd else "not installed",
            "Run 'pip install sounddevice' for low-latency WASAPI output.",
        )

    backend = AudioBackend()
    backend_ok = backend.available()
    add(
        "Audio server",
        backend_ok,
        backend.server_label() if backend_ok else "not reachable",
        "Start PipeWire/WirePlumber or PulseAudio." if sys.platform != "win32" else "Verify Windows Audio service (Audiosrv).",
    )
    if backend_ok:
        if sys.platform != "win32":
            add("PipeWire", backend.is_pipewire(), backend.server_name(), "PulseAudio also works.")
        else:
            add("WASAPI", True, backend.server_name(), "Windows Core Audio driver.")
        default = backend.default_sink()
        add(
            "Default sink",
            bool(default),
            default or "none",
            "Select an output in your desktop sound settings.",
        )
        try:
            modules = backend.modules()
            listener = [
                m
                for m in modules
                if m.name == "module-native-protocol-tcp"
                and m.arg("port", "4713") == str(config.sharing.port)
            ]
            add(
                "Audio listener",
                bool(listener),
                f"module {listener[0].id}" if listener else "not loaded",
                "Run 'soundhop share on'.",
                warn=not listener,
            )
            tunnels = [
                m
                for m in modules
                if m.name.startswith("module-tunnel-")
                and m.arg("sink_name").startswith(SINK_PREFIX)
            ]
            add(
                "Tunnel sinks",
                True,
                f"{len(tunnels)} active" if tunnels else "none",
                warn=not tunnels,
            )
        except (BackendError, OSError) as exc:
            add("Audio modules", False, str(exc), "Check your audio server.")

    listener_port = config.sharing.port
    add(
        "Local TCP port",
        tcp_reachable("127.0.0.1", listener_port) if backend_ok else False,
        f"127.0.0.1:{listener_port}",
        "If sharing is on, the listener may be blocked or not loaded.",
        warn=True,
    )

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("", config.discovery.port))
        add("Discovery port", True, f"UDP {config.discovery.port} available")
    except OSError as exc:
        add(
            "Discovery port",
            False,
            f"UDP {config.discovery.port}: {exc}",
            "Another process (perhaps the daemon) may already be using it.",
            warn=True,
        )
    finally:
        sock.close()

    has_mdns = mdns_available()
    add(
        "mDNS (optional)",
        has_mdns,
        "python-zeroconf installed" if has_mdns else "not installed (UDP still works)",
        "pip install 'soundhop[mdns]'",
        warn=not has_mdns,
    )
    add(
        "Avahi",
        command_exists("avahi-daemon") or command_exists("avahi-browse"),
        "available"
        if command_exists("avahi-daemon") or command_exists("avahi-browse")
        else "not found",
        "Install avahi for mDNS compatibility with other apps.",
        warn=True,
    )

    fw = detect_firewall()
    add(
        "Firewall",
        True,
        f"{fw.kind}: {'active' if fw.active else 'inactive'} ({fw.detail})",
        "Run 'soundhop firewall open' if peers cannot connect.",
    )

    daemon = _daemon_snapshot()
    add(
        "Daemon",
        daemon is not None,
        (
            f"running (pid {daemon.get('daemon', {}).get('pid')})"
            if daemon
            else "not running"
        ),
        "Run 'soundhop daemon' or install the user service.",
        warn=daemon is None,
    )
    svc = service_status()
    add(
        "User service",
        svc.get("active", False),
        f"{svc.get('state', 'unknown')} ({svc.get('unit')})",
        "Run 'soundhop service install'.",
        warn=not svc.get("active", False),
    )

    # Audio effects / DSP inspection (EasyEffects, JamesDSP, NoiseTorch)
    dsp_detected: list[str] = []
    if backend_ok:
        try:
            sink_names = [s.name.lower() for s in backend.sinks()]
            if any("easyeffects" in s for s in sink_names) or process_running("easyeffects"):
                dsp_detected.append("EasyEffects")
            if any("jamesdsp" in s for s in sink_names) or process_running("jamesdsp"):
                dsp_detected.append("JamesDSP")
            if any("noisetorch" in s for s in sink_names) or process_running("noisetorch"):
                dsp_detected.append("NoiseTorch")
        except Exception:
            pass

    if dsp_detected:
        add(
            "DSP / Audio effects",
            True,
            f"{', '.join(dsp_detected)} active",
            "Notice: Heavy equalizers/convolvers can increase latency or CPU load on network audio.",
            warn=True,
        )
    else:
        add("DSP / Audio effects", True, "none active (direct PipeWire processing)")
    add(
        "Tray (optional)",
        tray_dependency_available(),
        "PySide6 available" if tray_dependency_available() else "PySide6 not installed",
        tray_dependency_hint(),
        warn=not tray_dependency_available(),
    )

    # Android / scrcpy integration
    android_mgr = AndroidManager(
        scrcpy_path=config.android.scrcpy_path,
        adb_path=config.android.adb_path,
    )
    tools = android_mgr.get_tool_status()
    add(
        "scrcpy (Android)",
        tools.scrcpy_path is not None,
        f"{tools.scrcpy_version} ({tools.scrcpy_path})" if tools.scrcpy_path else "not found",
        "Run 'soundhop android install' or install scrcpy from package manager.",
        warn=tools.scrcpy_path is None,
    )
    add(
        "adb (Android)",
        tools.adb_path is not None,
        f"{tools.adb_version} ({tools.adb_path})" if tools.adb_path else "not found",
        "Bundled with scrcpy standalone or install android-tools.",
        warn=tools.adb_path is None,
    )
    if tools.adb_path is not None:
        devices = android_mgr.list_devices()
        ready_devices = [d for d in devices if d.is_ready]
        dev_desc = (
            f"{len(ready_devices)} ready ({', '.join(d.display_name for d in ready_devices)})"
            if ready_devices
            else ("no devices" if not devices else f"{len(devices)} unauthorized/offline")
        )
        add(
            "Android devices",
            bool(ready_devices),
            dev_desc,
            "Enable USB debugging on your phone and connect via USB or Wi-Fi.",
            warn=not ready_devices,
        )
    add(
        "Config",
        True,
        str(config.path),
    )
    add(
        "State",
        True,
        str(state_path()),
    )
    return {"ok": all(c["ok"] or c["warn"] for c in checks), "checks": checks}


def cmd_doctor(args: argparse.Namespace) -> int:
    checks = _run_doctor()
    if _json_enabled(args):
        emit_json(checks)
        return 0 if checks["ok"] else 1
    print(style(f"{APP_NAME} doctor", "bold"))
    print()
    for check in checks["checks"]:
        icon = style("✓", "green") if check["ok"] else style("✗", "red")
        if check.get("warn"):
            icon = style("!", "yellow")
        print(f"  {icon} {check['name']:<24} {check['detail']}")
        if check.get("hint") and not check["ok"]:
            print(f"      {style(check['hint'], 'dim')}")
    print()
    if checks["ok"]:
        print(style("Everything looks good.", "green"))
    else:
        print(style("Some checks need attention.", "yellow"))
    return 0 if checks["ok"] else 1


def cmd_bug_report(args: argparse.Namespace) -> int:
    """Generate a clean, sanitized bug report for GitHub issues."""
    backend = AudioBackend()
    doctor = _run_doctor()

    distro = "Unknown Linux"
    os_rel = Path("/etc/os-release")
    if os_rel.exists():
        try:
            for line in os_rel.read_text(errors="ignore").splitlines():
                if line.startswith("PRETTY_NAME="):
                    distro = line.split("=", 1)[1].strip('"\'')
                    break
        except OSError:
            pass

    session_type = os.environ.get("XDG_SESSION_TYPE", "unknown")
    desktop = os.environ.get("XDG_CURRENT_DESKTOP", "unknown")
    wayland = bool(os.environ.get("WAYLAND_DISPLAY"))

    log_tail = ""
    path = log_path()
    if path.exists():
        try:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
            log_tail = "\n".join(lines[-25:])
        except OSError:
            pass

    checks_summary = "\n".join(
        f"- [{'x' if c['ok'] else ' '}] **{c['name']}**: {c['detail']}"
        for c in doctor.get("checks", [])
    )

    report = f"""### Environment
- **OS**: {distro} (Kernel: {platform.release()})
- **Desktop**: {desktop} ({session_type}, Wayland: {wayland})
- **Python**: {platform.python_version()}
- **SoundHop Version**: {VERSION} (protocol {PROTOCOL_VERSION})
- **Audio Server**: {backend.server_label() if backend.available() else 'Not reachable'}

### Diagnostic Checks (`soundhop doctor`)
{checks_summary}

### Recent Daemon Logs
```text
{log_tail or '(no recent logs)'}
```
"""
    if getattr(args, "open_browser", False):
        encoded_body = urllib.parse.quote(report)
        issue_url = f"{ISSUES_URL}/new?body={encoded_body}"
        if command_exists("xdg-open"):
            subprocess.call(
                ["xdg-open", issue_url],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            print("Opening browser to submit bug report...")
            return 0
        print(f"Please submit your report at:\n{issue_url}")
        return 0

    if getattr(args, "output_file", None):
        out_path = Path(args.output_file)
        try:
            out_path.write_text(report, encoding="utf-8")
            print(f"Bug report written to {out_path}")
            return 0
        except OSError as exc:
            error(f"Could not write bug report: {exc}")
            return 1

    print(report)
    print(
        style(
            "Tip: Copy and paste the markdown above into a GitHub issue, or run 'soundhop bug-report --open'.",
            "dim",
        )
    )
    return 0
