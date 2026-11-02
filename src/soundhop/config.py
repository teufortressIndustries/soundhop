"""TOML configuration with sensible defaults and a tiny writer.

The file lives at ``$XDG_CONFIG_HOME/soundhop/config.toml``.  It is written by
hand so the core has no third-party dependency; reading uses :mod:`tomllib`
(Python 3.11+) or :mod:`tomli` on older interpreters.
"""

from __future__ import annotations

import copy
import ipaddress
import shlex
from dataclasses import dataclass, field, fields
from pathlib import Path
from typing import Any

try:  # Python 3.11+
    import tomllib as _toml
except ModuleNotFoundError:  # pragma: no cover - exercised on Python 3.10
    import tomli as _toml  # type: ignore[no-redef]

from .constants import (
    DEFAULT_AUDIO_PORT,
    DEFAULT_DISCOVERY_PORT,
    DEFAULT_LAN_NETWORKS,
)
from .util import (
    atomic_write,
    config_path,
    hostname_name,
    new_device_id,
)

SECURITY_MODES = ("trusted-lan", "cookie", "open")

_SECURITY_ALIASES = {
    "lan": "trusted-lan",
    "trusted": "trusted-lan",
    "private": "trusted-lan",
    "anonymous": "open",
    "none": "open",
}


class ConfigError(ValueError):
    pass


@dataclass
class DeviceConfig:
    name: str = field(default_factory=hostname_name)
    id: str = field(default_factory=new_device_id)


@dataclass
class SharingConfig:
    enabled: bool = True
    port: int = DEFAULT_AUDIO_PORT
    security: str = "trusted-lan"
    allowed_networks: list[str] = field(
        default_factory=lambda: list(DEFAULT_LAN_NETWORKS)
    )


@dataclass
class ConnectionConfig:
    auto_connect: bool = True
    switch_default: bool = True
    restore_default: bool = True
    move_streams: bool = True
    disconnect_on_quit: bool = True
    last_peer: str = ""
    reconnect_delay_ms: int = 3000
    latency_ms: int | None = None
    extra_tunnel_args: list[str] = field(default_factory=list)
    disconnect_grace_s: float = 12.0


@dataclass
class DiscoveryConfig:
    udp: bool = True
    mdns: bool = True
    port: int = DEFAULT_DISCOVERY_PORT
    interval_s: float = 3.0
    timeout_s: float = 20.0


@dataclass
class UiConfig:
    notifications: bool = True
    start_tray: bool = False


@dataclass
class Config:
    path: Path
    device: DeviceConfig = field(default_factory=DeviceConfig)
    sharing: SharingConfig = field(default_factory=SharingConfig)
    connection: ConnectionConfig = field(default_factory=ConnectionConfig)
    discovery: DiscoveryConfig = field(default_factory=DiscoveryConfig)
    ui: UiConfig = field(default_factory=UiConfig)

    # -- loading ---------------------------------------------------------

    @classmethod
    def load(cls, path: Path | None = None) -> "Config":
        path = path or config_path()
        if not path.exists():
            cfg = cls(path=path)
            cfg.device.name = hostname_name()
            try:
                cfg.save()
            except OSError:
                pass
            return cfg
        try:
            raw = _toml.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise ConfigError(f"Cannot read {path}: {exc}") from exc
        if not isinstance(raw, dict):
            raise ConfigError(f"{path} does not contain a TOML table")
        cfg = cls.from_dict(raw, path=path)
        return cfg

    @classmethod
    def from_dict(cls, raw: dict[str, Any], *, path: Path | None = None) -> "Config":
        def section(name: str) -> dict[str, Any]:
            value = raw.get(name, {})
            return value if isinstance(value, dict) else {}

        cfg = cls(path=path or config_path())
        _apply_dataclass(cfg.device, section("device"), "device")
        _apply_dataclass(cfg.sharing, section("sharing"), "sharing")
        _apply_dataclass(cfg.connection, section("connection"), "connection")
        _apply_dataclass(cfg.discovery, section("discovery"), "discovery")
        _apply_dataclass(cfg.ui, section("ui"), "ui")
        cfg.normalize()
        return cfg

    def normalize(self) -> None:
        self.device.name = (self.device.name or hostname_name()).strip() or hostname_name()
        self.device.id = (
            self.device.id.strip() if isinstance(self.device.id, str) else ""
        ) or new_device_id()

        self.sharing.enabled = _as_bool(self.sharing.enabled, True)
        self.sharing.port = _port(self.sharing.port, DEFAULT_AUDIO_PORT)
        self.sharing.security = _security(self.sharing.security)
        raw_networks = self.sharing.allowed_networks
        if isinstance(raw_networks, str):
            raw_networks = [raw_networks]
        if not isinstance(raw_networks, list):
            raise ConfigError("sharing.allowed_networks must be a list")
        networks: list[str] = []
        for value in raw_networks or DEFAULT_LAN_NETWORKS:
            value = str(value).strip()
            if not value:
                continue
            try:
                ipaddress.ip_network(value, strict=False)
            except ValueError as exc:
                raise ConfigError(f"Invalid network in sharing.allowed_networks: {value}") from exc
            if value not in networks:
                networks.append(value)
        self.sharing.allowed_networks = networks or list(DEFAULT_LAN_NETWORKS)

        self.discovery.udp = _as_bool(self.discovery.udp, True)
        self.discovery.mdns = _as_bool(self.discovery.mdns, True)
        self.discovery.port = _port(self.discovery.port, DEFAULT_DISCOVERY_PORT)
        self.discovery.interval_s = max(1.0, float(self.discovery.interval_s))
        self.discovery.timeout_s = max(5.0, float(self.discovery.timeout_s))

        self.connection.auto_connect = _as_bool(self.connection.auto_connect, True)
        self.connection.switch_default = _as_bool(
            self.connection.switch_default, True
        )
        self.connection.restore_default = _as_bool(
            self.connection.restore_default, True
        )
        self.connection.move_streams = _as_bool(self.connection.move_streams, True)
        self.connection.disconnect_on_quit = _as_bool(
            self.connection.disconnect_on_quit, True
        )
        self.connection.reconnect_delay_ms = max(
            0, int(self.connection.reconnect_delay_ms)
        )
        if self.connection.latency_ms is not None:
            latency = int(self.connection.latency_ms)
            self.connection.latency_ms = latency if latency > 0 else None
        if not isinstance(self.connection.extra_tunnel_args, list):
            raise ConfigError("connection.extra_tunnel_args must be a list")
        self.connection.extra_tunnel_args = [
            str(x) for x in self.connection.extra_tunnel_args
        ]
        self.connection.disconnect_grace_s = max(
            3.0, float(self.connection.disconnect_grace_s)
        )

        self.ui.notifications = _as_bool(self.ui.notifications, True)
        self.ui.start_tray = _as_bool(self.ui.start_tray, False)

    # -- saving ----------------------------------------------------------

    def save(self, path: Path | None = None) -> Path:
        path = path or self.path
        atomic_write(path, self.to_toml())
        self.path = path
        return path

    def to_toml(self) -> str:
        s = self
        lines = [
            "# SoundHop configuration",
            "# See 'soundhop --help' or the README for descriptions.",
            "",
            "[device]",
            f"name = {_q(s.device.name)}",
            f"id = {_q(s.device.id)}",
            "",
            "[sharing]",
            f"enabled = {_bool(s.sharing.enabled)}  # expose this device's speakers to the LAN",
            f"port = {s.sharing.port}",
            f"security = {_q(s.sharing.security)}  # trusted-lan | cookie | open",
            f"allowed_networks = {_list(s.sharing.allowed_networks)}",
            "",
            "[connection]",
            f"auto_connect = {_bool(s.connection.auto_connect)}              # reconnect to the last used device",
            f"switch_default = {_bool(s.connection.switch_default)}            # make the remote device the default output",
            f"restore_default = {_bool(s.connection.restore_default)}           # restore local output on disconnect",
            f"move_streams = {_bool(s.connection.move_streams)}              # move currently playing audio too",
            f"disconnect_on_quit = {_bool(s.connection.disconnect_on_quit)}      # disconnect remote audio when closing tray",
            f"last_peer = {_q(s.connection.last_peer)}",
            f"reconnect_delay_ms = {s.connection.reconnect_delay_ms}",
            f"latency_ms = {_num(s.connection.latency_ms)}",
            f"extra_tunnel_args = {_list(s.connection.extra_tunnel_args)}",
            f"disconnect_grace_s = {_num(s.connection.disconnect_grace_s)}",
            "",
            "[discovery]",
            f"udp = {_bool(s.discovery.udp)}                       # zero-dependency LAN broadcast",
            f"mdns = {_bool(s.discovery.mdns)}                      # use python-zeroconf when installed",
            f"port = {s.discovery.port}",
            f"interval_s = {_num(s.discovery.interval_s)}",
            f"timeout_s = {_num(s.discovery.timeout_s)}",
            "",
            "[ui]",
            f"notifications = {_bool(s.ui.notifications)}",
            f"start_tray = {_bool(s.ui.start_tray)}",
            "",
        ]
        return "\n".join(lines)

    # -- introspection ---------------------------------------------------

    def to_dict(self) -> dict[str, Any]:
        return {
            "device": _asdict(self.device),
            "sharing": _asdict(self.sharing),
            "connection": _asdict(self.connection),
            "discovery": _asdict(self.discovery),
            "ui": _asdict(self.ui),
        }

    def get(self, dotted: str) -> Any:
        current: Any = self
        for part in dotted.split("."):
            if not hasattr(current, part):
                raise ConfigError(f"Unknown configuration key: {dotted}")
            current = getattr(current, part)
        return current

    def set(self, dotted: str, value: Any) -> None:
        parts = dotted.split(".")
        if len(parts) != 2:
            raise ConfigError("Use section.key, for example connection.auto_connect")
        section_name, key = parts
        section = getattr(self, section_name, None)
        if section is None or not hasattr(section, key):
            raise ConfigError(f"Unknown configuration key: {dotted}")
        if key == "id":
            raise ConfigError("device.id is managed automatically")
        setattr(section, key, value)
        self.normalize()

    def apply_patch(self, patch: dict[str, Any]) -> None:
        """Apply a nested patch from the tray settings dialog or IPC."""

        for section_name, values in patch.items():
            section = getattr(self, section_name, None)
            if section is None or not isinstance(values, dict):
                continue
            for key, value in values.items():
                if key == "id" or not hasattr(section, key):
                    continue
                setattr(section, key, value)
        self.normalize()

    def copy(self) -> "Config":
        return copy.deepcopy(self)


def _apply_dataclass(target: Any, raw: dict[str, Any], section: str) -> None:
    valid = {f.name for f in fields(target)}
    for key, value in raw.items():
        if key in valid:
            setattr(target, key, value)


def _port(value: Any, default: int) -> int:
    try:
        port = int(value)
    except (TypeError, ValueError):
        return default
    return port if 1 <= port <= 65535 else default


def _security(value: Any) -> str:
    if not isinstance(value, str):
        return "trusted-lan"
    value = _SECURITY_ALIASES.get(value.strip().lower(), value.strip().lower())
    if value not in SECURITY_MODES:
        raise ConfigError(
            f"sharing.security must be one of {', '.join(SECURITY_MODES)}"
        )
    return value


def _asdict(obj: Any) -> dict[str, Any]:
    return {f.name: getattr(obj, f.name) for f in fields(obj)}


def _q(value: str) -> str:
    return '"' + str(value).replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n") + '"'


def _bool(value: bool) -> str:
    return "true" if value else "false"


def _as_bool(value: Any, default: bool) -> bool:
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        lowered = value.strip().lower()
        if lowered in {"true", "yes", "on", "1"}:
            return True
        if lowered in {"false", "no", "off", "0", ""}:
            return False
    if isinstance(value, (int, float)):
        return bool(value)
    return default


def _num(value: float | int | None) -> str:
    if value is None:
        return "0"  # latency_ms = 0 means "use the backend default"
    if isinstance(value, int):
        return str(value)
    return f"{value:g}"


def _list(values: list[str]) -> str:
    return "[" + ", ".join(_q(v) for v in values) + "]"


def parse_config_value(raw: str) -> Any:
    """Parse a value supplied to ``soundhop config set``."""

    text = raw.strip()
    lowered = text.lower()
    if lowered in {"true", "yes", "on"}:
        return True
    if lowered in {"false", "no", "off"}:
        return False
    if lowered in {"none", "null", ""}:
        return None
    if text.startswith("[") and text.endswith("]"):
        try:
            parsed = _toml.loads(f"value = {text}")["value"]
            if isinstance(parsed, list):
                return [str(x) for x in parsed]
        except ValueError:
            pass
        return [part.strip().strip("\"'") for part in text[1:-1].split(",") if part.strip()]
    try:
        return int(text)
    except ValueError:
        pass
    try:
        return float(text)
    except ValueError:
        pass
    try:
        return shlex.split(text)[0] if text.startswith(("\"", "'")) else text
    except ValueError:
        return text


def default_config_text() -> str:
    """Return the commented default configuration (useful for docs/tests)."""

    return Config(path=config_path()).to_toml()
