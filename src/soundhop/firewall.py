"""Firewall helpers for UFW and firewalld.

SoundHop never runs ``sudo`` from the daemon.  Opening ports is an explicit
step in ``soundhop setup`` or ``soundhop firewall open`` so the user always
sees and approves the change.
"""

from __future__ import annotations

import logging
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass, field

log = logging.getLogger(__name__)

MDNS_PORT = 5353


@dataclass
class FirewallState:
    kind: str = "none"
    active: bool = False
    detail: str = ""

    def to_dict(self) -> dict[str, object]:
        return {"kind": self.kind, "active": self.active, "detail": self.detail}


@dataclass
class FirewallResult:
    firewall: FirewallState
    commands: list[list[str]] = field(default_factory=list)
    ran: bool = False
    ok: bool = True
    errors: list[str] = field(default_factory=list)
    note: str = ""

    def to_dict(self) -> dict[str, object]:
        return {
            "firewall": self.firewall.to_dict(),
            "commands": [" ".join(c) for c in self.commands],
            "ran": self.ran,
            "ok": self.ok,
            "errors": self.errors,
            "note": self.note,
        }


def detect() -> FirewallState:
    if shutil.which("ufw"):
        try:
            result = subprocess.run(
                ["ufw", "status"], capture_output=True, text=True, timeout=5
            )
            output = (result.stdout + result.stderr).lower()
            if "status: active" in output:
                return FirewallState("ufw", True, "ufw is active")
            if "status: inactive" in output:
                return FirewallState("ufw", False, "ufw is installed but inactive")
            return FirewallState("ufw", False, "ufw state could not be determined")
        except (OSError, subprocess.TimeoutExpired):
            pass
    if shutil.which("firewall-cmd"):
        try:
            result = subprocess.run(
                ["systemctl", "is-active", "--quiet", "firewalld"], timeout=5
            )
            if result.returncode == 0:
                return FirewallState("firewalld", True, "firewalld is active")
            return FirewallState("firewalld", False, "firewalld is installed but inactive")
        except (OSError, subprocess.TimeoutExpired):
            pass
    if shutil.which("nft"):
        return FirewallState(
            "nftables",
            True,
            "nftables is present; SoundHop will print commands instead of editing rules",
        )
    return FirewallState("none", False, "No supported firewall detected")


def _sudo_prefix() -> list[str]:
    if os.geteuid() == 0:
        return []
    if not shutil.which("sudo"):
        return []
    if sys.stdin.isatty():
        return ["sudo"]
    return ["sudo", "-n"]


def _default_zone() -> str:
    try:
        result = subprocess.run(
            ["firewall-cmd", "--get-default-zone"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        zone = result.stdout.strip()
        if zone:
            return zone
    except (OSError, subprocess.TimeoutExpired):
        pass
    return ""


def open_ports(
    audio_port: int,
    discovery_port: int,
    *,
    dry_run: bool = False,
    include_mdns: bool = True,
) -> FirewallResult:
    state = detect()
    result = FirewallResult(firewall=state)
    if not state.active:
        result.note = state.detail
        return result

    sudo = _sudo_prefix()
    if state.kind == "ufw":
        result.commands = [
            [*sudo, "ufw", "allow", f"{audio_port}/tcp"],
            [*sudo, "ufw", "allow", f"{discovery_port}/udp"],
        ]
        if include_mdns:
            result.commands.append([*sudo, "ufw", "allow", f"{MDNS_PORT}/udp"])
    elif state.kind == "firewalld":
        zone = _default_zone()
        zone_args = ["--zone", zone] if zone else []
        result.commands = [
            [
                *sudo,
                "firewall-cmd",
                *zone_args,
                f"--add-port={audio_port}/tcp",
                "--permanent",
            ],
            [
                *sudo,
                "firewall-cmd",
                *zone_args,
                f"--add-port={discovery_port}/udp",
                "--permanent",
            ],
        ]
        if include_mdns:
            result.commands.append([
                *sudo,
                "firewall-cmd",
                *zone_args,
                f"--add-port={MDNS_PORT}/udp",
                "--permanent",
            ])
            result.commands.append([*sudo, "firewall-cmd", *zone_args, "--add-service=mdns", "--permanent"])
        result.commands.append([*sudo, "firewall-cmd", "--reload"])
    else:
        result.note = (
            "nftables detected. Add these rules yourself (or use ufw/firewalld):\\n"
            f"  nft add rule inet filter input tcp dport {audio_port} accept\\n"
            f"  nft add rule inet filter input udp dport {discovery_port} accept\\n"
            f"  nft add rule inet filter input udp dport {MDNS_PORT} accept\\n"
            "Persist them with your distribution's nftables service."
        )
        result.ok = False
        return result

    if dry_run:
        result.note = "Dry run: no firewall changes made"
        return result

    if not sudo and os.geteuid() != 0:
        result.ok = False
        result.errors.append("sudo is not installed; run the commands manually")
        return result

    for command in result.commands:
        try:
            proc = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=30,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            result.ok = False
            result.errors.append(f"{' '.join(command)}: {exc}")
            break
        if proc.returncode != 0:
            message = (proc.stderr or proc.stdout).strip()
            # firewalld may not know an "mdns" service; the UDP rule above is
            # enough, so do not fail the whole operation for that one command.
            if "--add-service=mdns" in command:
                log.debug("Could not add mdns service: %s", message)
                continue
            result.ok = False
            result.errors.append(f"{' '.join(command)}: {message or 'failed'}")
            break
    result.ran = True
    return result


def close_ports(
    audio_port: int,
    discovery_port: int,
    *,
    dry_run: bool = False,
    include_mdns: bool = True,
) -> FirewallResult:
    state = detect()
    result = FirewallResult(firewall=state)
    if not state.active:
        result.note = state.detail
        return result
    sudo = _sudo_prefix()
    if state.kind == "ufw":
        result.commands = [
            [*sudo, "ufw", "delete", "allow", f"{audio_port}/tcp"],
            [*sudo, "ufw", "delete", "allow", f"{discovery_port}/udp"],
        ]
        if include_mdns:
            result.commands.append(
                [*sudo, "ufw", "delete", "allow", f"{MDNS_PORT}/udp"]
            )
    elif state.kind == "firewalld":
        zone = _default_zone()
        zone_args = ["--zone", zone] if zone else []
        result.commands = [
            [*sudo, "firewall-cmd", *zone_args, f"--remove-port={audio_port}/tcp", "--permanent"],
            [*sudo, "firewall-cmd", *zone_args, f"--remove-port={discovery_port}/udp", "--permanent"],
        ]
        if include_mdns:
            result.commands.append(
                [*sudo, "firewall-cmd", *zone_args, f"--remove-port={MDNS_PORT}/udp", "--permanent"]
            )
            result.commands.append(
                [*sudo, "firewall-cmd", *zone_args, "--remove-service=mdns", "--permanent"]
            )
        result.commands.append([*sudo, "firewall-cmd", "--reload"])
    else:
        result.note = "Remove the SoundHop nftables rules manually."
        result.ok = False
        return result
    if dry_run:
        result.note = "Dry run: no firewall changes made"
        return result
    for command in result.commands:
        try:
            proc = subprocess.run(command, capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired) as exc:
            result.ok = False
            result.errors.append(str(exc))
            break
        if proc.returncode != 0:
            message = (proc.stderr or proc.stdout).strip()
            # Removing a rule that is not there is harmless.
            if "Could not delete non-existent rule" in message or "NOT FOUND" in message.upper():
                continue
            result.ok = False
            result.errors.append(f"{' '.join(command)}: {message or 'failed'}")
            break
    result.ran = True
    return result
