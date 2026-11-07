#!/usr/bin/env python3
"""Offline tests of tools/replay/timing_truth.py (the WP3 ground-truth timing
checker). Synthetic shim logs + sidecars from a simulated timeline; every
bound and both references are flipped to FAIL. Run:
  python3 -I tools/replay/tests/timing_truth_test.py   (prints PASS/FAIL lines)
"""
import json
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
import timing_truth as tt  # noqa: E402

SID = "0e9b7f3c-0000-4000-8000-000000000001"
P = 1_762_400_000_000  # the shim tee's New clock: wall
SEG = 10


def argv(fps=15, seg=SEG, sid=SID):
    return [
        "-e", "videotestsrc", "is-live=true", "pattern=ball",
        "!", "video/x-raw,format=I420,width=1280,height=720,framerate=%d/1" % fps,
        "!", "tee", "name=t", "t.", "!", "queue", "!", "x264enc", "key-int-max=%d" % (2 * fps),
        "!", "h264parse", "!", "splitmuxsink", "muxer=mp4mux fragment-duration=1000",
        "location=/w/b/home/recordings/video/cam0/%s/%%05d.mp4" % sid,
        "max-size-time=%d" % (seg * 1_000_000_000),
    ]


class Side:
    """A side dir: shim.ndjson + sidecars."""

    def __init__(self, root, tool="xorgate-capture", fps=15, seg=SEG, playing=P):
        self.dir = root
        self.fps, self.seg, self.playing = fps, seg, playing
        self.shim = [
            {"k": "spawn", "tool": tool, "pid": 77, "w": playing - 400,
             "argv": (["__capture-host"] if tool == "xorgate-capture" else []) + argv(fps, seg)},
            {"k": "line", "pid": 77, "fd": 1, "line": "Setting pipeline to PLAYING ...", "w": playing - 1},
            {"k": "line", "pid": 77, "fd": 1, "line": "New clock: GstSystemClock", "w": playing},
        ]
        self.cars = {}

    def host_line(self, n, frame_wall, lag, f0_mono=5_000_000_000, mono_off_ms=0):
        """The host's XG-FRAGMENT for fragment n whose first frame is at
        `frame_wall` (its pipeline-mono instant = f0 + n·seg)."""
        frame_mono = f0_mono + n * self.seg * 1_000_000_000 + mono_off_ms * 1_000_000
        cb_mono = frame_mono + lag * 1_000_000
        cb_wall = frame_wall + lag
        self.shim.append({"k": "line", "pid": 77, "fd": 1, "w": cb_wall + 1, "line": "XG-FRAGMENT " + json.dumps({
            "v": 1, "id": n, "frameMonoNs": frame_mono, "cbMonoNs": cb_mono, "cbWallMs": cb_wall,
            "lagMs": lag, "startTs": cb_wall - lag}, separators=(",", ":"))})

    def car(self, n, st, anchor, where="final/recordings/video/cam0"):
        self.cars[n] = (st, anchor, where)

    def write(self):
        with open(os.path.join(self.dir, "shim.ndjson"), "w") as f:
            for v in self.shim:
                f.write(json.dumps(v) + "\n")
        for n, (st, anchor, where) in self.cars.items():
            d = os.path.join(self.dir, where, SID)
            os.makedirs(d, exist_ok=True)
            with open(os.path.join(d, "%05d.json" % n), "w") as f:
                json.dump({"seq": n, "startTs": st, "durationMs": self.seg * 1000, "anchor": anchor}, f)
        return self.dir


def exact_side(root, fps=15, n=5, first=12, off=None, shim_shift=0, host_off_ms=0):
    """Exact: frames at P + 9 + N·seg; the sidecar startTs = that frame
    (+ `off` per seq); the shim's New clock read `shim_shift` ms later."""
    s = Side(root, fps=fps, playing=P + shim_shift)
    for k in range(n):
        frame = P + 9 + k * SEG * 1000
        s.host_line(k, frame, first if k == 0 else 2_010, mono_off_ms=host_off_ms if k else 0)
        s.car(k, frame + (off or {}).get(k, 0), "pts")
    return s


class T(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="tt-")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_check(self, side, source="sidecars", min_fragments=1):
        res = tt.check(side.write(), source, min_fragments)
        checked = [r for r in res if r["verdict"] != "SKIP"]
        self.assertEqual(len(checked), 1, res)
        return checked[0]

    def test_exact_within_one_frame_passes_both_references(self):
        r = self.run_check(exact_side(self.tmp), min_fragments=5)
        self.assertEqual(r["verdict"], "PASS", r)
        self.assertEqual(r["mode"], "exact")
        self.assertAlmostEqual(r["bound_ms"], 86.7, places=1)
        self.assertEqual([x["d_shim"] for x in r["rows"]], [9.0] * 5)
        self.assertEqual([x["d_host"] for x in r["rows"]], [0.0] * 5)

    def test_exact_off_by_more_than_a_frame_plus_20_fails(self):
        r = self.run_check(exact_side(self.tmp, off={3: 87 - 9 + 1}))
        self.assertEqual(r["verdict"], "FAIL", r)
        self.assertTrue(any("fragment 3" in e for e in r["errors"]), r["errors"])
        # one ms inside: passes
        shutil.rmtree(self.tmp)
        os.makedirs(self.tmp)
        r = self.run_check(exact_side(self.tmp, off={3: 86 - 9}))
        self.assertEqual(r["verdict"], "PASS", r)

    def test_fragment_zero_is_checked_too(self):
        r = self.run_check(exact_side(self.tmp, off={0: -120}))
        self.assertEqual(r["verdict"], "FAIL", r)
        self.assertTrue(any("fragment 0" in e for e in r["errors"]), r["errors"])

    def test_the_shim_reference_is_independent_of_the_host(self):
        # The host's own mapping is perfect, but the shim's tee saw PLAYING
        # 200 ms later than the frames imply: FAIL on the shim reference alone.
        r = self.run_check(exact_side(self.tmp, shim_shift=200))
        self.assertEqual(r["verdict"], "FAIL", r)
        self.assertTrue(all("shim reference" in e for e in r["errors"]), r["errors"])
        self.assertTrue(all(x["d_host"] == 0.0 for x in r["rows"]))

    def test_the_host_reference_is_checked_too(self):
        # The host's own pipeline-time spacing is 150 ms off from fragment 1
        # on (its mono instants do not match N·step): FAIL on the host reference.
        r = self.run_check(exact_side(self.tmp, host_off_ms=150))
        self.assertEqual(r["verdict"], "FAIL", r)
        self.assertTrue(any("host reference" in e for e in r["errors"]), r["errors"])
        self.assertFalse(any("shim reference" in e for e in r["errors"]), r["errors"])

    def test_a_missing_host_line_fails_the_exact_check(self):
        s = exact_side(self.tmp)
        s.shim = [v for v in s.shim if '"id":2,' not in v.get("line", "")]
        r = self.run_check(s)
        self.assertEqual(r["verdict"], "FAIL", r)

    def test_thirty_fps_has_the_narrower_bound(self):
        r = self.run_check(exact_side(self.tmp, fps=30, off={2: 60 - 9}))
        self.assertAlmostEqual(r["bound_ms"], 53.3, places=1)
        self.assertEqual(r["verdict"], "FAIL", r)
        shutil.rmtree(self.tmp)
        os.makedirs(self.tmp)
        r = self.run_check(exact_side(self.tmp, fps=15, off={2: 60 - 9}))
        self.assertEqual(r["verdict"], "PASS", r)

    def test_legacy_control_is_two_seconds_late_with_boundary(self):
        s = Side(self.tmp, tool="gst-launch-1.0")
        for k in range(5):
            frame = P + 9 + k * SEG * 1000
            birth = frame + 2_050
            s.car(k, (frame + SEG * 1000 + 2_050 - SEG * 1000) if k == 0 else birth, "boundary" if k == 0 else "fstat")
        r = self.run_check(s, min_fragments=5)
        self.assertEqual((r["verdict"], r["mode"]), ("PASS", "legacy"), r)
        # +1.7 s: outside 2.1 s +/- 0.3 s
        s.car(2, P + 9 + 2 * SEG * 1000 + 1_700, "fstat")
        r = self.run_check(s)
        self.assertEqual(r["verdict"], "FAIL", r)
        s.car(2, P + 9 + 2 * SEG * 1000 + 2_050, "fstat")
        s.car(0, P + 9 + 2_050, "fstat")
        r = self.run_check(s)
        self.assertEqual(r["verdict"], "FAIL", r)
        self.assertTrue(any("anchor" in e for e in r["errors"]), r["errors"])

    def test_gaps_mixed_sessions_and_min_fragments_fail(self):
        s = exact_side(self.tmp)
        del s.cars[2]
        self.assertEqual(self.run_check(s)["verdict"], "FAIL")
        s = exact_side(self.tmp)
        st, _, w = s.cars[3]
        s.cars[3] = (st, "fstat", w)
        self.assertEqual(self.run_check(s)["verdict"], "FAIL")
        s = exact_side(self.tmp, n=3)
        self.assertEqual(self.run_check(s, min_fragments=5)["verdict"], "FAIL")

    def test_s3_bodies_count_and_lines_mode_reads_the_host(self):
        s = exact_side(self.tmp)
        for k in list(s.cars):
            st, a, _ = s.cars[k]
            s.cars[k] = (st, a, "s3/media-sessions/v1/dev/cam0")
        self.assertEqual(self.run_check(s)["verdict"], "PASS")
        s = exact_side(self.tmp)
        s.cars = {}
        r = self.run_check(s, source="lines", min_fragments=5)
        self.assertEqual(r["verdict"], "PASS", r)

    def test_exit_codes(self):
        self.assertEqual(tt.main([self.tmp]), 2)  # nothing to check
        exact_side(self.tmp).write()
        self.assertEqual(tt.main([self.tmp]), 0)
        exact_side(self.tmp, off={1: 500}).write()
        self.assertEqual(tt.main([self.tmp]), 1)


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(T)
    fails = 0
    for t in suite:
        r = unittest.TestResult()
        t.run(r)
        ok = r.wasSuccessful()
        fails += 0 if ok else 1
        print(("PASS " if ok else "FAIL ") + t.id().split(".")[-1])
        for _, tb in r.failures + r.errors:
            print(tb)
    print("timing_truth: %d tests, %d failed" % (suite.countTestCases(), fails))
    sys.exit(1 if fails else 0)
