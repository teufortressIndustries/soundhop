"""Android scrcpy audio capture integration subcommands for SoundHop CLI."""

from __future__ import annotations

import argparse
import os
import sys
import time
from dataclasses import asdict
from typing import Any

from .. import ipc
from ..android import (
    AndroidCaptureSession,
    AndroidManager,
    SUPPORTED_AUDIO_CODECS,
)
from ..config import Config
from ..constants import APP_NAME
from ..controller import Controller
from ..util import format_duration, style
from .ui import _json_enabled, emit_json, error, info, ok, warning


def _is_running() -> bool:
    cli = sys.modules.get("soundhop.cli")
    if cli is not None and hasattr(cli, "is_running"):
        return bool(cli.is_running())
    return ipc.is_running()


def _request(method: str, timeout: float = 15.0, **params: Any) -> Any:
    cli = sys.modules.get("soundhop.cli")
    if cli is not None and hasattr(cli, "request"):
        return cli.request(method, timeout=timeout, **params)
    return ipc.request(method, timeout=timeout, **params)


def cmd_android(args: argparse.Namespace) -> int:
    sub = getattr(args, "subaction", None)
    known_actions = {
        "start",
        "capture",
        "connect",
        "stop",
        "disconnect",
        "status",
        "devices",
        "wifi",
        "encoders",
        "codecs",
        "install",
    }
    if sub is not None and sub not in known_actions:
        if getattr(args, "target", None) is None:
            args.target = sub
            sub = "start"
            args.subaction = "start"
        else:
            error(f"Unknown android action: {sub}")
            return 1

    if sub in ("stop", "disconnect"):
        return cmd_android_stop(args)
    if sub == "status":
        return cmd_android_status(args)
    if sub == "devices":
        return cmd_android_devices(args)
    if sub == "wifi":
        return cmd_android_wifi(args)
    if sub in ("encoders", "codecs"):
        return cmd_android_encoders(args)
    if sub == "install":
        return cmd_android_install(args)
    return cmd_android_start(args)


def cmd_android_install(args: argparse.Namespace) -> int:
    config = Config.load()
    mgr = AndroidManager(
        scrcpy_path=config.android.scrcpy_path, adb_path=config.android.adb_path
    )
    print(style("Installing standalone scrcpy and adb...", "bold"))
    print("Source: Genymobile GitHub release (Apache 2.0 license)")

    last_pct = [-1]

    def progress(current: int, total: int) -> None:
        if total > 0:
            pct = int((current / total) * 100)
            if pct != last_pct[0]:
                last_pct[0] = pct
                mb_cur = current / (1024 * 1024)
                mb_tot = total / (1024 * 1024)
                print(
                    f"\r  Downloading: {mb_cur:.1f} MB / {mb_tot:.1f} MB [{pct}%]",
                    end="",
                    flush=True,
                )

    try:
        installed = mgr.install_scrcpy(progress_cb=progress)
        print()
        ok(f"Installed scrcpy to {installed}")
        status = mgr.get_tool_status()
        if status.adb_path:
            ok(f"adb ready at {status.adb_path}")
        return 0
    except Exception as exc:
        print()
        error(str(exc))
        return 1


def cmd_android_status(args: argparse.Namespace) -> int:
    data = None
    if _is_running():
        try:
            data = _request("android_status", timeout=3.0)
        except Exception:
            pass
    if data is None:
        config = Config.load()
        controller = Controller(config=config)
        tools = controller.android_tool_status()
        devices = controller.android_list_devices()
        session = controller.android_session.status()
        data = {"tools": tools, "devices": devices, "session": session}

    if _json_enabled(args):
        emit_json(data)
        return 0

    tools = data.get("tools", {})
    devices = data.get("devices", [])
    session = data.get("session", {})

    print(style(f"{APP_NAME} Android status", "bold"))
    print()

    scrcpy_path = tools.get("scrcpy_path")
    scrcpy_ver = tools.get("scrcpy_version") or "installed"
    adb_path = tools.get("adb_path")
    adb_ver = tools.get("adb_version") or "installed"

    if scrcpy_path:
        print(f"  {'scrcpy':<12} {scrcpy_ver} ({scrcpy_path})")
    else:
        print(
            f"  {'scrcpy':<12} {style('not installed', 'yellow')} (run 'soundhop android install')"
        )

    if adb_path:
        print(f"  {'adb':<12} {adb_ver} ({adb_path})")
    else:
        print(f"  {'adb':<12} {style('not installed', 'yellow')}")

    if session.get("active"):
        dev_name = session.get("device_name") or session.get("serial", "Android")
        uptime = format_duration(session.get("uptime_s"))
        print(
            f"  {'Capture':<12} {style('active', 'green')} ({dev_name}, {session.get('codec', '').upper()}, pid {session.get('pid')}, {uptime})"
        )
    else:
        print(f"  {'Capture':<12} idle")

    print()
    if not devices:
        print(style("No Android devices connected.", "dim"))
    else:
        print(style("Connected Android devices:", "bold"))
        for d in devices:
            kind = "Wi-Fi" if d.get("is_wifi") else "USB"
            state_style = (
                style("ready", "green")
                if d.get("state") == "device"
                else style(d.get("state", "unknown"), "yellow")
            )
            ver = (
                f"(Android {d.get('android_version')})"
                if d.get("android_version")
                else ""
            )
            print(
                f"  • {d.get('manufacturer', '')} {d.get('model', d.get('serial'))} [{kind}] {ver} — {state_style}"
            )
            print(f"    serial: {d.get('serial')}")

    return 0


def cmd_android_devices(args: argparse.Namespace) -> int:
    config = Config.load()
    mgr = AndroidManager(
        scrcpy_path=config.android.scrcpy_path, adb_path=config.android.adb_path
    )
    devices = mgr.list_devices()
    if _json_enabled(args):
        emit_json([asdict(d) for d in devices])
        return 0

    if not devices:
        info("No Android devices connected.")
        info("Plug your device in via USB or enable Wireless Debugging.")
        return 0

    print(style("Connected Android devices:", "bold"))
    for idx, d in enumerate(devices, 1):
        transport = "Wi-Fi" if d.is_wifi else "USB"
        status_col = (
            style("ready", "green") if d.is_ready else style(d.state, "yellow")
        )
        ver_info = f"Android {d.android_version}" if d.android_version else ""
        print(
            f"  {idx}) {d.display_name:<24} [{transport}] {ver_info} ({d.serial}) — {status_col}"
        )
    return 0


def cmd_android_wifi(args: argparse.Namespace) -> int:
    config = Config.load()
    mgr = AndroidManager(
        scrcpy_path=config.android.scrcpy_path, adb_path=config.android.adb_path
    )
    target = getattr(args, "target", None)
    if target:
        ok_flag, msg = mgr.connect_wifi(target)
        if ok_flag:
            ok(msg)
            return 0
        error(msg)
        return 1

    devices = mgr.list_devices()
    usb_devs = [d for d in devices if not d.is_wifi and d.is_ready]
    if not usb_devs:
        error(
            "No ready USB devices found to switch to Wi-Fi mode. Connect a device via USB first or specify an IP: soundhop android wifi 192.168.1.99"
        )
        return 1

    dev = usb_devs[0]
    info(f"Switching {dev.display_name} ({dev.serial}) to Wi-Fi mode...")
    ip = mgr.get_device_wifi_ip(dev.serial)
    ok_flag, msg = mgr.enable_tcpip(dev.serial)
    if not ok_flag:
        error(msg)
        return 1
    if ip:
        time.sleep(1.0)
        conn_ok, conn_msg = mgr.connect_wifi(ip)
        if conn_ok:
            ok(f"Connected wirelessly to {ip}:5555. You can now unplug the USB cable!")
            return 0
        warning(
            f"Device switched to TCP mode, but connecting to {ip}:5555 failed: {conn_msg}"
        )
        return 0
    ok("Device switched to TCP mode on port 5555. Connect using: soundhop android wifi <phone-ip>")
    return 0


def cmd_android_encoders(args: argparse.Namespace) -> int:
    config = Config.load()
    mgr = AndroidManager(
        scrcpy_path=config.android.scrcpy_path, adb_path=config.android.adb_path
    )
    target = getattr(args, "target", None)
    try:
        encs = mgr.list_encoders(target)
    except Exception as exc:
        error(str(exc))
        return 1

    if _json_enabled(args):
        emit_json(asdict(encs))
        return 0

    print(style("Supported Android Audio Codecs & Encoders:", "bold"))
    for codec, encoder_list in encs.codecs.items():
        print(f"\n  {style(codec.upper(), 'cyan', 'bold')}:")
        if encoder_list:
            for enc in encoder_list:
                print(f"    • {enc}")
        else:
            print(f"    • default Android {codec} encoder")
    return 0


def cmd_android_start(args: argparse.Namespace) -> int:
    config = Config.load()
    mgr = AndroidManager(
        scrcpy_path=config.android.scrcpy_path, adb_path=config.android.adb_path
    )
    tools = mgr.get_tool_status()

    interactive = sys.stdin.isatty() and not getattr(args, "yes", False)

    # 1. Verify tools
    if not tools.scrcpy_path:
        if interactive:
            print(style("scrcpy is not installed.", "yellow", "bold"))
            print("SoundHop uses scrcpy for low-latency native Android audio capture.")
            try:
                ans = (
                    input(
                        "Would you like SoundHop to download and install official standalone scrcpy now? [Y/n]: "
                    )
                    .strip()
                    .lower()
                )
            except EOFError:
                ans = "n"
            if ans in ("", "y", "yes"):
                install_rc = cmd_android_install(args)
                if install_rc != 0:
                    return install_rc
                tools = mgr.get_tool_status()
            else:
                error(
                    "scrcpy is required. Please install scrcpy using your distribution's package manager."
                )
                return 1
        else:
            error(
                "scrcpy is not installed. Run 'soundhop android install' or install it via package manager."
            )
            return 1

    if not tools.adb_path:
        error("adb is not found. Please install adb or run 'soundhop android install'.")
        return 1

    # 2. Discover devices
    target_str = getattr(args, "target", None)
    devices = mgr.list_devices()

    target_device = None
    if target_str:
        for d in devices:
            if d.serial == target_str or (d.ip and d.ip == target_str):
                target_device = d
                break
        if not target_device and (":" in target_str or "." in target_str):
            mgr.connect_wifi(target_str)
            devices = mgr.list_devices()
            for d in devices:
                if d.serial == target_str or d.ip == target_str or (d.ip and target_str.startswith(d.ip)):
                    target_device = d
                    break

    while target_device is None:
        if not devices:
            print()
            print(style("No connected Android devices found.", "yellow", "bold"))
            print()
            print("To connect your Android device:")
            print(
                "  1. On your phone: Settings -> About Phone -> tap 'Build Number' 7 times to enable Developer options."
            )
            print("  2. In Developer Options:")
            print("     - Turn on 'USB Debugging'.")
            print("     - (Optional for wireless) Turn on 'Wireless Debugging'.")
            print("  3. Connect phone to PC using a USB cable.")
            print(
                "  4. Look at phone screen: tap 'Allow USB debugging' (check 'Always allow from this computer')."
            )
            print()
            if not interactive:
                error("No devices detected. Please connect an Android device.")
                return 1
            try:
                resp = input(
                    "Enter device IP (e.g. 192.168.1.99) or press Enter to scan again [q to quit]: "
                ).strip()
            except EOFError:
                return 1
            if resp.lower() in ("q", "quit", "exit"):
                return 0
            if resp:
                info(f"Connecting to {resp}...")
                mgr.connect_wifi(resp)
            devices = mgr.list_devices()
            continue

        unauthorized = [d for d in devices if d.state == "unauthorized"]
        ready = [d for d in devices if d.is_ready]

        if unauthorized and not ready:
            print()
            warning(f"Device {unauthorized[0].serial} is connected but UNAUTHORIZED.")
            print(
                "Please unlock your phone, check the screen, and tap 'Allow USB debugging'."
            )
            if not interactive:
                return 1
            try:
                input("Press Enter after confirming on phone... ")
            except EOFError:
                return 1
            devices = mgr.list_devices()
            continue

        if not ready:
            devices = mgr.list_devices()
            continue

        if len(ready) == 1:
            target_device = ready[0]
            if interactive:
                conn_type = "Wi-Fi" if target_device.is_wifi else "USB"
                ans = (
                    input(
                        f"Found {target_device.display_name} [{conn_type}]. Capture audio? [Y/n]: "
                    )
                    .strip()
                    .lower()
                )
                if ans not in ("", "y", "yes"):
                    return 0
            break
        else:
            print()
            print(style("Multiple Android devices detected:", "bold"))
            for idx, d in enumerate(ready, 1):
                ctype = "Wi-Fi" if d.is_wifi else "USB"
                ver = f"(Android {d.android_version})" if d.android_version else ""
                print(f"  {idx}) {d.display_name:<24} [{ctype}] {ver} ({d.serial})")
            if not interactive:
                target_device = ready[0]
                break
            try:
                ans = input(f"Select device [1-{len(ready)}]: ").strip()
            except EOFError:
                return 1
            if ans.isdigit() and 1 <= int(ans) <= len(ready):
                target_device = ready[int(ans) - 1]
                break
            continue

    # Offer to switch USB device to Wi-Fi if connected by USB
    if not target_device.is_wifi and not getattr(args, "usb", False) and interactive:
        try:
            sw = (
                input(
                    f"Device is connected via USB. Switch to wireless (Wi-Fi) mode? [Y/n]: "
                )
                .strip()
                .lower()
            )
        except EOFError:
            sw = "n"
        if sw in ("", "y", "yes"):
            ip = mgr.get_device_wifi_ip(target_device.serial)
            ok_sw, msg_sw = mgr.enable_tcpip(target_device.serial)
            if ok_sw and ip:
                time.sleep(1.0)
                conn_ok, conn_msg = mgr.connect_wifi(ip)
                if conn_ok:
                    ok(f"Connected wirelessly to {ip}:5555. You can now unplug the USB cable!")
                    time.sleep(0.5)
                    for d in mgr.list_devices():
                        if d.ip == ip:
                            target_device = d
                            break

    # 3. Audio Codec & Compatibility Options
    codec = getattr(args, "codec", None) or config.android.default_codec or "aac"
    source = getattr(args, "source", None)
    if not source:
        major_v = target_device.major_version
        if target_device.sdk_version >= 31 or major_v >= 12:
            source = "output"
        elif target_device.sdk_version >= 30 or major_v >= 11:
            source = "playback"
        elif 0 < target_device.sdk_version < 30 or (0 < major_v < 11):
            warning(
                f"Android {target_device.android_version or target_device.sdk_version} does not support native internal audio capture."
            )
            warning("Falling back to microphone capture (--audio-source=mic).")
            source = "mic"
        else:
            source = config.android.default_source or "output"

    buffer_ms = getattr(args, "buffer_ms", None) or config.android.buffer_ms or 50
    bit_rate = getattr(args, "bit_rate", None) or config.android.bit_rate or 128000
    duplicate = bool(getattr(args, "dup", False) or config.android.duplicate_audio)

    # 4. Run Audio Capture
    detach = bool(getattr(args, "detach", False))

    if detach:
        if _is_running():
            res = _request(
                "android_start",
                serial=target_device.serial,
                name=target_device.display_name,
                codec=codec,
                source=source,
                buffer_ms=buffer_ms,
                bit_rate=bit_rate,
                duplicate=duplicate,
            )
            ok(
                f"Audio capture started in background for {target_device.display_name} (PID {res.get('pid')})."
            )
            ok("Run 'soundhop android stop' to stop streaming.")
            return 0
        else:
            error(
                "Background capture requires the SoundHop daemon. Start it with 'soundhop daemon --detach' or omit --detach."
            )
            return 1

    session = AndroidCaptureSession(mgr)
    print()
    ok(
        f"Starting audio capture from {style(target_device.display_name, 'bold')} ({target_device.serial})"
    )
    print(f"  Codec:       {codec.upper()} ({bit_rate // 1000} kbps)")
    print(f"  Source:      {source}")
    print(f"  Buffer:      {buffer_ms} ms")
    print()
    print(style("Streaming audio to your speakers. Press Ctrl+C to stop.", "green"))

    try:
        session.start(
            target_device.serial,
            device_name=target_device.display_name,
            codec=codec,
            source=source,
            buffer_ms=buffer_ms,
            bit_rate=bit_rate,
            duplicate=duplicate,
        )
        while session.is_active:
            time.sleep(0.5)
    except KeyboardInterrupt:
        print()
        info("Stopping audio capture...")
    except Exception as exc:
        error(str(exc))
        return 1
    finally:
        session.stop()
    return 0


def cmd_android_stop(args: argparse.Namespace) -> int:
    if _is_running():
        pid = 0
        try:
            status = _request("android_status", timeout=2.0)
            pid = int(status.get("session", {}).get("pid", 0) or 0)
        except Exception:
            pass
        try:
            _request("android_stop", timeout=3.0)
            ok("Android audio capture stopped.")
            return 0
        except Exception:
            if pid > 0:
                import signal

                try:
                    os.kill(pid, signal.SIGTERM)
                except OSError:
                    pass
            ok("Android audio capture stopped.")
            return 0

    config = Config.load()
    mgr = AndroidManager(
        scrcpy_path=config.android.scrcpy_path, adb_path=config.android.adb_path
    )
    session = AndroidCaptureSession(mgr)
    session.stop()
    ok("Android audio capture stopped.")
    return 0
