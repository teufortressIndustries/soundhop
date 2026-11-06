"""Windows AudioBackend implementation for SoundHop.

Integrates Windows audio playback and loopback capture with the PulseAudio
native wire protocol so Windows machines can seamlessly send and receive audio
to/from Linux SoundHop nodes.
"""

from __future__ import annotations

import ipaddress
import logging
import select
import socket
import threading
import time
from collections.abc import Sequence
from typing import Any

from ...backend import AudioBackend, BackendError, Listener, Module, Sink, SinkInput
from ...constants import DEFAULT_LAN_NETWORKS, SINK_PREFIX
from ...util import sanitize_sink_name
from .audio import WindowsLoopbackRecorder, WindowsPlayer, list_audio_endpoints
from .protocol import (
    PA_COMMAND_AUTH,
    PA_COMMAND_CREATE_PLAYBACK_STREAM,
    PA_COMMAND_GET_SERVER_INFO,
    PA_COMMAND_REPLY,
    PA_COMMAND_SET_CLIENT_NAME,
    PA_SAMPLE_S16LE,
    SampleSpec,
    TagStructReader,
    make_command_auth,
    make_command_create_playback_stream,
    make_command_set_client_name,
    make_reply_auth,
    make_reply_create_playback_stream,
    make_reply_ok,
    make_reply_server_info,
    read_packet,
    write_packet,
)

log = logging.getLogger(__name__)


class WindowsAudioBackend(AudioBackend):
    def __init__(self, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._listener_thread: threading.Thread | None = None
        self._listener_socket: socket.socket | None = None
        self._listener_stop = threading.Event()
        self._listener_port: int | None = None
        self._listener_security: str = "trusted-lan"
        self._listener_id = 100
        self._next_module_id = 101
        self._modules: list[Module] = []
        self._default_sink: str | None = None

        # Active sender tunnel sessions: sink_name -> session
        self._active_senders: dict[str, _ActiveTunnelSession] = {}
        self._lock = threading.RLock()

    # -- availability & server info --------------------------------------

    def available(self) -> bool:
        return True

    def invalidate(self) -> None:
        pass

    def info(self, *, refresh: bool = False) -> dict[str, str]:
        return {
            "Server Name": self.server_name(),
            "Server Version": self.server_version(),
            "Default Sink": self.default_sink(),
            "Default Source": self.default_source(),
        }

    def server_name(self) -> str:
        return "Windows Audio (WASAPI)"

    def server_version(self) -> str:
        return "10.0"

    def server_label(self) -> str:
        return f"{self.server_name()} {self.server_version()}"

    def is_pipewire(self) -> bool:
        return False

    def default_sink(self) -> str:
        with self._lock:
            if self._default_sink and self.sink_exists(self._default_sink):
                return self._default_sink
        endpoints = list_audio_endpoints()
        return endpoints[0]["name"] if endpoints else "Default Output"

    def default_source(self) -> str:
        return "Default Input"

    # -- graph listing ---------------------------------------------------

    def sinks(self) -> list[Sink]:
        endpoints = list_audio_endpoints()
        sinks_list: list[Sink] = []
        for idx, ep in enumerate(endpoints):
            sinks_list.append(
                Sink(
                    id=idx + 1,
                    name=ep["name"],
                    driver="WASAPI",
                    spec=f"s16le {ep['channels']}ch {ep['default_samplerate']}Hz",
                    state="RUNNING",
                )
            )
        with self._lock:
            for sink_name, session in self._active_senders.items():
                sinks_list.append(
                    Sink(
                        id=session.module_id,
                        name=sink_name,
                        driver="module-tunnel-sink",
                        spec="s16le 2ch 48000Hz",
                        state="RUNNING" if session.is_alive() else "SUSPENDED",
                    )
                )
        return sinks_list

    def sink(self, name: str) -> Sink | None:
        for s in self.sinks():
            if s.name == name:
                return s
        return None

    def sink_exists(self, name: str) -> bool:
        return self.sink(name) is not None

    def sink_inputs(self) -> list[SinkInput]:
        return []

    def modules(self) -> list[Module]:
        with self._lock:
            return list(self._modules)

    def module(self, module_id: int) -> Module | None:
        for m in self.modules():
            if m.id == module_id:
                return m
        return None

    def find_modules(
        self,
        name: str | None = None,
        *,
        sink_name: str | None = None,
        contains: str | None = None,
    ) -> list[Module]:
        found: list[Module] = []
        for m in self.modules():
            if name is not None and m.name != name:
                continue
            if sink_name is not None and m.arg("sink_name") != sink_name:
                continue
            if contains is not None and contains not in m.args:
                continue
            found.append(m)
        return found

    def module_for_sink(self, sink_name: str) -> Module | None:
        for m in self.modules():
            if m.name.startswith("module-tunnel-") and m.arg("sink_name") == sink_name:
                return m
        return None

    def load_module(self, name: str, args: Sequence[str] | str = ()) -> int:
        with self._lock:
            mod_id = self._next_module_id
            self._next_module_id += 1
            args_str = " ".join(args) if isinstance(args, (list, tuple)) else str(args)
            self._modules.append(Module(id=mod_id, name=name, args=args_str))
            return mod_id

    def unload_module(self, module_id: int, *, ignore_missing: bool = True) -> bool:
        with self._lock:
            if module_id == self._listener_id:
                self._stop_listener_locked()
                self._modules = [m for m in self._modules if m.id != module_id]
                return True

            matching_name = None
            for name, session in self._active_senders.items():
                if session.module_id == module_id:
                    matching_name = name
                    break
            if matching_name is not None:
                session = self._active_senders.pop(matching_name)
                session.stop()
                self._modules = [m for m in self._modules if m.id != module_id]
                if self._default_sink == matching_name:
                    self._default_sink = None
                return True

            for idx, m in enumerate(self._modules):
                if m.id == module_id:
                    self._modules.pop(idx)
                    return True

            if not ignore_missing:
                raise BackendError(f"Could not unload module {module_id}: module not found")
            return False

    def unload_tunnels(self, *, sink_prefix: str = SINK_PREFIX) -> list[int]:
        removed: list[int] = []
        with self._lock:
            for name in list(self._active_senders.keys()):
                if name.startswith(sink_prefix):
                    session = self._active_senders.pop(name)
                    session.stop()
                    removed.append(session.module_id)
                    self._modules = [m for m in self._modules if m.id != session.module_id]
            for m in list(self._modules):
                if m.name.startswith("module-tunnel-") and m.arg("sink_name").startswith(
                    sink_prefix
                ):
                    if m.id not in removed:
                        removed.append(m.id)
                    self._modules.remove(m)
        return removed

    # -- default routing -------------------------------------------------

    def set_default_sink(self, name: str) -> None:
        if not name:
            raise BackendError("Refusing to set an empty default sink")
        with self._lock:
            self._default_sink = name
        self.invalidate()

    def move_sink_inputs(self, target: str, *, source: str | None = None) -> int:
        return 0

    # -- listener (receiver mode) ----------------------------------------

    def ensure_tcp_listener(
        self,
        port: int,
        security: str,
        allowed_networks: Sequence[str] = DEFAULT_LAN_NETWORKS,
    ) -> Listener:
        with self._lock:
            if self._listener_socket is not None and self._listener_port == port:
                return Listener(
                    module_id=self._listener_id,
                    created=False,
                    args=f"port={port} security={security}",
                    port=port,
                    security=security,
                )

            # Start Python PulseAudio TCP server for Windows speakers
            self._stop_listener_locked()
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(("0.0.0.0", port))
            sock.listen(5)
            sock.settimeout(0.5)

            self._listener_socket = sock
            self._listener_port = port
            self._listener_security = security
            self._listener_stop.clear()
            self._listener_thread = threading.Thread(
                target=self._listener_loop,
                args=(sock, security, list(allowed_networks)),
                name="soundhop-pa-listener",
                daemon=True,
            )
            self._listener_thread.start()

            mod_args = f"port={port} auth-anonymous=1"
            self._modules = [m for m in self._modules if m.id != self._listener_id]
            self._modules.append(
                Module(id=self._listener_id, name="module-native-protocol-tcp", args=mod_args)
            )

            return Listener(
                module_id=self._listener_id,
                created=True,
                args=mod_args,
                port=port,
                security=security,
            )

    def remove_listener(self, module_id: int) -> bool:
        with self._lock:
            self._stop_listener_locked()
            self._modules = [m for m in self._modules if m.id != module_id]
            return True

    def _stop_listener_locked(self) -> None:
        self._listener_stop.set()
        if self._listener_socket:
            try:
                self._listener_socket.close()
            except Exception:
                pass
            self._listener_socket = None
        if self._listener_thread:
            self._listener_thread.join(timeout=1.0)
            self._listener_thread = None

    def _listener_loop(
        self, server_sock: socket.socket, security: str, allowed_networks: list[str]
    ) -> None:
        while not self._listener_stop.is_set():
            try:
                res = server_sock.accept()
                if not isinstance(res, tuple) or len(res) < 2:
                    continue
                conn, addr = res
                client_ip = addr[0] if isinstance(addr, tuple) and len(addr) > 0 else "127.0.0.1"
            except TimeoutError:
                continue
            except OSError:
                break

            # Security ACL check
            if security == "trusted-lan":
                ip_obj = ipaddress.ip_address(client_ip)
                allowed = False
                for net_str in allowed_networks:
                    try:
                        if ip_obj in ipaddress.ip_network(net_str, strict=False):
                            allowed = True
                            break
                    except ValueError:
                        continue
                if not allowed:
                    log.warning("Rejected audio connection from %s (ACL check)", client_ip)
                    conn.close()
                    continue

            # Serve incoming audio stream
            client_thread = threading.Thread(
                target=self._serve_audio_client,
                args=(conn,),
                name="soundhop-pa-client",
                daemon=True,
            )
            client_thread.start()

    def _serve_audio_client(self, conn: socket.socket) -> None:
        player = WindowsPlayer()
        player.start()
        stream_channel = 0
        try:
            conn_file = conn.makefile("rwb", buffering=0)
            while not self._listener_stop.is_set():
                pkt = read_packet(conn_file)
                if pkt is None:
                    break

                if pkt.is_control:
                    # Parse tagstruct command
                    reader = TagStructReader(pkt.data)
                    try:
                        cmd = reader.read_u32()
                        tag = reader.read_u32()
                    except Exception:
                        continue

                    if cmd == PA_COMMAND_AUTH:
                        conn.sendall(make_reply_auth(tag))
                    elif cmd == PA_COMMAND_SET_CLIENT_NAME:
                        conn.sendall(make_reply_ok(tag))
                    elif cmd == PA_COMMAND_GET_SERVER_INFO:
                        conn.sendall(make_reply_server_info(tag, default_sink=self.default_sink()))
                    elif cmd == PA_COMMAND_CREATE_PLAYBACK_STREAM:
                        try:
                            spec = reader.read_sample_spec()
                        except Exception:
                            spec = SampleSpec(format=PA_SAMPLE_S16LE, channels=2, rate=48000)
                        if spec.rate != player.sample_rate or spec.channels != player.channels:
                            player.close()
                            player = WindowsPlayer(sample_rate=spec.rate, channels=spec.channels)
                            player.start()
                        conn.sendall(
                            make_reply_create_playback_stream(
                                tag, stream_index=0, channel=stream_channel, spec=spec
                            )
                        )
                    else:
                        conn.sendall(make_reply_ok(tag))
                else:
                    # Audio data frame on stream channel
                    player.write(pkt.data)
        except Exception as exc:
            log.debug("PulseAudio client connection closed: %s", exc)
        finally:
            player.close()
            try:
                conn.close()
            except Exception:
                pass

    # -- tunnel sinks (sender mode) --------------------------------------

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
        with self._lock:
            if sink_name in self._active_senders:
                old_session = self._active_senders.pop(sink_name)
                old_session.stop()
                self._modules = [m for m in self._modules if m.id != old_session.module_id]

            mod_id = self.load_module(
                "module-tunnel-sink",
                [f"server=tcp:{host}:{port}", f"sink_name={sink_name}"],
            )

            session = _ActiveTunnelSession(
                host=host,
                port=port,
                sink_name=sink_name,
                module_id=mod_id,
                reconnect_delay_ms=reconnect_delay_ms,
            )
            session.start()
            self._active_senders[sink_name] = session
            return mod_id

    def disconnect_tunnel(self, sink_name: str) -> bool:
        sink_name = sanitize_sink_name(sink_name)
        with self._lock:
            module = self.module_for_sink(sink_name)
            if module is not None:
                return self.unload_module(module.id)
            session = self._active_senders.pop(sink_name, None)
            if session:
                session.stop()
                return True
            return False

    # -- diagnostics -----------------------------------------------------

    def soundhop_modules(self) -> list[Module]:
        return [m for m in self.modules() if m.arg("sink_name").startswith(SINK_PREFIX)]

    def describe(self) -> dict[str, object]:
        return {
            "available": self.available(),
            "server": self.server_label(),
            "pipewire": False,
            "default_sink": self.default_sink(),
            "sinks": [{"name": s.name, "state": s.state, "spec": s.spec} for s in self.sinks()],
        }


class _ActiveTunnelSession:
    """Active sender session: captures loopback audio and sends to remote receiver."""

    def __init__(
        self,
        host: str,
        port: int,
        sink_name: str,
        module_id: int,
        reconnect_delay_ms: int = 3000,
    ) -> None:
        self.host = host
        self.port = port
        self.sink_name = sink_name
        self.module_id = module_id
        self.reconnect_delay_ms = reconnect_delay_ms
        self._sock: socket.socket | None = None
        self._recorder: WindowsLoopbackRecorder | None = None
        self._stop = threading.Event()
        self._session_error = threading.Event()
        self._thread: threading.Thread | None = None
        self._stream_channel = 0

    def is_alive(self) -> bool:
        return self._thread is not None and self._thread.is_alive() and not self._stop.is_set()

    def start(self) -> None:
        self._stop.clear()
        self._session_error.clear()
        self._thread = threading.Thread(
            target=self._run_sender,
            name=f"soundhop-sender-{self.sink_name}",
            daemon=True,
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._session_error.set()
        if self._recorder:
            self._recorder.stop()
            self._recorder = None
        if self._sock:
            try:
                self._sock.close()
            except Exception:
                pass
            self._sock = None
        if self._thread:
            self._thread.join(timeout=2.0)
            self._thread = None

    def _run_sender(self) -> None:
        retry_delay = max(0.5, self.reconnect_delay_ms / 1000.0)
        while not self._stop.is_set():
            try:
                sock = socket.create_connection((self.host, self.port), timeout=5.0)
                self._sock = sock
                sock_file = sock.makefile("rwb", buffering=0)

                # Handshake with remote PulseAudio/PipeWire server
                # 1. Auth: sends protocol_version and cookie
                sock.sendall(make_command_auth(tag=0, protocol_version_or_cookie=35))
                auth_rep = read_packet(sock_file)
                if not auth_rep or not auth_rep.is_control:
                    raise ConnectionError("No response to PA_COMMAND_AUTH")
                auth_reader = TagStructReader(auth_rep.data)
                if auth_reader.read_u32() != PA_COMMAND_REPLY:
                    raise ConnectionError("Server rejected PA_COMMAND_AUTH credentials")

                # 2. Client Name
                sock.sendall(
                    make_command_set_client_name(tag=1, client_name="SoundHop Windows Sender")
                )
                name_rep = read_packet(sock_file)
                if not name_rep or not name_rep.is_control:
                    raise ConnectionError("No response to PA_COMMAND_SET_CLIENT_NAME")
                name_reader = TagStructReader(name_rep.data)
                if name_reader.read_u32() != PA_COMMAND_REPLY:
                    raise ConnectionError("Server rejected PA_COMMAND_SET_CLIENT_NAME")

                # 3. Create Playback Stream (sink_name="" targets remote default sink)
                sock.sendall(make_command_create_playback_stream(tag=2, sink_name=""))
                reply = read_packet(sock_file)
                if not reply or not reply.is_control:
                    raise ConnectionError("No response to PA_COMMAND_CREATE_PLAYBACK_STREAM")
                reader = TagStructReader(reply.data)
                cmd = reader.read_u32()  # REPLY
                if cmd != PA_COMMAND_REPLY:
                    raise ConnectionError(f"Create playback stream failed with command code {cmd}")
                reader.read_u32()  # tag
                reader.read_u32()  # stream index
                self._stream_channel = reader.read_u32()

                # 4. Start WASAPI loopback capture and send audio frames
                def on_pcm_chunk(pcm_chunk: bytes) -> None:
                    if self._stop.is_set() or self._session_error.is_set() or not self._sock:
                        return
                    try:
                        packet = write_packet(self._stream_channel, pcm_chunk)
                        self._sock.sendall(packet)
                    except Exception as exc:
                        log.debug("Tunnel sendall error for %s: %s", self.sink_name, exc)
                        self._session_error.set()

                self._recorder = WindowsLoopbackRecorder(callback=on_pcm_chunk)
                self._recorder.start()

                self._session_error.clear()
                while not self._stop.is_set() and not self._session_error.is_set():
                    if self._recorder and not self._recorder.is_alive():
                        log.warning(
                            "WASAPI loopback recorder exited unexpectedly for %s", self.sink_name
                        )
                        break
                    if hasattr(sock, "fileno"):
                        try:
                            fn = sock.fileno()
                            if fn >= 0:
                                r, _, _ = select.select([sock], [], [], 0.5)
                                if r:
                                    peek = sock.recv(1, socket.MSG_PEEK)
                                    if not peek:
                                        log.info(
                                            "Remote receiver %s:%s closed connection",
                                            self.host,
                                            self.port,
                                        )
                                        break
                            else:
                                time.sleep(0.1)
                        except OSError:
                            break
                    else:
                        time.sleep(0.1)
            except Exception as exc:
                if not self._stop.is_set():
                    log.warning(
                        "Tunnel connection to %s:%s lost or failed: %s; retrying in %.1fs",
                        self.host,
                        self.port,
                        exc,
                        retry_delay,
                    )
            finally:
                if self._recorder:
                    self._recorder.stop()
                    self._recorder = None
                if self._sock:
                    try:
                        self._sock.close()
                    except Exception:
                        pass
                    self._sock = None
                self._session_error.clear()

            if self._stop.wait(retry_delay):
                break
