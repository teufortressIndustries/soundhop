"""Peer discovery: zero-dependency UDP broadcast plus optional mDNS.

SoundHop uses a tiny JSON beacon on UDP 47777.  It deliberately does not rely
on Avahi, D-Bus, or broadcast being enabled on every interface: both machines
run the same small protocol, and ``probe``/``offer`` messages make ``soundhop
connect`` feel instant.  When python-zeroconf is installed the daemon also
publishes an ``_soundhop._tcp`` service, which helps on networks that filter
limited broadcast but allow multicast.
"""

from __future__ import annotations

import json
import logging
import select
import socket
import sys
import threading
import time
from collections.abc import Callable, Iterable
from dataclasses import asdict, dataclass, field
from typing import Any

from .constants import (
    BEACON_INTERVAL,
    MDNS_SERVICE_TYPE,
    PEER_TIMEOUT,
    PROTOCOL_VERSION,
    VERSION,
)
from .util import (
    broadcast_addresses,
    hostname_name,
    is_private_address,
    local_addresses,
    now,
    slugify,
)

log = logging.getLogger(__name__)

MAGIC = "soundhop"
DiscoveryCallback = Callable[["Peer"], None]
RemovalCallback = Callable[[str], None]


class DiscoveryError(RuntimeError):
    pass


class MdnsUnavailable(DiscoveryError):
    pass


# ---------------------------------------------------------------------------
# peer model
# ---------------------------------------------------------------------------


@dataclass
class Peer:
    id: str
    name: str
    host: str
    port: int
    version: str = ""
    protocol: int = PROTOCOL_VERSION
    security: str = "trusted-lan"
    platform: str = "linux"
    last_seen: float = field(default_factory=now)
    source: str = "udp"

    @property
    def age(self) -> float:
        return max(0.0, now() - self.last_seen)

    @property
    def compatible(self) -> bool:
        return self.protocol == PROTOCOL_VERSION

    @property
    def display_name(self) -> str:
        return self.name or self.id[:8] or self.host

    def to_dict(self) -> dict[str, Any]:
        data = asdict(self)
        data["age"] = round(self.age, 1)
        data["compatible"] = self.compatible
        return data


class PeerStore:
    """Thread-safe collection of recently seen peers."""

    def __init__(self, timeout: float = PEER_TIMEOUT) -> None:
        self.timeout = timeout
        self._peers: dict[str, Peer] = {}
        self._lock = threading.RLock()
        self.on_peer: DiscoveryCallback | None = None
        self.on_remove: RemovalCallback | None = None

    def update(self, peer: Peer) -> bool:
        if not peer.id or not is_private_address(peer.host):
            return False
        changed = False
        with self._lock:
            previous = self._peers.get(peer.id)
            if previous is None:
                changed = True
            elif (
                previous.host != peer.host
                or previous.port != peer.port
                or previous.name != peer.name
                or previous.source != peer.source
            ):
                changed = True
            # Keep the freshest timestamp.  A probe reply from mDNS should not
            # make a UDP peer look older.
            if previous is not None and previous.last_seen > peer.last_seen:
                peer.last_seen = previous.last_seen
            self._peers[peer.id] = peer
        if changed and self.on_peer is not None:
            try:
                self.on_peer(peer)
            except Exception:  # pragma: no cover - callbacks must not kill discovery
                log.exception("Peer callback failed")
        return changed

    def remove(self, peer_id: str) -> None:
        with self._lock:
            existed = self._peers.pop(peer_id, None) is not None
        if existed and self.on_remove is not None:
            try:
                self.on_remove(peer_id)
            except Exception:  # pragma: no cover
                log.exception("Peer removal callback failed")

    def get(self, peer_id: str) -> Peer | None:
        with self._lock:
            return self._peers.get(peer_id)

    def snapshot(self, *, prune: bool = True) -> list[Peer]:
        if prune:
            self.prune()
        with self._lock:
            peers = list(self._peers.values())
        return sorted(peers, key=lambda p: (not p.compatible, p.display_name.lower()))

    def prune(self) -> list[str]:
        cutoff = now() - self.timeout
        with self._lock:
            stale = [pid for pid, p in self._peers.items() if p.last_seen < cutoff]
            for pid in stale:
                self._peers.pop(pid, None)
        if self.on_remove is not None:
            for pid in stale:
                try:
                    self.on_remove(pid)
                except Exception:
                    log.exception("Peer removal callback failed")
        return stale


# ---------------------------------------------------------------------------
# UDP beacon protocol
# ---------------------------------------------------------------------------


def make_beacon(
    device_id: str,
    name: str,
    port: int,
    *,
    security: str = "trusted-lan",
    message_type: str = "offer",
    platform: str | None = None,
) -> bytes:
    plat = platform or ("windows" if sys.platform == "win32" else "linux")
    payload = {
        "magic": MAGIC,
        "proto": PROTOCOL_VERSION,
        "type": message_type,
        "id": device_id,
        "name": name,
        "port": int(port),
        "ver": VERSION,
        "security": security,
        "platform": plat,
    }
    return json.dumps(payload, separators=(",", ":")).encode("utf-8")


def parse_beacon(
    data: bytes, *, host: str, source: str = "udp"
) -> tuple[dict[str, Any] | None, Peer | None]:
    if not is_private_address(host):
        return None, None
    try:
        payload = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return None, None
    if not isinstance(payload, dict) or payload.get("magic") != MAGIC:
        return None, None
    message_type = payload.get("type", "offer")
    if message_type != "offer":
        return payload, None
    if isinstance(payload.get("port"), bool) or isinstance(payload.get("proto"), bool):
        return payload, None
    raw_id = str(payload.get("id", "")).strip()
    if not raw_id or len(raw_id) > 128:
        return payload, None
    try:
        peer = Peer(
            id=raw_id,
            name=str(payload.get("name") or hostname_name())[:100],
            host=host,
            port=int(payload["port"]),
            version=str(payload.get("ver", "")),
            protocol=int(payload.get("proto", PROTOCOL_VERSION)),
            security=str(payload.get("security", "trusted-lan")),
            platform=str(payload.get("platform", "linux")),
            source=source,
        )
    except (KeyError, TypeError, ValueError):
        return payload, None
    if not (1 <= peer.port <= 65535):
        return None, None
    return payload, peer


class UdpBeaconService:
    """Advertise this device and answer discovery probes."""

    def __init__(
        self,
        *,
        device_id: str,
        name: str,
        audio_port: int,
        discovery_port: int,
        security: str = "trusted-lan",
        interval: float = BEACON_INTERVAL,
        store: PeerStore | None = None,
        bind_host: str = "",
        targets: Iterable[str] | None = None,
    ) -> None:
        self.device_id = device_id
        self.name = name
        self.audio_port = audio_port
        self.discovery_port = discovery_port
        self.security = security
        self.interval = max(1.0, interval)
        self.store = store or PeerStore()
        self.bind_host = bind_host
        self._targets = list(targets) if targets is not None else None
        self._sock: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._advertising = True
        self._last_send = 0.0

    def start(self) -> None:
        with self._lock:
            if self._sock is not None:
                return
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
            try:
                sock.bind((self.bind_host, self.discovery_port))
            except OSError as exc:
                sock.close()
                raise DiscoveryError(f"Cannot listen on UDP {self.discovery_port}: {exc}") from exc
            sock.settimeout(0.5)
            self._sock = sock
        self._stop.clear()
        self._thread = threading.Thread(target=self._loop, name="soundhop-discovery", daemon=True)
        self._thread.start()
        self._send_beacon()

    def _targets_now(self) -> list[str]:
        if self._targets is not None:
            return self._targets
        return broadcast_addresses(include_loopback=True)

    def _loop(self) -> None:
        while not self._stop.is_set():
            if self._advertising and time.monotonic() - self._last_send >= self.interval:
                self._send_beacon()
            sock = self._sock
            if sock is None:
                break
            try:
                readable, _, _ = select.select([sock], [], [], 0.5)
            except (OSError, ValueError):
                break
            if not readable:
                continue
            try:
                data, addr = sock.recvfrom(8192)
            except OSError:
                continue
            try:
                self._handle(data, addr[0])
            except Exception:
                log.debug("Error handling UDP packet from %s", addr[0], exc_info=True)

    def _handle(self, data: bytes, host: str) -> None:
        if not is_private_address(host):
            return
        payload, peer = parse_beacon(data, host=host, source="udp")
        if payload is None:
            return
        if payload.get("type") == "probe":
            if self._advertising:
                reply_port = self.discovery_port
                raw_port = payload.get("reply_port")
                if raw_port is not None and not isinstance(raw_port, bool):
                    try:
                        p = int(raw_port)
                        if 1 <= p <= 65535:
                            reply_port = p
                    except (ValueError, TypeError):
                        pass
                self._send_beacon((host, reply_port))
            return
        if payload.get("type") in {"bye", "goodbye"}:
            peer_id = str(payload.get("id", ""))
            if peer_id and peer_id != self.device_id:
                self.store.remove(peer_id)
            return
        if peer is None or peer.id == self.device_id:
            return
        self.store.update(peer)

    def _send_beacon(self, destination: tuple[str, int] | None = None) -> None:
        sock = self._sock
        if sock is None:
            return
        payload = make_beacon(
            self.device_id,
            self.name,
            self.audio_port,
            security=self.security,
        )
        destinations = (
            [destination]
            if destination
            else [(target, self.discovery_port) for target in self._targets_now()]
        )
        for dest in destinations:
            if dest is None:
                continue
            try:
                sock.sendto(payload, dest)
            except OSError as exc:
                log.debug("Could not send beacon to %s: %s", dest, exc)
        self._last_send = time.monotonic()

    def update(
        self,
        *,
        name: str | None = None,
        audio_port: int | None = None,
        security: str | None = None,
        advertising: bool | None = None,
    ) -> None:
        if name is not None:
            self.name = name
        if audio_port is not None:
            self.audio_port = audio_port
        if security is not None:
            self.security = security
        if advertising is not None:
            self._advertising = advertising
        self._send_beacon()

    def stop(self) -> None:
        self._stop.set()
        with self._lock:
            sock = self._sock
        if sock is not None:
            try:
                payload = make_beacon(
                    self.device_id, self.name, self.audio_port, security=self.security
                ).replace(b'"type":"offer"', b'"type":"bye"')
                for target in self._targets_now():
                    try:
                        sock.sendto(payload, (target, self.discovery_port))
                    except OSError:
                        pass
            except OSError:
                pass
        if self._thread is not None:
            self._thread.join(timeout=2.0)
            self._thread = None
        with self._lock:
            if self._sock is not None:
                try:
                    self._sock.close()
                except OSError:
                    pass
                self._sock = None


# ---------------------------------------------------------------------------
# optional mDNS
# ---------------------------------------------------------------------------


def mdns_available() -> bool:
    try:
        import zeroconf  # noqa: F401
    except ModuleNotFoundError:
        return False
    return True


class MdnsService:
    """Publish and browse ``_soundhop._tcp`` when python-zeroconf exists."""

    def __init__(
        self,
        *,
        device_id: str,
        name: str,
        audio_port: int,
        security: str = "trusted-lan",
        store: PeerStore | None = None,
    ) -> None:
        self.device_id = device_id
        self.name = name
        self.audio_port = audio_port
        self.security = security
        self.store = store or PeerStore()
        self._zc: Any = None
        self._info: Any = None
        self._browser: Any = None
        self._peer_by_name: dict[str, str] = {}

    def start_browsing(self) -> None:
        try:
            from zeroconf import ServiceBrowser, ServiceListener, Zeroconf
        except ModuleNotFoundError as exc:  # pragma: no cover - depends on extra
            raise MdnsUnavailable(
                "mDNS needs the optional 'zeroconf' package (pip install 'soundhop[mdns]')"
            ) from exc
        if self._zc is None:
            self._zc = Zeroconf()
        if self._browser is not None:
            return

        store = self.store
        zc = self._zc
        device_id = self.device_id
        peer_by_name = self._peer_by_name

        class Listener(ServiceListener):
            def add_service(self, zc_: Any, type_: str, name_: str) -> None:
                self._resolve(zc_, type_, name_)

            def update_service(self, zc_: Any, type_: str, name_: str) -> None:
                self._resolve(zc_, type_, name_)

            def remove_service(self, zc_: Any, type_: str, name_: str) -> None:
                peer_id = peer_by_name.pop(name_, None)
                if not peer_id:
                    try:
                        info = zc_.get_service_info(type_, name_, timeout=500)
                    except Exception:
                        info = None
                    if info is not None:
                        props = _decode_props(info.properties)
                        peer_id = props.get("id", "")
                if peer_id:
                    store.remove(peer_id)

            def _resolve(self, zc_: Any, type_: str, name_: str) -> None:
                try:
                    info = zc_.get_service_info(type_, name_, timeout=1500)
                except Exception:
                    info = None
                if info is None:
                    return
                props = _decode_props(info.properties)
                addresses = []
                try:
                    addresses = info.parsed_addresses()
                except Exception:
                    try:
                        addresses = [socket.inet_ntoa(info.address)]
                    except Exception:
                        addresses = []
                host = _pick_address(addresses)
                if not host or not is_private_address(host):
                    return
                peer_id = props.get("id", "") or info.name
                try:
                    peer = Peer(
                        id=peer_id,
                        name=props.get("name", info.name.split(".")[0]),
                        host=host,
                        port=int(props.get("port", info.port)),
                        version=props.get("ver", ""),
                        protocol=int(props.get("proto", PROTOCOL_VERSION)),
                        security=props.get("security", "trusted-lan"),
                        source="mdns",
                    )
                except (TypeError, ValueError):
                    return
                if peer.id != device_id:
                    peer_by_name[name_] = peer.id
                    store.update(peer)

        self._browser = ServiceBrowser(zc, MDNS_SERVICE_TYPE, Listener())

    def start_advertising(self) -> None:
        try:
            from zeroconf import ServiceInfo
        except ModuleNotFoundError as exc:  # pragma: no cover
            raise MdnsUnavailable(
                "mDNS needs the optional 'zeroconf' package (pip install 'soundhop[mdns]')"
            ) from exc
        if self._zc is None:
            from zeroconf import Zeroconf

            self._zc = Zeroconf()
        if self._info is not None:
            self.stop_advertising()
        addresses = []
        for addr in local_addresses(include_loopback=False):
            try:
                if ":" in addr:
                    addresses.append(socket.inet_pton(socket.AF_INET6, addr))
                else:
                    addresses.append(socket.inet_aton(addr))
            except OSError:
                continue
        instance = f"soundhop-{slugify(self.device_id, max_length=12)}"
        service_name = f"{instance}.{MDNS_SERVICE_TYPE}"
        info = ServiceInfo(
            MDNS_SERVICE_TYPE,
            service_name,
            addresses=addresses,
            port=int(self.audio_port),
            properties={
                "id": self.device_id,
                "name": self.name,
                "port": str(int(self.audio_port)),
                "ver": VERSION,
                "proto": str(PROTOCOL_VERSION),
                "security": self.security,
            },
        )
        self._zc.register_service(info)
        self._info = info

    def stop_advertising(self) -> None:
        if self._info is not None and self._zc is not None:
            try:
                self._zc.unregister_service(self._info)
            except Exception:
                log.debug("Could not unregister mDNS service", exc_info=True)
        self._info = None

    def stop(self) -> None:
        self.stop_advertising()
        if self._browser is not None:
            try:
                self._browser.cancel()
            except Exception:
                pass
            self._browser = None
        if self._zc is not None:
            try:
                self._zc.close()
            except Exception:
                pass
            self._zc = None

    def update(
        self, *, name: str | None = None, audio_port: int | None = None, security: str | None = None
    ) -> None:
        if name is not None:
            self.name = name
        if audio_port is not None:
            self.audio_port = audio_port
        if security is not None:
            self.security = security
        if self._info is not None:
            self.start_advertising()


def _decode_props(properties: dict[Any, Any]) -> dict[str, str]:
    decoded: dict[str, str] = {}
    for key, value in properties.items():
        if isinstance(key, bytes):
            key = key.decode("utf-8", "replace")
        if isinstance(value, bytes):
            value = value.decode("utf-8", "replace")
        if key:
            decoded[str(key)] = "" if value is None else str(value)
    return decoded


def _pick_address(addresses: Iterable[str]) -> str:
    for address in addresses:
        if is_private_address(address) and not address.startswith("127.") and address != "::1":
            return address
    for address in addresses:
        if is_private_address(address):
            return address
    return ""


# ---------------------------------------------------------------------------
# one-shot probe (used by `soundhop discover` without a daemon)
# ---------------------------------------------------------------------------


class UdpProbe:
    def __init__(self, *, discovery_port: int, device_id: str = "") -> None:
        self.discovery_port = discovery_port
        self.device_id = device_id
        self._sock: socket.socket | None = None

    def discover(
        self,
        *,
        timeout: float = 2.5,
        targets: Iterable[str] | None = None,
        store: PeerStore | None = None,
    ) -> list[Peer]:
        store = store or PeerStore()
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        sock.bind(("", 0))
        sock.settimeout(0.25)
        self._sock = sock
        try:
            destinations = (
                list(targets) if targets is not None else broadcast_addresses(include_loopback=True)
            )
            payload = make_beacon(
                self.device_id or "probe",
                hostname_name(),
                0,
                message_type="probe",
            )
            probe = json.loads(payload)
            probe["reply_port"] = sock.getsockname()[1]
            datagram = json.dumps(probe, separators=(",", ":")).encode("utf-8")
            deadline = time.monotonic() + max(0.2, timeout)
            next_send = 0.0
            while time.monotonic() < deadline:
                if time.monotonic() >= next_send:
                    for target in destinations:
                        try:
                            sock.sendto(datagram, (target, self.discovery_port))
                        except OSError:
                            pass
                    next_send = time.monotonic() + 0.7
                try:
                    data, addr = sock.recvfrom(8192)
                except TimeoutError:
                    continue
                except OSError:
                    break
                _, peer = parse_beacon(data, host=addr[0], source="udp")
                if peer is not None and is_private_address(peer.host) and peer.id != self.device_id:
                    store.update(peer)
            return store.snapshot()
        finally:
            try:
                sock.close()
            except OSError:
                pass
            self._sock = None


# ---------------------------------------------------------------------------
# combined manager used by the daemon
# ---------------------------------------------------------------------------


class DiscoveryManager:
    def __init__(
        self,
        *,
        device_id: str,
        name: str,
        audio_port: int,
        discovery_port: int,
        security: str = "trusted-lan",
        interval: float = BEACON_INTERVAL,
        use_udp: bool = True,
        use_mdns: bool = True,
        store: PeerStore | None = None,
    ) -> None:
        self.store = store or PeerStore()
        self.use_udp = use_udp
        self.use_mdns = use_mdns
        self._udp: UdpBeaconService | None = None
        self._mdns: MdnsService | None = None
        self._started = False
        if use_udp:
            self._udp = UdpBeaconService(
                device_id=device_id,
                name=name,
                audio_port=audio_port,
                discovery_port=discovery_port,
                security=security,
                interval=interval,
                store=self.store,
            )
        if use_mdns:
            self._mdns = MdnsService(
                device_id=device_id,
                name=name,
                audio_port=audio_port,
                security=security,
                store=self.store,
            )

    def start(self, *, advertising: bool = True) -> list[str]:
        """Start discovery.  Returns human-readable warnings."""

        warnings: list[str] = []
        self._started = True
        if self._udp is not None:
            try:
                self._udp.start()
            except DiscoveryError as exc:
                warnings.append(str(exc))
                self._udp = None
        if self._mdns is not None:
            try:
                self._mdns.start_browsing()
                if advertising:
                    self._mdns.start_advertising()
            except MdnsUnavailable as exc:
                log.info("%s", exc)
                self._mdns = None
            except Exception as exc:  # pragma: no cover - zeroconf runtime quirks
                warnings.append(f"mDNS unavailable: {exc}")
                log.debug("mDNS setup failed", exc_info=True)
        return warnings

    def set_advertising(self, enabled: bool) -> None:
        if self._udp is not None:
            self._udp.update(advertising=enabled)
        if self._mdns is not None:
            try:
                if enabled:
                    self._mdns.start_advertising()
                else:
                    self._mdns.stop_advertising()
            except Exception:
                log.debug("Could not update mDNS advertisement", exc_info=True)

    def update(
        self,
        *,
        name: str | None = None,
        audio_port: int | None = None,
        security: str | None = None,
    ) -> None:
        if self._udp is not None:
            self._udp.update(name=name, audio_port=audio_port, security=security)
        if self._mdns is not None:
            try:
                self._mdns.update(name=name, audio_port=audio_port, security=security)
            except Exception:
                log.debug("Could not update mDNS service", exc_info=True)

    def peers(self) -> list[Peer]:
        return self.store.snapshot()

    def stop(self) -> None:
        if self._udp is not None:
            self._udp.stop()
            self._udp = None
        if self._mdns is not None:
            self._mdns.stop()
            self._mdns = None
        self._started = False


def discover_once(
    *,
    device_id: str = "",
    discovery_port: int,
    timeout: float = 2.5,
    use_udp: bool = True,
    use_mdns: bool = True,
) -> list[Peer]:
    """Discover peers without a running daemon."""

    store = PeerStore()
    if use_udp:
        try:
            UdpProbe(discovery_port=discovery_port, device_id=device_id).discover(
                timeout=timeout, store=store
            )
        except OSError as exc:
            log.debug("UDP discover_once failed: %s", exc)
    if use_mdns and mdns_available():
        try:
            mdns = MdnsService(
                device_id=device_id or "discover",
                name=hostname_name(),
                audio_port=0,
                store=store,
            )
            try:
                mdns.start_browsing()
                deadline = time.monotonic() + max(0.3, min(timeout, 2.0))
                while time.monotonic() < deadline:
                    time.sleep(0.1)
            finally:
                mdns.stop()
        except Exception as exc:
            log.debug("mDNS discover_once failed: %s", exc)
    return store.snapshot()
