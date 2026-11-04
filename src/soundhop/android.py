"""Android audio capture integration using scrcpy and adb.

This module provides discovery, device pairing, audio encoder query,
subprocess lifecycle management, and optional standalone installation for
Genymobile scrcpy and the Android Debug Bridge (adb).
"""

from __future__ import annotations

import logging
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import threading
import time
import urllib.request
import zipfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Sequence

from .util import CommandError, Result, data_dir, run

log = logging.getLogger(__name__)

DEFAULT_SCRCPY_VERSION = "5.0.1"
SCRCPY_DOWNLOAD_URL = (
    "https://github.com/Genymobile/scrcpy/releases/download/v{version}/"
    "scrcpy-linux-{arch}-v{version}.tar.gz"
)

SUPPORTED_AUDIO_CODECS = ("opus", "aac", "flac", "raw")
DEFAULT_AUDIO_CODEC = "aac"
DEFAULT_AUDIO_BIT_RATE = 128000
DEFAULT_AUDIO_BUFFER_MS = 50


class AndroidError(RuntimeError):
    """Base error for Android audio capture operations."""


@dataclass
class AdbDevice:
    serial: str
    state: str  # "device", "unauthorized", "offline", "no permissions"
    model: str = ""
    product: str = ""
    device: str = ""
    transport_id: str = ""
    is_wifi: bool = False
    ip: str = ""
    port: int = 5555
    android_version: str = ""
    sdk_version: int = 0
    manufacturer: str = ""

    @property
    def is_ready(self) -> bool:
        return self.state == "device"

    @property
    def major_version(self) -> int:
        if not self.android_version:
            return 0
        m = re.match(r"^(\d+)", self.android_version.strip())
        return int(m.group(1)) if m else 0

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


@dataclass
class AudioEncoderInfo:
    codecs: dict[str, list[str]] = field(default_factory=dict)
    raw_output: str = ""

    @property
    def available_codecs(self) -> list[str]:
        return [c for c in SUPPORTED_AUDIO_CODECS if c in self.codecs or c == "raw"]


@dataclass
class ToolStatus:
    scrcpy_path: Path | None
    scrcpy_version: str
    adb_path: Path | None
    adb_version: str
    is_managed: bool

    @property
    def ready(self) -> bool:
        return self.scrcpy_path is not None and self.adb_path is not None


class AndroidManager:
    """Manages scrcpy and adb discovery, device connectivity, and tool installation."""

    def __init__(self, *, scrcpy_path: str = "", adb_path: str = "") -> None:
        self.custom_scrcpy = scrcpy_path.strip()
        self.custom_adb = adb_path.strip()

    @staticmethod
    def managed_dir() -> Path:
        return data_dir() / "scrcpy"

    def find_scrcpy(self) -> Path | None:
        """Find scrcpy binary, checking config, managed dir, common user apps, and PATH."""
        candidates: list[Path] = []
        if self.custom_scrcpy:
            candidates.append(Path(self.custom_scrcpy).expanduser())

        names = ["scrcpy.exe", "scrcpy"] if sys.platform == "win32" else ["scrcpy"]

        # Managed SoundHop directory
        managed = self.managed_dir()
        for name in names:
            candidates.append(managed / name)
        if managed.is_dir():
            try:
                for name in names:
                    for sub in managed.glob(f"scrcpy*/{name}"):
                        candidates.append(sub)
            except OSError:
                pass

        # Common user application directories (e.g. ~/Applications/scrcpy-*)
        home = Path.home()
        apps_dir = home / "Applications"
        if apps_dir.is_dir():
            try:
                for name in names:
                    for sub in apps_dir.glob(f"scrcpy*/{name}"):
                        candidates.append(sub)
                    candidates.append(apps_dir / "scrcpy" / name)
            except OSError:
                pass

        for name in names:
            candidates.append(home / ".local" / "bin" / name)

        for candidate in candidates:
            if candidate.is_file() and (sys.platform == "win32" or os.access(candidate, os.X_OK)):
                return candidate.resolve()

        for name in names:
            path_bin = shutil.which(name)
            if path_bin:
                return Path(path_bin).resolve()

        return None

    def find_adb(self) -> Path | None:
        """Find adb binary, checking config, scrcpy directory, managed dir, and PATH."""
        candidates: list[Path] = []
        if self.custom_adb:
            candidates.append(Path(self.custom_adb).expanduser())

        adb_names = ["adb.exe", "adb"] if sys.platform == "win32" else ["adb"]

        # If scrcpy was found, check if adb is right next to it (official release bundles it)
        scrcpy_bin = self.find_scrcpy()
        if scrcpy_bin:
            for name in adb_names:
                candidates.append(scrcpy_bin.parent / name)

        managed = self.managed_dir()
        for name in adb_names:
            candidates.append(managed / name)
        if managed.is_dir():
            try:
                for name in adb_names:
                    for sub in managed.glob(f"scrcpy*/{name}"):
                        candidates.append(sub)
            except OSError:
                pass

        home = Path.home()
        apps_dir = home / "Applications"
        if apps_dir.is_dir():
            try:
                for name in adb_names:
                    for sub in apps_dir.glob(f"scrcpy*/{name}"):
                        candidates.append(sub)
            except OSError:
                pass

        for name in adb_names:
            candidates.append(home / ".local" / "bin" / name)

        for candidate in candidates:
            if candidate.is_file() and (sys.platform == "win32" or os.access(candidate, os.X_OK)):
                return candidate.resolve()

        for name in adb_names:
            path_bin = shutil.which(name)
            if path_bin:
                return Path(path_bin).resolve()

        return None

    def get_tool_status(self) -> ToolStatus:
        scrcpy_bin = self.find_scrcpy()
        adb_bin = self.find_adb()
        scrcpy_ver = ""
        adb_ver = ""
        is_managed = False

        if scrcpy_bin:
            is_managed = str(self.managed_dir()) in str(scrcpy_bin)
            try:
                res = run([str(scrcpy_bin), "--version"], timeout=3.0)
                # First line e.g. "scrcpy 5.0.1 <https://...>"
                first_line = (res.stdout or res.stderr).splitlines()[0]
                m = re.search(r"scrcpy\s+([0-9.]+)", first_line)
                scrcpy_ver = m.group(1) if m else first_line
            except Exception as exc:
                log.debug("Error checking scrcpy version: %s", exc)

        if adb_bin:
            try:
                res = run([str(adb_bin), "version"], timeout=3.0)
                m = re.search(r"Android Debug Bridge version\s+([0-9.]+)", res.stdout)
                adb_ver = m.group(1) if m else "installed"
            except Exception as exc:
                log.debug("Error checking adb version: %s", exc)

        return ToolStatus(
            scrcpy_path=scrcpy_bin,
            scrcpy_version=scrcpy_ver,
            adb_path=adb_bin,
            adb_version=adb_ver,
            is_managed=is_managed,
        )

    def install_scrcpy(
        self,
        *,
        version: str = DEFAULT_SCRCPY_VERSION,
        progress_cb: Callable[[int, int], None] | None = None,
    ) -> Path:
        """Download and install official standalone scrcpy into user data directory.

        Downloads the prebuilt tarball from Genymobile's GitHub releases, which contains
        scrcpy, scrcpy-server, bundled adb, ffmpeg/sdl dependencies, and the Apache 2.0 license.
        """
        dest_dir = self.managed_dir()
        dest_dir.mkdir(parents=True, exist_ok=True)
        if sys.platform == "win32":
            url = f"https://github.com/Genymobile/scrcpy/releases/download/v{version}/scrcpy-win64-v{version}.zip"
            archive_path = dest_dir / f"scrcpy-win64-{version}.zip"
        else:
            raw_arch = platform.machine().lower()
            if raw_arch in ("x86_64", "amd64"):
                arch = "x86_64"
            elif raw_arch in ("aarch64", "arm64"):
                arch = "aarch64"
            else:
                raise AndroidError(
                    f"Unsupported architecture for prebuilt scrcpy: {raw_arch}. "
                    "Please install scrcpy using your system package manager."
                )

            url = SCRCPY_DOWNLOAD_URL.format(version=version, arch=arch)
            archive_path = dest_dir / f"scrcpy-{arch}-{version}.tar.gz"

        log.info("Downloading scrcpy %s from %s to %s", version, url, archive_path)
        try:
            req = urllib.request.Request(
                url, headers={"User-Agent": "SoundHop-AndroidManager"}
            )
            with urllib.request.urlopen(req, timeout=30.0) as resp, open(
                archive_path, "wb"
            ) as out_file:
                total = int(resp.headers.get("Content-Length", 0))
                downloaded = 0
                chunk_size = 65536
                while True:
                    chunk = resp.read(chunk_size)
                    if not chunk:
                        break
                    out_file.write(chunk)
                    downloaded += len(chunk)
                    if progress_cb:
                        progress_cb(downloaded, total)
        except Exception as exc:
            if archive_path.exists():
                archive_path.unlink(missing_ok=True)
            raise AndroidError(f"Failed to download scrcpy from {url}: {exc}") from exc

        # Extract archive safely into dest_dir
        try:
            dest_dir_resolved = dest_dir.resolve()
            if sys.platform == "win32":
                with zipfile.ZipFile(archive_path, "r") as z:
                    names = z.namelist()
                    prefix = ""
                    if names and "/" in names[0]:
                        prefix = names[0].split("/")[0] + "/"
                    for member in z.infolist():
                        target_name = (
                            member.filename[len(prefix) :]
                            if prefix and member.filename.startswith(prefix)
                            else member.filename
                        )
                        if not target_name:
                            continue
                        target_path = (dest_dir / target_name).resolve()
                        try:
                            if os.path.commonpath([dest_dir_resolved, target_path]) != str(dest_dir_resolved):
                                continue
                        except ValueError:
                            continue
                        if member.is_dir():
                            target_path.mkdir(parents=True, exist_ok=True)
                        else:
                            target_path.parent.mkdir(parents=True, exist_ok=True)
                            with z.open(member) as src, open(target_path, "wb") as dst:
                                shutil.copyfileobj(src, dst)
            else:
                with tarfile.open(archive_path, "r:gz") as tar:
                    # Extract members directly flattening if single folder
                    members = tar.getmembers()
                    prefix = ""
                    if members and "/" in members[0].name:
                        prefix = members[0].name.split("/")[0] + "/"

                    for m in members:
                        # Strip top-level directory prefix so binaries sit directly in dest_dir
                        target_name = m.name[len(prefix) :] if prefix and m.name.startswith(prefix) else m.name
                        if not target_name:
                            continue
                        # Security check: avoid directory traversal
                        target_path = (dest_dir / target_name).resolve()
                        try:
                            if os.path.commonpath([dest_dir_resolved, target_path]) != str(dest_dir_resolved):
                                continue
                        except ValueError:
                            continue

                        if m.issym():
                            target_path.parent.mkdir(parents=True, exist_ok=True)
                            if os.path.isabs(m.linkname):
                                continue
                            link_target_path = (target_path.parent / m.linkname).resolve()
                            try:
                                if os.path.commonpath([dest_dir_resolved, link_target_path]) != str(dest_dir_resolved):
                                    continue
                            except ValueError:
                                continue
                            if target_path.is_dir() and not target_path.is_symlink():
                                shutil.rmtree(target_path)
                            else:
                                target_path.unlink(missing_ok=True)
                            target_path.symlink_to(m.linkname)
                        elif m.islnk():
                            target_path.parent.mkdir(parents=True, exist_ok=True)
                            link_target_name = (
                                m.linkname[len(prefix) :]
                                if prefix and m.linkname.startswith(prefix)
                                else m.linkname
                            )
                            link_target_path = (dest_dir / link_target_name).resolve()
                            try:
                                if os.path.commonpath([dest_dir_resolved, link_target_path]) != str(dest_dir_resolved):
                                    continue
                            except ValueError:
                                continue
                            try:
                                if target_path.is_dir() and not target_path.is_symlink():
                                    shutil.rmtree(target_path)
                                else:
                                    target_path.unlink(missing_ok=True)
                                os.link(link_target_path, target_path)
                            except OSError as exc:
                                log.debug("Hardlink creation failed for %s -> %s: %s", target_path, link_target_path, exc)
                        elif m.isdir():
                            target_path.mkdir(parents=True, exist_ok=True)
                        else:
                            target_path.parent.mkdir(parents=True, exist_ok=True)
                            extracted = tar.extractfile(m)
                            if extracted:
                                if target_path.is_dir() and not target_path.is_symlink():
                                    shutil.rmtree(target_path)
                                else:
                                    target_path.unlink(missing_ok=True)
                                with open(target_path, "wb") as f:
                                    shutil.copyfileobj(extracted, f)
                                if m.name.endswith("scrcpy") or m.name.endswith("adb") or (m.mode & 0o111):
                                    try:
                                        os.chmod(target_path, 0o755)
                                    except OSError:
                                        pass
        except Exception as exc:
            raise AndroidError(f"Failed to extract scrcpy archive: {exc}") from exc
        finally:
            archive_path.unlink(missing_ok=True)

        scrcpy_bin = dest_dir / "scrcpy"
        if not (scrcpy_bin.is_file() and os.access(scrcpy_bin, os.X_OK)):
            # Check subfolder
            for sub in dest_dir.glob("**/scrcpy"):
                if sub.is_file() and os.access(sub, os.X_OK):
                    scrcpy_bin = sub
                    break

        if not scrcpy_bin.exists():
            raise AndroidError("scrcpy binary not found after extraction")

        return scrcpy_bin

    def _run_adb(
        self,
        args: Sequence[str],
        *,
        serial: str | None = None,
        timeout: float = 10.0,
        check: bool = False,
    ) -> Result:
        adb_bin = self.find_adb()
        if not adb_bin:
            raise AndroidError("adb command not found. Please install adb or run scrcpy install.")
        cmd = [str(adb_bin)]
        if serial:
            cmd.extend(["-s", serial])
        cmd.extend(args)
        return run(cmd, timeout=timeout, check=check)

    def list_devices(self) -> list[AdbDevice]:
        """Query ADB for connected devices (USB and Wi-Fi) with detailed metadata."""
        adb_bin = self.find_adb()
        if not adb_bin:
            return []

        try:
            res = self._run_adb(["devices", "-l"], timeout=5.0)
        except Exception as exc:
            log.warning("adb devices failed: %s", exc)
            return []

        devices: list[AdbDevice] = []
        for line in res.stdout.splitlines():
            line = line.strip()
            if not line or line.startswith("List of devices") or line.startswith("*"):
                continue

            parts = line.split()
            if len(parts) < 2:
                continue

            serial = parts[0]
            state = parts[1]

            model = ""
            product = ""
            device_name = ""
            transport_id = ""

            for item in parts[2:]:
                if item.startswith("model:"):
                    model = item[len("model:") :]
                elif item.startswith("product:"):
                    product = item[len("product:") :]
                elif item.startswith("device:"):
                    device_name = item[len("device:") :]
                elif item.startswith("transport_id:"):
                    transport_id = item[len("transport_id:") :]

            is_wifi = ":" in serial
            ip = serial.split(":")[0] if is_wifi else ""
            port = int(serial.split(":")[1]) if is_wifi and serial.split(":")[1].isdigit() else 5555

            dev = AdbDevice(
                serial=serial,
                state=state,
                model=model,
                product=product,
                device=device_name,
                transport_id=transport_id,
                is_wifi=is_wifi,
                ip=ip,
                port=port,
            )

            # If device is authorized, query Android version and manufacturer
            if state == "device":
                try:
                    self.enrich_device_info(dev)
                except Exception as exc:
                    log.debug("Failed enriching device %s: %s", serial, exc)

            devices.append(dev)

        return devices

    def enrich_device_info(self, device: AdbDevice) -> None:
        """Fetch Android release version, SDK version, and manufacturer via getprop."""
        try:
            res = self._run_adb(
                ["shell", "getprop ro.build.version.release; getprop ro.build.version.sdk; getprop ro.product.manufacturer; getprop ro.product.model"],
                serial=device.serial,
                timeout=4.0,
            )
            lines = [l.strip() for l in res.stdout.splitlines() if l.strip()]
            if len(lines) >= 1 and lines[0]:
                device.android_version = lines[0]
            if len(lines) >= 2 and lines[1].isdigit():
                device.sdk_version = int(lines[1])
            if len(lines) >= 3 and lines[2]:
                device.manufacturer = lines[2]
            if len(lines) >= 4 and lines[3] and not device.model:
                device.model = lines[3]
        except Exception as exc:
            log.debug("Failed getting properties for device %s: %s", device.serial, exc)

    def get_device_wifi_ip(self, serial: str) -> str | None:
        """Detect the device's local Wi-Fi IP address."""
        # Method 1: ip -f inet addr show wlan0
        try:
            res = self._run_adb(
                ["shell", "ip -f inet addr show wlan0"],
                serial=serial,
                timeout=3.0,
            )
            m = re.search(r"inet\s+(\d+\.\d+\.\d+\.\d+)", res.stdout)
            if m:
                return m.group(1)
        except Exception:
            pass

        # Method 1b: ip -f inet addr (any wlan interface)
        try:
            res = self._run_adb(
                ["shell", "ip -f inet addr"],
                serial=serial,
                timeout=3.0,
            )
            for line in res.stdout.splitlines():
                if "inet " in line and re.search(r"\bwlan\d*\b", line):
                    m = re.search(r"inet\s+(\d+\.\d+\.\d+\.\d+)", line)
                    if m and not m.group(1).startswith("127."):
                        return m.group(1)
        except Exception:
            pass

        # Method 2: ip route show
        try:
            res = self._run_adb(
                ["shell", "ip route"],
                serial=serial,
                timeout=3.0,
            )
            for line in res.stdout.splitlines():
                if re.search(r"\bwlan\d*\b", line) and "src" in line:
                    m = re.search(r"src\s+(\d+\.\d+\.\d+\.\d+)", line)
                    if m:
                        return m.group(1)
        except Exception:
            pass

        # Method 3: getprop
        for prop in ("dhcp.wlan0.ipaddress", "dhcp.wlan1.ipaddress"):
            try:
                res = self._run_adb(
                    ["shell", f"getprop {prop}"],
                    serial=serial,
                    timeout=3.0,
                )
                ip = res.stdout.strip()
                if ip and re.match(r"^\d+\.\d+\.\d+\.\d+$", ip):
                    return ip
            except Exception:
                pass

        return None

    def enable_tcpip(self, serial: str, port: int = 5555) -> tuple[bool, str]:
        """Switch a connected USB device to TCP/IP debugging mode on port."""
        try:
            res = self._run_adb(["tcpip", str(port)], serial=serial, timeout=8.0)
            if res.ok or "restarting in TCP mode" in res.stdout:
                return True, f"Device switched to TCP mode on port {port}"
            return False, res.stderr or res.stdout or "Failed switching to TCP mode"
        except Exception as exc:
            return False, str(exc)

    def connect_wifi(self, host: str, port: int = 5555) -> tuple[bool, str]:
        """Connect ADB over Wi-Fi to host:port."""
        target = f"{host}:{port}" if ":" not in host else host
        try:
            res = self._run_adb(["connect", target], timeout=8.0)
            out = (res.stdout + " " + res.stderr).strip()
            if "connected to" in out or "already connected" in out:
                return True, f"Connected to {target}"
            return False, out or f"Failed to connect to {target}"
        except Exception as exc:
            return False, str(exc)

    def disconnect_wifi(self, target: str | None = None) -> tuple[bool, str]:
        """Disconnect ADB from Wi-Fi target, or all if target is None."""
        args = ["disconnect"]
        if target:
            args.append(target)
        try:
            res = self._run_adb(args, timeout=5.0)
            return True, (res.stdout or "Disconnected").strip()
        except Exception as exc:
            return False, str(exc)

    def list_encoders(self, serial: str | None = None) -> AudioEncoderInfo:
        """Query scrcpy for supported audio encoders on the target device."""
        scrcpy_bin = self.find_scrcpy()
        if not scrcpy_bin:
            raise AndroidError("scrcpy is not installed.")

        cmd = [str(scrcpy_bin), "--list-encoders"]
        if serial:
            cmd.extend(["-s", serial])

        try:
            res = run(cmd, timeout=10.0)
            output = res.stdout + "\n" + res.stderr
        except Exception as exc:
            log.warning("scrcpy --list-encoders failed: %s", exc)
            return AudioEncoderInfo(codecs={"opus": [], "aac": [], "flac": [], "raw": []})

        codecs: dict[str, list[str]] = {}
        in_audio = False
        for line in output.splitlines():
            line_str = line.strip()
            if "List of audio encoders:" in line_str:
                in_audio = True
                continue
            if in_audio:
                if line_str.startswith("[server]") or not line_str:
                    continue
                # Format e.g. "--audio-codec=opus --audio-encoder=c2.android.opus.encoder (sw)"
                m_codec = re.search(r"--audio-codec=([a-zA-Z0-9_-]+)", line_str)
                m_enc = re.search(r"--audio-encoder=([a-zA-Z0-9._-]+)", line_str)
                if m_codec:
                    cname = m_codec.group(1).lower()
                    enc_name = m_enc.group(1) if m_enc else ""
                    if cname not in codecs:
                        codecs[cname] = []
                    if enc_name and enc_name not in codecs[cname]:
                        codecs[cname].append(enc_name)

        if not codecs:
            # Fallback standard codecs supported on modern Android
            codecs = {"opus": [], "aac": [], "flac": [], "raw": []}

        return AudioEncoderInfo(codecs=codecs, raw_output=output)

    def build_capture_args(
        self,
        serial: str,
        *,
        codec: str = DEFAULT_AUDIO_CODEC,
        source: str | None = None,
        buffer_ms: int | None = DEFAULT_AUDIO_BUFFER_MS,
        bit_rate: int | None = DEFAULT_AUDIO_BIT_RATE,
        duplicate: bool = False,
        extra_args: Sequence[str] | None = None,
    ) -> list[str]:
        """Construct command arguments for headless audio-only capture."""
        scrcpy_bin = self.find_scrcpy()
        if not scrcpy_bin:
            raise AndroidError("scrcpy binary not found.")

        cmd = [
            str(scrcpy_bin),
            "-s",
            serial,
            "--no-video",
            "--no-window",
            "--no-control",
            "--require-audio",
        ]

        codec_clean = codec.strip().lower()
        if codec_clean in SUPPORTED_AUDIO_CODECS:
            cmd.append(f"--audio-codec={codec_clean}")

        if source:
            source_clean = source.strip().lower()
            cmd.append(f"--audio-source={source_clean}")

        if buffer_ms and buffer_ms > 0:
            cmd.append(f"--audio-buffer={int(buffer_ms)}")

        if bit_rate and bit_rate > 0:
            cmd.append(f"--audio-bit-rate={int(bit_rate)}")

        if duplicate:
            cmd.append("--audio-dup")

        if extra_args:
            cmd.extend(extra_args)

        return cmd


class AndroidCaptureSession:
    """Manages an active background scrcpy audio capture process."""

    def __init__(self, manager: AndroidManager) -> None:
        self.manager = manager
        self._proc: subprocess.Popen[str] | None = None
        self._lock = threading.RLock()
        self._serial: str = ""
        self._device_name: str = ""
        self._codec: str = ""
        self._source: str = ""
        self._started_at: float = 0.0
        self._log_lines: list[str] = []
        self._monitor_thread: threading.Thread | None = None
        self._last_error: str = ""

    @property
    def is_active(self) -> bool:
        with self._lock:
            if self._proc is None:
                return False
            return self._proc.poll() is None

    def status(self) -> dict[str, Any]:
        with self._lock:
            active = self._proc is not None and self._proc.poll() is None
            elapsed = time.time() - self._started_at if active else 0.0
            return {
                "active": active,
                "serial": self._serial if active else "",
                "device_name": self._device_name if active else "",
                "codec": self._codec if active else "",
                "source": self._source if active else "",
                "pid": self._proc.pid if (active and self._proc) else 0,
                "started_at": self._started_at if active else 0.0,
                "uptime_s": elapsed,
                "last_error": self._last_error,
                "recent_logs": list(self._log_lines[-20:]),
            }

    def start(
        self,
        serial: str,
        *,
        device_name: str = "",
        codec: str = DEFAULT_AUDIO_CODEC,
        source: str | None = None,
        buffer_ms: int | None = DEFAULT_AUDIO_BUFFER_MS,
        bit_rate: int | None = DEFAULT_AUDIO_BIT_RATE,
        duplicate: bool = False,
        extra_args: Sequence[str] | None = None,
    ) -> dict[str, Any]:
        """Start capturing audio from the specified Android device."""
        with self._lock:
            if self._proc is not None and self._proc.poll() is None:
                raise AndroidError(
                    f"Audio capture is already active on {self._device_name or self._serial} (PID {self._proc.pid}). "
                    "Stop the current capture before starting another."
                )

            args = self.manager.build_capture_args(
                serial,
                codec=codec,
                source=source,
                buffer_ms=buffer_ms,
                bit_rate=bit_rate,
                duplicate=duplicate,
                extra_args=extra_args,
            )

            log.info("Starting scrcpy audio capture: %s", " ".join(args))
            popen_kwargs: dict[str, Any] = {
                "stdout": subprocess.PIPE,
                "stderr": subprocess.STDOUT,
                "text": True,
                "bufsize": 1,
            }
            if sys.platform == "win32" and hasattr(subprocess, "CREATE_NO_WINDOW"):
                popen_kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW

            try:
                self._proc = subprocess.Popen(args, **popen_kwargs)
            except Exception as exc:
                self._last_error = str(exc)
                raise AndroidError(f"Failed to start scrcpy: {exc}") from exc

            self._serial = serial
            self._device_name = device_name or serial
            self._codec = codec
            self._source = source or "output"
            self._started_at = time.time()
            self._log_lines.clear()
            self._last_error = ""

            self._monitor_thread = threading.Thread(
                target=self._reader_loop,
                name="soundhop-scrcpy-reader",
                daemon=True,
            )
            self._monitor_thread.start()

            # Wait briefly to detect fast exits (e.g. invalid arguments or unauthorized device)
            time.sleep(0.4)
            ret = self._proc.poll()
            if ret is not None:
                err_msg = "\n".join(self._log_lines[-10:]) or f"Process exited with code {ret}"
                self._last_error = err_msg
                self._proc = None
                raise AndroidError(f"scrcpy failed to start:\n{err_msg}")

            return self.status()

    def _reader_loop(self) -> None:
        proc = self._proc
        if not proc or not proc.stdout:
            return
        try:
            for raw_line in iter(proc.stdout.readline, ""):
                line = raw_line.rstrip()
                if not line:
                    continue
                with self._lock:
                    self._log_lines.append(line)
                    if len(self._log_lines) > 200:
                        self._log_lines.pop(0)
                if "ERROR:" in line or "WARN:" in line:
                    log.warning("[scrcpy] %s", line)
                else:
                    log.debug("[scrcpy] %s", line)
        except Exception as exc:
            log.debug("scrcpy reader error: %s", exc)
        finally:
            with self._lock:
                if self._proc is proc:
                    rc = proc.poll()
                    if rc is not None and rc != 0 and not self._last_error:
                        self._last_error = f"scrcpy terminated with code {rc}"
                    log.info("scrcpy capture session ended (exit code: %s)", rc)

    def stop(self) -> dict[str, Any]:
        """Stop running scrcpy audio capture process."""
        with self._lock:
            proc = self._proc
            if proc is None:
                return self.status()

            if proc.poll() is None:
                log.info("Stopping scrcpy process (PID %s)", proc.pid)
                try:
                    proc.terminate()
                    try:
                        proc.wait(timeout=2.0)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=1.0)
                except Exception as exc:
                    log.warning("Error stopping scrcpy process: %s", exc)

            self._proc = None
            return self.status()
