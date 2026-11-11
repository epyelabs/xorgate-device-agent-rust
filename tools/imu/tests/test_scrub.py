"""Scrubber tests on SYNTHETIC transcripts (tests/synth.py)."""
import copy
import json
import os
import struct
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)

import scrub_imutx as S  # noqa: E402
import synth  # noqa: E402

# Offsets inside a channel-3 full read of `basic()`: header 0-3, FB 4-8,
# RV 9-22 (id 9, seq 10, status 11, delay 12, words 13-20, accuracy 21-22),
# LA 23-32 (id 23, seq 24, status 25, delay 26, words 27-32).
RV_WORDS = [(13, 14), (15, 16), (17, 18), (19, 20)]
RV_ACC = (21, 22)
RV_DELAY = 12
LA_WORDS = [(27, 28), (29, 30), (31, 32)]
LA_DELAY = 26
SENSITIVE = {RV_DELAY, LA_DELAY, *sum(RV_WORDS, ()), *RV_ACC, *sum(LA_WORDS, ())}


def stub_oracle(ops):
    """A stand-in for the WP2 oracle: echoes the ops and, after every full
    channel-3 read, prints a sample computed from the LAST RV/LA in the read.
    Good enough to exercise the hook and the shape check."""
    out = [{"op": "stdout", "line": '{"type": "ready", "address": "0x4a"}', "t_us": 0}]
    for op in ops:
        out.append(op)
        data = bytes.fromhex(op.get("hex") or "")
        if op["op"] == "read" and len(data) == 33 and data[2] == 3:
            i, j, k, r = struct.unpack_from("<4h", data, 13)
            x, y, z = struct.unpack_from("<3h", data, 27)
            q = {"r": r / 16384, "i": i / 16384, "j": j / 16384, "k": k / 16384}
            la = {"x": x / 256, "y": y / 256, "z": z / 256}
            out.append({"op": "stdout", "line": json.dumps({"type": "sample", "quat": q, "linaccel": la}),
                        "t_us": op["t_us"]})
    return out


def raw_with_stub_stdout(tx):
    ops = tx.ops()
    return [tx.recs[0]] + stub_oracle(ops)


class Classify(unittest.TestCase):
    def test_golden_positions(self):
        tx, fulls = synth.basic()
        cls = S.classify(tx.ops())
        for f in fulls:
            sens = {k for (i, k), role in cls.roles.items() if i == f and role in S.SENSITIVE_KINDS}
            self.assertEqual(sens, SENSITIVE)
            kinds = {k: cls.roles[(f, k)] for k in SENSITIVE}
            self.assertEqual({kinds[k] for k in sum(RV_WORDS, ())}, {"rv"})
            self.assertEqual({kinds[k] for k in RV_ACC}, {"rv_acc"})
            self.assertEqual({kinds[k] for k in sum(LA_WORDS, ())}, {"la"})
            self.assertEqual({kinds[RV_DELAY], kinds[LA_DELAY]}, {"delay"})
        self.assertEqual(cls.conflicts, 0)

    def test_control_and_advert_kept(self):
        tx, _ = synth.basic()
        cls = S.classify(tx.ops())
        sens = [p for p, role in cls.roles.items() if role in S.SENSITIVE_KINDS]
        ops = tx.ops()
        for i, _k in sens:
            self.assertEqual(bytes.fromhex(ops[i]["hex"])[2], 3, "only channel-3 bytes are sensitive here")

    def test_partial_remainder_and_keyerror(self):
        tx = synth.Tx()
        tx.probe()
        tx.send(1, b"\x01")
        la = synth.la_report(1, (11, 12, 13))
        # FB + RV + the first 5 bytes of an LA: Unprocessable Batch bytes (5).
        f1 = tx.packet(3, synth.base_ts() + synth.rv_report(0, (1, 2, 3, 4), 5) + la[:5])
        # FB + an unknown id 0x77 + 6 more bytes: KeyError(0x77).
        f2 = tx.packet(3, synth.base_ts() + bytes([0x77, 1, 2, 3, 4, 5, 6]))
        cls = S.classify(tx.ops())
        r1 = {k: v for (i, k), v in cls.roles.items() if i == f1}
        self.assertEqual(r1[23], S.KEEP)  # the partial LA's id
        self.assertEqual(r1[26], "delay")
        self.assertEqual(r1[27], "la")  # a lone low byte of the first word
        r2 = {k: v for (i, k), v in cls.roles.items() if i == f2}
        self.assertEqual(r2[9], S.KEEP)  # the unknown id decides the error text
        self.assertEqual([r2[k] for k in range(10, 16)], ["tail"] * 6)

    def test_bytes_beyond_the_packet_are_tail(self):
        tx = synth.Tx()
        tx.probe()
        tx.send(1, b"\x01")
        payload = synth.base_ts() + synth.rv_report(0, (1, 2, 3, 4), 5)
        # The full read's own header claims only the FB report (9 bytes); the
        # RV bytes behind it are read but never part of the packet.
        f = tx.packet(3, payload, full_header=synth.header(9, 3, 7))
        cls = S.classify(tx.ops())
        r = {k: v for (i, k), v in cls.roles.items() if i == f}
        self.assertEqual([r[k] for k in range(9, 23)], ["tail"] * 14)

    def test_stale_buffer_bytes_belong_to_their_origin_op(self):
        tx = synth.Tx()
        tx.probe()
        tx.send(1, b"\x01")
        f1 = tx.packet(3, synth.base_ts() + synth.rv_report(0, (1, 2, 3, 4), 5))
        # 9-byte read whose own header claims 23: bytes 9-22 come from f1.
        f2 = tx.packet(3, synth.base_ts(), full_header=synth.header(23, 3, 8))
        cls = S.classify(tx.ops())
        self.assertEqual(cls.roles[(f1, 13)], "rv")
        self.assertFalse(any(i == f2 and k >= 9 for (i, k) in cls.roles))

    def test_large_read_replaces_the_buffer(self):
        tx = synth.Tx()
        tx.probe()
        tx.send(1, b"\x01")
        big = synth.base_ts() * 120  # 600 bytes of base timestamps
        tx.packet(3, big)
        cls = S.classify(tx.ops())
        self.assertEqual(cls.packets, 1)
        self.assertEqual(cls.conflicts, 0)


class Scrub(unittest.TestCase):
    def setUp(self):
        self.tx, self.fulls = synth.basic()
        self.raw = self.tx.recs

    def scrub(self, **kw):
        kw.setdefault("stdout_pending", True)
        return S.scrub(self.raw, **kw)

    def test_self_check_passes_and_framing_kept(self):
        out, stats = self.scrub()
        self.assertEqual(S.self_check(self.raw, out), [])
        self.assertEqual(stats["fields"]["rv"], 3)
        self.assertEqual(stats["fields"]["la"], 3)
        raw_ops, new_ops = S.ops_of(self.raw), S.ops_of(out)
        for f in self.fulls:
            a, b = bytes.fromhex(raw_ops[f]["hex"]), bytes.fromhex(new_ops[f]["hex"])
            for k in range(len(a)):
                if k in SENSITIVE:
                    continue
                self.assertEqual(a[k], b[k], f"framing byte {k} changed")
            for lo, hi in RV_WORDS + LA_WORDS + [RV_ACC]:
                self.assertNotEqual(a[lo:hi + 1], b[lo:hi + 1])
            self.assertNotEqual(a[RV_DELAY], b[RV_DELAY])
        self.assertEqual(out[0]["scrubbed"]["stdout"], "pending-oracle")
        self.assertFalse(any(r["op"] == "stdout" for r in out))

    def test_deterministic(self):
        a, _ = self.scrub(seed=5)
        b, _ = self.scrub(seed=5)
        c, _ = self.scrub(seed=6)
        self.assertEqual(a, b)
        self.assertNotEqual(a, c)

    def _plant(self, out, f, lo, n):
        out = copy.deepcopy(out)
        ops = [r for r in out if r["op"] not in ("meta", "stdout")]
        raw = bytes.fromhex(S.ops_of(self.raw)[f]["hex"])
        new = bytearray.fromhex(ops[f]["hex"])
        new[lo:lo + n] = raw[lo:lo + n]
        ops[f]["hex"] = new.hex()
        return out

    def test_planted_rv_word_is_caught(self):
        out, _ = self.scrub()
        v = S.self_check(self.raw, self._plant(out, self.fulls[1], 17, 2))
        self.assertEqual(len(v), 1)
        self.assertIn("rv unit", v[0])

    def test_planted_accuracy_word_and_delay_byte_are_caught(self):
        out, _ = self.scrub()
        self.assertIn("rv_acc", S.self_check(self.raw, self._plant(out, self.fulls[0], 21, 2))[0])
        self.assertIn("delay", S.self_check(self.raw, self._plant(out, self.fulls[2], LA_DELAY, 1))[0])

    def test_changed_framing_byte_is_caught(self):
        out, _ = self.scrub()
        bad = copy.deepcopy(out)
        ops = S.ops_of(bad)
        b = bytearray.fromhex(ops[self.fulls[0]]["hex"])
        b[10] ^= 0xFF  # the RV sequence byte
        ops[self.fulls[0]]["hex"] = b.hex()
        self.assertIn("framing", S.self_check(self.raw, bad)[0])

    def test_positional_coincidences_do_not_false_alarm(self):
        # A stationary sensor: every LA word is 0 or 1 in the raw file, and the
        # scrubbed file carries 0 and 1 at OTHER positions. Positionally every
        # word still differs, so there is no alarm (a multiset check would).
        tx = synth.Tx()
        tx.probe()
        tx.send(1, b"\x01")
        for n in range(4):
            tx.packet(3, synth.base_ts() + synth.la_report(n, (0, 1, 0)))
        raw = tx.recs
        out, _ = S.scrub(raw, stdout_pending=True)
        ops = S.ops_of(out)
        # Rewrite each scrubbed LA to (1, 0, 1): the raw values, rotated.
        rewritten = 0
        for op in ops:
            data = bytearray.fromhex(op.get("hex") or "")
            if op["op"] == "read" and len(data) == 19:  # header + FB + LA
                data[13:19] = struct.pack("<3h", 1, 0, 1)
                op["hex"] = data.hex()
                rewritten += 1
        self.assertEqual(rewritten, 4)
        self.assertEqual(S.self_check(raw, out), [])
        # ...while the raw value at its OWN position is still caught.
        ops[-1]["hex"] = ops[-1]["hex"][:26] + struct.pack("<3h", 0, 1, 0).hex()
        self.assertEqual(len(S.self_check(raw, out)), 3)

    def test_draw_equal_to_original_is_redrawn(self):
        # A generator that returns exactly the raw values everywhere.
        class Echo(S.SyntheticGen):
            def __init__(self):
                super().__init__(1)
                self.rvs = [(1000 + n, -2000, 3000, 12000 - n) for n in range(3)]
                self.las = [(5 + n, -7, 9) for n in range(3)]

            def rv(self):
                return list(self.rvs.pop(0))

            def rv_acc(self):
                return [0x0123]

            def la(self):
                return list(self.las.pop(0))

        out, stats = self.scrub(gen=Echo())
        self.assertEqual(S.self_check(self.raw, out), [])
        self.assertGreaterEqual(stats["redraws"], 3 * 8)

    def test_refuses_double_scrub_and_non_imutx(self):
        out, _ = self.scrub()
        with self.assertRaises(S.ScrubError):
            S.scrub(out, stdout_pending=True)
        with self.assertRaises(S.ScrubError):
            S.scrub([{"op": "read"}], stdout_pending=True)


class OracleHook(unittest.TestCase):
    def setUp(self):
        self.tx, self.fulls = synth.basic()
        self.raw = raw_with_stub_stdout(self.tx)

    def test_no_oracle_is_refused(self):
        with self.assertRaises(S.OracleUnavailable):
            S.scrub(self.raw)

    def test_stub_oracle_recomputes_stdout(self):
        out, _ = S.scrub(self.raw, oracle=stub_oracle)
        self.assertEqual(out[0]["scrubbed"]["stdout"], "oracle")
        raw_samples = [r["line"] for r in self.raw if r["op"] == "stdout" and "sample" in r["line"]]
        new_samples = [r["line"] for r in out if r["op"] == "stdout" and "sample" in r["line"]]
        self.assertEqual(len(raw_samples), len(new_samples))
        for a, b in zip(raw_samples, new_samples):
            self.assertNotEqual(a, b)

    def test_oracle_that_changes_ops_is_rejected(self):
        def bad(ops):
            recs = stub_oracle(ops)
            recs[-1] = dict(recs[-1], dur_us=1)
            return recs

        with self.assertRaises(S.ScrubError):
            S.scrub(self.raw, oracle=bad)

    def test_oracle_with_a_different_shape_is_rejected(self):
        def bad(ops):
            return stub_oracle(ops) + [{"op": "stdout", "line": '{"type": "error", "error": "x"}', "t_us": 0}]

        with self.assertRaises(S.ScrubError):
            S.scrub(self.raw, oracle=bad)

    def test_hex_dump_lines_compare_masked(self):
        a = [{"op": "stdout", "line": "DBG::\t\t[0x04] 0x05 0x00 0x03 0x21 "}]
        b = [{"op": "stdout", "line": "DBG::\t\t[0x04] 0x05 0x00 0x03 0x9C "}]
        self.assertEqual(S.stdout_shape_violations(a, b), [])


if __name__ == "__main__":
    unittest.main()
