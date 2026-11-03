"""Shared constants."""

from __future__ import annotations

APP_NAME = "SoundHop"
APP_SLUG = "soundhop"
APP_ID = "io.github.soundhop"
VERSION = "1.1.2"
REPO_URL = "https://github.com/teufortressIndustries/soundhop"
ISSUES_URL = "https://github.com/teufortressIndustries/soundhop/issues"

#: Version of the on-the-wire discovery protocol.  Bump only for breaking
#: changes; peers with a different version are still displayed but are not
#: auto-connected.
PROTOCOL_VERSION = 1

#: PulseAudio native protocol port used for audio.
DEFAULT_AUDIO_PORT = 4713

#: UDP port used by SoundHop's zero-configuration discovery.
DEFAULT_DISCOVERY_PORT = 47777

#: mDNS service type used when python-zeroconf is available.
MDNS_SERVICE_TYPE = "_soundhop._tcp.local."

#: Prefix for PulseAudio/PipeWire modules and sinks created by SoundHop.
MODULE_PREFIX = "soundhop_"
SINK_PREFIX = "soundhop_"

#: How often a daemon announces itself, and how long a peer is considered
#: online after its last announcement.
BEACON_INTERVAL = 3.0
PEER_TIMEOUT = 20.0

#: How long to wait for a new tunnel sink to appear in the graph.
SINK_TIMEOUT = 3.0

#: How often the daemon checks whether it should auto-connect/reconnect.
SUPERVISOR_INTERVAL = 2.0

#: Number of failed TCP probes before an auto-connected peer is dropped.
OFFLINE_PROBE_LIMIT = 3

#: Default private networks that may connect in ``trusted-lan`` mode.
DEFAULT_LAN_NETWORKS = (
    "127.0.0.1",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "fc00::/7",
    "::1",
)
