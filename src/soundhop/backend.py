"""PipeWire / PulseAudio backend built on the stable ``pactl`` CLI.

PipeWire's PulseAudio compatibility layer implements the two modules SoundHop
needs (``module-native-protocol-tcp`` and ``module-tunnel-sink``) and ``pactl``
is present on every desktop distro that ships either server, so this is the
smallest common denominator that still gives us real, system-wide audio.
"""

from __future__ import annotations

import logging
import re
import time
from dataclasses import dataclass
from typing import Callable, Sequence

from .constants import (
    DEFAULT_LAN_NETWORKS,
    SINK_PREFIX,
    SINK_TIMEOUT,
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
class SinkInput:
    id: int
    sink: int
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
                (result.stderr or result.stdout).strip()
                or f"pactl {' '.join(args)} failed"
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
        return "pipewire" in self.server_name().lower() or "pipewire" in self.server_version().lower()

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
            result = self._pactl(
                "move-sink-input", str(item.id), target, check=False
            )
            if result.ok:
                moved += 1
            else:
                log.debug(
                    "Could not move sink-input %s: %s",
                    item.id,
                    (result.stderr or result.stdout).strip(),
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

    # -- diagnostics -----------------------------------------------------

    def soundhop_modules(self) -> list[Module]:
        result: list[Module] = []
        for module in self.modules():
            sink_name = module.arg("sink_name")
            if sink_name.startswith(SINK_PREFIX):
                result.append(module)
        return result

    def describe(self) -> dict[str, object]:
        return {
            "available": self.available(),
            "server": self.server_label(),
            "pipewire": self.is_pipewire(),
            "default_sink": self.default_sink(),
            "sinks": [
                {"name": s.name, "state": s.state, "spec": s.spec} for s in self.sinks()
            ],
        }


def listener_security(args: str) -> str:
    if "auth-anonymous=1" not in args:
        return "cookie"
    if "auth-ip-acl" in args:
        return "trusted-lan"
    return "open"
