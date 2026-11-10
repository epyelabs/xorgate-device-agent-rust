#!/usr/bin/env python3
"""Offline tests of tools/replay/timing_truth.py (the WP3 ground-truth timing
checker). Synthetic shim logs + sidecars from a simulated timeline; every
bound and both references are flipped to FAIL. Run:
  python3 -I tools/replay/tests/timing_truth_test.py   (prints PASS/FAIL lines)
"""
import contextlib
import io
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
        self.mp4s = None  # None = one non-empty mp4 per sidecar
        self.births = {}  # seq -> the player's statx birth (default: the sidecar's startTs)

    def eos(self, w):
        self.shim.append({"k": "line", "pid": 77, "fd": 1, "line": "handling interrupt.", "w": w})

    def got_eos(self, w):
        self.shim.append({"k": "line", "pid": 77, "fd": 1, "line": 'Got EOS from element "pipeline0".', "w": w})

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
        seqs = sorted(self.cars) if self.mp4s is None else self.mp4s
        files = {}
        for q in seqs:
            b = self.births.get(q, self.cars[q][0] if q in self.cars else None)
            files["video/cam0/%s/%05d.mp4" % (SID, q)] = {"size": 100, **({} if b is None else {"birth": b})}
        with open(os.path.join(self.dir, "recordings.json"), "w") as f:
            json.dump({"files": files}, f)
        for n, (st, anchor, where) in self.cars.items():
            d = os.path.join(self.dir, where, SID)
            os.makedirs(d, exist_ok=True)
            with open(os.path.join(d, "%05d.json" % n), "w") as f:
                body = {"seq": n, "startTs": st, "durationMs": self.seg * 1000, "anchor": anchor}
                if anchor is None:
                    del body["anchor"]
                json.dump(body, f)
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

    def test_a_legacy_last_fragment_cut_inside_a_gop_is_a_cut_not_a_fail(self):
        # The stop's EOS < 1 GOP after fragment 4's rotation: its file opens
        # at the EOS flush, +700 ms after its frame. Last fragment only;
        # never later than the band; a middle fragment at +700 still FAILS.
        def side(seq_off, eos=709, mp4s=None):
            shutil.rmtree(self.tmp, ignore_errors=True)
            os.makedirs(self.tmp)
            s = Side(self.tmp, tool="gst-launch-1.0")
            for k in range(5):
                frame = P + 9 + k * SEG * 1000
                d = seq_off.get(k, 2_050)
                s.car(k, (frame + d) if k else (P + 9 + 2_050), "boundary" if k == 0 else "fstat")
            if eos is not None:
                s.eos(P + 4 * SEG * 1000 + eos)
            s.mp4s = mp4s
            return s
        r = self.run_check(side({4: 700}))
        self.assertEqual(r["verdict"], "PASS", r)
        self.assertTrue(r["rows"][-1].get("cut"), r["rows"][-1])
        self.assertEqual(self.run_check(side({2: 700}))["verdict"], "FAIL")
        self.assertEqual(self.run_check(side({4: 2_600}))["verdict"], "FAIL")
        self.assertEqual(self.run_check(side({4: -20}))["verdict"], "FAIL")
        # WP3 review F4: the last SIDECAR is not the last mp4 (a later mp4
        # lost its sidecar) -> not a cut -> FAILS.
        self.assertEqual(self.run_check(side({4: 700}, mp4s=[0, 1, 2, 3, 4, 5]))["verdict"], "FAIL")
        # The stop came one GOP or more after the rotation keyframe -> FAILS.
        self.assertEqual(self.run_check(side({4: 700}, eos=2_100))["verdict"], "FAIL")
        # The file did not open at the stop (|d - e| > slack) -> FAILS.
        self.assertEqual(self.run_check(side({4: 700}, eos=400))["verdict"], "FAIL")
        # No stop line at all -> FAILS.
        self.assertEqual(self.run_check(side({4: 700}, eos=None))["verdict"], "FAIL")

    def test_the_cut_window_spans_interrupt_to_got_eos_with_the_review_conditions(self):
        # F-7A-CUT (review-close): a-rec-boot-defer cam1 00010 relative to
        # expected(10): handling interrupt. +742, file born +806, startTs
        # +807, Got EOS +808 (gst's send_event took 65 ms). With both
        # markers the file must open in [interrupt - 20, Got EOS + 20];
        # startTs must be the player's birth within 1 ms.
        def side(d, intr=742, got=808, birth_off=-1, mp4s=None):
            shutil.rmtree(self.tmp, ignore_errors=True)
            os.makedirs(self.tmp)
            s = Side(self.tmp, tool="xorgate-capture")
            for k in range(5):
                frame = P + k * SEG * 1000
                off = d if k == 4 else 2_010
                s.car(k, (frame + off) if k else (P + 2_010), "boundary" if k == 0 else "fstat")
            exp4 = P + 4 * SEG * 1000
            if intr is not None:
                s.eos(exp4 + intr)
            if got is not None:
                s.got_eos(exp4 + got)
            s.births[4] = exp4 + d + birth_off
            s.mp4s = mp4s
            return s
        r = self.run_check(side(807))
        self.assertEqual(r["verdict"], "PASS", r)
        self.assertTrue(r["rows"][-1].get("cut"), r["rows"][-1])
        self.assertEqual(r["rows"][-1]["eos_window"], [742, 808], r["rows"][-1])
        # Negatives, each FAILS:
        # (1) the file opened after Got EOS + 20;
        self.assertEqual(self.run_check(side(808 + 21))["verdict"], "FAIL")
        # (2) the file opened before interrupt - 20;
        self.assertEqual(self.run_check(side(742 - 21))["verdict"], "FAIL")
        # (3) in the window but not the session's last non-empty mp4;
        self.assertEqual(self.run_check(side(807, mp4s=[0, 1, 2, 3, 4, 5]))["verdict"], "FAIL")
        # (4) no Got EOS line: today's one-marker rule, +65 from the interrupt;
        self.assertEqual(self.run_check(side(742 + 65, got=None))["verdict"], "FAIL")
        # (5) a stalled EOS: Got EOS > one GOP after the interrupt, d inside it;
        self.assertEqual(self.run_check(side(1_200, intr=100, got=100 + 2_001))["verdict"], "FAIL")
        # (6) in the window but startTs is not the player's birth (2 ms off).
        self.assertEqual(self.run_check(side(807, birth_off=-2))["verdict"], "FAIL")
        # Controls: the window edges themselves pass; the same one-marker
        # shape within 20 ms still passes (today's rule unchanged).
        self.assertEqual(self.run_check(side(808 + 20))["verdict"], "PASS")
        self.assertEqual(self.run_check(side(742 - 20))["verdict"], "PASS")
        self.assertEqual(self.run_check(side(742 + 15, got=None))["verdict"], "PASS")
        # No recorded birth at all: never a cut.
        s = side(807)
        s.births[4] = None
        self.assertEqual(self.run_check(s)["verdict"], "FAIL")

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

    def test_expect_pins_the_mode_and_is_required(self):
        # WP3 review F1: the run's configured mode, never inferred: an exact
        # run whose sidecars are all legacy (an agent that ignored its stamps)
        # FAILS under --expect exact, and an exact session under --expect legacy.
        s = Side(self.tmp, tool="xorgate-capture")
        for k in range(5):
            frame = P + 9 + k * SEG * 1000
            s.car(k, (P + 9 + 2_050) if k == 0 else frame + 2_050, "boundary" if k == 0 else "fstat")
        s.write()
        res = tt.check(self.tmp, "sidecars", 1, "exact")
        self.assertEqual(res[0]["verdict"], "FAIL", res)
        self.assertTrue(any("--expect exact" in e for e in res[0]["errors"]), res[0]["errors"])
        self.assertEqual(tt.check(self.tmp, "sidecars", 1, "legacy")[0]["verdict"], "PASS")
        shutil.rmtree(self.tmp)
        os.makedirs(self.tmp)
        exact_side(self.tmp).write()
        self.assertEqual(tt.check(self.tmp, "sidecars", 1, "legacy")[0]["verdict"], "FAIL")
        self.assertEqual(tt.check(self.tmp, "sidecars", 1, "exact")[0]["verdict"], "PASS")
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as e:
                tt.main([self.tmp])  # no --expect with --source sidecars
        self.assertEqual(e.exception.code, 2)

    def test_a_sidecar_without_an_anchor_is_never_pts(self):
        # Round-2 N5b: a missing anchor key does not pass as an exact row.
        s = exact_side(self.tmp)
        st, _, w = s.cars[2]
        s.cars[2] = (st, None, w)
        s.write()
        for mode in ("exact", "legacy"):
            r = tt.check(self.tmp, "sidecars", 1, mode)[0]
            self.assertEqual(r["verdict"], "FAIL", (mode, r))
            self.assertTrue(any("without an anchor" in e for e in r["errors"]), r["errors"])

    def test_exit_codes(self):
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(tt.main([self.tmp, "--expect", "exact"]), 2)  # nothing to check
            exact_side(self.tmp).write()
            self.assertEqual(tt.main([self.tmp, "--expect", "exact"]), 0)
            exact_side(self.tmp, off={1: 500}).write()
            self.assertEqual(tt.main([self.tmp, "--expect", "exact"]), 1)


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
