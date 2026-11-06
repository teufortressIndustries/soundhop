"""Audio playback and loopback capture for Windows.

Uses sounddevice and soundcard to provide low-latency WASAPI playback
and loopback recording with optional lazy imports and fallback mocks for testing.
"""

from __future__ import annotations

import logging
import threading
from collections.abc import Callable

log = logging.getLogger(__name__)


class WindowsPlayer:
    """Streams incoming PCM chunks to Windows default audio playback device."""

    def __init__(self, sample_rate: int = 48000, channels: int = 2) -> None:
        self.sample_rate = sample_rate
        self.channels = channels
        self._stream = None
        self._lock = threading.Lock()
        self._closed = False

    def start(self) -> None:
        with self._lock:
            self._closed = False
            if self._stream is not None:
                return
            try:
                import sounddevice as sd

                self._stream = sd.RawOutputStream(
                    samplerate=self.sample_rate,
                    channels=self.channels,
                    dtype="int16",
                    latency="low",
                )
                self._stream.start()
                log.debug(
                    "Opened sounddevice RawOutputStream (%s Hz, %s ch)",
                    self.sample_rate,
                    self.channels,
                )
            except Exception as exc:
                log.warning("Could not open sounddevice audio output: %s", exc)
                self._stream = None

    def write(self, pcm_data: bytes) -> None:
        with self._lock:
            if self._closed or not pcm_data:
                return
            if self._stream is not None:
                try:
                    self._stream.write(pcm_data)
                except Exception as exc:
                    log.debug("sounddevice write error: %s", exc)

    def close(self) -> None:
        with self._lock:
            self._closed = True
            if self._stream is not None:
                try:
                    self._stream.stop()
                    self._stream.close()
                except Exception as exc:
                    log.debug("sounddevice stream close error: %s", exc)
                self._stream = None


class WindowsLoopbackRecorder:
    """Captures default Windows speaker audio output (WASAPI Loopback)."""

    def __init__(
        self,
        callback: Callable[[bytes], None],
        sample_rate: int = 48000,
        channels: int = 2,
        block_size: int = 1024,
    ) -> None:
        self.callback = callback
        self.sample_rate = sample_rate
        self.channels = channels
        self.block_size = block_size
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None

    def is_alive(self) -> bool:
        return (
            self._thread is not None and self._thread.is_alive() and not self._stop_event.is_set()
        )

    def start(self) -> None:
        if self._thread is not None and self._thread.is_alive():
            return
        self._stop_event.clear()
        self._thread = threading.Thread(
            target=self._record_loop,
            name="soundhop-wasapi-loopback",
            daemon=True,
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop_event.set()
        if self._thread is not None:
            self._thread.join(timeout=2.0)
            self._thread = None

    def _record_loop(self) -> None:
        try:
            import soundcard as sc

            try:
                import numpy as np
            except ImportError:
                np = None

            speaker = sc.default_speaker()
            if speaker is None:
                log.warning("No default speaker found for WASAPI loopback capture")
                return

            loopback = sc.get_microphone(id=str(speaker.name), include_loopback=True)
            if loopback is None:
                log.warning("Could not get loopback microphone for %s", speaker.name)
                return

            log.info("Capturing audio from %s in loopback mode", speaker.name)
            with loopback.recorder(
                samplerate=self.sample_rate, channels=self.channels, blocksize=self.block_size
            ) as rec:
                while not self._stop_event.is_set():
                    data = rec.record(numframes=self.block_size)
                    if np is not None:
                        pcm_int16 = np.clip(data * 32767.0, -32768, 32767).astype(np.int16)
                        self.callback(pcm_int16.tobytes())
                    elif isinstance(data, (bytes, bytearray)):
                        self.callback(bytes(data))
                    elif hasattr(data, "tobytes"):
                        self.callback(data.tobytes())
        except Exception as exc:
            log.warning("WASAPI loopback capture stopped: %s", exc)


def list_audio_endpoints() -> list[dict[str, str]]:
    """List available audio output devices on Windows."""
    try:
        import sounddevice as sd

        devices: list[dict[str, str]] = []
        dev_list = sd.query_devices()
        seen_names: set[str] = set()

        # Prioritize WASAPI devices if host APIs are queried
        wasapi_api_idx: int | None = None
        try:
            for h_idx, h_info in enumerate(sd.query_hostapis()):
                if "wasapi" in str(h_info.get("name", "")).lower():
                    wasapi_api_idx = h_idx
                    break
        except Exception:
            pass

        def device_sort_key(item: tuple[int, dict]) -> tuple[int, int]:
            idx, dev = item
            is_wasapi = 0 if dev.get("hostapi") == wasapi_api_idx else 1
            return (is_wasapi, idx)

        for idx, dev in sorted(enumerate(dev_list), key=device_sort_key):
            if dev.get("max_output_channels", 0) > 0:
                name = str(dev.get("name", f"Output {idx}")).strip()
                if not name or name in seen_names:
                    continue
                seen_names.add(name)
                devices.append(
                    {
                        "id": str(idx),
                        "name": name,
                        "channels": str(dev.get("max_output_channels", 2)),
                        "default_samplerate": str(int(dev.get("default_samplerate", 48000))),
                    }
                )
        return (
            devices
            if devices
            else [
                {
                    "id": "0",
                    "name": "Default Windows Audio Device",
                    "channels": "2",
                    "default_samplerate": "48000",
                }
            ]
        )
    except Exception as exc:
        log.debug("Failed querying audio endpoints via sounddevice: %s", exc)
        return [
            {
                "id": "0",
                "name": "Default Windows Audio Device",
                "channels": "2",
                "default_samplerate": "48000",
            }
        ]
