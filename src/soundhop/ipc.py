"""Small JSON-over-Unix-socket control channel for the daemon."""

from __future__ import annotations

import json
import logging
import os
import socket
import threading
from pathlib import Path
from typing import Any, Callable

from .util import ensure_dir, socket_path

log = logging.getLogger(__name__)

Handler = Callable[[dict[str, Any]], dict[str, Any]]
_MAX_REQUEST = 1024 * 1024


class DaemonNotRunning(RuntimeError):
    pass


class IpcError(RuntimeError):
    pass


class IpcServer:
    def __init__(self, handler: Handler, *, path: Path | None = None) -> None:
        self.handler = handler
        self.path = path or socket_path()
        self._sock: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._is_tcp = False

    def start(self) -> None:
        ensure_dir(self.path.parent)
        try:
            if self.path.exists() or (hasattr(self.path, "is_socket") and self.path.is_socket()):
                self.path.unlink()
        except OSError:
            pass

        port_file = self.path.with_suffix(".port")
        try:
            if port_file.exists():
                port_file.unlink()
        except OSError:
            pass

        sock: socket.socket | None = None
        use_unix = hasattr(socket, "AF_UNIX")
        if use_unix:
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.bind(str(self.path))
                if hasattr(os, "chmod"):
                    try:
                        os.chmod(self.path, 0o600)
                    except OSError:
                        pass
                self._is_tcp = False
            except OSError:
                if sock:
                    sock.close()
                sock = None

        if sock is None:
            # Fallback to localhost TCP for Windows builds without AF_UNIX support
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(("127.0.0.1", 0))
            bound_port = sock.getsockname()[1]
            port_file.write_text(f"{bound_port}\n", encoding="utf-8")
            self._is_tcp = True

        sock.listen(16)
        sock.settimeout(0.5)
        self._sock = sock
        self._stop.clear()
        self._thread = threading.Thread(
            target=self._accept_loop, name="soundhop-ipc", daemon=True
        )
        self._thread.start()

    def _accept_loop(self) -> None:
        while not self._stop.is_set():
            sock = self._sock
            if sock is None:
                break
            try:
                conn, _ = sock.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            thread = threading.Thread(
                target=self._serve_client,
                args=(conn,),
                name="soundhop-ipc-client",
                daemon=True,
            )
            thread.start()

    def _serve_client(self, conn: socket.socket) -> None:
        try:
            conn.settimeout(10.0)
            data = b""
            while b"\n" not in data and len(data) < _MAX_REQUEST:
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
            if not data:
                return
            line = data.split(b"\n", 1)[0]
            try:
                request = json.loads(line.decode("utf-8"))
            except (UnicodeDecodeError, ValueError) as exc:
                response = {"ok": False, "error": f"Invalid request: {exc}"}
            else:
                if not isinstance(request, dict):
                    response = {"ok": False, "error": "Request must be a JSON object"}
                else:
                    try:
                        response = self.handler(request)
                        if "ok" not in response:
                            response = {"ok": True, **response}
                    except Exception as exc:  # noqa: BLE001 - report to caller
                        log.debug("IPC handler failed", exc_info=True)
                        response = {"ok": False, "error": str(exc)}
            conn.sendall(json.dumps(response).encode("utf-8") + b"\n")
        except (OSError, ValueError):
            log.debug("IPC client disconnected", exc_info=True)
        finally:
            try:
                conn.close()
            except OSError:
                pass

    def stop(self) -> None:
        self._stop.set()
        sock = self._sock
        self._sock = None
        if sock is not None:
            try:
                sock.close()
            except OSError:
                pass
        if self._thread is not None:
            self._thread.join(timeout=2.0)
            self._thread = None
        try:
            if self.path.exists():
                self.path.unlink()
        except OSError:
            pass
        try:
            port_file = self.path.with_suffix(".port")
            if port_file.exists():
                port_file.unlink()
        except OSError:
            pass


def request(
    command: str,
    *,
    path: Path | None = None,
    timeout: float = 5.0,
    **payload: Any,
) -> dict[str, Any]:
    """Send one command to a running daemon."""

    target = path or socket_path()
    request_data = {"cmd": command, **payload}
    port_file = target.with_suffix(".port")

    sock: socket.socket | None = None
    if port_file.exists():
        try:
            port_str = port_file.read_text(encoding="utf-8").strip()
            if port_str.isdigit():
                sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                sock.settimeout(timeout)
                sock.connect(("127.0.0.1", int(port_str)))
        except (OSError, ValueError):
            if sock:
                sock.close()
            sock = None

    if sock is None:
        if not hasattr(socket, "AF_UNIX"):
            raise DaemonNotRunning("SoundHop daemon is not running")
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(timeout)
        try:
            sock.connect(str(target))
        except (FileNotFoundError, ConnectionRefusedError) as exc:
            sock.close()
            raise DaemonNotRunning("SoundHop daemon is not running") from exc
        except OSError as exc:
            sock.close()
            raise DaemonNotRunning(f"SoundHop daemon is not reachable: {exc}") from exc

    try:
        sock.sendall(json.dumps(request_data).encode("utf-8") + b"\n")
        data = b""
        while b"\n" not in data and len(data) < _MAX_REQUEST:
            chunk = sock.recv(65536)
            if not chunk:
                break
            data += chunk
    except socket.timeout as exc:
        raise IpcError("The SoundHop daemon did not respond in time") from exc
    except OSError as exc:
        raise DaemonNotRunning(f"SoundHop daemon is not reachable: {exc}") from exc
    finally:
        try:
            sock.close()
        except OSError:
            pass
    if not data:
        raise IpcError("Empty response from the SoundHop daemon")
    try:
        response = json.loads(data.split(b"\n", 1)[0].decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise IpcError(f"Malformed response from the daemon: {exc}") from exc
    if not isinstance(response, dict):
        raise IpcError("Malformed response from the daemon")
    if not response.get("ok", False):
        raise IpcError(str(response.get("error", "Unknown daemon error")))
    return response


def is_running(path: Path | None = None) -> bool:
    try:
        response = request("ping", path=path, timeout=0.5)
    except (DaemonNotRunning, IpcError, OSError):
        return False
    return bool(response.get("pong"))
