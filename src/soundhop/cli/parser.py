"""Command-line argument parser construction for SoundHop CLI."""

from __future__ import annotations

import argparse
from typing import Any

from ..android import SUPPORTED_AUDIO_CODECS
from ..config import SECURITY_MODES
from ..constants import VERSION
from .android_cmds import cmd_android
from .core_cmds import (
    cmd_auto,
    cmd_completions,
    cmd_config,
    cmd_connect,
    cmd_daemon,
    cmd_disconnect,
    cmd_discover,
    cmd_firewall,
    cmd_logs,
    cmd_service,
    cmd_setup,
    cmd_share,
    cmd_status,
    cmd_stop,
    cmd_tray,
    cmd_version,
)
from .doctor import cmd_bug_report, cmd_doctor


def _common_flags() -> argparse.ArgumentParser:
    common = argparse.ArgumentParser(add_help=False)
    # SUPPRESS keeps a flag supplied before the subcommand from being reset by
    # the subparser's own default.
    common.add_argument(
        "--json",
        action="store_true",
        default=argparse.SUPPRESS,
        help="machine-readable output",
    )
    common.add_argument(
        "--verbose",
        action="store_true",
        default=argparse.SUPPRESS,
        help="more diagnostics",
    )
    common.add_argument(
        "--no-color",
        action="store_true",
        default=argparse.SUPPRESS,
        help="disable color output",
    )
    return common


def build_parser() -> argparse.ArgumentParser:
    common = _common_flags()
    parser = argparse.ArgumentParser(
        prog="soundhop",
        description="Play audio on another device on your local network.",
        epilog=(
            "Quick start: soundhop setup && soundhop connect. "
            "See 'soundhop doctor' if something is not working."
        ),
        parents=[common],
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s {VERSION}")
    sub = parser.add_subparsers(dest="command", metavar="COMMAND")

    def add(name: str, help_text: str, func: Any) -> argparse.ArgumentParser:
        p = sub.add_parser(
            name, help=help_text, description=help_text, parents=[common]
        )
        p.set_defaults(func=func)
        return p

    p = add("setup", "configure this device for local audio sharing", cmd_setup)
    p.add_argument(
        "--yes", action="store_true", help="accept defaults (non-interactive)"
    )
    p.add_argument("--name", help="this device's advertised name")
    p.add_argument("--port", type=int, help="audio port (default 4713)")
    p.add_argument("--security", choices=SECURITY_MODES, help="listener security mode")
    p.add_argument(
        "--auto",
        dest="auto",
        action="store_true",
        default=None,
        help="enable auto-connect",
    )
    p.add_argument(
        "--no-auto", dest="auto", action="store_false", help="disable auto-connect"
    )
    p.add_argument(
        "--no-firewall", action="store_true", help="do not touch the firewall"
    )
    p.add_argument("--no-mdns", action="store_true", help="do not open the mDNS port")
    p.add_argument(
        "--no-service", action="store_true", help="do not install a user service"
    )
    p.add_argument(
        "--no-tray", action="store_true", help="do not add tray autostart"
    )

    add("status", "show daemon, sharing, peers and current output", cmd_status)
    p = add("discover", "find devices on the local network", cmd_discover)
    p.add_argument(
        "--timeout", type=float, default=2.5, help="seconds to scan (default 2.5)"
    )
    add("devices", "alias for discover", cmd_discover).add_argument(
        "--timeout", type=float, default=2.5
    )

    p = add("connect", "play this device's audio on another device", cmd_connect)
    p.add_argument("peer", nargs="?", help="device name, id, or host")
    p.add_argument("--host", help="connect directly to a host")
    p.add_argument("--port", type=int, help="remote audio port")
    p.add_argument("--name", help="friendly name for a manual host")
    p.add_argument(
        "--no-switch",
        action="store_true",
        help="create the sink but do not make it default",
    )
    p.add_argument(
        "--no-move",
        action="store_true",
        help="do not move currently playing streams",
    )

    p = add("disconnect", "stop playing on the remote device", cmd_disconnect)
    p.add_argument(
        "peer", nargs="?", help="device name or id (default: current/all)"
    )
    p.add_argument(
        "--all", action="store_true", help="remove all SoundHop tunnel sinks"
    )
    p.add_argument(
        "--forget", action="store_true", help="also forget the last device"
    )
    p.add_argument(
        "--no-restore",
        action="store_true",
        help="do not restore the previous output",
    )

    p = add("share", "share this device's speakers", cmd_share)
    p.add_argument(
        "action", nargs="?", choices=("on", "off", "status"), default="status"
    )

    p = add("auto", "control auto-connect", cmd_auto)
    p.add_argument(
        "action", nargs="?", choices=("on", "off", "status"), default="status"
    )

    p = add("daemon", "run the background daemon", cmd_daemon)
    p.add_argument(
        "--detach", action="store_true", help="start in the background and return"
    )

    add("stop", "stop the background daemon", cmd_stop)

    p = add("service", "manage the systemd user service", cmd_service)
    p.add_argument(
        "action",
        choices=(
            "install",
            "uninstall",
            "start",
            "stop",
            "restart",
            "enable",
            "disable",
            "status",
        ),
        nargs="?",
        default="status",
    )
    p.add_argument("--no-enable", action="store_true", help="do not enable at login")
    p.add_argument(
        "--no-start", action="store_true", help="do not start immediately"
    )
    p.add_argument("--tray", action="store_true", help="also add tray autostart")

    p = add("firewall", "open or inspect the required firewall ports", cmd_firewall)
    p.add_argument(
        "action", choices=("open", "close", "status"), nargs="?", default="status"
    )
    p.add_argument(
        "--dry-run", action="store_true", help="print commands without running them"
    )
    p.add_argument("--no-mdns", action="store_true", help="do not open UDP 5353")

    p = add("config", "inspect or edit the configuration", cmd_config)
    p.add_argument(
        "action",
        choices=("path", "show", "get", "set", "edit", "reload"),
        nargs="?",
        default="show",
    )
    p.add_argument("key", nargs="?", help="dotted key, e.g. connection.auto_connect")
    p.add_argument("value", nargs="?", help="new value for 'config set'")

    add("doctor", "diagnose common problems", cmd_doctor)
    add("tray", "run the optional system tray", cmd_tray)
    add("gui", "alias for tray", cmd_tray)

    p = add("logs", "show the daemon log", cmd_logs)
    p.add_argument(
        "--lines", type=int, default=100, help="number of lines (default 100)"
    )
    p.add_argument("-f", "--follow", action="store_true", help="follow the log")

    p = add(
        "bug-report",
        "generate a diagnostic report for bug reporting",
        cmd_bug_report,
    )
    p.add_argument(
        "--open",
        dest="open_browser",
        action="store_true",
        help="open GitHub issues in browser",
    )
    p.add_argument("-o", "--output", dest="output_file", help="save report to file")
    sub.add_parser(
        "report", parents=[common], help="alias for bug-report"
    ).set_defaults(func=cmd_bug_report)

    p = add("completions", "print shell completion code", cmd_completions)
    p.add_argument("shell", choices=("bash", "zsh", "fish"))

    p = add(
        "android",
        "capture and play audio from an Android device via scrcpy",
        cmd_android,
    )
    p.add_argument(
        "subaction",
        nargs="?",
        choices=(
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
        ),
        default=None,
        help="android subcommand (default: interactive capture wizard)",
    )
    p.add_argument("target", nargs="?", help="device serial, IP address, or number")
    p.add_argument(
        "--codec",
        choices=SUPPORTED_AUDIO_CODECS,
        help="audio codec (default: aac)",
    )
    p.add_argument(
        "--source",
        choices=("output", "playback", "mic"),
        help="audio source (default: output for Android 12+, playback for Android 11)",
    )
    p.add_argument(
        "--buffer",
        type=int,
        dest="buffer_ms",
        help="audio buffer delay in ms (default: 50)",
    )
    p.add_argument(
        "--bit-rate",
        type=int,
        help="audio bit rate in bps (default: 128000)",
    )
    p.add_argument(
        "--dup",
        action="store_true",
        help="duplicate audio (keep playing on device)",
    )
    p.add_argument(
        "--usb",
        action="store_true",
        help="force USB transport (do not switch to Wi-Fi)",
    )
    p.add_argument(
        "--detach",
        action="store_true",
        help="run capture in background (requires daemon)",
    )
    p.add_argument(
        "--yes",
        "-y",
        action="store_true",
        help="accept defaults without interactive prompts",
    )

    add("version", "show version information", cmd_version)
    return parser
