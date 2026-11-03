"""SoundHop command-line interface.

The CLI is deliberately usable in two modes:

* friendly defaults for everyday use (``soundhop connect``, ``soundhop status``),
* scriptable ``--json`` output and a complete set of power-user commands.

It is a thin client of the daemon for anything that needs to persist or
auto-reconnect; simple read-only commands work without a daemon.
"""

from __future__ import annotations

import os
import sys
from typing import Sequence

from ..backend import BackendError
from ..config import ConfigError
from ..controller import ControllerError
from ..discovery import Peer
from ..ipc import DaemonNotRunning, IpcError, is_running, request
from .android_cmds import (
    cmd_android,
    cmd_android_devices,
    cmd_android_encoders,
    cmd_android_install,
    cmd_android_start,
    cmd_android_status,
    cmd_android_stop,
    cmd_android_wifi,
)
from .core_cmds import (
    _daemon_snapshot,
    _ensure_daemon,
    _get_peers,
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
from .doctor import _run_doctor, cmd_bug_report, cmd_doctor
from .parser import _common_flags, build_parser
from .ui import (
    _json_enabled,
    _looks_like_host,
    _manual_peer,
    _match_peer,
    _peer_from_dict,
    _select_peer,
    _status_icon,
    emit_json,
    error,
    info,
    ok,
    print_peers,
    print_status,
    warning,
)

__all__ = [
    "DaemonNotRunning",
    "IpcError",
    "Peer",
    "_common_flags",
    "_daemon_snapshot",
    "_ensure_daemon",
    "_get_peers",
    "_json_enabled",
    "_looks_like_host",
    "_manual_peer",
    "_match_peer",
    "_peer_from_dict",
    "_run_doctor",
    "_select_peer",
    "_status_icon",
    "build_parser",
    "cmd_android",
    "cmd_android_devices",
    "cmd_android_encoders",
    "cmd_android_install",
    "cmd_android_start",
    "cmd_android_status",
    "cmd_android_stop",
    "cmd_android_wifi",
    "cmd_auto",
    "cmd_bug_report",
    "cmd_completions",
    "cmd_config",
    "cmd_connect",
    "cmd_daemon",
    "cmd_disconnect",
    "cmd_discover",
    "cmd_doctor",
    "cmd_firewall",
    "cmd_logs",
    "cmd_service",
    "cmd_setup",
    "cmd_share",
    "cmd_status",
    "cmd_stop",
    "cmd_tray",
    "cmd_version",
    "emit_json",
    "error",
    "info",
    "is_running",
    "main",
    "ok",
    "print_peers",
    "print_status",
    "request",
    "warning",
]


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    for name, default in (("json", False), ("verbose", False), ("no_color", False)):
        if not hasattr(args, name):
            setattr(args, name, default)
    if args.no_color:
        os.environ["NO_COLOR"] = "1"
    if not getattr(args, "func", None):
        parser.print_help()
        return 0
    try:
        return int(args.func(args))
    except KeyboardInterrupt:
        print()
        return 130
    except (ConfigError, ControllerError, BackendError, IpcError) as exc:
        error(str(exc))
        return 1
    except DaemonNotRunning as exc:
        error(str(exc))
        print(
            "Start it with 'soundhop daemon' or 'soundhop service install'.",
            file=sys.stderr,
        )
        return 1
    except (OSError, ValueError) as exc:
        error(str(exc))
        return 1
    except BrokenPipeError:
        return 0
