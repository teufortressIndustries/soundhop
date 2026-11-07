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
    def from_dict(cls, raw: dict[str, Any]) -> KnownPeer:
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
class KnownAndroidDevice:
    serial: str
    model: str = ""
    manufacturer: str = ""
    last_ip: str = ""
    last_port: int = 5555
    last_seen: float = 0.0
    last_connected: float = 0.0
    is_wifi: bool = False
    auto_connect: bool = False

    @property
    def display_name(self) -> str:
        parts: list[str] = []
        if self.manufacturer:
            parts.append(self.manufacturer)
        if self.model and self.model != self.manufacturer:
            parts.append(self.model)
        if not parts:
            parts.append(self.serial)
        return " ".join(parts)

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> KnownAndroidDevice:
        return cls(
            serial=str(raw.get("serial", "")).strip(),
            model=str(raw.get("model", "")).strip(),
            manufacturer=str(raw.get("manufacturer", "")).strip(),
            last_ip=str(raw.get("last_ip", "")).strip(),
            last_port=int(raw.get("last_port", 5555) or 5555),
            last_seen=float(raw.get("last_seen", 0.0) or 0.0),
            last_connected=float(raw.get("last_connected", 0.0) or 0.0),
            is_wifi=bool(raw.get("is_wifi", False)),
            auto_connect=bool(raw.get("auto_connect", False)),
        )


@dataclass
class State:
    previous_default_sink: str = ""
    previous_default_source: str = ""
    connected_peer: str = ""
    connected_name: str = ""
    connected_host: str = ""
    connected_port: int = 0
    connected_sink: str = ""
    connected_module: int = 0
    connected_source: str = ""
    connected_source_module: int = 0
    mic_direction: str = ""
    remote_mic_null_module: int = 0
    remote_mic_remap_module: int = 0
    local_mic_loopback_module: int = 0
    connected_since: float = 0.0
    connected_by: str = "manual"
    auto_paused: bool = False
    listener_module: int = 0
    listener_created: bool = False
    listener_port: int = 0
    listener_security: str = ""
    routed_apps: dict[str, str] = field(default_factory=dict)
    last_synced_volume: int = -1
    last_synced_mute: bool = False
    known_peers: dict[str, KnownPeer] = field(default_factory=dict)
    known_android_devices: dict[str, KnownAndroidDevice] = field(default_factory=dict)
    daemon_last_started: float = 0.0
    android_active: bool = False
    android_as_mic: bool = False
    android_serial: str = ""
    android_model: str = ""
    android_codec: str = ""
    android_source: str = ""
    android_pid: int = 0
    android_since: float = 0.0

    # -- persistence -----------------------------------------------------

    @classmethod
    def load(cls, path: Path | None = None) -> State:
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
            "previous_default_source",
            "connected_peer",
            "connected_name",
            "connected_host",
            "connected_sink",
            "connected_source",
            "mic_direction",
            "connected_by",
            "listener_security",
            "android_serial",
            "android_model",
            "android_codec",
            "android_source",
        ):
            setattr(state, key, str(raw.get(key, "") or ""))
        for key in (
            "connected_port",
            "connected_module",
            "connected_source_module",
            "remote_mic_null_module",
            "remote_mic_remap_module",
            "local_mic_loopback_module",
            "listener_module",
            "listener_port",
            "android_pid",
            "last_synced_volume",
        ):
            try:
                setattr(state, key, int(raw.get(key, 0) or 0))
            except (TypeError, ValueError):
                pass
        for key in ("connected_since", "daemon_last_started", "android_since"):
            try:
                setattr(state, key, float(raw.get(key, 0) or 0))
            except (TypeError, ValueError):
                pass
        state.auto_paused = bool(raw.get("auto_paused", False))
        state.listener_created = bool(raw.get("listener_created", False))
        state.android_active = bool(raw.get("android_active", False))
        state.android_as_mic = bool(raw.get("android_as_mic", False))
        state.last_synced_mute = bool(raw.get("last_synced_mute", False))
        routed = raw.get("routed_apps", {})
        if isinstance(routed, dict):
            state.routed_apps = {str(k): str(v) for k, v in routed.items()}
        peers = raw.get("known_peers", {})
        if isinstance(peers, dict):
            for peer_id, value in peers.items():
                if isinstance(value, dict):
                    value.setdefault("id", peer_id)
                    peer = KnownPeer.from_dict(value)
                    if peer.id:
                        state.known_peers[peer.id] = peer
        android_devs = raw.get("known_android_devices", {})
        if isinstance(android_devs, dict):
            for serial, value in android_devs.items():
                if isinstance(value, dict):
                    value.setdefault("serial", serial)
                    dev = KnownAndroidDevice.from_dict(value)
                    if dev.serial:
                        state.known_android_devices[dev.serial] = dev
        return state

    def save(self, path: Path | None = None) -> Path:
        path = path or state_path()
        self.prune_known_peers()
        self.prune_known_android_devices()
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
        self.connected_source = ""
        self.connected_source_module = 0
        self.mic_direction = ""
        self.remote_mic_null_module = 0
        self.remote_mic_remap_module = 0
        self.local_mic_loopback_module = 0
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
            peer_id: peer for peer_id, peer in self.known_peers.items() if peer.last_seen >= cutoff
        }
        if len(keep) > limit:
            newest = sorted(keep.values(), key=lambda p: p.last_seen, reverse=True)[:limit]
            keep = {p.id: p for p in newest}
        self.known_peers = keep

    def remember_android_device(
        self,
        serial: str,
        *,
        model: str = "",
        manufacturer: str = "",
        ip: str = "",
        port: int = 5555,
        is_wifi: bool = False,
        connected: bool = False,
        when: float | None = None,
    ) -> KnownAndroidDevice:
        now = when if when is not None else daemon_time()
        existing = self.known_android_devices.get(serial)
        if existing:
            if model:
                existing.model = model
            if manufacturer:
                existing.manufacturer = manufacturer
            if ip:
                existing.last_ip = ip
            if port:
                existing.last_port = port
            if is_wifi:
                existing.is_wifi = is_wifi
            existing.last_seen = now
            if connected:
                existing.last_connected = now
            return existing

        dev = KnownAndroidDevice(
            serial=serial,
            model=model,
            manufacturer=manufacturer,
            last_ip=ip,
            last_port=port,
            last_seen=now,
            last_connected=now if connected else 0.0,
            is_wifi=is_wifi,
        )
        self.known_android_devices[serial] = dev
        return dev

    def forget_android_device(self, serial: str) -> None:
        self.known_android_devices.pop(serial, None)

    def prune_known_android_devices(
        self, *, max_age: float = 90 * 86400, limit: int = 50
    ) -> None:
        if not self.known_android_devices:
            return
        cutoff = daemon_time() - max_age
        keep = {
            s: d
            for s, d in self.known_android_devices.items()
            if max(d.last_seen, d.last_connected) >= cutoff
        }
        if len(keep) > limit:
            newest = sorted(
                keep.values(),
                key=lambda d: max(d.last_seen, d.last_connected),
                reverse=True,
            )[:limit]
            keep = {d.serial: d for d in newest}
        self.known_android_devices = keep


def daemon_time() -> float:
    import time

    return time.time()
