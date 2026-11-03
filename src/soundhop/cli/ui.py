"""Terminal UI, ANSI styling, tables, and interactive prompts for SoundHop CLI."""

from __future__ import annotations

import argparse
import json
import sys
import time
from dataclasses import fields
from typing import Any, Iterable, Sequence

from ..constants import APP_NAME, VERSION
from ..discovery import Peer
from ..util import format_age, format_duration, style


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

    android = data.get("android") or {}
    if android.get("active"):
        dev_name = android.get("device_name") or android.get("serial", "Android")
        codec_name = (android.get("codec") or "").upper()
        uptime = format_duration(android.get("uptime_s"))
        print(
            f"  {'Android':<12} {style('streaming from ' + dev_name, 'green')} "
            f"({codec_name}, pid {android.get('pid')}, {uptime})"
        )

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
