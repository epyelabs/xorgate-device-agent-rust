"""Converter tests on SYNTHETIC strace text (no real IMU bytes anywhere)."""
import contextlib
import io
import json
import os
import stat
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import imutx  # noqa: E402


def hx(b):
    return '"' + "".join(f"\\x{c:02x}" for c in b) + '"'


DEV = hx(b"/dev/i2c-1")
CPU = hx(b"/proc/cpuinfo")
GPIO = hx(b"/dev/gpiochip0")

OUT2 = b'ple"}' + b"\n"
OUT3 = b"x\n"
OUT1 = b'{"type": "ready", "address": "0x4a"}' + b"\n" + b'{"type": "sam'

# pid 4100 = the main tracee (first line); pid 4101 = a second task.
# Columns: pid, wall (-tt), (+ rel) (-r), body, <dur> (-T).
# The rel deltas are chosen so t_us is easy to check by hand (cumulative,
# first line = 0). Line 14 steps the WALL clock back one hour (an NTP step):
# t_us must keep following the monotonic -r deltas.
SAMPLE = [
    f"4100  10:00:00.000100 (+     0.000000) openat(AT_FDCWD, {CPU}, O_RDONLY|O_CLOEXEC) = 3 <0.000010>",
    f"4100  10:00:00.000200 (+     0.000100) read(3, {hx(b'Serial')}, 4096) = 6 <0.000005>",
    f"4100  10:00:00.000300 (+     0.000100) openat(AT_FDCWD, {DEV}, O_RDWR|O_CLOEXEC) = 3 <0.000020>",
    "4100  10:00:00.000400 (+     0.000100) ioctl(3, I2C_SLAVE, 0x4a) = 0 <0.000003>",
    '4100  10:00:00.000500 (+     0.000100) write(3, "", 0) = 0 <0.000300>',
    "4100  10:00:00.000600 (+     0.000100) ioctl(3, _IOC(_IOC_NONE, 0x7, 0x3, 0), 0x4a) = 0 <0.000002>",
    f"4100  10:00:00.000700 (+     0.000100) write(3, {hx(bytes([5, 0, 1, 0, 1]))}, 5) = 5 <0.000400>",
    f"4100  10:00:00.000800 (+     0.000100) openat(AT_FDCWD, {GPIO}, O_RDWR|O_CLOEXEC) = 4 <0.000010>",
    f"4100  10:00:00.000900 (+     0.000100) read(4, {hx(bytes([0, 1]))}, 2) = 2 <0.000004>",
    "4100  10:00:00.001000 (+     0.000100) ioctl(3, I2C_SLAVE, 0x4a) = 0 <0.000002>",
    "4100  10:00:00.001100 (+     0.000100) read(3, 0x7f12345678, 4) = -1 EAGAIN (Resource temporarily unavailable) <0.000050>",
    f"4100  10:00:00.001200 (+     0.000100) write(3, {hx(bytes([6, 0, 2, 0, 0xF9, 0]))}, 6) = -1 EIO (Input/output error) <0.000700>",
    "4100  10:00:00.001300 (+     0.000100) read(3,  <unfinished ...>",
    f"4101  09:00:00.001350 (+     0.000050) read(3, {hx(bytes([0x99]))}, 1) = 1 <0.000003>",
    f"4100  09:00:00.001400 (+     0.000050) <... read resumed>{hx(bytes([0x14, 0, 3, 7]))}, 4) = 4 <0.000090>",
    f"4100  09:00:00.001500 (+     0.000100) write(1, {hx(OUT1)}, {len(OUT1)}) = {len(OUT1)} <0.000006>",
    f"4100  09:00:00.001600 (+     0.000100) write(1, {hx(OUT2)}, 6) = 6 <0.000004>",
    f"4101  09:00:00.001700 (+     0.000100) write(1, {hx(OUT3)}, 2) = 2 <0.000004>",
    f"4100  09:00:00.001800 (+     0.000100) read(3, {hx(bytes([1, 2]))}..., 8) = 8 <0.000004>",
    f"4100  09:00:00.001900 (+     0.000100) write(4, {hx(bytes([7]))}, 1) = 1 <0.000004>",
    "4100  09:00:00.002000 (+     0.000100) --- SIGTERM {si_signo=SIGTERM, si_code=SI_USER, si_pid=1, si_uid=0} ---",
    "4100  09:00:00.002100 (+     0.000100) +++ killed by SIGTERM +++",
]

EXPECTED = [
    {"op": "open", "path": "/dev/i2c-1", "flags": "O_RDWR|O_CLOEXEC", "ret": 3, "errno": None, "t_us": 200, "dur_us": 20},
    {"op": "ioctl", "req": 0x0703, "arg": 0x4A, "ret": 0, "errno": None, "t_us": 300, "dur_us": 3},
    {"op": "write", "len": 0, "hex": "", "ret": 0, "errno": None, "t_us": 400, "dur_us": 300},
    {"op": "ioctl", "req": 0x0703, "arg": 0x4A, "ret": 0, "errno": None, "t_us": 500, "dur_us": 2},
    {"op": "write", "len": 5, "hex": "0500010001", "ret": 5, "errno": None, "t_us": 600, "dur_us": 400},
    {"op": "ioctl", "req": 0x0703, "arg": 0x4A, "ret": 0, "errno": None, "t_us": 900, "dur_us": 2},
    {"op": "read", "len": 4, "hex": "", "ret": -1, "errno": "EAGAIN", "t_us": 1000, "dur_us": 50},
    {"op": "write", "len": 6, "hex": "06000200f900", "ret": -1, "errno": "EIO", "t_us": 1100, "dur_us": 700},
    {"op": "read", "len": 4, "hex": "14000307", "ret": 4, "errno": None, "t_us": 1200, "dur_us": 90},
    {"op": "stdout", "line": '{"type": "ready", "address": "0x4a"}', "t_us": 1400},
    {"op": "stdout", "line": '{"type": "sample"}', "t_us": 1500},
    {"op": "read", "len": 8, "trunc": True, "hex": "0102", "ret": 8, "errno": None, "t_us": 1700, "dur_us": 4},
    {"op": "signal", "sig": "SIGTERM", "t_us": 1900},
    {"op": "exit", "signal": "SIGTERM", "t_us": 2000},
]


class ConvertSample(unittest.TestCase):
    def setUp(self):
        self.records, self.counts = imutx.convert_lines(SAMPLE)

    def test_meta(self):
        self.assertEqual(self.records[0]["op"], "meta")
        self.assertEqual(self.records[0]["format"], "imutx/1")
        self.assertEqual(self.records[0]["clock"], "monotonic")

    def test_exact_records(self):
        self.assertEqual(self.records[1:], EXPECTED)

    def test_errnos_kept(self):
        errs = [r["errno"] for r in self.records if r.get("errno")]
        self.assertEqual(errs, ["EAGAIN", "EIO"])

    def test_only_device_fd_and_stdout(self):
        # fd 3 BEFORE the device open was /proc/cpuinfo; fd 4 is the gpiochip.
        for r in self.records[1:]:
            self.assertIn(r["op"], ("open", "ioctl", "read", "write", "stdout", "signal", "exit"))
        self.assertNotIn("53657269616c", json.dumps(self.records))  # "Serial"
        self.assertNotIn('"hex": "0001"', json.dumps(self.records))
        self.assertNotIn('"07"', json.dumps(self.records))  # the write on fd 4
        self.assertNotIn("99", "".join(r.get("hex", "") for r in self.records[1:]))

    def test_counts(self):
        self.assertEqual(self.counts["other_pid_stdout_writes"], 1)
        self.assertEqual(self.counts["foreign_pid_device_ops"], 1)
        self.assertEqual(self.counts["truncated_strings"], 1)
        self.assertEqual(self.counts["dropped_syscalls"], 6)

    def test_monotonic_through_wall_step(self):
        ts = [r["t_us"] for r in self.records[1:]]
        self.assertEqual(ts, sorted(ts))
        # The wall clock went back 1 h between the unfinished read and its
        # resume; the resumed read keeps its entry time and the next stdout
        # write is 200 us later, not -3.6e9 us.
        self.assertEqual(self.records[9]["t_us"], 1200)
        self.assertEqual(self.records[10]["t_us"], 1400)


class ConvertEdges(unittest.TestCase):
    def test_failed_device_open_is_kept(self):
        recs, _ = imutx.convert_lines(
            [f"7 10:00:00.000000 (+     0.000000) openat(AT_FDCWD, {DEV}, O_RDWR|O_CLOEXEC) = -1 ENOENT (No such file or directory) <0.000010>"]
        )
        self.assertEqual(recs[1]["op"], "open")
        self.assertEqual((recs[1]["ret"], recs[1]["errno"]), (-1, "ENOENT"))

    def test_fd_reuse_drops_the_device_mapping(self):
        lines = [
            f"7 10:00:00.000000 (+     0.000000) openat(AT_FDCWD, {DEV}, O_RDWR|O_CLOEXEC) = 3 <0.000010>",
            f"7 10:00:00.000100 (+     0.000100) openat(AT_FDCWD, {CPU}, O_RDONLY) = 3 <0.000010>",
            f"7 10:00:00.000200 (+     0.000100) read(3, {hx(b'abc')}, 3) = 3 <0.000010>",
        ]
        recs, counts = imutx.convert_lines(lines)
        self.assertEqual([r["op"] for r in recs[1:]], ["open"])
        self.assertEqual(counts["fd_reuse"], 1)

    def test_wall_clock_fallback_wraps_midnight(self):
        lines = [
            f"7 23:59:59.999900 openat(AT_FDCWD, {DEV}, O_RDWR) = 3 <0.000010>",
            "7 00:00:00.000100 ioctl(3, I2C_SLAVE, 0x4a) = 0 <0.000001>",
        ]
        recs, _ = imutx.convert_lines(lines)
        self.assertEqual(recs[0]["clock"], "wall")
        self.assertEqual(recs[2]["t_us"], 200)

    def test_unfinished_at_end_is_kept_unresolved(self):
        lines = [
            f"7 10:00:00.000000 (+     0.000000) openat(AT_FDCWD, {DEV}, O_RDWR) = 3 <0.000010>",
            "7 10:00:00.000100 (+     0.000100) read(3,  <unfinished ...>",
            "7 10:00:00.000200 (+     0.000100) +++ killed by SIGKILL +++",
        ]
        recs, _ = imutx.convert_lines(lines)
        self.assertEqual(recs[-1]["op"], "read")
        self.assertIsNone(recs[-1]["ret"])
        self.assertEqual(recs[-2], {"op": "exit", "signal": "SIGKILL", "t_us": 200})

    def test_unavailable_result(self):
        lines = [
            f"7 10:00:00.000000 (+     0.000000) openat(AT_FDCWD, {DEV}, O_RDWR) = 3 <0.000010>",
            "7 10:00:00.000100 (+     0.000100) read(3, 0x1234, 4) = ? <unavailable>",
        ]
        recs, _ = imutx.convert_lines(lines)
        self.assertIsNone(recs[-1]["ret"])

    def test_partial_stdout_tail(self):
        lines = [f"7 10:00:00.000000 (+     0.000000) write(1, {hx(b'no newline')}, 10) = 10 <0.000001>"]
        recs, _ = imutx.convert_lines(lines)
        self.assertEqual(recs[-1], {"op": "stdout", "line": "no newline", "t_us": 0, "partial": True})

    def test_garbage_line_is_a_parse_error(self):
        with self.assertRaises(imutx.ParseError):
            imutx.convert_lines(["this is not strace"])


class ConvertCli(unittest.TestCase):
    def test_raw_deleted_after_success_and_0600(self):
        with tempfile.TemporaryDirectory() as d:
            raw, out = os.path.join(d, "w0-1.strace"), os.path.join(d, "w0-1.imutx")
            with open(raw, "w") as f:
                f.write("\n".join(SAMPLE) + "\n")
            with contextlib.redirect_stdout(io.StringIO()):
                rc = imutx.main(["convert", raw, "-o", out])
            self.assertEqual(rc, 0)
            self.assertFalse(os.path.exists(raw), "F18: the raw strace must be deleted")
            self.assertEqual(stat.S_IMODE(os.stat(out).st_mode), 0o600)
            self.assertEqual(imutx.read_records(out)[1:], EXPECTED)

    def test_raw_kept_on_parse_failure(self):
        with tempfile.TemporaryDirectory() as d:
            raw, out = os.path.join(d, "w0-1.strace"), os.path.join(d, "w0-1.imutx")
            with open(raw, "w") as f:
                f.write("garbage\n")
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(imutx.main(["convert", raw, "-o", out]), 2)
            self.assertTrue(os.path.exists(raw))
            self.assertFalse(os.path.exists(out))

    def test_summary_is_counts_only(self):
        s = imutx.summarize(imutx.convert_lines(SAMPLE)[0], {})
        self.assertEqual(s["ops"], {"open": 1, "ioctl": 3, "write": 3, "read": 3})
        self.assertEqual(s["errnos"], {"EAGAIN": 1, "EIO": 1})
        self.assertEqual(s["stdout"]["ready"], 1)
        self.assertEqual(s["stdout"]["sample"], 1)
        self.assertNotIn("hex", json.dumps(s))


if __name__ == "__main__":
    unittest.main()
