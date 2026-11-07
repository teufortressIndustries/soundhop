"""PipeWire / PulseAudio backend built on the stable ``pactl`` CLI.

PipeWire's PulseAudio compatibility layer implements the two modules SoundHop
needs (``module-native-protocol-tcp`` and ``module-tunnel-sink``) and ``pactl``
is present on every desktop distro that ships either server, so this is the
smallest common denominator that still gives us real, system-wide audio.
"""

from __future__ import annotations

import logging
import re
import sys
import time
from collections.abc import Callable, Sequence
from dataclasses import asdict, dataclass
from typing import Any

from .constants import (
    DEFAULT_LAN_NETWORKS,
    SINK_PREFIX,
    SINK_TIMEOUT,
    SOURCE_PREFIX,
    SOURCE_TIMEOUT,
)
from .util import (
    Result,
    ToolMissing,
    command_exists,
    run,
    sanitize_sink_name,
    tcp_server_arg,
)

log = logging.getLogger(__name__)


class BackendError(RuntimeError):
    pass


@dataclass
class Module:
    id: int
    name: str
    args: str = ""

    def arg(self, key: str, default: str = "") -> str:
        match = re.search(rf"(?:^|\s){re.escape(key)}=([^\s]+)", self.args)
        return match.group(1) if match else default


@dataclass
class Sink:
    id: int
    name: str
    driver: str = ""
    spec: str = ""
    state: str = ""

    @property
    def description(self) -> str:
        return self.name


@dataclass
class Source:
    id: int
    name: str
    driver: str = ""
    spec: str = ""
    state: str = ""

    @property
    def description(self) -> str:
        return self.name


@dataclass
class SinkInput:
    id: int
    sink: int
    client: int = 0
    driver: str = ""


@dataclass
class SinkInputInfo:
    id: int
    sink: int
    sink_name: str = ""
    app_name: str = ""
    binary: str = ""
    node_name: str = ""
    pid: int = 0
    media_name: str = ""
    icon_name: str = ""
    volume_percent: int = 100
    mute: bool = False
    is_remote: bool = False

    @property
    def display_name(self) -> str:
        return self.app_name or self.binary or self.node_name or self.media_name or f"Stream #{self.id}"

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass
class SourceOutput:
    id: int
    source: int
    client: int = 0
    driver: str = ""


@dataclass
class Listener:
    module_id: int
    created: bool
    args: str
    port: int
    security: str


class AudioBackend:
    def __new__(cls, *args: Any, **kwargs: Any) -> Any:
        if (
            cls is AudioBackend
            and sys.platform == "win32"
            and "runner" not in kwargs
            and "pactl" not in kwargs
        ):
            from .platform.windows.backend import WindowsAudioBackend

            return super().__new__(WindowsAudioBackend)
        return super().__new__(cls)

    def __init__(
        self,
        *,
        pactl: str = "pactl",
        runner: Callable[..., Result] = run,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.pactl = pactl
        self._run = runner
        self._sleep = sleep
        self._info_cache: dict[str, str] | None = None

    # -- low-level -------------------------------------------------------

    def available(self) -> bool:
        if self._run is run and not command_exists(self.pactl):
            return False
        try:
            return self._pactl("info", check=False).ok
        except (ToolMissing, OSError):
            return False

    def _pactl(self, *args: str, check: bool = True, timeout: float = 15.0) -> Result:
        result = self._run([self.pactl, *args], timeout=timeout)
        if check and not result.ok:
            raise BackendError(
                (result.stderr or result.stdout).strip() or f"pactl {' '.join(args)} failed"
            )
        return result

    # -- server info -----------------------------------------------------

    def info(self, *, refresh: bool = False) -> dict[str, str]:
        if self._info_cache is not None and not refresh:
            return self._info_cache
        result = self._pactl("info")
        data: dict[str, str] = {}
        for line in result.stdout.splitlines():
            if ": " in line:
                key, value = line.split(": ", 1)
                data[key.strip()] = value.strip()
        self._info_cache = data
        return data

    def invalidate(self) -> None:
        self._info_cache = None

    def server_name(self) -> str:
        return self.info().get("Server Name", "PulseAudio")

    def server_version(self) -> str:
        return self.info().get("Server Version", "")

    def server_label(self) -> str:
        name = self.server_name()
        version = self.server_version()
        return f"{name} {version}".strip()

    def is_pipewire(self) -> bool:
        return (
            "pipewire" in self.server_name().lower() or "pipewire" in self.server_version().lower()
        )

    def default_sink(self) -> str:
        return self.info().get("Default Sink", "")

    def default_source(self) -> str:
        return self.info().get("Default Source", "")

    # -- graph listing ---------------------------------------------------

    def modules(self) -> list[Module]:
        result = self._pactl("list", "short", "modules")
        modules: list[Module] = []
        for line in result.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) < 2 or not parts[0].isdigit():
                continue
            modules.append(
                Module(
                    id=int(parts[0]),
                    name=parts[1].strip(),
                    args=parts[2].strip() if len(parts) > 2 else "",
                )
            )
        return modules

    def module(self, module_id: int) -> Module | None:
        for module in self.modules():
            if module.id == module_id:
                return module
        return None

    def find_modules(
        self,
        name: str | None = None,
        *,
        sink_name: str | None = None,
        contains: str | None = None,
    ) -> list[Module]:
        found: list[Module] = []
        for module in self.modules():
            if name is not None and module.name != name:
                continue
            if sink_name is not None and module.arg("sink_name") != sink_name:
                continue
            if contains is not None and contains not in module.args:
                continue
            found.append(module)
        return found

    def sinks(self) -> list[Sink]:
        result = self._pactl("list", "short", "sinks")
        sinks: list[Sink] = []
        for line in result.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) < 2 or not parts[0].isdigit():
                continue
            sinks.append(
                Sink(
                    id=int(parts[0]),
                    name=parts[1].strip(),
                    driver=parts[2].strip() if len(parts) > 2 else "",
                    spec=parts[3].strip() if len(parts) > 3 else "",
                    state=parts[4].strip() if len(parts) > 4 else "",
                )
            )
        return sinks

    def sink(self, name: str) -> Sink | None:
        for sink in self.sinks():
            if sink.name == name:
                return sink
        return None

    def sink_exists(self, name: str) -> bool:
        return self.sink(name) is not None

    def module_for_sink(self, sink_name: str) -> Module | None:
        for module in self.modules():
            if module.name.startswith("module-tunnel-") and module.arg("sink_name") == sink_name:
                return module
        return None

    def sources(self) -> list[Source]:
        result = self._pactl("list", "short", "sources")
        srcs: list[Source] = []
        for line in result.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) < 2 or not parts[0].isdigit():
                continue
            srcs.append(
                Source(
                    id=int(parts[0]),
                    name=parts[1].strip(),
                    driver=parts[2].strip() if len(parts) > 2 else "",
                    spec=parts[3].strip() if len(parts) > 3 else "",
                    state=parts[4].strip() if len(parts) > 4 else "",
                )
            )
        return srcs

    def source(self, name: str) -> Source | None:
        for s in self.sources():
            if s.name == name:
                return s
        return None

    def source_exists(self, name: str) -> bool:
        return self.source(name) is not None

    def module_for_source(self, source_name: str) -> Module | None:
        for module in self.modules():
            if module.name.startswith("module-tunnel-") and module.arg("source_name") == source_name:
                return module
        return None

    def sink_inputs(self) -> list[SinkInput]:
        result = self._pactl("list", "short", "sink-inputs")
        inputs: list[SinkInput] = []
        for line in result.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) < 2 or not parts[0].isdigit():
                continue
            try:
                sink_index = int(parts[1])
            except ValueError:
                continue
            inputs.append(
                SinkInput(
                    id=int(parts[0]),
                    sink=sink_index,
                    client=int(parts[2]) if len(parts) > 2 and parts[2].isdigit() else 0,
                    driver=parts[3].strip() if len(parts) > 3 else "",
                )
            )
        return inputs

    def detailed_sink_inputs(self) -> list[SinkInputInfo]:
        """Return all active sink inputs with rich metadata (app name, binary, PID, volume)."""
        sinks_by_index = {s.id: s.name for s in self.sinks()}
        remote_sink_names = {
            m.arg("sink_name") for m in self.soundhop_modules() if m.arg("sink_name")
        }

        # Try JSON parsing first (PipeWire and PulseAudio 16+)
        try:
            res = self._pactl("-f", "json", "list", "sink-inputs", check=False)
            if res.ok and res.stdout.strip():
                import json

                raw_list = json.loads(res.stdout)
                if isinstance(raw_list, list):
                    items: list[SinkInputInfo] = []
                    for raw in raw_list:
                        props = raw.get("properties", {}) or {}
                        idx = int(raw.get("index", 0) or 0)
                        sink_idx = int(raw.get("sink", 0) or 0)
                        sink_name = sinks_by_index.get(sink_idx, "")
                        mute = bool(raw.get("mute", False))
                        vol_dict = raw.get("volume", {}) or {}
                        vol_pct = 100
                        if isinstance(vol_dict, dict):
                            for v in vol_dict.values():
                                if isinstance(v, dict) and "value_percent" in v:
                                    m = re.search(r"(\d+)%", str(v["value_percent"]))
                                    if m:
                                        vol_pct = int(m.group(1))
                                        break
                        pid_val = 0
                        raw_pid = props.get("application.process.id", "")
                        if str(raw_pid).isdigit():
                            pid_val = int(raw_pid)

                        node_name = str(props.get("node.name", "")).strip()
                        app_name = str(props.get("application.name", "")).strip()
                        binary = str(props.get("application.process.binary", "")).strip()
                        if not app_name and node_name:
                            app_name = node_name
                        if not binary and node_name:
                            binary = node_name

                        items.append(
                            SinkInputInfo(
                                id=idx,
                                sink=sink_idx,
                                sink_name=sink_name,
                                app_name=app_name,
                                binary=binary,
                                node_name=node_name,
                                pid=pid_val,
                                media_name=str(props.get("media.name", "")).strip(),
                                icon_name=str(props.get("application.icon_name", "")).strip(),
                                volume_percent=vol_pct,
                                mute=mute,
                                is_remote=sink_name in remote_sink_names,
                            )
                        )
                    return items
        except Exception as exc:
            log.debug("JSON sink-inputs failed, falling back to text: %s", exc)

        return self._parse_text_sink_inputs(sinks_by_index, remote_sink_names)

    def _parse_text_sink_inputs(
        self, sinks_by_index: dict[int, str], remote_sink_names: set[str]
    ) -> list[SinkInputInfo]:
        try:
            res = self._pactl("list", "sink-inputs", check=False)
            if not res.ok:
                return []
        except Exception:
            return []

        blocks = re.split(r"\n(?=Sink Input #\d+)", res.stdout)
        results: list[SinkInputInfo] = []
        for block in blocks:
            header = re.match(r"Sink Input #(\d+)", block.strip())
            if not header:
                continue
            inp_id = int(header.group(1))
            sink_match = re.search(r"Sink:\s*(\d+)", block)
            sink_idx = int(sink_match.group(1)) if sink_match else 0
            sink_name = sinks_by_index.get(sink_idx, "")
            mute_match = re.search(r"Mute:\s*(yes|no)", block, re.I)
            mute = mute_match.group(1).lower() == "yes" if mute_match else False

            vol_match = re.search(r"Volume:.*?(\d+)%", block)
            vol_pct = int(vol_match.group(1)) if vol_match else 100

            app_match = re.search(r'application\.name\s*=\s*"([^"]+)"', block)
            app_name = app_match.group(1).strip() if app_match else ""

            bin_match = re.search(r'application\.process\.binary\s*=\s*"([^"]+)"', block)
            binary = bin_match.group(1).strip() if bin_match else ""

            node_match = re.search(r'node\.name\s*=\s*"([^"]+)"', block)
            node_name = node_match.group(1).strip() if node_match else ""
            if not app_name and node_name:
                app_name = node_name
            if not binary and node_name:
                binary = node_name

            pid_match = re.search(r'application\.process\.id\s*=\s*"(\d+)"', block)
            pid = int(pid_match.group(1)) if pid_match else 0

            media_match = re.search(r'media\.name\s*=\s*"([^"]+)"', block)
            media_name = media_match.group(1).strip() if media_match else ""

            icon_match = re.search(r'application\.icon_name\s*=\s*"([^"]+)"', block)
            icon_name = icon_match.group(1).strip() if icon_match else ""

            results.append(
                SinkInputInfo(
                    id=inp_id,
                    sink=sink_idx,
                    sink_name=sink_name,
                    app_name=app_name,
                    binary=binary,
                    node_name=node_name,
                    pid=pid,
                    media_name=media_name,
                    icon_name=icon_name,
                    volume_percent=vol_pct,
                    mute=mute,
                    is_remote=sink_name in remote_sink_names,
                )
            )
        return results

    # -- module operations ----------------------------------------------

    def load_module(self, name: str, args: Sequence[str] | str = ()) -> int:
        if isinstance(args, str):
            parts = args.split()
        else:
            parts = [str(x) for x in args]
        result = self._pactl("load-module", name, *parts)
        text = result.stdout.strip()
        match = re.search(r"\b(\d+)\b", text)
        if not match:
            raise BackendError(f"pactl did not return a module id (output: {text!r})")
        module_id = int(match.group(1))
        log.debug("Loaded %s as module %s", name, module_id)
        return module_id

    def unload_module(self, module_id: int, *, ignore_missing: bool = True) -> bool:
        result = self._pactl("unload-module", str(module_id), check=False)
        if not result.ok:
            message = (result.stderr or result.stdout).strip()
            if ignore_missing or "No such entity" in message or "not loaded" in message:
                log.debug("Module %s was already gone: %s", module_id, message)
                return False
            raise BackendError(message or f"Could not unload module {module_id}")
        log.debug("Unloaded module %s", module_id)
        return True

    def unload_tunnels(self, *, sink_prefix: str = SINK_PREFIX) -> list[int]:
        removed: list[int] = []
        for module in self.modules():
            if not module.name.startswith("module-tunnel-"):
                continue
            if module.arg("sink_name").startswith(sink_prefix):
                if self.unload_module(module.id):
                    removed.append(module.id)
        return removed

    # -- default routing -------------------------------------------------

    def set_default_sink(self, name: str) -> None:
        if not name:
            raise BackendError("Refusing to set an empty default sink")
        self._pactl("set-default-sink", name)
        self.invalidate()

    def move_sink_inputs(self, target: str, *, source: str | None = None) -> int:
        """Move playback streams to ``target``.

        If ``source`` is given only streams currently on that sink are moved.
        Returns the number of streams moved.
        """

        wanted_sink_id: int | None = None
        if source:
            sink = self.sink(source)
            if sink is None:
                log.debug("Source sink %s is gone; not moving any streams", source)
                return 0
            wanted_sink_id = sink.id
        moved = 0
        for item in self.sink_inputs():
            if wanted_sink_id is not None and item.sink != wanted_sink_id:
                continue
            result = self._pactl("move-sink-input", str(item.id), target, check=False)
            if result.ok:
                moved += 1
            else:
                log.debug(
                    "Could not move sink-input %s: %s",
                    item.id,
                    (result.stderr or result.stdout).strip(),
                )
        return moved

    def set_default_source(self, name: str) -> None:
        if not name:
            raise BackendError("Refusing to set an empty default source")
        self._pactl("set-default-source", name)
        self.invalidate()

    def source_outputs(self) -> list[SourceOutput]:
        result = self._pactl("list", "short", "source-outputs", check=False)
        if not result.ok:
            return []
        items: list[SourceOutput] = []
        for line in result.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) < 2 or not parts[0].isdigit():
                continue
            try:
                items.append(
                    SourceOutput(
                        id=int(parts[0]),
                        source=int(parts[1]) if parts[1].isdigit() else 0,
                        client=int(parts[2]) if len(parts) > 2 and parts[2].isdigit() else 0,
                        driver=parts[3].strip() if len(parts) > 3 else "",
                    )
                )
            except ValueError:
                continue
        return items

    def move_source_outputs(
        self,
        target: str,
        *,
        source: str | None = None,
        exclude_modules: Sequence[int] = (),
        skip_monitors: bool = True,
    ) -> int:
        wanted_source_id: int | None = None
        if source:
            src = self.source(source)
            if src is None:
                return 0
            wanted_source_id = src.id
        exclude_set = {int(x) for x in exclude_modules if str(x).isdigit() and int(x) > 0}
        moved = 0
        detailed = self._pactl("list", "source-outputs", check=False)
        if detailed.ok and detailed.stdout:
            current_id: int | None = None
            current_owner: int | None = None
            current_source: int | None = None
            is_monitor = False
            for line in detailed.stdout.splitlines():
                m_so = re.match(r"^Source Output #(\d+)", line)
                if m_so:
                    if current_id is not None:
                        should_skip = (
                            (current_owner in exclude_set)
                            or (skip_monitors and is_monitor)
                            or (wanted_source_id is not None and current_source != wanted_source_id)
                        )
                        if not should_skip:
                            res = self._pactl("move-source-output", str(current_id), target, check=False)
                            if res.ok:
                                moved += 1
                    current_id = int(m_so.group(1))
                    current_owner = None
                    current_source = None
                    is_monitor = False
                    continue
                m_owner = re.search(r"Owner Module:\s*(\d+)", line)
                if m_owner:
                    current_owner = int(m_owner.group(1))
                m_src = re.search(r"Source:\s*(\d+)", line)
                if m_src:
                    current_source = int(m_src.group(1))
                if 'stream.capture.sink = "true"' in line or 'media.role = "production"' in line:
                    is_monitor = True

            if current_id is not None:
                should_skip = (
                    (current_owner in exclude_set)
                    or (skip_monitors and is_monitor)
                    or (wanted_source_id is not None and current_source != wanted_source_id)
                )
                if not should_skip:
                    res = self._pactl("move-source-output", str(current_id), target, check=False)
                    if res.ok:
                        moved += 1
            return moved

        for item in self.source_outputs():
            if wanted_source_id is not None and item.source != wanted_source_id:
                continue
            res = self._pactl("move-source-output", str(item.id), target, check=False)
            if res.ok:
                moved += 1
        return moved

    def move_app_streams(self, identifier: str | int, target: str) -> int:
        """Move stream(s) matching identifier (stream ID, PID, binary, node or app name) to target sink."""
        target_str = str(identifier).strip().lower()
        if not target_str:
            return 0
        moved = 0
        for item in self.detailed_sink_inputs():
            matched = False
            if str(item.id) == target_str:
                matched = True
            elif str(item.pid) == target_str and item.pid > 0:
                matched = True
            elif item.binary and (item.binary.lower() == target_str or target_str in item.binary.lower()):
                matched = True
            elif item.node_name and (item.node_name.lower() == target_str or target_str in item.node_name.lower()):
                matched = True
            elif item.app_name and (
                item.app_name.lower() == target_str
                or target_str in item.app_name.lower()
                or item.app_name.lower() in target_str
            ):
                matched = True
            elif item.media_name and target_str in item.media_name.lower():
                matched = True

            if matched:
                if item.sink_name and item.sink_name == target:
                    continue
                res = self._pactl("move-sink-input", str(item.id), target, check=False)
                if res.ok:
                    moved += 1
                else:
                    log.debug(
                        "Could not move sink-input %s to %s: %s",
                        item.id,
                        target,
                        (res.stderr or res.stdout).strip(),
                    )
        return moved

    # -- listener --------------------------------------------------------

    def ensure_tcp_listener(
        self,
        port: int,
        security: str,
        allowed_networks: Sequence[str] = DEFAULT_LAN_NETWORKS,
    ) -> Listener:
        """Reuse or create the native-protocol TCP listener.

        Reusing an existing listener is important: desktop distributions often
        load one already, and reloading it would briefly drop every client.
        """

        existing = self.modules()
        for module in existing:
            if module.name != "module-native-protocol-tcp":
                continue
            module_port = module.arg("port")
            if not module_port:
                module_port = "4713"
            try:
                matches = int(module_port) == int(port)
            except ValueError:
                matches = False
            if matches:
                log.debug("Reusing TCP listener module %s on port %s", module.id, port)
                return Listener(
                    module_id=module.id,
                    created=False,
                    args=module.args,
                    port=port,
                    security=listener_security(module.args),
                )

        if security == "cookie":
            args = [f"port={port}"]
        elif security == "open":
            args = [f"port={port}", "auth-anonymous=1"]
        else:
            networks = list(allowed_networks)
            if "127.0.0.1" not in networks:
                networks.insert(0, "127.0.0.1")
            args = [
                f"port={port}",
                "auth-anonymous=1",
                "auth-ip-acl=" + ";".join(networks),
            ]
        module_id = self.load_module("module-native-protocol-tcp", args)
        return Listener(
            module_id=module_id,
            created=True,
            args=" ".join(args),
            port=port,
            security=security,
        )

    def remove_listener(self, module_id: int) -> bool:
        return self.unload_module(module_id)

    # -- tunnel sinks ----------------------------------------------------

    def connect_tunnel(
        self,
        *,
        host: str,
        port: int,
        sink_name: str,
        description: str,
        reconnect_delay_ms: int = 3000,
        latency_ms: int | None = None,
        extra_args: Sequence[str] = (),
    ) -> int:
        sink_name = sanitize_sink_name(sink_name)
        existing = self.sink(sink_name)
        if existing is not None:
            module = self.module_for_sink(sink_name)
            if module is not None:
                self.unload_module(module.id)
            self._sleep(0.15)
            if self.sink_exists(sink_name):
                raise BackendError(
                    f"A sink named {sink_name!r} already exists and is not owned by SoundHop"
                )

        safe_description = re.sub(r"[^A-Za-z0-9_.-]+", "-", description).strip("-_")
        safe_description = (safe_description or "remote")[:64]
        args = [
            f"server={tcp_server_arg(host, port)}",
            f"sink_name={sink_name}",
        ]
        if reconnect_delay_ms:
            args.append(f"reconnect_delay_msec={int(reconnect_delay_ms)}")
        if latency_ms:
            args.append(f"latency_msec={int(latency_ms)}")
        extra_strs = [str(x) for x in extra_args]
        if not any(x.startswith("rate=") for x in extra_strs):
            args.append("rate=48000")
        if not any(x.startswith("channels=") for x in extra_strs):
            args.append("channels=2")
        if not any(x.startswith("format=") for x in extra_strs):
            args.append("format=s16le")
        args.extend(extra_strs)
        args.append(f"sink_properties=device.description=SoundHop-{safe_description}")

        module_id = self.load_module("module-tunnel-sink", args)
        if not self._wait_for_sink(sink_name, SINK_TIMEOUT):
            self.unload_module(module_id)
            raise BackendError(
                f"PipeWire accepted the tunnel but sink {sink_name!r} did not appear"
            )
        return module_id

    def disconnect_tunnel(self, sink_name: str) -> bool:
        module = self.module_for_sink(sink_name)
        if module is None:
            # The sink may have been left behind without a discoverable module.
            return False
        return self.unload_module(module.id)

    def _wait_for_sink(self, name: str, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.sink_exists(name):
                return True
            self._sleep(0.1)
        return False

    # -- tunnel sources (microphone pass-through) ------------------------

    def connect_tunnel_source(
        self,
        *,
        host: str,
        port: int,
        source_name: str,
        description: str,
        remote_source: str = "",
        reconnect_delay_ms: int = 3000,
        latency_ms: int | None = None,
        extra_args: Sequence[str] = (),
    ) -> int:
        source_name = sanitize_sink_name(source_name)
        existing = self.source(source_name)
        if existing is not None:
            module = self.module_for_source(source_name)
            if module is not None:
                self.unload_module(module.id)
            self._sleep(0.15)
            if self.source_exists(source_name):
                raise BackendError(
                    f"A source named {source_name!r} already exists and is not owned by SoundHop"
                )

        safe_description = re.sub(r"[^A-Za-z0-9_.-]+", "-", description).strip("-_")
        safe_description = (safe_description or "remote")[:64]
        args = [
            f"server={tcp_server_arg(host, port)}",
            f"source_name={source_name}",
        ]
        if remote_source:
            args.append(f"source={remote_source}")
        if reconnect_delay_ms:
            args.append(f"reconnect_delay_msec={int(reconnect_delay_ms)}")
        if latency_ms:
            args.append(f"latency_msec={int(latency_ms)}")
        extra_strs = [str(x) for x in extra_args]
        if not any(x.startswith("rate=") for x in extra_strs):
            args.append("rate=48000")
        if not any(x.startswith("channels=") for x in extra_strs):
            args.append("channels=2")
        if not any(x.startswith("format=") for x in extra_strs):
            args.append("format=s16le")
        args.extend(extra_strs)
        args.append(
            f"source_properties=device.description=SoundHop-Microphone-{safe_description} device.icon_name=audio-input-microphone device.class=sound"
        )

        module_id = self.load_module("module-tunnel-source", args)
        if not self._wait_for_source(source_name, SOURCE_TIMEOUT):
            self.unload_module(module_id)
            raise BackendError(
                f"PipeWire accepted the tunnel but source {source_name!r} did not appear"
            )
        return module_id

    def disconnect_tunnel_source(self, source_name: str) -> bool:
        module = self.module_for_source(source_name)
        if module is None:
            return False
        return self.unload_module(module.id)

    def send_mic_to_remote(
        self,
        *,
        host: str,
        port: int,
        description: str,
        local_source: str = "@DEFAULT_SOURCE@",
    ) -> tuple[int, int, int, int, str]:
        """Send local microphone to remote receiver as a virtual input device.

        Returns (remote_null_id, remote_remap_id, local_tunnel_id, local_loopback_id, remote_source_name).
        """
        safe_description = re.sub(r"[^A-Za-z0-9_.-]+", "-", description).strip("-_")[:32] or "local"
        remote_sink_name = f"soundhop_rem_mic_sink_{safe_description.lower()}"
        remote_source_name = f"soundhop_rem_mic_{safe_description.lower()}"
        local_tunnel_name = f"soundhop_tx_mic_{safe_description.lower()}"

        # 1. On remote receiver: load null-sink to accept audio tunnel
        remote_target = tcp_server_arg(host, port)
        remote_props = (
            f"device.description=SoundHop-Mic-Sink-{safe_description} "
            f"device.class=sound"
        )
        res = self._pactl(
            "-s",
            remote_target,
            "load-module",
            "module-null-sink",
            f"sink_name={remote_sink_name}",
            f"sink_properties={remote_props}",
        )
        m = re.search(r"\b(\d+)\b", res.stdout)
        remote_null_id = int(m.group(1)) if m else 0

        # 2. On remote receiver: load module-remap-source so that apps like Discord/WebRTC see a REAL microphone rather than a .monitor
        remote_remap_id = 0
        actual_source = f"{remote_sink_name}.monitor"
        try:
            remap_res = self._pactl(
                "-s",
                remote_target,
                "load-module",
                "module-remap-source",
                f"source_name={remote_source_name}",
                f"master={remote_sink_name}.monitor",
                f"source_properties=device.description='SoundHop Microphone ({description})' device.class=sound",
            )
            rm = re.search(r"\b(\d+)\b", remap_res.stdout)
            if rm:
                remote_remap_id = int(rm.group(1))
                actual_source = remote_source_name
        except Exception as exc:
            log.warning("Could not load module-remap-source on remote (falling back to monitor): %s", exc)

        # 3. Set default source on remote
        try:
            self._pactl("-s", remote_target, "set-default-source", actual_source, check=False)
        except Exception:
            pass

        # 4. Move active recording streams on remote (e.g. Discord, Audacity) to the new microphone
        try:
            detailed = self._pactl("-s", remote_target, "list", "source-outputs", check=False)
            if detailed.ok and detailed.stdout:
                current_id = None
                current_owner = None
                for line in detailed.stdout.splitlines():
                    m_so = re.match(r"^Source Output #(\d+)", line)
                    if m_so:
                        if current_id and current_owner != remote_remap_id:
                            self._pactl("-s", remote_target, "move-source-output", current_id, actual_source, check=False)
                        current_id = m_so.group(1)
                        current_owner = None
                    m_owner = re.search(r"Owner Module:\s*(\d+)", line)
                    if m_owner:
                        current_owner = int(m_owner.group(1))
                if current_id and current_owner != remote_remap_id:
                    self._pactl("-s", remote_target, "move-source-output", current_id, actual_source, check=False)
        except Exception as exc:
            log.warning("Could not move remote source-outputs: %s", exc)

        # 5. On local machine: load module-tunnel-sink to remote_sink_name
        local_tunnel_id = self.load_module(
            "module-tunnel-sink",
            [
                f"server={remote_target}",
                f"sink={remote_sink_name}",
                f"sink_name={local_tunnel_name}",
                "rate=48000",
                "channels=2",
                "format=s16le",
            ],
        )

        # 6. On local machine: load module-loopback from local mic to local_tunnel_name
        local_loopback_id = self.load_module(
            "module-loopback",
            [
                f"source={local_source}",
                f"sink={local_tunnel_name}",
                "latency_msec=30",
            ],
        )

        return (remote_null_id, remote_remap_id, local_tunnel_id, local_loopback_id, actual_source)

    def stop_send_mic_to_remote(
        self,
        *,
        host: str,
        port: int,
        remote_null_id: int = 0,
        remote_remap_id: int = 0,
        local_tunnel_id: int = 0,
        local_loopback_id: int = 0,
    ) -> None:
        """Tear down sending local microphone to remote receiver."""
        if local_loopback_id:
            try:
                self.unload_module(local_loopback_id)
            except Exception:
                pass
        if local_tunnel_id:
            try:
                self.unload_module(local_tunnel_id)
            except Exception:
                pass
        if host and port:
            remote_target = tcp_server_arg(host, port)
            if remote_remap_id:
                try:
                    self._pactl("-s", remote_target, "unload-module", str(remote_remap_id), check=False)
                except Exception:
                    pass
            if remote_null_id:
                try:
                    self._pactl("-s", remote_target, "unload-module", str(remote_null_id), check=False)
                except Exception:
                    pass

    def _wait_for_source(self, name: str, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.source_exists(name):
                return True
            self._sleep(0.1)
        return False

    # -- diagnostics -----------------------------------------------------

    def soundhop_modules(self) -> list[Module]:
        result: list[Module] = []
        for module in self.modules():
            sink_name = module.arg("sink_name")
            source_name = module.arg("source_name")
            if (
                (sink_name and (sink_name.startswith(SINK_PREFIX) or sink_name.startswith("soundhop_rem_mic_sink_")))
                or (source_name and (source_name.startswith(SOURCE_PREFIX) or source_name.startswith("soundhop_rem_mic_")))
            ):
                result.append(module)
        return result

    def cleanup_orphaned_remote_mic_modules(self) -> list[int]:
        """Unload receiver-side remote microphone modules whose sender tunnel has disconnected."""
        mods = self.modules()
        rem_nulls = [
            m
            for m in mods
            if m.name == "module-null-sink" and m.arg("sink_name").startswith("soundhop_rem_mic_sink_")
        ]
        rem_remaps = [
            m
            for m in mods
            if m.name == "module-remap-source" and m.arg("source_name").startswith("soundhop_rem_mic_")
        ]
        if not rem_nulls and not rem_remaps:
            return []

        sinks_by_name = {s.name: s.id for s in self.sinks()}
        active_sink_ids = {inp.sink for inp in self.sink_inputs()}
        removed: list[int] = []
        active_monitors: set[str] = set()

        for null_mod in rem_nulls:
            sink_name = null_mod.arg("sink_name")
            sink_id = sinks_by_name.get(sink_name)
            if sink_id is not None and sink_id in active_sink_ids:
                active_monitors.add(f"{sink_name}.monitor")
                continue
            for remap_mod in list(rem_remaps):
                if remap_mod.arg("master") == f"{sink_name}.monitor":
                    if self.unload_module(remap_mod.id):
                        removed.append(remap_mod.id)
                    rem_remaps.remove(remap_mod)
            if self.unload_module(null_mod.id):
                removed.append(null_mod.id)

        for remap_mod in rem_remaps:
            if remap_mod.arg("master") not in active_monitors:
                if self.unload_module(remap_mod.id):
                    removed.append(remap_mod.id)

        return removed

    # -- volume management -----------------------------------------------

    def get_sink_volume(self, sink_name: str) -> int:
        try:
            res = self._pactl("get-sink-volume", sink_name, check=False)
            if res.ok:
                m = re.search(r"(\d+)%", res.stdout)
                if m:
                    return int(m.group(1))
        except Exception as exc:
            log.debug("get-sink-volume failed for %s: %s", sink_name, exc)
        return 100

    def set_sink_volume(self, sink_name: str, volume_percent: int) -> None:
        vol = max(0, min(150, int(volume_percent)))
        self._pactl("set-sink-volume", sink_name, f"{vol}%", check=False)

    def get_sink_mute(self, sink_name: str) -> bool:
        try:
            res = self._pactl("get-sink-mute", sink_name, check=False)
            if res.ok:
                return "yes" in res.stdout.lower()
        except Exception:
            pass
        return False

    def set_sink_mute(self, sink_name: str, mute: bool) -> None:
        val = "1" if mute else "0"
        self._pactl("set-sink-mute", sink_name, val, check=False)

    def remote_set_volume(
        self,
        host: str,
        port: int,
        volume_percent: int,
        mute: bool | None = None,
    ) -> None:
        server_target = tcp_server_arg(host, port)
        vol = max(0, min(150, int(volume_percent)))
        self._pactl("-s", server_target, "set-sink-volume", "@DEFAULT_SINK@", f"{vol}%", check=False)
        if mute is not None:
            self._pactl("-s", server_target, "set-sink-mute", "@DEFAULT_SINK@", "1" if mute else "0", check=False)

    def describe(self) -> dict[str, object]:
        return {
            "available": self.available(),
            "server": self.server_label(),
            "pipewire": self.is_pipewire(),
            "default_sink": self.default_sink(),
            "sinks": [{"name": s.name, "state": s.state, "spec": s.spec} for s in self.sinks()],
        }


def listener_security(args: str) -> str:
    if "auth-anonymous=1" not in args:
        return "cookie"
    if "auth-ip-acl" in args:
        return "trusted-lan"
    return "open"


LinuxAudioBackend = AudioBackend


def create_backend(*args: Any, **kwargs: Any) -> AudioBackend:
    """Create the platform-appropriate audio backend."""
    if sys.platform == "win32" and "runner" not in kwargs and "pactl" not in kwargs:
        from .platform.windows.backend import WindowsAudioBackend

        return WindowsAudioBackend(*args, **kwargs)
    return LinuxAudioBackend(*args, **kwargs)
