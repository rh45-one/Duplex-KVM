"""Run setup against a fake NetworkManager; never touch host networking."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/debian-setup-link.sh"
FAKE_NMCLI = """#!/usr/bin/python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ['NM_LOG'], 'a') as log:
    log.write(json.dumps(args) + '\\n')
if args == ['-t', '-f', 'DEVICE,TYPE', 'device', 'status']:
    print(os.environ.get('NM_DEVICES', 'enp2s0:ethernet'))
elif args[:3] == ['-g', 'GENERAL.TYPE', 'device']:
    print(os.environ.get('NM_DEVICE_TYPE', 'ethernet'))
elif args[:3] == ['-g', 'connection.uuid', 'connection']:
    sys.exit(0 if os.environ.get('NM_EXISTS', '1') == '1' else 10)
elif args[:2] in (['connection', 'add'], ['connection', 'modify']):
    pass
elif args[:4] == ['--wait', '20', 'connection', 'up']:
    sys.exit(int(os.environ.get('NM_UP_EXIT', '0')))
else:
    sys.exit('Unexpected nmcli command: ' + repr(args))
"""


class NetworkSetupTests(unittest.TestCase):
    def run_setup(self, *args, **settings):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fake_nmcli = root / "nmcli"
            fake_nmcli.write_text(FAKE_NMCLI)
            fake_nmcli.chmod(0o755)
            # Only the root guard is bypassed in the test copy. --network-only
            # skips all host file writes; every network operation uses the fake.
            script = SCRIPT.read_text()
            guard = 'if [[ $EUID -ne 0 ]]; then'
            self.assertEqual(script.count(guard), 1)
            test_script = root / "setup.sh"
            test_script.write_text(script.replace(guard, 'if false; then'))
            log = root / "nmcli.jsonl"
            result = subprocess.run(
                ["bash", str(test_script), "--network-only", *args],
                env={**os.environ, "PATH": f"{root}:/usr/bin:/bin",
                     "NM_LOG": str(log), **settings},
                capture_output=True, text=True,
            )
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result, calls

    def test_legacy_profile_repaired_without_disconnecting_or_touching_dhcp(self):
        result, calls = self.run_setup("enp2s0")
        self.assertEqual(result.returncode, 0, result.stderr)
        mutations = [c for c in calls if c[:2] == ["connection", "modify"]]
        self.assertEqual(len(mutations), 1)
        self.assertEqual(mutations[0][2], "duplex-kvm-1gbe")
        props = dict(zip(mutations[0][3::2], mutations[0][4::2]))
        self.assertEqual(props["connection.autoconnect"], "no")
        self.assertEqual(props["ipv4.never-default"], "yes")
        self.assertEqual(props["ipv4.gateway"], "")
        self.assertEqual(props["ipv4.dns"], "")
        self.assertFalse(any("delete" in c or "down" in c or "up" in c for c in calls))

    def test_new_profile_is_safe_from_creation(self):
        result, calls = self.run_setup(NM_EXISTS="0")
        self.assertEqual(result.returncode, 0, result.stderr)
        create = next(c for c in calls if c[:2] == ["connection", "add"])
        props = dict(zip(create[2::2], create[3::2]))
        self.assertEqual(props["connection.autoconnect"], "no")
        self.assertEqual(props["ipv4.never-default"], "yes")
        self.assertFalse(any("up" in c for c in calls))

    def test_explicit_passthrough_is_manual_and_low_priority(self):
        result, calls = self.run_setup("enp2s0", "--connect", "--passthrough")
        self.assertEqual(result.returncode, 0, result.stderr)
        modify = next(c for c in calls if c[:2] == ["connection", "modify"])
        props = dict(zip(modify[3::2], modify[4::2]))
        self.assertEqual(props["connection.autoconnect"], "no")
        self.assertEqual(props["ipv4.gateway"], "192.168.137.1")
        self.assertEqual(props["ipv4.route-metric"], "2000")
        self.assertEqual(props["ipv4.dns-priority"], "2000")
        self.assertEqual(calls[-1], ["--wait", "20", "connection", "up", "duplex-kvm-1gbe", "ifname", "enp2s0"])

    def test_refuses_ambiguous_adapter(self):
        result, calls = self.run_setup(NM_DEVICES="enp2s0:ethernet\nenx1:ethernet")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c[0] == "connection" for c in calls))

    def test_refuses_wifi_interface(self):
        result, calls = self.run_setup("wlan0", NM_DEVICE_TYPE="wifi")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c[0] == "connection" for c in calls))

    def test_activation_failure_is_reported(self):
        result, _ = self.run_setup("enp2s0", "--connect", NM_UP_EXIT="4")
        self.assertEqual(result.returncode, 4)
        self.assertNotIn("Setup Complete", result.stdout)


if __name__ == "__main__":
    unittest.main()
