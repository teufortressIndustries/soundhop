"""The SoundHop user daemon.

It owns three things:

* the native-protocol TCP listener that lets other devices send audio here,
* discovery (UDP broadcast and optional mDNS),
* the auto-connect/reconnect supervisor and the local control socket.

Keeping these in one small user service is what makes daily use seamless: the
CLI and the tray are just clients of the daemon.
"""

from __future__ import annotations

try:
    import fcntl
except ImportError:
    fcntl = None  # type: ignore[assignment]
import logging
import os
import signal
import subprocess
import sys
import threading
import time
from logging.handlers import RotatingFileHandler
from pathlib import Path
from typing import Any

from .config import Config, ConfigError, _as_bool
from .constants import (
    OFFLINE_PROBE_LIMIT,
    PROTOCOL_VERSION,
    SUPERVISOR_INTERVAL,
    VERSION,
)
from .controller import Controller, ControllerError
from .discovery import DiscoveryManager, Peer
from .ipc import IpcServer, is_running
from .util import (
    ensure_dir,
    is_private_address,
    lock_path,
    log_path,
    pid_path,
    runtime_dir,
    tcp_reachable,
)

log = logging.getLogger(__name__)


class Daemon:
    def __init__(
        self,
        *,
        config_path: Path | None = None,
        state_path: Path | None = None,
        notify_enabled: bool = True,
        ipc_path: Path | None = None,
    ) -> None:
        self.config_path = config_path
        self.state_path = state_path
        self.controller = Controller(
            config_path=config_path,
            state_path=state_path,
            notify_enabled=notify_enabled,
        )
        self.config = self.controller.config
        self.discovery: DiscoveryManager | None = None
        self.ipc = IpcServer(self.handle, path=ipc_path)
        self._stop = threading.Event()
        self._started = time.time()
        self._shutdown_done = threading.Event()
        self._supervisor: threading.Thread | None = None
        self._advertising = False
        self._warnings: list[str] = []
        self._auto_failures = 0
        self._next_auto_attempt = 0.0
        self._offline_count = 0
        self._last_reconcile = 0.0
        self._last_peer_version_warned = ""
        self._pid = os.getpid()

    # -- lifecycle -------------------------------------------------------

    def start(self) -> None:
        self.controller.reconcile()
        self._setup_sharing()
        self._setup_discovery()
        self.ipc.start()
        self._write_pid()
        self._supervisor = threading.Thread(
            target=self._supervisor_loop, name="soundhop-supervisor", daemon=True
        )
        self._supervisor.start()
        log.info(
            "SoundHop %s daemon started (pid %s, device %s, sharing=%s)",
            VERSION,
            self._pid,
            self.config.device.name,
            self._advertising,
        )

    def _setup_sharing(self) -> None:
        self._advertising = False
        if not self.config.sharing.enabled:
            log.info("Sharing is disabled in the configuration")
            return
        try:
            self.controller.start_listener(notify_user=False)
            self._advertising = True
        except (ControllerError, OSError) as exc:
            warning = f"Could not start the audio listener: {exc}"
            self._warnings.append(warning)
            log.warning(warning)

    def _setup_discovery(self) -> None:
        if self.discovery is not None:
            self.discovery.stop()
            self.discovery = None
        self.discovery = DiscoveryManager(
            device_id=self.config.device.id,
            name=self.config.device.name,
            audio_port=self.config.sharing.port,
            discovery_port=self.config.discovery.port,
            security=self.config.sharing.security,
            interval=self.config.discovery.interval_s,
            use_udp=self.config.discovery.udp,
            use_mdns=self.config.discovery.mdns,
        )
        self.discovery.store.on_peer = self._on_peer_seen
        self.discovery.store.on_remove = self._on_peer_removed
        warnings = self.discovery.start(advertising=self._advertising)
        self._warnings.extend(warnings)
        for warning in warnings:
            log.warning("%s", warning)
        log.info(
            "Discovery started (udp=%s, mdns=%s, advertising=%s)",
            self.config.discovery.udp,
            self.config.discovery.mdns,
            self._advertising,
        )

    def _on_peer_seen(self, peer: Peer) -> None:
        if peer.id == self.config.device.id:
            return
        log.debug(
            "Peer seen: %s (%s:%s, source=%s)",
            peer.display_name,
            peer.host,
            peer.port,
            peer.source,
        )

    def _on_peer_removed(self, peer_id: str) -> None:
        log.debug("Peer removed: %s", peer_id)

    def stop(self) -> None:
        if self._shutdown_done.is_set():
            return
        self._shutdown_done.set()
        self._stop.set()
        log.info("Shutting down")
        try:
            if self.controller.state.connected or self.controller.state.previous_default_sink:
                self.controller.disconnect(
                    restore=self.config.connection.restore_default,
                    pause_auto=False,
                    notify_user=False,
                )
            if self.controller.android_session.is_active:
                self.controller.android_stop_capture()
            self.controller.stop_listener(notify_user=False)
        except Exception:  # noqa: BLE001 - shutdown must always finish
            log.debug("Cleanup during shutdown failed", exc_info=True)
        if self.discovery is not None:
            self.discovery.stop()
            self.discovery = None
        self.ipc.stop()
        try:
            if pid_path().exists():
                pid_path().unlink()
        except OSError:
            pass
        log.info("Stopped")

    def request_stop(self) -> None:
        self._stop.set()

    def wait(self) -> None:
        self._stop.wait()

    def _write_pid(self) -> None:
        ensure_dir(runtime_dir())
        try:
            pid_path().write_text(str(self._pid) + "\n", encoding="utf-8")
            pid_path().chmod(0o600)
        except OSError:
            log.debug("Could not write pid file", exc_info=True)

    # -- IPC commands ----------------------------------------------------

    def handle(self, request: dict[str, Any]) -> dict[str, Any]:
        command = str(request.get("cmd", "")).strip().lower()
        if command in {"ping", "hello"}:
            return self._daemon_info(pong=True)
        if command == "status":
            return self._status()
        if command == "peers":
            return {"peers": [p.to_dict() for p in self._peers()], **self._daemon_info()}
        if command == "discover":
            return {"peers": [p.to_dict() for p in self._peers()], **self._daemon_info()}
        if command == "connect":
            return self._handle_connect(request)
        if command == "disconnect":
            return self._handle_disconnect(request)
        if command == "share":
            return self._handle_share(request)
        if command == "auto":
            enabled = _as_bool(request.get("enabled"), True)
            result = self.controller.set_auto(enabled)
            self._reset_auto_backoff()
            return {**result, **self._daemon_info()}
        if command == "pause_auto":
            return self.controller.pause_auto(_as_bool(request.get("paused"), True))
        if command == "forget":
            return self._handle_forget(request)
        if command == "reload":
            return self._reload()
        if command in {"config", "get_config"}:
            return self._handle_config(request)
        if command in {"system", "info", "doctor"}:
            return self._system_info()
        if command in {"android_status"}:
            return {
                "session": self.controller.android_session.status(),
                "tools": self.controller.android_tool_status(),
                "devices": self.controller.android_list_devices(),
                **self._daemon_info(),
            }
        if command in {"android_devices"}:
            return {
                "devices": self.controller.android_list_devices(),
                **self._daemon_info(),
            }
        if command in {"android_wifi"}:
            action = request.get("action", "connect")
            if action == "switch":
                ok, msg = self.controller.android_switch_wifi(
                    str(request.get("serial", "")),
                    port=int(request.get("port", 5555) or 5555),
                )
            elif action == "disconnect":
                ok, msg = self.controller.android_disconnect_wifi(
                    request.get("target"),
                )
            else:
                host_val = str(request.get("host", "") or request.get("ip", "")).strip()
                if host_val and not is_private_address(host_val):
                    return {
                        **self._daemon_info(),
                        "ok": False,
                        "message": f"Refusing to connect to non-private address {host_val}",
                    }
                ok, msg = self.controller.android_connect_wifi(
                    host_val,
                    port=int(request.get("port", 5555) or 5555),
                )
            return {**self._daemon_info(), "ok": ok, "message": msg}
        if command in {"android_encoders"}:
            return {
                **self.controller.android_list_encoders(request.get("serial")),
                **self._daemon_info(),
            }
        if command in {"android_start", "android_capture"}:
            res = self.controller.android_start_capture(
                str(request.get("serial", "")),
                device_name=str(request.get("name", "")),
                codec=request.get("codec"),
                source=request.get("source"),
                buffer_ms=request.get("buffer_ms"),
                bit_rate=request.get("bit_rate"),
                duplicate=request.get("duplicate"),
            )
            return {**res, **self._daemon_info()}
        if command in {"android_stop", "android_stop_capture"}:
            res = self.controller.android_stop_capture()
            return {**res, **self._daemon_info()}
        if command in {"android_install"}:
            res = self.controller.android_install_scrcpy()
            return {**res, **self._daemon_info()}
        if command in {"shutdown", "stop", "quit"}:
            self.request_stop()
            return {"stopping": True, **self._daemon_info()}
        if command in {"version"}:
            return {"version": VERSION, "protocol": PROTOCOL_VERSION}
        return {"ok": False, "error": f"Unknown command: {command}"}

    def _daemon_info(self, *, pong: bool = False) -> dict[str, Any]:
        data: dict[str, Any] = {
            "ok": True,
            "daemon": {
                "running": True,
                "pid": self._pid,
                "started": self._started,
                "uptime": time.time() - self._started,
                "version": VERSION,
                "protocol": PROTOCOL_VERSION,
                "socket": str(self.ipc.path),
                "log": str(log_path()),
            },
            "warnings": list(self._warnings),
        }
        if pong:
            data["pong"] = True
        return data

    def _status(self) -> dict[str, Any]:
        peers = self._peers()
        data = self.controller.status(peers)
        data.update(self._daemon_info())
        data["discovery"] = {
            "udp": self.config.discovery.udp,
            "mdns": self.config.discovery.mdns,
            "port": self.config.discovery.port,
            "advertising": self._advertising,
        }
        return data

    def _peers(self) -> list[Peer]:
        if self.discovery is None:
            return []
        peers = self.discovery.peers()
        return [p for p in peers if p.id != self.config.device.id]

    def _resolve_peer(self, request: dict[str, Any]) -> Peer:
        peer_id = str(request.get("peer", "") or request.get("peer_id", "")).strip()
        host = str(request.get("host", "")).strip()
        port = int(request.get("port", self.config.sharing.port) or self.config.sharing.port)
        if peer_id:
            for peer in self._peers():
                if peer.id == peer_id:
                    if not is_private_address(peer.host):
                        raise ControllerError(
                            f"Peer address {peer.host} is not a private LAN address"
                        )
                    return peer
            known = self.controller.known_peer(peer_id)
            if known is not None:
                if not is_private_address(known.host):
                    raise ControllerError(f"Peer address {known.host} is not a private LAN address")
                return known
            # Allow an id passed with an explicit host (tray/manual use).
            if host:
                if not is_private_address(host):
                    raise ControllerError(f"Peer address {host} is not a private LAN address")
                return Peer(
                    id=peer_id,
                    name=str(request.get("name", peer_id)),
                    host=host,
                    port=port,
                    source="manual",
                )
            raise ControllerError(
                f"Device {peer_id} is not online and is not in the known-device list"
            )
        if host:
            if not is_private_address(host):
                raise ControllerError(f"Peer address {host} is not a private LAN address")
            return Peer(
                id=str(request.get("id", "") or f"manual-{host}"),
                name=str(request.get("name", host)),
                host=host,
                port=port,
                source="manual",
            )
        raise ControllerError("connect requires a peer id or host")

    def _handle_connect(self, request: dict[str, Any]) -> dict[str, Any]:
        peer = self._resolve_peer(request)
        switch_default = request.get("switch_default")
        move_streams = request.get("move_streams")
        result = self.controller.connect(
            peer,
            switch_default=None if switch_default is None else _as_bool(switch_default, True),
            move_streams=None if move_streams is None else _as_bool(move_streams, True),
            by=str(request.get("by", "manual")),
        )
        self._reset_auto_backoff()
        self._offline_count = 0
        return {**result, **self._daemon_info()}

    def _handle_disconnect(self, request: dict[str, Any]) -> dict[str, Any]:
        result = self.controller.disconnect(
            forget=_as_bool(request.get("forget"), False),
            restore=request.get("restore"),
            pause_auto=_as_bool(request.get("pause_auto"), True),
            all_tunnels=_as_bool(request.get("all"), False),
            stop_android=_as_bool(request.get("stop_android"), False),
        )
        return {**result, **self._daemon_info()}

    def _handle_share(self, request: dict[str, Any]) -> dict[str, Any]:
        enabled = _as_bool(request.get("enabled"), True)
        result = self.controller.share(enabled)
        self._advertising = enabled
        if self.discovery is not None:
            self.discovery.set_advertising(enabled)
        if enabled:
            self._warnings = [w for w in self._warnings if not w.startswith("Could not start")]
        data = self._status()
        data.pop("peers", None)
        return {**result, **data}

    def _handle_forget(self, request: dict[str, Any]) -> dict[str, Any]:
        peer_id = str(request.get("peer", "") or request.get("peer_id", ""))
        if peer_id:
            self.controller.state.forget_peer(peer_id)
            if self.config.connection.last_peer == peer_id:
                self.config.connection.last_peer = ""
            self.controller.save()
        return {
            "forgotten": peer_id,
            **self._daemon_info(),
        }

    def _handle_config(self, request: dict[str, Any]) -> dict[str, Any]:
        if "patch" in request and isinstance(request["patch"], dict):
            self.controller.config.apply_patch(request["patch"])
            self.controller.config.save(self.config_path)
            self._reload()
        if "set" in request and isinstance(request["set"], dict):
            for key, value in request["set"].items():
                self.controller.config.set(str(key), value)
            self.controller.config.save(self.config_path)
            self._reload()
        return {
            "config": self.controller.config.to_dict(),
            "path": str(self.controller.config.path),
            **self._daemon_info(),
        }

    def _reload(self) -> dict[str, Any]:
        try:
            new_config = Config.load(self.config_path)
        except ConfigError as exc:
            raise ControllerError(f"Configuration error: {exc}") from exc
        old = self.controller.config
        sharing_changed = (
            old.sharing.enabled != new_config.sharing.enabled
            or old.sharing.port != new_config.sharing.port
            or old.sharing.security != new_config.sharing.security
            or old.sharing.allowed_networks != new_config.sharing.allowed_networks
        )
        discovery_changed = (
            old.discovery.udp != new_config.discovery.udp
            or old.discovery.mdns != new_config.discovery.mdns
            or old.discovery.port != new_config.discovery.port
            or old.discovery.interval_s != new_config.discovery.interval_s
            or old.device.id != new_config.device.id
        )
        with self.controller.lock:
            if sharing_changed and old.sharing.enabled:
                try:
                    self.controller.stop_listener(notify_user=False)
                except Exception:
                    log.debug("Could not stop old listener during reload", exc_info=True)
            self.controller.config = new_config
            self.config = new_config
            if sharing_changed and new_config.sharing.enabled:
                try:
                    self.controller.start_listener(notify_user=False)
                    self._advertising = True
                except (ControllerError, OSError) as exc:
                    self._advertising = False
                    self._warnings.append(f"Could not restart the audio listener: {exc}")
                    log.warning("Could not restart the audio listener: %s", exc)
            elif not new_config.sharing.enabled:
                self._advertising = False
            self.controller.state.save(self.state_path)
        if self.discovery is not None:
            if discovery_changed:
                self._setup_discovery()
            else:
                self.discovery.update(
                    name=new_config.device.name,
                    audio_port=new_config.sharing.port,
                    security=new_config.sharing.security,
                )
                self.discovery.set_advertising(self._advertising)
        self._reset_auto_backoff()
        log.info("Configuration reloaded")
        return {"reloaded": True, **self._status()}

    def _system_info(self) -> dict[str, Any]:
        data = self.controller.system_info()
        data.update(self._daemon_info())
        data["discovery"] = {
            "udp": self.config.discovery.udp,
            "mdns": self.config.discovery.mdns,
            "port": self.config.discovery.port,
            "advertising": self._advertising,
        }
        data["peers"] = [p.to_dict() for p in self._peers()]
        return data

    # -- supervisor ------------------------------------------------------

    def _supervisor_loop(self) -> None:
        while not self._stop.is_set():
            try:
                self._supervise()
            except Exception:  # noqa: BLE001 - keep the daemon alive
                log.exception("Auto-connect supervisor failed")
            self._stop.wait(SUPERVISOR_INTERVAL)

    def _supervise(self) -> None:
        if self._stop.is_set():
            return
        now = time.monotonic()
        if now - self._last_reconcile >= 10.0:
            self._last_reconcile = now
            try:
                self.controller.reconcile()
            except Exception:
                log.debug("Periodic reconcile failed", exc_info=True)
        state = self.controller.state
        config = self.controller.config
        if state.connected:
            host, port = state.connected_host, state.connected_port
            if host and port:
                if not tcp_reachable(host, port, timeout=2.0):
                    self._offline_count += 1
                    log.debug(
                        "Peer %s unreachable (%s/%s)",
                        state.connected_name,
                        self._offline_count,
                        OFFLINE_PROBE_LIMIT,
                    )
                    if self._offline_count >= OFFLINE_PROBE_LIMIT:
                        log.info(
                            "Peer %s went offline; restoring local audio", state.connected_name
                        )
                        self.controller.disconnect(
                            restore=config.connection.restore_default,
                            pause_auto=False,
                            notify_user=True,
                        )
                        self._offline_count = 0
                else:
                    self._offline_count = 0
            else:
                self._offline_count = 0
            return

        self._offline_count = 0
        if not config.connection.auto_connect or state.auto_paused:
            return
        peer_id = config.connection.last_peer
        if not peer_id:
            return
        if time.monotonic() < self._next_auto_attempt:
            return

        peer = None
        if self.discovery is not None:
            peer = self.discovery.store.get(peer_id)
        if peer is None:
            peer = self.controller.known_peer(peer_id)
        if peer is None:
            return
        if peer.id == self.config.device.id:
            return
        if not peer.compatible:
            if self._last_peer_version_warned != peer.id:
                self._last_peer_version_warned = peer.id
                log.warning(
                    "Not auto-connecting to %s: protocol %s != %s",
                    peer.display_name,
                    peer.protocol,
                    PROTOCOL_VERSION,
                )
            return
        if not is_private_address(peer.host):
            return
        if not tcp_reachable(peer.host, peer.port, timeout=2.0):
            return
        log.info("Auto-connecting to %s", peer.display_name)
        try:
            self.controller.connect(peer, by="auto", notify_user=True)
            self._reset_auto_backoff()
        except ControllerError as exc:
            self._auto_failures += 1
            delay = min(60.0, 5.0 * (2 ** min(self._auto_failures - 1, 4)))
            self._next_auto_attempt = time.monotonic() + delay
            log.warning("Auto-connect failed: %s (retrying in %.0fs)", exc, delay)

    def _reset_auto_backoff(self) -> None:
        self._auto_failures = 0
        self._next_auto_attempt = 0.0
        self._offline_count = 0


# ---------------------------------------------------------------------------
# process entry point
# ---------------------------------------------------------------------------


def configure_logging(verbose: bool = False, *, to_file: bool = False) -> None:
    level = logging.DEBUG if verbose else logging.INFO
    formatter = logging.Formatter(
        "%(asctime)s %(levelname)-7s %(name)s: %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )
    root = logging.getLogger()
    root.setLevel(level)
    for handler in list(root.handlers):
        root.removeHandler(handler)
    if to_file:
        ensure_dir(log_path().parent)
        handler: logging.Handler = RotatingFileHandler(
            log_path(),
            maxBytes=5 * 1024 * 1024,
            backupCount=3,
            encoding="utf-8",
        )
        try:
            log_path().chmod(0o600)
        except OSError:
            pass
    else:
        handler = logging.StreamHandler()
    handler.setFormatter(formatter)
    root.addHandler(handler)


def _acquire_lock() -> Any | None:
    ensure_dir(runtime_dir())
    handle = open(lock_path(), "a+")
    if fcntl is not None:
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            handle.close()
            return None
    elif sys.platform == "win32":
        try:
            import msvcrt

            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
        except (OSError, ImportError):
            handle.close()
            return None
    handle.seek(0)
    handle.truncate()
    handle.write(f"{os.getpid()}\n")
    handle.flush()
    return handle


def run_daemon(
    *,
    verbose: bool = False,
    config_path: Path | None = None,
    state_path: Path | None = None,
    notify_enabled: bool = True,
    log_to_file: bool = False,
) -> int:
    configure_logging(verbose, to_file=log_to_file)
    lock = _acquire_lock()
    if lock is None:
        log.info("SoundHop daemon is already running")
        return 0

    try:
        config = Config.load(config_path)
    except ConfigError as exc:
        log.error("%s", exc)
        return 2

    if config.device.name:
        pass
    daemon = Daemon(
        config_path=config_path,
        state_path=state_path,
        notify_enabled=notify_enabled,
    )

    def handle_signal(signum: int, _frame: Any) -> None:
        log.info("Received signal %s", signum)
        daemon.request_stop()

    try:
        if hasattr(signal, "SIGTERM"):
            signal.signal(signal.SIGTERM, handle_signal)
        if hasattr(signal, "SIGINT"):
            signal.signal(signal.SIGINT, handle_signal)
        if hasattr(signal, "SIGBREAK"):
            signal.signal(signal.SIGBREAK, handle_signal)
        if hasattr(signal, "SIGHUP"):
            signal.signal(signal.SIGHUP, lambda *_: daemon._reload())  # type: ignore[attr-defined]
    except (ValueError, OSError):
        pass

    try:
        daemon.start()
        daemon.wait()
    except Exception:
        log.exception("SoundHop daemon crashed")
        return 1
    finally:
        daemon.stop()
        try:
            lock.close()
        except Exception:
            pass
    return 0


def ensure_daemon(
    *,
    timeout: float = 6.0,
    verbose: bool = False,
    prefer_service: bool = True,
) -> bool:
    """Start the daemon if needed.  Returns True when the socket answers."""

    if is_running():
        return True
    if os.environ.get("SOUNDHOP_NO_SPAWN"):
        return False

    # If the user installed the user service, use it so systemd owns the
    # process.  Otherwise launch a detached copy that survives this CLI.
    if prefer_service:
        try:
            from .service import unit_installed

            if unit_installed() and _systemctl_user("start", "soundhop.service"):
                if _wait_for_socket(timeout):
                    return True
        except Exception:
            log.debug("Could not start via systemd", exc_info=True)

    env = os.environ.copy()
    package_root = str(Path(__file__).resolve().parent.parent)
    old_pythonpath = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        package_root if not old_pythonpath else package_root + os.pathsep + old_pythonpath
    )
    ensure_dir(log_path().parent)
    try:
        log_file = open(log_path(), "ab")
    except OSError:
        log_file = subprocess.DEVNULL  # type: ignore[assignment]
    appimage = os.environ.get("APPIMAGE")
    if getattr(sys, "frozen", False):
        cmd = [sys.executable, "daemon"]
    elif appimage:
        cmd = [appimage, "daemon"]
    else:
        cmd = [sys.executable, "-m", "soundhop", "daemon"]
    if verbose:
        cmd.append("--verbose")

    popen_kwargs: dict[str, Any] = {
        "stdin": subprocess.DEVNULL,
        "stdout": log_file,
        "stderr": log_file,
        "env": env,
    }
    if sys.platform == "win32":
        if hasattr(subprocess, "CREATE_NO_WINDOW"):
            popen_kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW
    else:
        popen_kwargs["start_new_session"] = True
        popen_kwargs["close_fds"] = True

    try:
        subprocess.Popen(cmd, **popen_kwargs)
    except OSError as exc:
        log.error("Could not start the daemon: %s", exc)
        return False
    finally:
        try:
            if log_file is not subprocess.DEVNULL:
                log_file.close()  # type: ignore[union-attr]
        except Exception:
            pass
    if _wait_for_socket(timeout):
        return True
    log.error("Daemon did not start; see %s", log_path())
    return False


def _wait_for_socket(timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if is_running():
            return True
        time.sleep(0.15)
    return False


def _systemctl_user(*args: str) -> bool:
    try:
        result = subprocess.run(
            ["systemctl", "--user", *args],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def daemon_pid() -> int | None:
    try:
        return int(pid_path().read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return None


def daemon_log_tail(lines: int = 30) -> str:
    try:
        content = log_path().read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return ""
    return "\n".join(content[-lines:])


def _debug_request(command: str) -> dict[str, Any] | None:
    try:
        from .ipc import request

        return request(command)
    except Exception:
        return None
