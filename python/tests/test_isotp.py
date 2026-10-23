import unittest

import _path  # noqa: F401
from obd.isotp import can_frames, decode_dtcs, find_payload, is_no_data, isotp_reassemble, supported_bitmap


class IsotpTests(unittest.TestCase):
    def test_single_frames_two_ecus(self):
        per = isotp_reassemble(["7E804410C0E80", "7E904410C0E80"])
        self.assertEqual(per, {"7E8": "410C0E80", "7E9": "410C0E80"})

    def test_search_noise_skipped(self):
        per = isotp_reassemble(["SEARCHING...", "7E8064100BE3CE813", "7E906410098180001"])
        self.assertEqual(per["7E8"], "4100BE3CE813")
        self.assertEqual(per["7E9"], "410098180001")

    def test_multiframe_pid68_from_survey(self):
        per = isotp_reassemble(["7E810094168034E5000", "7E821000000AAAAAAAA"])
        self.assertEqual(per, {"7E8": "4168034E5000000000"})

    def test_multiframe_vin_placeholder(self):
        lines = ["7E81014490201575647", "7E8215445535456494E", "7E82230303030303031"]
        per = isotp_reassemble(lines)
        payload = per["7E8"]
        self.assertTrue(payload.startswith("490201"))
        self.assertEqual(bytes.fromhex(payload[6:]).decode(), "WVGTESTVIN0000001")

    def test_multiframe_interleaved_ecus(self):
        lines = ["7E8100D416D02000006", "7E9034105 8C".replace(" ", ""), "7E8219B000000000000"]
        per = isotp_reassemble(lines)
        self.assertEqual(per["7E8"], "416D020000069B000000000000")
        self.assertEqual(per["7E9"], "41058C")

    def test_out_of_order_consecutive_frame_ignored(self):
        per = isotp_reassemble(["7E8100D416D02000006", "7E8229B000000000000"])
        self.assertEqual(per["7E8"], "416D02000006")

    def test_29bit_header(self):
        frames = can_frames(["18DAF11004410C0E80"])
        self.assertEqual(frames, [("18DAF110", "04410C0E80")])
        self.assertEqual(isotp_reassemble(["18DAF11004410C0E80"]), {"18DAF110": "410C0E80"})

    def test_supported_bitmap_matches_survey(self):
        pids = supported_bitmap("4100BE3CE813", 0)
        self.assertEqual(sorted(pids), [1, 3, 4, 5, 6, 7, 11, 12, 13, 14, 17, 18, 19, 21, 28, 31, 32])
        pids = supported_bitmap("410098180001", 0)
        self.assertEqual(sorted(pids), [1, 4, 5, 12, 13, 32])
        self.assertEqual(supported_bitmap("4100BE", 0), set())

    def test_no_data(self):
        self.assertTrue(is_no_data(["NO DATA"]))
        self.assertTrue(is_no_data(["SEARCHING...", "UNABLE TO CONNECT"]))
        self.assertTrue(is_no_data([]))
        self.assertFalse(is_no_data(["7E803410D00"]))

    def test_dtcs(self):
        self.assertEqual(decode_dtcs("4300"), [])
        self.assertEqual(decode_dtcs("430201430196"), ["P0143", "P0196"])
        self.assertEqual(decode_dtcs("4A014123"), ["C0123"])
        self.assertEqual(decode_dtcs("4A01C123"), ["U0123"])
        self.assertEqual(decode_dtcs("47019A00"), ["B1A00"])

    def test_find_payload(self):
        per = {"7E9": "410D00", "7E8": "410C0E80"}
        self.assertEqual(find_payload(per, "410C"), ("7E8", "410C0E80"))
        self.assertEqual(find_payload(per, "412F"), (None, None))


if __name__ == "__main__":
    unittest.main()
