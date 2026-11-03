"""High-level connection logic shared by the CLI, daemon and tray."""

from __future__ import annotations

import logging
import shlex
import threading
import time
from dataclasses import asdict
from pathlib import Path
from typing import Any

from .android import (
    AndroidCaptureSession,
    AndroidError,
    AndroidManager,
)
from .backend import (
    AudioBackend,
    BackendError,
    Listener,
    Module,
    listener_security,
)
from .config import Config
from .constants import (
    PROTOCOL_VERSION,
    SINK_PREFIX,
    VERSION,
)
from .discovery import Peer
from .state import State
from .util import (
    format_duration,
    is_private_address,
    local_addresses,
    notify,
    short_id,
    slugify,
    tcp_reachable,
)

log = logging.getLogger(__name__)


class ControllerError(RuntimeError):
    pass


class Controller:
    def __init__(
        self,
        *,
        config: Config | None = None,
        state: State | None = None,
        backend: AudioBackend | None = None,
        config_path: Path | None = None,
        state_path: Path | None = None,
        notify_enabled: bool = True,
    ) -> None:
        self.config = config or Config.load(config_path)
        self.state = state or State.load(state_path)
        self.backend = backend or AudioBackend()
        self._config_path = config_path
        self._state_path = state_path
        self._notify_enabled = notify_enabled
        self.lock = threading.RLock()
        self.android_manager = AndroidManager(
            scrcpy_path=self.config.android.scrcpy_path,
            adb_path=self.config.android.adb_path,
        )
        self.android_session = AndroidCaptureSession(self.android_manager)

    # -- persistence -----------------------------------------------------

    def save(self) -> None:
        self.config.save(self._config_path)
        self.state.save(self._state_path)

    def reload_config(self) -> Config:
        with self.lock:
            new = Config.load(self._config_path)
            self.config = new
            self.android_manager.custom_scrcpy = new.android.scrcpy_path
            self.android_manager.custom_adb = new.android.adb_path
            return new

    # -- status ----------------------------------------------------------

    def status(self, peers: list[Peer] | None = None) -> dict[str, Any]:
        with self.lock:
            data: dict[str, Any] = {
                "version": VERSION,
                "protocol": PROTOCOL_VERSION,
                "config_path": str(self.config.path),
                "device": {
                    "name": self.config.device.name,
                    "id": self.config.device.id,
                },
                "sharing": {
                    "enabled": bool(self.config.sharing.enabled),
                    "port": self.config.sharing.port,
                    "security": self.config.sharing.security,
                    "listener_module": self.state.listener_module,
                    "listener_created": self.state.listener_created,
                    "listener_security": self.state.listener_security,
                    "addresses": local_addresses(),
                },
                "auto": {
                    "enabled": bool(self.config.connection.auto_connect),
                    "paused": bool(self.state.auto_paused),
                    "last_peer": self.config.connection.last_peer,
                },
                "android": self.android_session.status(),
                "connection": None,
                "peers": [p.to_dict() for p in (peers or [])],
                "known_peers": [
                    asdict(p) for p in sorted(
                        self.state.known_peers.values(),
                        key=lambda p: p.last_seen,
                        reverse=True,
                    )
                ],
                "backend": {},
            }
            try:
                if self.backend.available():
                    data["backend"] = {
                        "available": True,
                        "server": self.backend.server_label(),
                        "pipewire": self.backend.is_pipewire(),
                        "default_sink": self.backend.default_sink(),
                        "sinks": [
                            {
                                "name": sink.name,
                                "state": sink.state,
                                "spec": sink.spec,
                                "soundhop": sink.name.startswith(SINK_PREFIX),
                            }
                            for sink in self.backend.sinks()
                        ],
                    }
                else:
                    data["backend"] = {"available": False}
            except (BackendError, OSError) as exc:
                data["backend"] = {"available": False, "error": str(exc)}

            if self.state.connected:
                sink = None
                module = None
                try:
                    sink = self.backend.sink(self.state.connected_sink)
                    module = self.backend.module(self.state.connected_module) if self.state.connected_module else None
                except (BackendError, OSError):
                    pass
                data["connection"] = {
                    "connected": True,
                    "peer_id": self.state.connected_peer,
                    "name": self.state.connected_name,
                    "host": self.state.connected_host,
                    "port": self.state.connected_port,
                    "sink": self.state.connected_sink,
                    "module": self.state.connected_module,
                    "since": self.state.connected_since,
                    "duration": format_duration(
                        time.time() - self.state.connected_since
                        if self.state.connected_since
                        else None
                    ),
                    "by": self.state.connected_by,
                    "sink_present": sink is not None,
                    "module_present": module is not None,
                }
            return data

    # -- discovery helpers ----------------------------------------------

    def remember_peer(self, peer: Peer) -> None:
        self.state.remember_peer(peer, when=time.time())

    def known_peer(self, peer_id: str) -> Peer | None:
        known = self.state.known_peers.get(peer_id)
        if known is None:
            return None
        return Peer(
            id=known.id,
            name=known.name,
            host=known.host,
            port=known.port,
            version=known.version,
            security=known.security,
            last_seen=known.last_seen,
            source="remembered",
        )

    def sink_name_for(self, peer: Peer) -> str:
        label = slugify(peer.display_name, max_length=32)
        return f"{SINK_PREFIX}{label}_{short_id(peer.id, 6)}"

    # -- sharing ---------------------------------------------------------

    def share(self, enabled: bool, *, notify_user: bool | None = None) -> dict[str, Any]:
        """Persist the user's sharing preference and start/stop the listener."""

        with self.lock:
            if enabled:
                result = self.start_listener(notify_user=notify_user)
                self.config.sharing.enabled = True
            else:
                self.config.sharing.enabled = False
                result = self.stop_listener(notify_user=notify_user)
            self.save()
            return result

    def start_listener(self, *, notify_user: bool | None = None) -> dict[str, Any]:
        """Start the TCP listener without changing the sharing preference."""

        with self.lock:
            self._ensure_backend()
            self._reconcile_listener()
            listener = self._ensure_listener()
            if self._should_notify(notify_user):
                notify(
                    "SoundHop",
                    f"Sharing this device's speakers on port {listener.port}",
                )
            return {
                "enabled": True,
                "port": listener.port,
                "security": listener.security,
                "module": listener.module_id,
                "created": listener.created,
                "addresses": local_addresses(),
            }

    def stop_listener(self, *, notify_user: bool | None = None) -> dict[str, Any]:
        """Stop the listener SoundHop created, if any.

        A listener that already existed (for example one loaded by the desktop)
        is left alone; removing it could break unrelated clients.
        """

        with self.lock:
            removed = False
            note = ""
            if self.state.listener_module:
                module = self._find_module(self.state.listener_module)
                if module is not None and self.state.listener_created:
                    removed = self.backend.unload_module(module.id)
                elif module is not None:
                    note = (
                        "A TCP audio listener not created by SoundHop is still "
                        "running. Discovery advertisements are off."
                    )
                self.state.listener_module = 0
                self.state.listener_created = False
                self.state.listener_port = 0
                self.state.listener_security = ""
                self.state.save(self._state_path)
            if self._should_notify(notify_user):
                notify("SoundHop", "No longer sharing this device's speakers")
            return {"enabled": False, "removed": removed, "note": note}

    def _ensure_listener(self) -> Listener:
        if self.state.listener_module:
            module = self._find_module(self.state.listener_module)
            if module is not None and module.name == "module-native-protocol-tcp":
                if module.arg("port", "4713") == str(self.config.sharing.port):
                    actual_security = listener_security(module.args)
                    self.state.listener_port = self.config.sharing.port
                    self.state.listener_security = actual_security
                    self.state.save(self._state_path)
                    return Listener(
                        module_id=module.id,
                        created=self.state.listener_created,
                        args=module.args,
                        port=self.config.sharing.port,
                        security=actual_security,
                    )
                if self.state.listener_created:
                    self.backend.unload_module(module.id)
                    self.state.listener_module = 0
                    self.state.listener_created = False

        listener = self.backend.ensure_tcp_listener(
            self.config.sharing.port,
            self.config.sharing.security,
            self.config.sharing.allowed_networks,
        )
        # If a module was already loaded by the desktop, only take ownership of
        # its bookkeeping, never unload it.
        if self.config.sharing.security != listener.security and not listener.created:
            log.warning(
                "Reusing existing TCP listener with security=%s (requested %s)",
                listener.security,
                self.config.sharing.security,
            )
        self.state.listener_module = listener.module_id
        self.state.listener_created = listener.created
        self.state.listener_port = listener.port
        self.state.listener_security = listener.security
        self.state.save(self._state_path)
        return listener

    def _reconcile_listener(self) -> None:
        if not self.state.listener_module:
            return
        module = self._find_module(self.state.listener_module)
        if module is None or module.name != "module-native-protocol-tcp":
            self.state.listener_module = 0
            self.state.listener_created = False
            self.state.listener_port = 0
            self.state.listener_security = ""

    def listener_running(self) -> bool:
        if not self.state.listener_module:
            return False
        module = self._find_module(self.state.listener_module)
        return module is not None and module.name == "module-native-protocol-tcp"

    # -- connections -----------------------------------------------------

    def connect(
        self,
        peer: Peer,
        *,
        switch_default: bool | None = None,
        move_streams: bool | None = None,
        by: str = "manual",
        notify_user: bool | None = None,
    ) -> dict[str, Any]:
        with self.lock:
            self._ensure_backend()
            if not peer.id:
                raise ControllerError("Invalid peer: missing device id")
            if peer.id == self.config.device.id:
                raise ControllerError("That is this device")
            if not is_private_address(peer.host):
                raise ControllerError(
                    f"Refusing to connect to non-private address {peer.host}"
                )
            if not peer.compatible:
                raise ControllerError(
                    f"{peer.display_name} speaks SoundHop protocol {peer.protocol}; "
                    f"this version speaks {PROTOCOL_VERSION}. Update both devices."
                )
            existing = self.state.connected_peer
            if existing == peer.id and self.state.connected:
                module = self._find_module(self.state.connected_module) if self.state.connected_module else None
                if module is not None and self.backend.sink_exists(self.state.connected_sink):
                    if switch_default is None:
                        switch_default = self.config.connection.switch_default
                    if switch_default:
                        self._make_default(self.state.connected_sink, move=move_streams)
                    return self._connection_dict()

            if not tcp_reachable(peer.host, peer.port):
                raise ControllerError(
                    f"{peer.display_name} is not accepting audio at "
                    f"{peer.host}:{peer.port}. Is it online and sharing?"
                )

            if self.state.connected:
                # Switch peers without losing the original local sink.
                self._disconnect_locked(
                    restore=False, pause_auto=False, notify_user=False, keep_previous=True
                )

            self._remember_previous_default()
            sink_name = self.sink_name_for(peer)
            switch_default = (
                self.config.connection.switch_default
                if switch_default is None
                else switch_default
            )
            move = self.config.connection.move_streams if move_streams is None else move_streams

            try:
                module_id = self.backend.connect_tunnel(
                    host=peer.host,
                    port=peer.port,
                    sink_name=sink_name,
                    description=peer.display_name,
                    reconnect_delay_ms=self.config.connection.reconnect_delay_ms,
                    latency_ms=self.config.connection.latency_ms,
                    extra_args=self.config.connection.extra_tunnel_args,
                )
            except (BackendError, OSError) as exc:
                raise ControllerError(
                    f"Could not create an audio tunnel to {peer.display_name}: {exc}"
                ) from exc

            self.state.connected_peer = peer.id
            self.state.connected_name = peer.display_name
            self.state.connected_host = peer.host
            self.state.connected_port = peer.port
            self.state.connected_sink = sink_name
            self.state.connected_module = module_id
            self.state.connected_since = time.time()
            self.state.connected_by = by
            self.state.auto_paused = False
            self.config.connection.last_peer = peer.id
            self.remember_peer(peer)
            self.save()

            switch_warning = ""
            if switch_default:
                try:
                    self._make_default(sink_name, move=move)
                except ControllerError as exc:
                    # The tunnel exists and audio can be routed manually; do
                    # not report the whole connection as failed.
                    switch_warning = str(exc)
                    log.warning("%s", switch_warning)

            if self._should_notify(notify_user):
                notify("SoundHop", f"Audio is now playing on {peer.display_name}")
            log.info("Connected to %s (%s:%s) via module %s", peer.display_name, peer.host, peer.port, module_id)
            result = self._connection_dict()
            if switch_warning:
                result["warning"] = switch_warning
            return result

    def disconnect(
        self,
        *,
        forget: bool = False,
        restore: bool | None = None,
        pause_auto: bool = True,
        notify_user: bool | None = None,
        all_tunnels: bool = False,
    ) -> dict[str, Any]:
        with self.lock:
            return self._disconnect_locked(
                restore=restore,
                pause_auto=pause_auto,
                forget=forget,
                notify_user=notify_user,
                all_tunnels=all_tunnels,
            )

    def _disconnect_locked(
        self,
        *,
        restore: bool | None = None,
        pause_auto: bool = True,
        forget: bool = False,
        notify_user: bool | None = None,
        keep_previous: bool = False,
        all_tunnels: bool = False,
    ) -> dict[str, Any]:
        restore = self.config.connection.restore_default if restore is None else restore
        removed: list[int] = []
        sink_name = self.state.connected_sink
        module_id = self.state.connected_module
        if module_id:
            if self.backend.unload_module(module_id):
                removed.append(module_id)
        if sink_name:
            module = self.backend.module_for_sink(sink_name)
            if module is not None and module.id not in removed:
                if self.backend.unload_module(module.id):
                    removed.append(module.id)
        # Remove any other SoundHop tunnels only when explicitly requested.
        if all_tunnels:
            for module in self.backend.soundhop_modules():
                if module.name.startswith("module-tunnel-") and module.id not in removed:
                    if self.backend.unload_module(module.id):
                        removed.append(module.id)

        previous = self.state.previous_default_sink
        restored_to = ""
        if restore:
            candidate = self._local_default_candidate(previous)
            if candidate:
                try:
                    self.backend.set_default_sink(candidate)
                    restored_to = candidate
                    if self.config.connection.move_streams:
                        self.backend.move_sink_inputs(candidate)
                except BackendError as exc:
                    log.warning("Could not restore default sink: %s", exc)

        name = self.state.connected_name
        peer_id = self.state.connected_peer
        self.state.clear_connection()
        if not keep_previous:
            self.state.previous_default_sink = ""
        if forget:
            if self.config.connection.last_peer == peer_id:
                self.config.connection.last_peer = ""
            if peer_id:
                self.state.forget_peer(peer_id)
            elif name:
                for known_id, known in list(self.state.known_peers.items()):
                    if known.name == name:
                        self.state.forget_peer(known_id)
        if pause_auto:
            self.state.auto_paused = True
        self.save()

        if removed or restored_to:
            if self._should_notify(notify_user):
                if restored_to:
                    notify("SoundHop", f"Audio restored to {restored_to}")
                else:
                    notify("SoundHop", "Remote audio disconnected")
        log.info(
            "Disconnected%s (modules=%s, restored=%s)",
            f" from {name}" if name else "",
            removed,
            restored_to or "-",
        )
        return {
            "disconnected": True,
            "peer": name,
            "removed_modules": removed,
            "restored_to": restored_to,
            "forgotten": bool(forget),
            "auto_paused": self.state.auto_paused,
        }

    def set_auto(self, enabled: bool) -> dict[str, Any]:
        with self.lock:
            self.config.connection.auto_connect = bool(enabled)
            if enabled:
                self.state.auto_paused = False
            self.save()
            return {
                "auto_connect": self.config.connection.auto_connect,
                "paused": self.state.auto_paused,
            }

    def toggle_auto(self) -> dict[str, Any]:
        return self.set_auto(not self.config.connection.auto_connect)

    def pause_auto(self, paused: bool = True) -> dict[str, Any]:
        with self.lock:
            self.state.auto_paused = bool(paused)
            self.state.save(self._state_path)
            return {"paused": self.state.auto_paused}

    # -- routing ---------------------------------------------------------

    def _remember_previous_default(self) -> None:
        current = self.backend.default_sink()
        previous = self.state.previous_default_sink
        if previous and self.backend.sink_exists(previous) and not previous.startswith(SINK_PREFIX):
            return
        if current and not current.startswith(SINK_PREFIX) and self.backend.sink_exists(current):
            self.state.previous_default_sink = current
        else:
            self.state.previous_default_sink = ""

    def _local_default_candidate(self, preferred: str = "") -> str:
        if preferred and self.backend.sink_exists(preferred) and not preferred.startswith(SINK_PREFIX):
            return preferred
        for sink in self.backend.sinks():
            if sink.name.startswith(SINK_PREFIX):
                continue
            if "auto_null" in sink.name or "dummy" in sink.name:
                continue
            return sink.name
        for sink in self.backend.sinks():
            if not sink.name.startswith(SINK_PREFIX):
                return sink.name
        return ""

    def _make_default(self, sink_name: str, *, move: bool | None = None) -> None:
        if move is None:
            move = self.config.connection.move_streams
        try:
            self.backend.set_default_sink(sink_name)
            if move:
                self.backend.move_sink_inputs(sink_name)
        except BackendError as exc:
            raise ControllerError(f"Could not switch the default output: {exc}") from exc

    def _connection_dict(self) -> dict[str, Any]:
        return {
            "connected": True,
            "peer_id": self.state.connected_peer,
            "name": self.state.connected_name,
            "host": self.state.connected_host,
            "port": self.state.connected_port,
            "sink": self.state.connected_sink,
            "module": self.state.connected_module,
            "since": self.state.connected_since,
            "by": self.state.connected_by,
        }

    # -- reconciliation / diagnostics ------------------------------------

    def reconcile(self) -> dict[str, Any]:
        """Bring state in line with the live audio graph."""

        actions: list[str] = []
        with self.lock:
            if not self.backend.available():
                return {"available": False, "actions": ["pactl is not reachable"]}
            if self.state.connected:
                module = (
                    self._find_module(self.state.connected_module)
                    if self.state.connected_module
                    else None
                )
                if module is None:
                    module = self.backend.module_for_sink(self.state.connected_sink)
                sink_ok = self.backend.sink_exists(self.state.connected_sink)
                if module is None or not sink_ok:
                    if module is not None:
                        self.backend.unload_module(module.id)
                    self.state.clear_connection()
                    actions.append("cleared stale connection state")
                    if self.config.connection.restore_default:
                        candidate = self._local_default_candidate(self.state.previous_default_sink)
                        if candidate:
                            try:
                                self.backend.set_default_sink(candidate)
                                if self.config.connection.move_streams:
                                    self.backend.move_sink_inputs(candidate)
                                actions.append(f"restored default sink to {candidate}")
                            except BackendError:
                                pass
                    self.state.save(self._state_path)
                elif module.id != self.state.connected_module and module.name.startswith("module-tunnel-"):
                    self.state.connected_module = module.id
                    actions.append("adopted tunnel module id")
                    self.state.save(self._state_path)
            if self.state.listener_module:
                module = self._find_module(self.state.listener_module)
                if module is None or module.name != "module-native-protocol-tcp":
                    self.state.listener_module = 0
                    self.state.listener_created = False
                    actions.append("cleared stale listener state")
                    self.state.save(self._state_path)
            return {"available": True, "actions": actions}

    def system_info(self) -> dict[str, Any]:
        data = self.status()
        try:
            data["modules"] = [
                {"id": m.id, "name": m.name, "args": m.args}
                for m in self.backend.modules()
            ]
            data["soundhop_modules"] = [
                {"id": m.id, "name": m.name, "args": m.args}
                for m in self.backend.soundhop_modules()
            ]
            data["sink_inputs"] = [
                asdict(item) for item in self.backend.sink_inputs()
            ]
        except (BackendError, OSError) as exc:
            data["error"] = str(exc)
        return data

    # -- android capture -------------------------------------------------

    def android_tool_status(self) -> dict[str, Any]:
        with self.lock:
            status = self.android_manager.get_tool_status()
            return {
                "scrcpy_path": str(status.scrcpy_path) if status.scrcpy_path else None,
                "scrcpy_version": status.scrcpy_version,
                "adb_path": str(status.adb_path) if status.adb_path else None,
                "adb_version": status.adb_version,
                "is_managed": status.is_managed,
                "ready": status.ready,
            }

    def android_list_devices(self) -> list[dict[str, Any]]:
        with self.lock:
            devices = self.android_manager.list_devices()
            return [asdict(d) for d in devices]

    def android_switch_wifi(self, serial: str, port: int = 5555) -> tuple[bool, str]:
        with self.lock:
            # First find wifi IP if possible
            ip = self.android_manager.get_device_wifi_ip(serial)
            ok, msg = self.android_manager.enable_tcpip(serial, port)
            if not ok:
                return False, msg
            if ip:
                time.sleep(1.0)
                conn_ok, conn_msg = self.android_manager.connect_wifi(ip, port)
                if conn_ok:
                    return True, f"Switched to Wi-Fi mode and connected to {ip}:{port}"
                return True, f"Switched to TCP mode, but Wi-Fi connect to {ip}:{port} failed: {conn_msg}"
            return True, f"Switched to TCP mode on port {port}. Please connect via Wi-Fi IP."

    def android_connect_wifi(self, host: str, port: int = 5555) -> tuple[bool, str]:
        with self.lock:
            return self.android_manager.connect_wifi(host, port)

    def android_disconnect_wifi(self, target: str | None = None) -> tuple[bool, str]:
        with self.lock:
            return self.android_manager.disconnect_wifi(target)

    def android_list_encoders(self, serial: str | None = None) -> dict[str, Any]:
        with self.lock:
            encoders = self.android_manager.list_encoders(serial)
            return {
                "codecs": encoders.codecs,
                "available_codecs": encoders.available_codecs,
            }

    def android_start_capture(
        self,
        serial: str,
        *,
        device_name: str = "",
        codec: str | None = None,
        source: str | None = None,
        buffer_ms: int | None = None,
        bit_rate: int | None = None,
        duplicate: bool | None = None,
    ) -> dict[str, Any]:
        with self.lock:
            codec_val = codec or self.config.android.default_codec
            source_val = source or self.config.android.default_source
            buf_val = buffer_ms or self.config.android.buffer_ms
            br_val = bit_rate or self.config.android.bit_rate
            dup_val = duplicate if duplicate is not None else self.config.android.duplicate_audio

            result = self.android_session.start(
                serial,
                device_name=device_name,
                codec=codec_val,
                source=source_val,
                buffer_ms=buf_val,
                bit_rate=br_val,
                duplicate=dup_val,
            )

            # Persist state
            self.state.android_active = True
            self.state.android_serial = serial
            self.state.android_model = device_name
            self.state.android_codec = codec_val
            self.state.android_pid = result.get("pid", 0)
            self.state.android_since = result.get("started_at", time.time())
            self.config.android.last_serial = serial
            self.save()

            if self._should_notify(None):
                notify(
                    "SoundHop: Android audio connected",
                    f"Streaming audio from {device_name or serial} ({codec_val.upper()})",
                )

            return result

    def android_stop_capture(self) -> dict[str, Any]:
        with self.lock:
            old_name = self.state.android_model or self.state.android_serial
            result = self.android_session.stop()
            self.state.android_active = False
            self.state.android_serial = ""
            self.state.android_model = ""
            self.state.android_codec = ""
            self.state.android_pid = 0
            self.state.android_since = 0.0
            self.save()

            if old_name and self._should_notify(None):
                notify(
                    "SoundHop: Android audio disconnected",
                    f"Stopped streaming audio from {old_name}",
                )

            return result

    def android_install_scrcpy(
        self,
        *,
        progress_cb: Any | None = None,
    ) -> dict[str, Any]:
        with self.lock:
            installed = self.android_manager.install_scrcpy(progress_cb=progress_cb)
            status = self.android_tool_status()
            return {
                "installed_path": str(installed),
                "tools": status,
            }

    # -- internal --------------------------------------------------------

    def _ensure_backend(self) -> None:
        if not self.backend.available():
            raise ControllerError(
                "PipeWire/PulseAudio is not reachable through pactl. "
                "Install pipewire-pulse or pulseaudio and try again."
            )

    def _find_module(self, module_id: int) -> Module | None:
        if not module_id:
            return None
        try:
            return self.backend.module(module_id)
        except (BackendError, OSError):
            return None

    def _should_notify(self, explicit: bool | None) -> bool:
        if explicit is not None:
            return explicit
        return self._notify_enabled and self.config.ui.notifications


def shell_join_module_args(args: str) -> list[str]:
    """Expose module args for debugging without running a shell."""

    return shlex.split(args)
