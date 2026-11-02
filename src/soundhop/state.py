"""Persistent runtime state (last connection, previous sink, known peers).

Configuration is for user choices; this file is for things SoundHop needs to
remember in order to restore your audio exactly where it was.
"""

from __future__ import annotations

import json
import logging
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

from .util import atomic_write, state_path

log = logging.getLogger(__name__)


@dataclass
class KnownPeer:
    id: str
    name: str
    host: str
    port: int
    last_seen: float = 0.0
    version: str = ""
    security: str = "trusted-lan"

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> "KnownPeer":
        return cls(
            id=str(raw.get("id", "")),
            name=str(raw.get("name", "")),
            host=str(raw.get("host", "")),
            port=int(raw.get("port", 0) or 0),
            last_seen=float(raw.get("last_seen", 0) or 0),
            version=str(raw.get("version", "")),
            security=str(raw.get("security", "trusted-lan")),
        )


@dataclass
class State:
    previous_default_sink: str = ""
    connected_peer: str = ""
    connected_name: str = ""
    connected_host: str = ""
    connected_port: int = 0
    connected_sink: str = ""
    connected_module: int = 0
    connected_since: float = 0.0
    connected_by: str = "manual"
    auto_paused: bool = False
    listener_module: int = 0
    listener_created: bool = False
    listener_port: int = 0
    listener_security: str = ""
    known_peers: dict[str, KnownPeer] = field(default_factory=dict)
    daemon_last_started: float = 0.0

    # -- persistence -----------------------------------------------------

    @classmethod
    def load(cls, path: Path | None = None) -> "State":
        path = path or state_path()
        if not path.exists():
            return cls()
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            log.warning("Ignoring unreadable state file %s: %s", path, exc)
            return cls()
        if not isinstance(raw, dict):
            return cls()
        state = cls()
        for key in (
            "previous_default_sink",
            "connected_peer",
            "connected_name",
            "connected_host",
            "connected_sink",
            "connected_by",
            "listener_security",
        ):
            setattr(state, key, str(raw.get(key, "") or ""))
        for key in ("connected_port", "connected_module", "listener_module", "listener_port"):
            try:
                setattr(state, key, int(raw.get(key, 0) or 0))
            except (TypeError, ValueError):
                pass
        for key in ("connected_since", "daemon_last_started"):
            try:
                setattr(state, key, float(raw.get(key, 0) or 0))
            except (TypeError, ValueError):
                pass
        state.auto_paused = bool(raw.get("auto_paused", False))
        state.listener_created = bool(raw.get("listener_created", False))
        peers = raw.get("known_peers", {})
        if isinstance(peers, dict):
            for peer_id, value in peers.items():
                if isinstance(value, dict):
                    value.setdefault("id", peer_id)
                    peer = KnownPeer.from_dict(value)
                    if peer.id:
                        state.known_peers[peer.id] = peer
        return state

    def save(self, path: Path | None = None) -> Path:
        path = path or state_path()
        self.prune_known_peers()
        payload = asdict(self)
        atomic_write(path, json.dumps(payload, indent=2, sort_keys=True) + "\n")
        return path

    # -- helpers ---------------------------------------------------------

    @property
    def connected(self) -> bool:
        return bool(self.connected_peer and self.connected_sink)

    def clear_connection(self) -> None:
        self.connected_peer = ""
        self.connected_name = ""
        self.connected_host = ""
        self.connected_port = 0
        self.connected_sink = ""
        self.connected_module = 0
        self.connected_since = 0.0
        self.connected_by = "manual"

    def remember_peer(self, peer: Any, *, when: float) -> None:
        """Store a discovery peer for offline/auto-connect use."""

        peer_id = getattr(peer, "id", "")
        if not peer_id:
            return
        self.known_peers[peer_id] = KnownPeer(
            id=peer_id,
            name=getattr(peer, "name", peer_id),
            host=getattr(peer, "host", ""),
            port=int(getattr(peer, "port", 0) or 0),
            last_seen=when,
            version=getattr(peer, "version", ""),
            security=getattr(peer, "security", "trusted-lan"),
        )

    def forget_peer(self, peer_id: str) -> None:
        self.known_peers.pop(peer_id, None)

    def prune_known_peers(self, *, max_age: float = 90 * 86400, limit: int = 50) -> None:
        if not self.known_peers:
            return
        cutoff = daemon_time() - max_age
        keep = {
            peer_id: peer
            for peer_id, peer in self.known_peers.items()
            if peer.last_seen >= cutoff
        }
        if len(keep) > limit:
            newest = sorted(keep.values(), key=lambda p: p.last_seen, reverse=True)[:limit]
            keep = {p.id: p for p in newest}
        self.known_peers = keep


def daemon_time() -> float:
    import time

    return time.time()
