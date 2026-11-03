"""Small utilities shared by the CLI, daemon and GUI."""

from __future__ import annotations

import ipaddress
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping, Sequence

from .constants import APP_SLUG

# ---------------------------------------------------------------------------
# process helpers
# ---------------------------------------------------------------------------


class ToolMissing(RuntimeError):
    """Raised when a required external command is not installed."""

    def __init__(self, command: str) -> None:
        super().__init__(f"Required command not found: {command}")
        self.command = command


@dataclass
class Result:
    args: Sequence[str]
    returncode: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.returncode == 0


def run(
    args: Sequence[str] | str,
    *,
    check: bool = False,
    timeout: float = 15.0,
    env: Mapping[str, str] | None = None,
    stdin: str | None = None,
) -> Result:
    """Run a command quietly and return its output.

    ``pactl`` and other system tools are invoked through this helper so tests
    can inject a fake runner.
    """

    if isinstance(args, str):
        args = [args]
    args = [str(a) for a in args]
    try:
        proc = subprocess.run(
            args,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=dict(env) if env is not None else None,
            input=stdin,
        )
    except FileNotFoundError as exc:
        raise ToolMissing(args[0]) from exc
    result = Result(args, proc.returncode, proc.stdout, proc.stderr)
    if check and result.returncode != 0:
        raise CommandError(result)
    return result


class CommandError(RuntimeError):
    def __init__(self, result: Result) -> None:
        command = " ".join(result.args)
        detail = (result.stderr or result.stdout).strip()
        super().__init__(f"Command failed ({result.returncode}): {command}\n{detail}")
        self.result = result


def command_exists(name: str) -> bool:
    return shutil.which(name) is not None


def process_running(name: str) -> bool:
    """Check if a process with the given name is currently running."""
    if shutil.which("pgrep"):
        try:
            return (
                subprocess.call(
                    ["pgrep", "-x", name],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                == 0
            )
        except OSError:
            pass
    proc = Path("/proc")
    if proc.is_dir():
        try:
            for entry in proc.iterdir():
                if entry.name.isdigit():
                    comm = entry / "comm"
                    if comm.is_file() and comm.read_text(errors="ignore").strip() == name:
                        return True
        except (OSError, PermissionError):
            pass
    return False


# ---------------------------------------------------------------------------
# XDG paths
# ---------------------------------------------------------------------------


def _home() -> Path:
    return Path(os.environ.get("HOME") or Path.home())


def config_dir() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME")
    return (Path(base) if base else _home() / ".config") / APP_SLUG


def state_dir() -> Path:
    base = os.environ.get("XDG_STATE_HOME")
    return (Path(base) if base else _home() / ".local" / "state") / APP_SLUG


def data_dir() -> Path:
    base = os.environ.get("XDG_DATA_HOME")
    return (Path(base) if base else _home() / ".local" / "share") / APP_SLUG


def runtime_dir() -> Path:
    base = os.environ.get("XDG_RUNTIME_DIR")
    if base:
        return Path(base) / APP_SLUG
    return Path(tempfile.gettempdir()) / f"{APP_SLUG}-{os.getuid()}"


def ensure_dir(path: Path, mode: int = 0o700) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    try:
        path.chmod(mode)
    except OSError:
        pass
    return path


def config_path() -> Path:
    return config_dir() / "config.toml"


def state_path() -> Path:
    return state_dir() / "state.json"


def log_path() -> Path:
    return state_dir() / "soundhop.log"


def socket_path() -> Path:
    return runtime_dir() / "control.sock"


def lock_path() -> Path:
    return runtime_dir() / "daemon.lock"


def pid_path() -> Path:
    return runtime_dir() / "daemon.pid"


def atomic_write(path: Path, text: str, mode: int = 0o600) -> None:
    ensure_dir(path.parent)
    unique_suffix = f"{os.getpid()}.{threading.get_ident()}.{uuid.uuid4().hex}"
    tmp = path.with_name(f".{path.name}.{unique_suffix}.tmp")
    try:
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_TRUNC
        fd = os.open(tmp, flags, mode)
        try:
            try:
                os.fchmod(fd, mode)
            except OSError:
                pass
            with open(fd, "w", encoding="utf-8") as f:
                f.write(text)
        except Exception:
            try:
                os.close(fd)
            except OSError:
                pass
            raise
        try:
            tmp.chmod(mode)
        except OSError:
            pass
        os.replace(tmp, path)
    finally:
        tmp.unlink(missing_ok=True)


# ---------------------------------------------------------------------------
# formatting
# ---------------------------------------------------------------------------


def supports_color() -> bool:
    if os.environ.get("NO_COLOR"):
        return False
    if os.environ.get("FORCE_COLOR"):
        return True
    if os.environ.get("TERM", "") == "dumb":
        return False
    return bool(getattr(sys.stdout, "isatty", lambda: False)())


_CODES = {
    "bold": "1",
    "dim": "2",
    "red": "31",
    "green": "32",
    "yellow": "33",
    "blue": "34",
    "magenta": "35",
    "cyan": "36",
    "gray": "90",
}


def style(text: str, *names: str) -> str:
    if not supports_color() or not names:
        return text
    codes = ";".join(_CODES[n] for n in names if n in _CODES)
    return f"\033[{codes}m{text}\033[0m" if codes else text


def format_age(seconds: float | None) -> str:
    if seconds is None:
        return "never"
    if seconds < 5:
        return "just now"
    if seconds < 60:
        return f"{int(seconds)}s ago"
    if seconds < 3600:
        return f"{int(seconds / 60)}m ago"
    if seconds < 86400:
        return f"{int(seconds / 3600)}h ago"
    return f"{int(seconds / 86400)}d ago"


def format_duration(seconds: float | None) -> str:
    if seconds is None:
        return "-"
    seconds = max(0, int(seconds))
    hours, rem = divmod(seconds, 3600)
    minutes, secs = divmod(rem, 60)
    if hours:
        return f"{hours}h {minutes:02d}m"
    if minutes:
        return f"{minutes}m {secs:02d}s"
    return f"{secs}s"


def human_bytes(num: int) -> str:
    value = float(num)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if value < 1024 or unit == "GiB":
            return f"{value:.0f} {unit}" if unit == "B" else f"{value:.1f} {unit}"
        value /= 1024
    return f"{value:.1f} GiB"


# ---------------------------------------------------------------------------
# names and network addresses
# ---------------------------------------------------------------------------


def slugify(value: str, *, max_length: int = 40) -> str:
    normalized = unicodedata.normalize("NFKD", value)
    ascii_value = normalized.encode("ascii", "ignore").decode("ascii")
    cleaned = re.sub(r"[^A-Za-z0-9]+", "_", ascii_value).strip("_").lower()
    if not cleaned:
        cleaned = "device"
    return cleaned[:max_length].strip("_") or "device"


def sanitize_sink_name(value: str, *, max_length: int = 64) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9_.-]+", "_", value).strip("_")
    return (cleaned or "device")[:max_length]


def hostname_name() -> str:
    try:
        name = socket.gethostname().split(".")[0].strip()
    except OSError:
        name = ""
    return name or "Linux device"


def new_device_id() -> str:
    return uuid.uuid4().hex


def short_id(value: str, length: int = 6) -> str:
    return re.sub(r"[^A-Za-z0-9]", "", value)[:length] or "000000"


def tcp_server_arg(host: str, port: int) -> str:
    """Return the ``server=`` value expected by module-tunnel-*.

    IPv6 addresses must be wrapped in brackets, and a zone suffix (``%eth0``)
    is not valid in this URI form.
    """

    host = host.strip()
    if host.startswith("[") and host.endswith("]"):
        host = host[1:-1]
    if host.lower() in {"localhost", "::1"} and host == "::1":
        return f"tcp:[{host}]:{int(port)}"
    if ":" in host:
        host = host.split("%", 1)[0]
        return f"tcp:[{host}]:{int(port)}"
    return f"tcp:{host}:{int(port)}"


def split_host_port(value: str, default_port: int) -> tuple[str, int]:
    """Parse ``host``, ``host:port``, ``[v6]:port`` or a bare IPv6 address."""

    value = value.strip()
    if not value:
        return value, default_port
    if value.startswith("["):
        end = value.find("]")
        if end != -1:
            host = value[1:end]
            rest = value[end + 1 :]
            if rest.startswith(":") and rest[1:].isdigit():
                return host, int(rest[1:])
            return host, default_port
    if value.count(":") == 1:
        host, maybe_port = value.rsplit(":", 1)
        if maybe_port.isdigit():
            return host, int(maybe_port)
    return value, default_port


def local_addresses(*, include_loopback: bool = False) -> list[str]:
    addresses: list[str] = []
    if command_exists("ip"):
        try:
            result = run(["ip", "-o", "addr", "show"], timeout=2.0)
            for line in result.stdout.splitlines():
                match = re.search(r"\binet6?\s+([0-9a-fA-F:.]+)/\d+", line)
                if not match:
                    continue
                addr = match.group(1)
                try:
                    ip = ipaddress.ip_address(addr)
                except ValueError:
                    continue
                if ip.is_loopback and not include_loopback:
                    continue
                if ip.is_link_local:
                    continue
                if addr not in addresses:
                    addresses.append(addr)
        except (ToolMissing, CommandError, OSError):
            pass
    if not addresses:
        try:
            for info in socket.getaddrinfo(socket.gethostname(), None):
                addr = info[4][0]
                try:
                    ip = ipaddress.ip_address(addr)
                except ValueError:
                    continue
                if ip.is_loopback and not include_loopback:
                    continue
                if ip.is_link_local:
                    continue
                if addr not in addresses:
                    addresses.append(addr)
        except OSError:
            pass
    if include_loopback and "127.0.0.1" not in addresses:
        addresses.append("127.0.0.1")
    return addresses


_broadcast_cache: tuple[float, list[str]] = (0.0, [])
_broadcast_cache_lock = threading.Lock()
_BROADCAST_CACHE_TTL = 30.0


def invalidate_broadcast_cache() -> None:
    global _broadcast_cache
    with _broadcast_cache_lock:
        _broadcast_cache = (0.0, [])


def broadcast_addresses(*, include_loopback: bool = True) -> list[str]:
    """Best-effort list of broadcast destinations for discovery probes."""

    global _broadcast_cache
    now_mono = time.monotonic()
    with _broadcast_cache_lock:
        cache_time, cached_targets = _broadcast_cache
        if now_mono - cache_time < _BROADCAST_CACHE_TTL and cached_targets:
            discovered = list(cached_targets)
        else:
            discovered = ["255.255.255.255"]
            if command_exists("ip"):
                try:
                    result = run(["ip", "-o", "-4", "addr", "show"], timeout=2.0)
                    for line in result.stdout.splitlines():
                        match = re.search(r"\binet\s+(\d+\.\d+\.\d+\.\d+)/(\d+)", line)
                        if not match:
                            continue
                        network = ipaddress.ip_network(
                            f"{match.group(1)}/{match.group(2)}", strict=False
                        )
                        if network.is_loopback:
                            continue
                        broadcast = str(network.broadcast_address)
                        if broadcast not in discovered:
                            discovered.append(broadcast)
                except (ToolMissing, CommandError, OSError, ValueError):
                    pass
            _broadcast_cache = (now_mono, list(discovered))

    targets = list(discovered)
    if include_loopback and "127.0.0.1" not in targets:
        targets.append("127.0.0.1")
    elif not include_loopback and "127.0.0.1" in targets:
        targets.remove("127.0.0.1")
    return targets


def tcp_reachable(host: str, port: int, timeout: float = 1.5) -> bool:
    try:
        with socket.create_connection((host, int(port)), timeout=timeout):
            return True
    except OSError:
        return False


def is_private_address(value: str) -> bool:
    val = str(value).strip()
    if not val:
        return False
    if val.startswith("[") and val.endswith("]"):
        val = val[1:-1]
    if val.lower() == "localhost":
        return True
    host_clean = val.split("%", 1)[0]
    try:
        ip = ipaddress.ip_address(host_clean)
        return ip.is_private or ip.is_loopback or ip.is_link_local
    except ValueError:
        pass
    try:
        infos = socket.getaddrinfo(val, None, socket.AF_UNSPEC, socket.SOCK_STREAM)
        if not infos:
            return False
        for info in infos:
            ip = ipaddress.ip_address(info[4][0])
            if not (ip.is_private or ip.is_loopback or ip.is_link_local):
                return False
        return True
    except (socket.gaierror, OSError, ValueError):
        return False


# ---------------------------------------------------------------------------
# desktop integration
# ---------------------------------------------------------------------------


def notify(title: str, body: str, *, urgency: str = "normal") -> None:
    """Send a desktop notification when a notifier is available."""

    if os.environ.get("SOUNDHOP_NO_NOTIFY"):
        return
    if command_exists("notify-send"):
        try:
            run(
                [
                    "notify-send",
                    "--app-name=SoundHop",
                    "--icon=soundhop",
                    f"--urgency={urgency}",
                    "--expire-time=4000",
                    title,
                    body,
                ],
                timeout=3.0,
            )
            return
        except (ToolMissing, CommandError, OSError):
            pass
    if command_exists("kdialog"):
        try:
            run(["kdialog", "--title", title, "--passivepopup", body, "5"], timeout=3.0)
        except (ToolMissing, CommandError, OSError):
            pass


def open_path(path: Path) -> None:
    """Open a file with the desktop's default handler, if possible."""

    if command_exists("xdg-open"):
        try:
            run(["xdg-open", str(path)], timeout=3.0)
            return
        except (ToolMissing, CommandError, OSError):
            pass


def now() -> float:
    return time.time()


def dedupe(values: Iterable[str]) -> list[str]:
    seen: set[str] = set()
    result: list[str] = []
    for value in values:
        if value and value not in seen:
            seen.add(value)
            result.append(value)
    return result
