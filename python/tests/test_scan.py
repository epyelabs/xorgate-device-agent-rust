"""obd.scan.parse_transcript: bluetoothctl output -> scan candidates (P3)."""
import unittest

import _path  # noqa: F401  (puts python/ on sys.path)
from obd.scan import parse_transcript

TRANSCRIPT = """\
[bluetooth]# scan on
Discovery started
\x1b[0;92m[CHG]\x1b[0m Controller B8:27:EB:00:00:01 Discovering: yes
\x1b[0;92m[NEW]\x1b[0m Device 10:21:3E:4F:33:65 OBDII
\x1b[0;92m[NEW]\x1b[0m Device 6C:3B:E2:11:22:33 6C-3B-E2-11-22-33
\x1b[0;93m[CHG]\x1b[0m Device 10:21:3E:4F:33:65 RSSI: -58
\x1b[0;92m[NEW]\x1b[0m Device 00:1D:A5:68:98:8B 00-1D-A5-68-98-8B
\x1b[0;93m[CHG]\x1b[0m Device 00:1D:A5:68:98:8B Name: V-LINK
\x1b[0;93m[CHG]\x1b[0m Device 00:1D:A5:68:98:8B RSSI: -71
\x1b[0;93m[CHG]\x1b[0m Device 6C:3B:E2:11:22:33 RSSI: -40
\x1b[0;93m[CHG]\x1b[0m Device 10:21:3E:4F:33:65 RSSI: -55
[bluetooth]# scan off
Discovery stopped
[bluetooth]# devices
Device 10:21:3E:4F:33:65 OBDII
Device AA:AA:AA:AA:AA:01 Living Room TV
Device 00:1D:A5:68:98:8B V-LINK
Device CC:CC:CC:CC:CC:01 Bonded But Absent
"""


class ParseTranscriptTests(unittest.TestCase):
    def test_named_devices_sorted_by_rssi_unnamed_and_unseen_dropped(self):
        # AA:AA...:01 was never seen during discovery and only appears in the
        # cached `devices` listing: not a candidate. Same for CC:CC...:01 (the
        # bench's bonded-but-absent adapter case).
        out = parse_transcript(TRANSCRIPT)
        self.assertEqual(
            out,
            [
                {"mac": "10:21:3E:4F:33:65", "name": "OBDII", "rssi": -55},
                {"mac": "00:1D:A5:68:98:8B", "name": "V-LINK", "rssi": -71},
            ],
        )

    def test_seen_via_chg_only_with_cached_name(self):
        # A cached device that IS in range shows up as [CHG] lines only; its
        # name comes from the `devices` listing.
        text = ("[CHG] Device 10:21:3E:4F:33:65 RSSI: -60\n"
                "[bluetooth]# devices\n"
                "Device 10:21:3E:4F:33:65 OBDII\n")
        self.assertEqual(parse_transcript(text),
                         [{"mac": "10:21:3E:4F:33:65", "name": "OBDII", "rssi": -60}])

    def test_empty_and_garbage(self):
        self.assertEqual(parse_transcript(""), [])
        self.assertEqual(parse_transcript("Agent registered\n[bluetooth]# quit\n"), [])

    def test_never_emits_a_pair_command_shape(self):
        # The scan module must not know how to pair: the only bluetoothctl
        # verbs it writes are listed here, and `pair` is not one of them.
        import inspect
        import obd.scan as scan
        src = inspect.getsource(scan)
        self.assertNotIn('"pair ', src)
        self.assertNotIn("'pair ", src)
        self.assertIn('"pairable off"', src)
        self.assertIn('"discoverable off"', src)


if __name__ == "__main__":
    unittest.main()
