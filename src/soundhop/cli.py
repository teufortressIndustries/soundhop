"""SoundHop command-line interface.

The CLI is deliberately usable in two modes:

* friendly defaults for everyday use (``soundhop connect``, ``soundhop status``),
* scriptable ``--json`` output and a complete set of power-user commands.

It is a thin client of the daemon for anything that needs to persist or
auto-reconnect; simple read-only commands work without a daemon.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import time
from dataclasses import fields
from typing import Any, Iterable, Sequence

from .backend import AudioBackend, BackendError
from .config import (
    Config,
    ConfigError,
    SECURITY_MODES,
    parse_config_value,
)
from .constants import (
    APP_NAME,
    DEFAULT_AUDIO_PORT,
    PROTOCOL_VERSION,
    SINK_PREFIX,
    VERSION,
)
from .controller import Controller, ControllerError
from .daemon import (
    daemon_log_tail,
    ensure_daemon,
    run_daemon,
)
from .discovery import Peer, discover_once, mdns_available
from .firewall import close_ports, detect as detect_firewall, open_ports
from .ipc import DaemonNotRunning, IpcError, is_running, request
from .service import (
    autostart_path,
    service_status,
    systemctl_available,
    tray_dependency_available,
    tray_dependency_hint,
    unit_installed,
)
from .state import State
from .util import (
    command_exists,
    format_age,
    format_duration,
    log_path,
    process_running,
    state_path,
    style,
    tcp_reachable,
)

# ---------------------------------------------------------------------------
# small output helpers
# ---------------------------------------------------------------------------


def _json_enabled(args: argparse.Namespace) -> bool:
    return bool(getattr(args, "json", False))


def emit_json(data: Any) -> None:
    print(json.dumps(data, indent=2, default=str, sort_keys=False))


def error(message: str) -> None:
    print(f"{style('error:', 'red', 'bold')} {message}", file=sys.stderr)


def info(message: str) -> None:
    print(message)


def warning(message: str) -> None:
    print(f"{style('warning:', 'yellow', 'bold')} {message}", file=sys.stderr)


def ok(message: str) -> None:
    print(f"{style('✓', 'green')} {message}")


def _status_icon(peer: Peer) -> str:
    if not peer.compatible:
        return style("!", "yellow")
    return style("●", "green")


def _peer_from_dict(raw: dict[str, Any]) -> Peer:
    valid = {f.name for f in fields(Peer)}
    return Peer(**{k: v for k, v in raw.items() if k in valid})


def print_peers(peers: Iterable[Peer], known: Iterable[Peer] = ()) -> None:
    online = list(peers)
    known_list = [p for p in known if p.id not in {q.id for q in online}]
    if not online and not known_list:
        info(style("No devices found yet.", "yellow"))
        info("")
        info("Make sure the other device is running SoundHop and sharing:")
        info("  soundhop setup        # first time on the other device")
        info("  soundhop share on     # enable its speakers")
        info("")
        info("If it is running, check that both devices are on the same network")
        info("and that UDP 47777 and TCP 4713 are allowed by the firewall.")
        info("You can also connect directly:  soundhop connect --host 192.168.1.42")
        return

    headers = ("", "NAME", "ADDRESS", "SOURCE", "SEEN")
    rows: list[tuple[str, str, str, str, str]] = []
    for peer in online:
        source = peer.source
        if peer.source == "udp":
            source = "lan"
        elif peer.source == "mdns":
            source = "mdns"
        rows.append(
            (
                _status_icon(peer),
                peer.display_name,
                f"{peer.host}:{peer.port}",
                source,
                format_age(peer.age),
            )
        )
    for peer in known_list:
        source = "known"
        rows.append(
            (
                style("○", "gray"),
                peer.display_name,
                f"{peer.host}:{peer.port}",
                source,
                format_age(max(0.0, time.time() - peer.last_seen)),
            )
        )

    widths = [max(len(h), *(len(r[i]) for r in rows)) for i, h in enumerate(headers)]
    # Do not count ANSI escapes when sizing the icon column.
    widths[0] = 2
    header_line = "  ".join(
        style(h.ljust(widths[i]), "dim") if h else " " * widths[i]
        for i, h in enumerate(headers)
    )
    print(header_line)
    for row in rows:
        cells = []
        for i, value in enumerate(row):
            width = widths[i]
            if i == 0:
                # The icon may contain ANSI codes; pad manually.
                plain_len = 2 if len(value) > 1 else 1
                cells.append(value + " " * max(0, width - plain_len))
            else:
                cells.append(value.ljust(width))
        print("  ".join(cells))
    if known_list:
        info("")
        info(style("○ = previously used device, not currently visible", "dim"))


def print_status(data: dict[str, Any]) -> None:
    daemon = data.get("daemon") or {}
    backend = data.get("backend") or {}
    sharing = data.get("sharing") or {}
    connection = data.get("connection")
    auto = data.get("auto") or {}
    device = data.get("device") or {}

    print(style(f"{APP_NAME} {data.get('version', VERSION)}", "bold"), end="")
    if device.get("name"):
        print(f" — {device['name']}")
    else:
        print()

    if daemon.get("running"):
        uptime = format_duration(daemon.get("uptime"))
        print(f"  {'Daemon':<12} running (pid {daemon.get('pid')}, {uptime})")
    else:
        print(f"  {'Daemon':<12} {style('not running', 'yellow')}")

    if backend.get("available"):
        print(f"  {'Backend':<12} {backend.get('server', 'unknown')}")
        default = backend.get("default_sink", "")
        print(f"  {'Default':<12} {default or style('none', 'yellow')}")
    else:
        print(f"  {'Backend':<12} {style('unavailable', 'red')}")

    if sharing.get("enabled"):
        security = sharing.get("security", "trusted-lan")
        print(
            f"  {'Sharing':<12} {style('on', 'green')} "
            f"port {sharing.get('port')} ({security})"
        )
        addresses = sharing.get("addresses") or []
        if addresses:
            print(f"  {'Addresses':<12} {', '.join(addresses)}")
    else:
        print(f"  {'Sharing':<12} {style('off', 'yellow')}")

    if connection:
        name = connection.get("name", "remote device")
        host = connection.get("host", "")
        port = connection.get("port", "")
        duration = connection.get("duration", "")
        print(
            f"  {'Output':<12} {style(name, 'cyan')} "
            f"({host}:{port}, {duration})"
        )
    else:
        print(f"  {'Output':<12} this device")

    auto_state = "on"
    if not auto.get("enabled", True):
        auto_state = "off"
    elif auto.get("paused"):
        auto_state = "paused"
    print(f"  {'Auto-connect':<12} {auto_state}")

    peers = data.get("peers") or []
    if peers:
        print("")
        print(style("Discovered devices", "bold"))
        print_peers([_peer_from_dict(p) for p in peers])

    warnings = data.get("warnings") or []
    for item in warnings:
        warning(item)


# ---------------------------------------------------------------------------
# daemon/client helpers
# ---------------------------------------------------------------------------


def _daemon_snapshot() -> dict[str, Any] | None:
    try:
        return request("status", timeout=2.0)
    except (DaemonNotRunning, IpcError, OSError):
        return None


def _ensure_daemon(quiet: bool = False, verbose: bool = False) -> bool:
    if is_running():
        return True
    if not ensure_daemon(verbose=verbose):
        if not quiet:
            error("The SoundHop daemon could not be started.")
            tail = daemon_log_tail(12)
            if tail:
                print(style("Recent log:", "dim"))
                print(tail, file=sys.stderr)
            else:
                print(f"Try running 'soundhop daemon' to see the error.", file=sys.stderr)
        return False
    return True


def _get_peers(timeout: float = 2.5, *, include_known: bool = False) -> tuple[list[Peer], list[Peer]]:
    """Return (online peers, known peers)."""

    online: list[Peer] = []
    daemon = _daemon_snapshot()
    if daemon is not None:
        online = [_peer_from_dict(p) for p in daemon.get("peers", [])]
    else:
        try:
            config = Config.load()
            online = discover_once(
                discovery_port=config.discovery.port,
                timeout=timeout,
                use_mdns=config.discovery.mdns,
            )
        except (ConfigError, OSError):
            online = []

    state = State.load()
    known: list[Peer] = []
    if include_known:
        for item in state.known_peers.values():
            known.append(
                Peer(
                    id=item.id,
                    name=item.name,
                    host=item.host,
                    port=item.port,
                    version=item.version,
                    security=item.security,
                    last_seen=item.last_seen,
                    source="known",
                )
            )
    return online, known


def _match_peer(query: str, peers: Sequence[Peer]) -> Peer | None:
    query = query.strip()
    if not query:
        return None
    exact = [p for p in peers if p.id == query]
    if exact:
        return exact[0]
    prefix = [p for p in peers if p.id.startswith(query)]
    if len(prefix) == 1:
        return prefix[0]
    named = [p for p in peers if p.display_name.lower() == query.lower()]
    if len(named) == 1:
        return named[0]
    partial = [p for p in peers if query.lower() in p.display_name.lower()]
    if len(partial) == 1:
        return partial[0]
    return None


def _select_peer(peers: Sequence[Peer], known: Sequence[Peer] = ()) -> Peer | None:
    candidates = list(peers)
    if not candidates:
        candidates = list(known)
    if not candidates:
        return None
    if len(candidates) == 1:
        print(f"Using {style(candidates[0].display_name, 'cyan')}.")
        return candidates[0]
    print("Available devices:")
    for index, peer in enumerate(candidates, 1):
        suffix = "" if peer in peers else style(" (last seen)", "dim")
        print(f"  [{index}] {peer.display_name} — {peer.host}:{peer.port}{suffix}")
    print("  [q] cancel")
    try:
        answer = input(f"Choose a device [1-{len(candidates)}]: ").strip() or "1"
    except EOFError:
        return None
    if answer.lower() in {"q", "quit", "cancel"}:
        return None
    try:
        index = int(answer)
    except ValueError:
        if 1 <= len(answer) and answer.isdigit():
            index = int(answer)
        else:
            return _match_peer(answer, candidates)
    if 1 <= index <= len(candidates):
        return candidates[index - 1]
    return None


def _looks_like_host(value: str) -> bool:
    return "." in value or ":" in value or value == "localhost"


def _manual_peer(host: str, port: int, name: str = "") -> Peer:
    return Peer(
        id=f"manual-{host}",
        name=name or host,
        host=host,
        port=port,
        source="manual",
    )


# ---------------------------------------------------------------------------
# command implementations
# ---------------------------------------------------------------------------


def cmd_status(args: argparse.Namespace) -> int:
    data = _daemon_snapshot()
    if data is None:
        try:
            config = Config.load()
            controller = Controller(config=config)
            peers = discover_once(
                discovery_port=config.discovery.port,
                timeout=min(1.5, config.discovery.timeout_s),
                use_mdns=config.discovery.mdns,
            )
            data = controller.status(peers)
            data["daemon"] = {"running": False}
        except (ConfigError, BackendError, OSError) as exc:
            error(str(exc))
            return 1
    if _json_enabled(args):
        emit_json(data)
        return 0
    print_status(data)
    return 0


def cmd_discover(args: argparse.Namespace) -> int:
    daemon = _daemon_snapshot()
    if daemon is not None:
        peers = [_peer_from_dict(p) for p in daemon.get("peers", [])]
        known: list[Peer] = []
        state = State.load()
        for item in state.known_peers.values():
            known.append(
                Peer(
                    id=item.id,
                    name=item.name,
                    host=item.host,
                    port=item.port,
                    version=item.version,
                    security=item.security,
                    last_seen=item.last_seen,
                    source="known",
                )
            )
    else:
        config = Config.load()
        try:
            peers = discover_once(
                discovery_port=config.discovery.port,
                timeout=args.timeout,
                use_mdns=config.discovery.mdns,
            )
        except (ConfigError, OSError) as exc:
            error(str(exc))
            return 1
        known = []
        state = State.load()
        for item in state.known_peers.values():
            known.append(
                Peer(
                    id=item.id,
                    name=item.name,
                    host=item.host,
                    port=item.port,
                    version=item.version,
                    security=item.security,
                    last_seen=item.last_seen,
                    source="known",
                )
            )
    if _json_enabled(args):
        emit_json(
            {
                "peers": [p.to_dict() for p in peers],
                "known_peers": [
                    {**p.to_dict(), "online": False} for p in known if p.id not in {q.id for q in peers}
                ],
            }
        )
        return 0
    print_peers(peers, known)
    return 0


def cmd_connect(args: argparse.Namespace) -> int:
    peer: Peer | None = None
    if args.host:
        peer = _manual_peer(args.host, args.port or DEFAULT_AUDIO_PORT, args.name or "")
    else:
        online, known = _get_peers(include_known=True)
        if args.peer:
            peer = _match_peer(args.peer, online) or _match_peer(args.peer, known)
            if peer is None and _looks_like_host(args.peer):
                peer = _manual_peer(args.peer, args.port or DEFAULT_AUDIO_PORT)
            if peer is None:
                error(f"No device matches {args.peer!r}.")
                print_peers(online, known)
                return 1
            if args.port:
                peer = Peer(
                    id=peer.id,
                    name=peer.name,
                    host=peer.host,
                    port=args.port,
                    version=peer.version,
                    protocol=peer.protocol,
                    security=peer.security,
                    source=peer.source,
                )
        else:
            if not sys.stdin.isatty():
                error("No device specified and stdin is not a terminal.")
                print_peers(online, known)
                return 1
            peer = _select_peer(online, known)
            if peer is None:
                if not online and not known:
                    print_peers(online, known)
                return 1

    if not _ensure_daemon(verbose=args.verbose):
        return 1
    payload: dict[str, Any] = {
        "peer": peer.id,
        "host": peer.host,
        "port": peer.port,
        "name": peer.display_name,
        "switch_default": not args.no_switch,
        "move_streams": not args.no_move,
        "by": "cli",
    }
    try:
        result = request("connect", timeout=30.0, **payload)
    except (DaemonNotRunning, IpcError, OSError) as exc:
        error(str(exc))
        return 1
    if _json_enabled(args):
        emit_json(result)
        return 0
    ok(f"Audio is now playing on {style(result.get('name', peer.display_name), 'cyan')}.")
    if result.get("sink"):
        print(f"  Sink: {result['sink']}")
    if args.no_switch:
        print("  The default output was left unchanged; select the sink manually.")
    return 0


def cmd_disconnect(args: argparse.Namespace) -> int:
    if is_running():
        try:
            result = request(
                "disconnect",
                timeout=20.0,
                forget=args.forget,
                restore=not args.no_restore,
                all=args.all or not args.peer,
            )
        except (DaemonNotRunning, IpcError, OSError) as exc:
            error(str(exc))
            return 1
    else:
        try:
            controller = Controller(config=Config.load())
            result = controller.disconnect(
                forget=args.forget,
                restore=not args.no_restore,
                all_tunnels=args.all or not args.peer,
            )
        except (ConfigError, ControllerError, BackendError, OSError) as exc:
            error(str(exc))
            return 1
    if _json_enabled(args):
        emit_json(result)
        return 0
    if result.get("restored_to"):
        ok(f"Audio restored to {result['restored_to']}.")
    else:
        ok("Remote audio disconnected.")
    if result.get("forgotten"):
        print("  The device was forgotten; auto-connect is off for it.")
    elif result.get("auto_paused"):
        print("  Auto-connect is paused until you connect again.")
    return 0


def cmd_share(args: argparse.Namespace) -> int:
    action = args.action or "status"
    if action == "status":
        data = _daemon_snapshot()
        if data is None:
            try:
                data = Controller(config=Config.load()).status()
                data["daemon"] = {"running": False}
            except (ConfigError, BackendError, OSError) as exc:
                error(str(exc))
                return 1
        if _json_enabled(args):
            emit_json(
                {
                    "sharing": data.get("sharing", {}),
                    "daemon": data.get("daemon", {}),
                    "warnings": data.get("warnings", []),
                }
            )
            return 0
        sharing = data.get("sharing", {})
        if sharing.get("enabled"):
            ok(
                f"Sharing is on (port {sharing.get('port')}, "
                f"{sharing.get('security', 'trusted-lan')})."
            )
            addresses = sharing.get("addresses") or []
            if addresses:
                print(f"  Reachable at: {', '.join(addresses)}")
        else:
            print(f"{style('Sharing is off.', 'yellow')} Run 'soundhop share on' to enable it.")
        return 0

    enabled = action == "on"
    if not _ensure_daemon(verbose=args.verbose):
        return 1
    try:
        result = request("share", timeout=20.0, enabled=enabled)
    except (DaemonNotRunning, IpcError, OSError) as exc:
        error(str(exc))
        return 1
    if _json_enabled(args):
        emit_json(result)
        return 0
    if enabled:
        ok(f"Sharing this device's speakers on port {result.get('port')}.")
        addresses = result.get("addresses") or []
        if addresses:
            print(f"  Reachable at: {', '.join(addresses)}")
    else:
        ok("Stopped sharing this device's speakers.")
        if result.get("note"):
            print(f"  {result['note']}")
    return 0


def cmd_auto(args: argparse.Namespace) -> int:
    action = args.action or "status"
    if action == "status":
        data = _daemon_snapshot()
        if data is None:
            config = Config.load()
            state = State.load()
            payload = {
                "auto_connect": config.connection.auto_connect,
                "paused": state.auto_paused,
                "last_peer": config.connection.last_peer,
            }
        else:
            payload = data.get("auto", {})
        if _json_enabled(args):
            emit_json(payload)
            return 0
        enabled = "on" if payload.get("auto_connect", payload.get("enabled")) else "off"
        if payload.get("paused"):
            enabled = "paused"
        print(f"Auto-connect: {enabled}")
        if payload.get("last_peer"):
            print(f"  Last device: {payload['last_peer']}")
        return 0

    enabled = action == "on"
    if is_running():
        try:
            result = request("auto", timeout=10.0, enabled=enabled)
        except (DaemonNotRunning, IpcError, OSError) as exc:
            error(str(exc))
            return 1
    else:
        config = Config.load()
        config.connection.auto_connect = enabled
        config.save()
        result = {"auto_connect": enabled, "paused": False}
    if _json_enabled(args):
        emit_json(result)
        return 0
    ok(f"Auto-connect {'enabled' if enabled else 'disabled'}.")
    return 0


def cmd_daemon(args: argparse.Namespace) -> int:
    if args.detach:
        if _ensure_daemon(verbose=args.verbose):
            ok("SoundHop daemon is running.")
            return 0
        return 1
    return run_daemon(verbose=args.verbose)


def cmd_stop(args: argparse.Namespace) -> int:
    stopped = False
    if is_running():
        try:
            request("shutdown", timeout=5.0)
            stopped = True
        except (DaemonNotRunning, IpcError, OSError):
            pass
    if not stopped and unit_installed() and systemctl_available():
        from .service import stop_service

        stopped = stop_service()
    if _json_enabled(args):
        emit_json({"stopped": stopped})
        return 0
    if stopped:
        ok("SoundHop daemon stopped.")
    else:
        print("SoundHop daemon is not running.")
    return 0


def cmd_service(args: argparse.Namespace) -> int:
    from . import service

    action = args.action
    if action == "install":
        result = service.install(
            enable=not args.no_enable,
            start=not args.no_start,
            tray=args.tray,
        )
        if _json_enabled(args):
            emit_json(result)
            return 0 if result.get("unit") else 1
        ok(f"Installed {result['unit']}.")
        if result.get("enabled"):
            print("  Enabled at login.")
        if result.get("started"):
            print("  Started now.")
        if result.get("tray_autostart"):
            print(f"  Tray autostart: {result['tray_autostart']}")
        return 0
    if action == "uninstall":
        result = service.uninstall()
        if _json_enabled(args):
            emit_json(result)
            return 0
        ok("SoundHop user service uninstalled.")
        return 0
    if action == "status":
        data = service_status()
        if _json_enabled(args):
            emit_json(data)
            return 0
        print(f"Unit:      {data.get('unit')}")
        print(f"Installed: {'yes' if data.get('installed') else 'no'}")
        print(f"Active:    {'yes' if data.get('active') else 'no'}")
        print(f"Enabled:   {'yes' if data.get('enabled') else 'no'}")
        if data.get("since"):
            print(f"Since:     {data['since']}")
        return 0
    if action == "start":
        result = service.start_service()
        if _json_enabled(args):
            emit_json({"started": result})
            return 0 if result else 1
        ok("Started." if result else "Could not start the service.")
        return 0 if result else 1
    if action == "stop":
        result = service.stop_service()
        if _json_enabled(args):
            emit_json({"stopped": result})
            return 0 if result else 1
        ok("Stopped." if result else "Could not stop the service.")
        return 0 if result else 1
    if action == "restart":
        result = service.restart_service()
        if _json_enabled(args):
            emit_json({"restarted": result})
            return 0 if result else 1
        ok("Restarted." if result else "Could not restart the service.")
        return 0 if result else 1
    if action == "enable":
        result = service.enable_service()
        if _json_enabled(args):
            emit_json({"enabled": result})
            return 0 if result else 1
        ok("Enabled." if result else "Could not enable the service.")
        return 0 if result else 1
    if action == "disable":
        result = service.disable_service()
        if _json_enabled(args):
            emit_json({"disabled": result})
            return 0 if result else 1
        ok("Disabled." if result else "Could not disable the service.")
        return 0 if result else 1
    error(f"Unknown service action: {action}")
    return 1


def cmd_firewall(args: argparse.Namespace) -> int:
    config = Config.load()
    if args.action == "status":
        state = detect_firewall()
        data = state.to_dict()
        if _json_enabled(args):
            emit_json(data)
            return 0
        print(f"Firewall: {state.kind}")
        print(f"Active:   {'yes' if state.active else 'no'}")
        print(f"Detail:   {state.detail}")
        return 0
    if args.action == "open":
        result = open_ports(
            config.sharing.port,
            config.discovery.port,
            dry_run=args.dry_run,
            include_mdns=not args.no_mdns,
        )
    elif args.action == "close":
        result = close_ports(
            config.sharing.port,
            config.discovery.port,
            dry_run=args.dry_run,
            include_mdns=not args.no_mdns,
        )
    else:
        error(f"Unknown firewall action: {args.action}")
        return 1
    data = result.to_dict()
    if _json_enabled(args):
        emit_json(data)
        return 0 if result.ok else 1
    if result.commands:
        print(style("Firewall commands:", "bold"))
        for command in result.commands:
            print("  " + " ".join(command))
    if result.note:
        print(result.note)
    for item in result.errors:
        error(item)
    if result.ran and result.ok:
        ok("Firewall updated.")
    return 0 if result.ok else 1


def cmd_config(args: argparse.Namespace) -> int:
    config = Config.load()
    action = args.action
    if action == "path":
        print(config.path)
        return 0
    if action == "show":
        if _json_enabled(args):
            emit_json(config.to_dict())
        else:
            try:
                print(config.path.read_text(encoding="utf-8"))
            except OSError:
                print(config.to_toml())
        return 0
    if action == "get":
        try:
            value = config.get(args.key)
        except ConfigError as exc:
            error(str(exc))
            return 1
        if _json_enabled(args):
            emit_json({"key": args.key, "value": value})
        else:
            print(value)
        return 0
    if action == "set":
        try:
            value = parse_config_value(args.value)
            config.set(args.key, value)
            config.save()
        except (ConfigError, ValueError) as exc:
            error(str(exc))
            return 1
        if is_running():
            try:
                request("reload", timeout=20.0)
            except (DaemonNotRunning, IpcError, OSError) as exc:
                warning(f"Saved, but the daemon could not reload: {exc}")
        if _json_enabled(args):
            emit_json({"key": args.key, "value": config.get(args.key)})
        else:
            ok(f"{args.key} = {config.get(args.key)}")
        return 0
    if action == "edit":
        editor = os.environ.get("VISUAL") or os.environ.get("EDITOR") or "nano"
        if not command_exists(editor.split()[0]):
            error(f"Editor not found: {editor}. Set $EDITOR.")
            return 1
        subprocess.call([*editor.split(), str(config.path)])
        if is_running():
            try:
                request("reload", timeout=20.0)
                ok("Configuration reloaded.")
            except (DaemonNotRunning, IpcError, OSError) as exc:
                warning(f"Saved, but reload failed: {exc}")
        return 0
    if action == "reload":
        if not is_running():
            print("Daemon is not running; configuration will be read on next start.")
            return 0
        try:
            result = request("reload", timeout=20.0)
        except (DaemonNotRunning, IpcError, OSError) as exc:
            error(str(exc))
            return 1
        if _json_enabled(args):
            emit_json(result)
        else:
            ok("Configuration reloaded.")
        return 0
    error(f"Unknown config action: {action}")
    return 1


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
    add(
        "pactl",
        command_exists("pactl"),
        shutil.which("pactl") or "not found",
        "Install pipewire-pulse or pulseaudio-utils.",
    )

    backend = AudioBackend()
    backend_ok = backend.available()
    add(
        "Audio server",
        backend_ok,
        backend.server_label() if backend_ok else "not reachable",
        "Start PipeWire/WirePlumber or PulseAudio.",
    )
    if backend_ok:
        add("PipeWire", backend.is_pipewire(), backend.server_name(), "PulseAudio also works.")
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
                m for m in modules if m.name.startswith("module-tunnel-") and m.arg("sink_name").startswith(SINK_PREFIX)
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
        "available" if command_exists("avahi-daemon") or command_exists("avahi-browse") else "not found",
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


def cmd_tray(args: argparse.Namespace) -> int:
    try:
        from .gui import run_tray
    except ImportError as exc:
        error(f"The tray needs PySide6: {exc}")
        print(tray_dependency_hint(), file=sys.stderr)
        return 1
    return run_tray(verbose=args.verbose)


def cmd_logs(args: argparse.Namespace) -> int:
    path = log_path()
    if not path.exists():
        print(f"No log file yet ({path}).")
        return 0
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as exc:
        error(str(exc))
        return 1
    tail = lines[-args.lines :]
    print("\n".join(tail))
    if args.follow:
        if command_exists("tail"):
            try:
                subprocess.call(["tail", "-n", "0", "-f", str(path)])
            except KeyboardInterrupt:
                return 0
        else:
            try:
                while True:
                    time.sleep(1)
                    new_lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
                    for line in new_lines[len(lines):]:
                        print(line)
                    lines = new_lines
            except KeyboardInterrupt:
                return 0
    return 0


def cmd_bug_report(args: argparse.Namespace) -> int:
    """Generate a clean, sanitized bug report for GitHub issues."""
    import urllib.parse
    from pathlib import Path

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
        issue_url = f"https://github.com/soundhop/soundhop/issues/new?body={encoded_body}"
        if command_exists("xdg-open"):
            subprocess.call(["xdg-open", issue_url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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
    print(style("Tip: Copy and paste the markdown above into a GitHub issue, or run 'soundhop bug-report --open'.", "dim"))
    return 0


def cmd_completions(args: argparse.Namespace) -> int:
    commands = (
        "setup status discover devices connect disconnect share auto daemon stop "
        "service firewall config doctor tray logs bug-report version completions"
    )
    if args.shell == "bash":
        print(
            f"""# SoundHop bash completion
_soundhop() {{
  local cur prev
  cur="${{COMP_WORDS[COMP_CWORD]}}"
  prev="${{COMP_WORDS[COMP_CWORD-1]}}"
  if [[ $COMP_CWORD -eq 1 ]]; then
    COMPREPLY=( $(compgen -W "{commands}" -- "$cur") )
  elif [[ "$prev" == "service" ]]; then
    COMPREPLY=( $(compgen -W "install uninstall start stop restart enable disable status" -- "$cur") )
  elif [[ "$prev" == "firewall" ]]; then
    COMPREPLY=( $(compgen -W "open close status" -- "$cur") )
  elif [[ "$prev" == "config" ]]; then
    COMPREPLY=( $(compgen -W "path show get set edit reload" -- "$cur") )
  elif [[ "$prev" == "share" ]]; then
    COMPREPLY=( $(compgen -W "on off status toggle" -- "$cur") )
  elif [[ "$prev" == "auto" ]]; then
    COMPREPLY=( $(compgen -W "on off pause resume status" -- "$cur") )
  fi
}}
complete -F _soundhop soundhop shop"""
        )
    elif args.shell == "zsh":
        print(
            f"""#compdef soundhop shop
# SoundHop zsh completion
_soundhop() {{
  local -a commands
  commands=({commands})
  _arguments '1:command:($commands)' '*::arg:->args'
  case $state in
    args)
      case $words[2] in
        service) _values 'action' install uninstall start stop restart enable disable status ;;
        firewall) _values 'action' open close status ;;
        config) _values 'action' path show get set edit reload ;;
        share) _values 'action' on off status toggle ;;
        auto) _values 'action' on off pause resume status ;;
      esac
      ;;
  esac
}}
_soundhop "$@\""""
        )
    elif args.shell == "fish":
        print(
            f"""# SoundHop fish completion
for cmd in soundhop shop
    complete -c $cmd -f -a "{commands}"
    complete -c $cmd -n "__fish_seen_subcommand_from service" -a "install uninstall start stop restart enable disable status"
    complete -c $cmd -n "__fish_seen_subcommand_from firewall" -a "open close status"
    complete -c $cmd -n "__fish_seen_subcommand_from config" -a "path show get set edit reload"
    complete -c $cmd -n "__fish_seen_subcommand_from share" -a "on off status toggle"
    complete -c $cmd -n "__fish_seen_subcommand_from auto" -a "on off pause resume status"
end"""
        )
    return 0


def cmd_setup(args: argparse.Namespace) -> int:
    try:
        config = Config.load()
    except ConfigError as exc:
        error(str(exc))
        return 1
    backend = AudioBackend()
    print(style(f"{APP_NAME} setup", "bold"))
    print()
    if backend.available():
        print(f"Audio server: {backend.server_label()}")
    else:
        error("PipeWire/PulseAudio is not reachable through pactl.")
        print("Install pipewire-pulse (PipeWire) or pulseaudio-utils, then retry.")
        return 2

    interactive = sys.stdin.isatty() and not args.yes

    def ask(prompt: str, default: str) -> str:
        if not interactive:
            return default
        try:
            answer = input(f"{prompt} [{default}]: ").strip()
        except EOFError:
            return default
        return answer or default

    def ask_yes(prompt: str, default: bool = True) -> bool:
        if not interactive:
            return default
        suffix = "Y/n" if default else "y/N"
        try:
            answer = input(f"{prompt} [{suffix}]: ").strip().lower()
        except EOFError:
            return default
        if not answer:
            return default
        return answer in {"y", "yes"}

    name = args.name or ask("This device's name", config.device.name)
    port_text = str(args.port or ask("Audio port", str(config.sharing.port)))
    try:
        port = int(port_text)
        if not 1 <= port <= 65535:
            raise ValueError
    except ValueError:
        error(f"Invalid port: {port_text}")
        return 1

    security = args.security
    if security is None:
        if interactive:
            print()
            print("Security mode:")
            print("  1) Trusted LAN (recommended) — anonymous auth, private IP ranges only")
            print("  2) Cookie — requires copying ~/.config/pulse/cookie to other devices")
            print("  3) Open — any host that can reach the port (not recommended)")
            choice = ask("Choose 1-3", "1")
            security = {"1": "trusted-lan", "2": "cookie", "3": "open"}.get(
                choice, "trusted-lan"
            )
        else:
            security = "trusted-lan"

    auto = args.auto
    if auto is None:
        auto = ask_yes("Reconnect automatically to the last device?", True)

    config.device.name = name
    config.sharing.port = port
    config.sharing.security = security
    config.sharing.enabled = True
    config.connection.auto_connect = auto
    config.connection.switch_default = ask_yes("Switch the default output automatically?", True)
    config.connection.restore_default = ask_yes("Restore local audio when disconnecting?", True)
    config.normalize()
    config.save()
    ok(f"Saved configuration to {config.path}")

    # Firewall ---------------------------------------------------------
    if not args.no_firewall:
        fw = detect_firewall()
        if fw.active:
            print()
            print(f"Firewall detected: {fw.kind} ({fw.detail})")
            result = open_ports(
                config.sharing.port,
                config.discovery.port,
                dry_run=False,
                include_mdns=not args.no_mdns,
            )
            for command in result.commands:
                print("  " + " ".join(command))
            for item in result.errors:
                warning(item)
            if result.ran and result.ok:
                ok("Firewall ports opened.")
            elif not result.ok:
                warning("Could not fully update the firewall; see the commands above.")
        else:
            print(f"Firewall: {fw.detail}")

    # Service ----------------------------------------------------------
    tray = False
    if not args.no_service and systemctl_available():
        if interactive:
            install_service = ask_yes("Install and enable the SoundHop user service?", True)
        else:
            install_service = True
        if install_service:
            from . import service

            result = service.install(
                enable=True,
                start=True,
                tray=False,
            )
            if result.get("unit"):
                ok(f"Installed {result['unit']}.")
            else:
                warning("Could not install the user service.")
            tray = tray_dependency_available()
            if tray and not args.no_tray:
                if interactive:
                    tray = ask_yes("Start the tray at login?", True)
                else:
                    tray = True
                if tray and service.install_tray_autostart():
                    ok(f"Tray autostart: {autostart_path()}")
                elif tray:
                    warning("Could not write the tray autostart file.")
        elif not interactive:
            install_service = False
    else:
        if not args.no_service:
            print("systemd user services are not available; starting a background daemon instead.")
        install_service = False

    if not install_service:
        if _ensure_daemon():
            ok("Daemon started.")

    if not tray_dependency_available():
        print()
        print(style("Tip:", "bold"), tray_dependency_hint())

    # Quick discovery ---------------------------------------------------
    print()
    print("Looking for other SoundHop devices…")
    try:
        peers = discover_once(
            discovery_port=config.discovery.port,
            timeout=2.0,
            use_mdns=config.discovery.mdns,
        )
    except OSError:
        peers = []
    if peers:
        ok(f"Found {len(peers)} device(s):")
        print_peers(peers)
    else:
        print("No other devices visible yet.")
    print()
    print(style("Next steps", "bold"))
    print("  soundhop connect            # pick a device and play audio there")
    print("  soundhop status             # see sharing, peers and the current output")
    print("  soundhop tray               # optional system tray (if PySide6 is installed)")
    print("  soundhop doctor             # diagnose discovery or firewall problems")
    print()
    print("On your other device, run 'soundhop setup' too, then connect from either side.")
    return 0


def cmd_version(args: argparse.Namespace) -> int:
    data = {
        "version": VERSION,
        "protocol": PROTOCOL_VERSION,
        "python": platform.python_version(),
        "platform": platform.platform(),
    }
    if _json_enabled(args):
        emit_json(data)
    else:
        print(f"{APP_NAME} {VERSION} (protocol {PROTOCOL_VERSION})")
    return 0


# ---------------------------------------------------------------------------
# argument parser
# ---------------------------------------------------------------------------


def _common_flags() -> argparse.ArgumentParser:
    common = argparse.ArgumentParser(add_help=False)
    # SUPPRESS keeps a flag supplied before the subcommand from being reset by
    # the subparser's own default.
    common.add_argument(
        "--json", action="store_true", default=argparse.SUPPRESS, help="machine-readable output"
    )
    common.add_argument(
        "--verbose", action="store_true", default=argparse.SUPPRESS, help="more diagnostics"
    )
    common.add_argument(
        "--no-color", action="store_true", default=argparse.SUPPRESS, help="disable color output"
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
        p = sub.add_parser(name, help=help_text, description=help_text, parents=[common])
        p.set_defaults(func=func)
        return p

    p = add("setup", "configure this device for local audio sharing", cmd_setup)
    p.add_argument("--yes", action="store_true", help="accept defaults (non-interactive)")
    p.add_argument("--name", help="this device's advertised name")
    p.add_argument("--port", type=int, help="audio port (default 4713)")
    p.add_argument("--security", choices=SECURITY_MODES, help="listener security mode")
    p.add_argument("--auto", dest="auto", action="store_true", default=None, help="enable auto-connect")
    p.add_argument("--no-auto", dest="auto", action="store_false", help="disable auto-connect")
    p.add_argument("--no-firewall", action="store_true", help="do not touch the firewall")
    p.add_argument("--no-mdns", action="store_true", help="do not open the mDNS port")
    p.add_argument("--no-service", action="store_true", help="do not install a user service")
    p.add_argument("--no-tray", action="store_true", help="do not add tray autostart")

    add("status", "show daemon, sharing, peers and current output", cmd_status)
    p = add("discover", "find devices on the local network", cmd_discover)
    p.add_argument("--timeout", type=float, default=2.5, help="seconds to scan (default 2.5)")
    add("devices", "alias for discover", cmd_discover).add_argument(
        "--timeout", type=float, default=2.5
    )

    p = add("connect", "play this device's audio on another device", cmd_connect)
    p.add_argument("peer", nargs="?", help="device name, id, or host")
    p.add_argument("--host", help="connect directly to a host")
    p.add_argument("--port", type=int, help="remote audio port")
    p.add_argument("--name", help="friendly name for a manual host")
    p.add_argument("--no-switch", action="store_true", help="create the sink but do not make it default")
    p.add_argument("--no-move", action="store_true", help="do not move currently playing streams")

    p = add("disconnect", "stop playing on the remote device", cmd_disconnect)
    p.add_argument("peer", nargs="?", help="device name or id (default: current/all)")
    p.add_argument("--all", action="store_true", help="remove all SoundHop tunnel sinks")
    p.add_argument("--forget", action="store_true", help="also forget the last device")
    p.add_argument("--no-restore", action="store_true", help="do not restore the previous output")

    p = add("share", "share this device's speakers", cmd_share)
    p.add_argument("action", nargs="?", choices=("on", "off", "status"), default="status")

    p = add("auto", "control auto-connect", cmd_auto)
    p.add_argument("action", nargs="?", choices=("on", "off", "status"), default="status")

    p = add("daemon", "run the background daemon", cmd_daemon)
    p.add_argument("--detach", action="store_true", help="start in the background and return")

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
    p.add_argument("--no-start", action="store_true", help="do not start immediately")
    p.add_argument("--tray", action="store_true", help="also add tray autostart")

    p = add("firewall", "open or inspect the required firewall ports", cmd_firewall)
    p.add_argument("action", choices=("open", "close", "status"), nargs="?", default="status")
    p.add_argument("--dry-run", action="store_true", help="print commands without running them")
    p.add_argument("--no-mdns", action="store_true", help="do not open UDP 5353")

    p = add("config", "inspect or edit the configuration", cmd_config)
    p.add_argument("action", choices=("path", "show", "get", "set", "edit", "reload"), nargs="?", default="show")
    p.add_argument("key", nargs="?", help="dotted key, e.g. connection.auto_connect")
    p.add_argument("value", nargs="?", help="new value for 'config set'")

    add("doctor", "diagnose common problems", cmd_doctor)
    add("tray", "run the optional system tray", cmd_tray)
    add("gui", "alias for tray", cmd_tray)

    p = add("logs", "show the daemon log", cmd_logs)
    p.add_argument("--lines", type=int, default=100, help="number of lines (default 100)")
    p.add_argument("-f", "--follow", action="store_true", help="follow the log")

    p = add("bug-report", "generate a diagnostic report for bug reporting", cmd_bug_report)
    p.add_argument("--open", dest="open_browser", action="store_true", help="open GitHub issues in browser")
    p.add_argument("-o", "--output", dest="output_file", help="save report to file")
    sub.add_parser("report", parents=[common], help="alias for bug-report").set_defaults(func=cmd_bug_report)

    p = add("completions", "print shell completion code", cmd_completions)
    p.add_argument("shell", choices=("bash", "zsh", "fish"))

    add("version", "show version information", cmd_version)
    return parser


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
        print("Start it with 'soundhop daemon' or 'soundhop service install'.", file=sys.stderr)
        return 1
    except (OSError, ValueError) as exc:
        error(str(exc))
        return 1
    except BrokenPipeError:
        return 0
