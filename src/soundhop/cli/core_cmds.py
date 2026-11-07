"""Core subcommands for SoundHop CLI: connect, share, status, daemon, service, etc."""

from __future__ import annotations

import argparse
import os
import platform
import subprocess
import sys
import time
from typing import Any

from .. import ipc
from ..backend import AudioBackend, BackendError
from ..config import Config, ConfigError, parse_config_value
from ..constants import APP_NAME, DEFAULT_AUDIO_PORT, PROTOCOL_VERSION, VERSION
from ..controller import Controller, ControllerError
from ..daemon import daemon_log_tail, ensure_daemon, run_daemon
from ..discovery import Peer, discover_once
from ..firewall import close_ports, open_ports
from ..firewall import detect as detect_firewall
from ..ipc import DaemonNotRunning, IpcError
from ..service import (
    autostart_path,
    service_status,
    systemctl_available,
    tray_dependency_available,
    tray_dependency_hint,
    unit_installed,
)
from ..state import State
from ..util import command_exists, log_path, style
from .ui import (
    _json_enabled,
    _looks_like_host,
    _manual_peer,
    _match_peer,
    _peer_from_dict,
    _select_peer,
    emit_json,
    error,
    info,
    ok,
    print_peers,
    print_status,
    warning,
)


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


def _daemon_snapshot() -> dict[str, Any] | None:
    try:
        return _request("status", timeout=2.0)
    except (DaemonNotRunning, IpcError, OSError):
        return None


def _ensure_daemon(quiet: bool = False, verbose: bool = False) -> bool:
    if _is_running():
        return True
    if not ensure_daemon(verbose=verbose):
        if not quiet:
            error("The SoundHop daemon could not be started.")
            tail = daemon_log_tail(12)
            if tail:
                print(style("Recent log:", "dim"))
                print(tail, file=sys.stderr)
            else:
                print("Try running 'soundhop daemon' to see the error.", file=sys.stderr)
        return False
    return True


def _get_peers(
    timeout: float = 2.5, *, include_known: bool = False
) -> tuple[list[Peer], list[Peer]]:
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
                    {**p.to_dict(), "online": False}
                    for p in known
                    if p.id not in {q.id for q in peers}
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
    selective_mode = bool(getattr(args, "selective", False) or getattr(args, "app", None))
    switch_default = False if (args.no_switch or selective_mode) else True
    move_streams = False if (args.no_move or selective_mode) else True

    payload: dict[str, Any] = {
        "peer": peer.id,
        "host": peer.host,
        "port": peer.port,
        "name": peer.display_name,
        "switch_default": switch_default,
        "move_streams": move_streams,
        "by": "cli",
        "with_mic": bool(getattr(args, "with_mic", False)),
        "mic_direction": getattr(args, "mic_direction", "send"),
    }
    try:
        result = _request("connect", timeout=30.0, **payload)
    except (DaemonNotRunning, IpcError, OSError) as exc:
        error(str(exc))
        return 1

    app_target = getattr(args, "app", None)
    if app_target:
        try:
            _request("route_app", app=app_target, target="remote", timeout=5.0)
            ok(f"Routed application '{app_target}' to {peer.display_name}.")
        except Exception as exc:
            warning(f"Could not route application '{app_target}': {exc}")

    if _json_enabled(args):
        emit_json(result)
        return 0
    ok(f"Audio is now playing on {style(result.get('name', peer.display_name), 'cyan')}.")
    if result.get("sink"):
        print(f"  Sink: {result['sink']}")
    if result.get("with_mic"):
        ok(f"Microphone pass-through active: {result.get('source')}")
    if selective_mode:
        print("  Selective routing active: default desktop audio remains on local speakers.")
    elif args.no_switch:
        print("  The default output was left unchanged; select the sink manually.")
    return 0


def cmd_disconnect(args: argparse.Namespace) -> int:
    if _is_running():
        try:
            result = _request(
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
        result = _request("share", timeout=20.0, enabled=enabled)
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
        status_label = "on" if payload.get("auto_connect", payload.get("enabled")) else "off"
        if payload.get("paused"):
            status_label = "paused"
        print(f"Auto-connect: {status_label}")
        if payload.get("last_peer"):
            print(f"  Last device: {payload['last_peer']}")
        return 0

    if action in ("pause", "resume"):
        paused = action == "pause"
        if _is_running():
            try:
                result = _request("pause_auto", timeout=10.0, paused=paused)
            except (DaemonNotRunning, IpcError, OSError) as exc:
                error(str(exc))
                return 1
        else:
            state = State.load()
            state.auto_paused = paused
            state.save()
            result = {"paused": paused}
        if _json_enabled(args):
            emit_json(result)
            return 0
        ok(f"Auto-connect {'paused' if paused else 'resumed'}.")
        return 0

    enabled = action == "on"
    if _is_running():
        try:
            result = _request("auto", timeout=10.0, enabled=enabled)
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
    if _is_running():
        try:
            _request("shutdown", timeout=5.0)
            stopped = True
        except (DaemonNotRunning, IpcError, OSError):
            pass
    if not stopped and unit_installed() and systemctl_available():
        from ..service import stop_service

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
    from .. import service

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
        ok_flag = service.start_service()
        if _json_enabled(args):
            emit_json({"started": ok_flag})
            return 0 if ok_flag else 1
        ok("Started." if ok_flag else "Could not start the service.")
        return 0 if ok_flag else 1
    if action == "stop":
        ok_flag = service.stop_service()
        if _json_enabled(args):
            emit_json({"stopped": ok_flag})
            return 0 if ok_flag else 1
        ok("Stopped." if ok_flag else "Could not stop the service.")
        return 0 if ok_flag else 1
    if action == "restart":
        ok_flag = service.restart_service()
        if _json_enabled(args):
            emit_json({"restarted": ok_flag})
            return 0 if ok_flag else 1
        ok("Restarted." if ok_flag else "Could not restart the service.")
        return 0 if ok_flag else 1
    if action == "enable":
        ok_flag = service.enable_service()
        if _json_enabled(args):
            emit_json({"enabled": ok_flag})
            return 0 if ok_flag else 1
        ok("Enabled." if ok_flag else "Could not enable the service.")
        return 0 if ok_flag else 1
    if action == "disable":
        ok_flag = service.disable_service()
        if _json_enabled(args):
            emit_json({"disabled": ok_flag})
            return 0 if ok_flag else 1
        ok("Disabled." if ok_flag else "Could not disable the service.")
        return 0 if ok_flag else 1
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
        if _is_running():
            try:
                _request("reload", timeout=20.0)
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
        if _is_running():
            try:
                _request("reload", timeout=20.0)
                ok("Configuration reloaded.")
            except (DaemonNotRunning, IpcError, OSError) as exc:
                warning(f"Saved, but reload failed: {exc}")
        return 0
    if action == "reload":
        if not _is_running():
            print("Daemon is not running; configuration will be read on next start.")
            return 0
        try:
            result = _request("reload", timeout=20.0)
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


def cmd_tray(args: argparse.Namespace) -> int:
    try:
        from ..gui import run_tray
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
                    for line in new_lines[len(lines) :]:
                        print(line)
                    lines = new_lines
            except KeyboardInterrupt:
                return 0
    return 0


def cmd_completions(args: argparse.Namespace) -> int:
    commands = (
        "setup status discover devices connect disconnect share auto daemon stop "
        "service firewall config doctor tray logs bug-report version completions android"
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
  elif [[ "$prev" == "android" ]]; then
    COMPREPLY=( $(compgen -W "start capture connect stop disconnect status devices wifi encoders codecs install" -- "$cur") )
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
        android) _values 'action' start capture connect stop disconnect status devices wifi encoders codecs install ;;
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
    complete -c $cmd -n "__fish_seen_subcommand_from android" -a "start capture connect stop disconnect status devices wifi encoders codecs install"
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
            security = {"1": "trusted-lan", "2": "cookie", "3": "open"}.get(choice, "trusted-lan")
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
            from .. import service

            svc_result = service.install(
                enable=True,
                start=True,
                tray=False,
            )
            if svc_result.get("unit"):
                ok(f"Installed {svc_result['unit']}.")
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


def cmd_apps(args: argparse.Namespace) -> int:
    try:
        data = ipc.request("apps_list", timeout=3.0)
        apps = data.get("apps", [])
    except Exception:
        backend = AudioBackend()
        apps = [item.to_dict() for item in backend.detailed_sink_inputs()]

    if _json_enabled(args):
        emit_json({"apps": apps})
        return 0

    if not apps:
        info("No active audio playback streams found.")
        return 0

    print()
    print(style("Active Audio Applications:", "bold"))
    for item in apps:
        app_name = item.get("app_name") or item.get("binary") or f"Stream #{item['id']}"
        dest = style("Remote (SoundHop)", "cyan") if item.get("is_remote") else style("Local Speakers", "green")
        vol = f"{item.get('volume_percent', 100)}%"
        pid = f"PID {item.get('pid')}" if item.get('pid') else ""
        print(f"  • {app_name:<24} [{dest}] (ID {item['id']}, {vol}, {pid})")
    print()
    return 0


def cmd_route(args: argparse.Namespace) -> int:
    target_app = getattr(args, "app", None)
    if not target_app:
        error("Please specify an application name, binary, or stream ID.")
        return 1

    dest = "local" if getattr(args, "local", False) else "remote"
    try:
        res = ipc.request("route_app", app=target_app, target=dest, timeout=4.0)
        moved = res.get("moved", 0)
        target_sink = res.get("target_sink", dest)
    except Exception:
        controller = Controller()
        try:
            res = controller.route_app(target_app, target=dest)
            moved = res.get("moved", 0)
            target_sink = res.get("target_sink", dest)
        except Exception as exc:
            error(f"Routing failed: {exc}")
            return 1

    if moved > 0:
        ok(f"Routed {moved} active stream(s) for '{target_app}' to {dest} ({target_sink}).")
        ok(f"Persistent rule saved: future audio from '{target_app}' will play on {dest}.")
        return 0
    ok(f"Saved persistent routing rule: audio from '{target_app}' will play on {dest} ({target_sink}).")
    info("No active streams right now; audio will automatically route to remote when this app plays.")
    return 0


def cmd_unroute(args: argparse.Namespace) -> int:
    args.local = True
    return cmd_route(args)


def cmd_mic(args: argparse.Namespace) -> int:
    action = getattr(args, "action", "status") or "status"
    if action == "status":
        direction = ""
        try:
            res = _request("mic_status", timeout=5.0)
            enabled = bool(res.get("with_mic"))
            direction = res.get("direction", "")
            src = res.get("source", "")
        except Exception:
            state = State.load()
            enabled = bool(state.connected_source)
            direction = state.mic_direction
            src = state.connected_source

        if _json_enabled(args):
            emit_json({"enabled": enabled, "direction": direction, "source": src})
            return 0
        if enabled:
            if direction == "send":
                ok(f"Microphone pass-through is active (Sending this device's mic to remote PC): {src}")
            elif direction == "receive":
                ok(f"Microphone pass-through is active (Receiving remote PC mic on this device): {src}")
            else:
                ok(f"Microphone pass-through is active: {src}")
        else:
            info("Microphone pass-through is inactive.")
        return 0

    if action in ("on", "enable", "send", "receive"):
        if action == "send":
            direction = "send"
        elif action == "receive":
            direction = "receive"
        else:
            direction = getattr(args, "direction", None) or "send"

        try:
            res = _request("enable_mic", direction=direction, timeout=10.0)
            if direction == "send":
                ok(f"Microphone pass-through enabled (Sending this device's mic to remote PC): {res.get('source')}")
            else:
                ok(f"Microphone pass-through enabled (Receiving remote PC mic on this device): {res.get('source')}")
        except Exception as exc:
            error(f"Could not enable microphone pass-through: {exc}")
            return 1
        return 0

    if action in ("off", "disable"):
        try:
            _request("disable_mic", timeout=10.0)
            ok("Microphone pass-through disabled.")
        except Exception as exc:
            error(f"Could not disable microphone pass-through: {exc}")
            return 1
        return 0

    error(f"Unknown mic action: {action}")
    return 1


def cmd_volume(args: argparse.Namespace) -> int:
    val_str = getattr(args, "value", None)
    mute_arg = getattr(args, "mute", None)

    volume_arg = None
    if val_str:
        clean_val = val_str.rstrip("%")
        if clean_val.isdigit():
            volume_arg = int(clean_val)
        elif val_str.lower() == "mute":
            mute_arg = True
        elif val_str.lower() in ("unmute", "un-mute"):
            mute_arg = False

    try:
        if volume_arg is not None or mute_arg is not None:
            res = _request("set_volume", volume=volume_arg, mute=mute_arg, timeout=5.0)
        else:
            res = _request("volume", timeout=5.0)
    except Exception:
        controller = Controller()
        if volume_arg is not None or mute_arg is not None:
            res = controller.set_volume(volume=volume_arg, mute=mute_arg)
        else:
            res = controller.get_volume()

    if _json_enabled(args):
        emit_json(res)
        return 0

    vol = res.get("volume", 100)
    mute_status = " [MUTED]" if res.get("mute") else ""
    conn_str = " (remote sync active)" if res.get("connected") else ""
    ok(f"Volume: {vol}%{mute_status}{conn_str}")
    return 0
