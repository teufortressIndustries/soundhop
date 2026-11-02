"""SoundHop - local network audio sharing for Linux.

The package intentionally keeps the core dependency-free: it talks to
PipeWire/PulseAudio through ``pactl`` and discovers peers with a small UDP
broadcast protocol.  mDNS (via python-zeroconf) and the Qt tray are optional
extras.
"""

from .constants import APP_NAME, VERSION

__all__ = ["APP_NAME", "VERSION", "__version__"]
__version__ = VERSION
