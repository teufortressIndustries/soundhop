"""Tests for firewall detection and command generation."""

import unittest
from unittest.mock import patch

from soundhop.firewall import FirewallState, close_ports, open_ports


class FirewallTest(unittest.TestCase):
    @patch("soundhop.firewall.detect")
    @patch("soundhop.firewall._sudo_prefix", return_value=["sudo"])
    def test_ufw_commands(self, mock_sudo, mock_detect) -> None:
        mock_detect.return_value = FirewallState(kind="ufw", active=True, detail="ufw active")

        # Open ports with mdns
        res = open_ports(4713, 47777, dry_run=True, include_mdns=True)
        self.assertTrue(res.ok)
        cmds = [" ".join(c) for c in res.commands]
        self.assertIn("sudo ufw allow 4713/tcp", cmds)
        self.assertIn("sudo ufw allow 47777/udp", cmds)
        self.assertIn("sudo ufw allow 5353/udp", cmds)

        # Open ports without mdns
        res_no_mdns = open_ports(4713, 47777, dry_run=True, include_mdns=False)
        cmds_no_mdns = [" ".join(c) for c in res_no_mdns.commands]
        self.assertIn("sudo ufw allow 4713/tcp", cmds_no_mdns)
        self.assertIn("sudo ufw allow 47777/udp", cmds_no_mdns)
        self.assertNotIn("sudo ufw allow 5353/udp", cmds_no_mdns)

        # Close ports
        close_res = close_ports(4713, 47777, dry_run=True, include_mdns=True)
        self.assertTrue(close_res.ok)
        close_cmds = [" ".join(c) for c in close_res.commands]
        self.assertIn("sudo ufw delete allow 4713/tcp", close_cmds)
        self.assertIn("sudo ufw delete allow 47777/udp", close_cmds)
        self.assertIn("sudo ufw delete allow 5353/udp", close_cmds)

    @patch("soundhop.firewall.detect")
    @patch("soundhop.firewall._sudo_prefix", return_value=["sudo"])
    @patch("soundhop.firewall._default_zone", return_value="FedoraWorkstation")
    def test_firewalld_commands(self, mock_zone, mock_sudo, mock_detect) -> None:
        mock_detect.return_value = FirewallState(kind="firewalld", active=True, detail="firewalld active")

        res = open_ports(4713, 47777, dry_run=True, include_mdns=True)
        self.assertTrue(res.ok)
        cmds = [" ".join(c) for c in res.commands]
        self.assertTrue(any("4713/tcp" in c for c in cmds))
        self.assertTrue(any("47777/udp" in c for c in cmds))
        self.assertTrue(any("5353/udp" in c for c in cmds))
        self.assertTrue(any("--reload" in c for c in cmds))

        # Without mdns
        res_no_mdns = open_ports(4713, 47777, dry_run=True, include_mdns=False)
        cmds_no_mdns = [" ".join(c) for c in res_no_mdns.commands]
        self.assertTrue(any("4713/tcp" in c for c in cmds_no_mdns))
        self.assertTrue(any("47777/udp" in c for c in cmds_no_mdns))
        self.assertFalse(any("5353/udp" in c for c in cmds_no_mdns))

    @patch("soundhop.firewall.detect")
    def test_windows_defender_commands(self, mock_detect) -> None:
        mock_detect.return_value = FirewallState(kind="windows-defender", active=True, detail="Windows Defender active")

        res = open_ports(4713, 47777, dry_run=True, include_mdns=True)
        self.assertTrue(res.ok)
        cmds = [" ".join(c) for c in res.commands]
        self.assertTrue(any("netsh advfirewall firewall add rule" in c and "4713" in c for c in cmds))
        self.assertTrue(any("netsh advfirewall firewall add rule" in c and "47777" in c for c in cmds))
        self.assertTrue(any("5353" in c for c in cmds))

        close_res = close_ports(4713, 47777, dry_run=True, include_mdns=True)
        self.assertTrue(close_res.ok)
        close_cmds = [" ".join(c) for c in close_res.commands]
        self.assertTrue(any("netsh advfirewall firewall delete rule" in c and "4713" in c for c in close_cmds))
        self.assertTrue(any("netsh advfirewall firewall delete rule" in c and "47777" in c for c in close_cmds))



if __name__ == "__main__":
    unittest.main()
