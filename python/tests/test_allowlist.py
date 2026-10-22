import random
import re
import unittest

import _path  # noqa: F401
from obd import allowlist
from obd.allowlist import (ALLOWED_CORPUS, BANNED_CORPUS, ALLOWLIST_SHA256, is_allowed,
                           normalize, self_test)


class AllowlistTests(unittest.TestCase):
    def test_banned_corpus_refused(self):
        for cmd in BANNED_CORPUS:
            self.assertFalse(is_allowed(cmd), "banned command passed: %r" % cmd)

    def test_allowed_corpus_passes(self):
        for cmd in ALLOWED_CORPUS:
            self.assertTrue(is_allowed(cmd), "allowed command refused: %r" % cmd)

    def test_every_uds_service_refused_with_any_payload(self):
        for sid in ("10", "11", "14", "22", "27", "28", "2E", "2F", "31", "34", "35",
                    "36", "37", "3E", "85", "04", "08"):
            for payload in ("", "00", "01", "F190", "FFFFFF", "1", "01 "):
                self.assertFalse(is_allowed(sid + payload), sid + payload)

    def test_protocol_forcing_refused(self):
        for p in "123456789ABC":
            self.assertFalse(is_allowed("ATSP" + p))
            self.assertFalse(is_allowed("ATTP" + p))
        self.assertTrue(is_allowed("ATSP0"))

    def test_self_test_clean(self):
        self.assertEqual(self_test(), [])

    def test_sha_is_stable(self):
        self.assertEqual(len(ALLOWLIST_SHA256), 64)
        self.assertEqual(ALLOWLIST_SHA256, allowlist.ALLOWLIST_SHA256)

    def test_normalize(self):
        self.assertEqual(normalize(" at z \r\n"), "ATZ")
        self.assertEqual(normalize("01 0c 1"), "010C1")

    def test_random_strings_property(self):
        rng = random.Random(1234)
        alphabet = "0123456789ABCDEFabcdef ATZSHPLMWCRFDNI@\r\n"
        at_ok = re.compile(allowlist.AT_PATTERN)
        obd_ok = re.compile(allowlist.OBD_PATTERN)
        passed = 0
        for _ in range(20000):
            s = "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 9)))
            if is_allowed(s):
                passed += 1
                c = normalize(s)
                self.assertTrue(at_ok.match(c) or obd_ok.match(c))
                self.assertNotIn(c[:2], ("04", "08", "10", "22", "27", "2E", "3E"))
                self.assertFalse(c.startswith("ATSH") or c.startswith("ATLP"))
        self.assertGreater(passed, 0)   # the generator does hit allowed shapes


if __name__ == "__main__":
    unittest.main()
