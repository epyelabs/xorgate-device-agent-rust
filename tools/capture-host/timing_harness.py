#!/usr/bin/env python3
"""Phase 7a WP3: the capture-host timing harness (K5's host half, no agent).

Runs inside the replay-rig image (`tools/capture-host/run-rig.sh timing`).
For each preset case it starts N sessions, one at a time, each through the
REAL rig shim `tools/replay/shims/xorgate-capture` (which swaps the camera for
`videotestsrc is-live=true pattern=ball`, tees every line with its own wall
stamp into <work>/shim.ndjson, and execs the host = RIG_REAL_HOST), lets it
record `--secs` seconds of `--seg`-second fragments, SIGINTs it (EOS), and
then runs the ground-truth checker `tools/replay/timing_truth.py --source
lines` on that session: every fragment's host `startTs` against the shim's
teed `New clock:` wall (+ N x seg) and against the host's own base-time
mapping, within one frame + 20 ms.

  timing_harness.py --host BIN --out DIR [--case record/720p15-1mbps/sw-h264 ...]
                    [--sessions 2] [--seg 10] [--secs 57] [--min-fragments 5]

Writes DIR/<case-slug>/s<i>/{shim.ndjson, truth.json, host.stdout, files.txt}
and DIR/summary.json; exit 0 = every session PASSES.
"""
import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import uuid

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
MATRIX = os.path.join(REPO, "fixtures/capture-host/argv-matrix.json")
SHIM = os.path.join(REPO, "tools/replay/shims/xorgate-capture")
TRUTH = os.path.join(REPO, "tools/replay/timing_truth.py")
DEFAULT_CASES = ["record/720p15-1mbps/sw-h264", "record/720p30-2mbps/sw-h264"]


def case_argv(name):
    cases = json.load(open(MATRIX))["cases"]
    for c in cases if isinstance(cases, list) else cases.values():
        if c.get("name") == name:
            return list(c["argv"])
    raise SystemExit("no matrix case %r" % name)


def session_argv(argv, work, sid, seg):
    out = []
    for t in argv:
        if t.startswith("location="):
            t = "location=%s/home/recordings/video/cam0/%s/%%05d.mp4" % (work, sid)
        elif t.startswith("socket-path="):
            t = "socket-path=%s/home/shm/cam0.sock" % work
        elif t.startswith("max-size-time="):
            t = "max-size-time=%d" % (seg * 1_000_000_000)
        out.append(t)
    return out


def run_session(host, case, i, out_dir, seg, secs):
    work = "/tmp/p7a-timing/%s/s%d" % (case.replace("/", "_"), i)
    shutil.rmtree(work, ignore_errors=True)
    sid = str(uuid.uuid4())
    for d in ("bin", "home/shm", "home/recordings/video/cam0/" + sid):
        os.makedirs(os.path.join(work, d), exist_ok=True)
    shim = os.path.join(work, "bin/xorgate-capture")
    shutil.copy(SHIM, shim)
    os.chmod(shim, 0o755)
    argv = ["__capture-host"] + session_argv(case_argv(case), work, sid, seg)
    env = dict(os.environ, RIG_REAL_HOST=host)
    so = open(os.path.join(out_dir, "host.stdout"), "w")
    se = open(os.path.join(out_dir, "host.stderr"), "w")
    t0 = time.time()
    p = subprocess.Popen([shim] + argv, stdout=so, stderr=se, env=env, start_new_session=True)
    time.sleep(secs)
    p.send_signal(signal.SIGINT)
    try:
        rc = p.wait(timeout=20)
    except subprocess.TimeoutExpired:
        p.kill()
        rc = p.wait()
    # the shim's detached tee logs `eof` once the host's pipes close
    log = os.path.join(work, "shim.ndjson")
    for _ in range(100):
        if os.path.exists(log) and '"k":"eof"' in open(log).read():
            break
        time.sleep(0.1)
    shutil.copy(log, os.path.join(out_dir, "shim.ndjson"))
    files = sorted(os.listdir(os.path.join(work, "home/recordings/video/cam0", sid)))
    with open(os.path.join(out_dir, "files.txt"), "w") as f:
        f.write("\n".join(files) + "\n")
    return {"rc": rc, "sid": sid, "files": files, "wall_s": round(time.time() - t0, 1)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--case", action="append")
    ap.add_argument("--sessions", type=int, default=2)
    ap.add_argument("--seg", type=int, default=10)
    ap.add_argument("--secs", type=float, default=57)
    ap.add_argument("--min-fragments", type=int, default=5)
    a = ap.parse_args()
    summary = []
    ok = True
    for case in a.case or DEFAULT_CASES:
        for i in range(a.sessions):
            d = os.path.join(a.out, case.replace("/", "_"), "s%d" % i)
            os.makedirs(d, exist_ok=True)
            load = os.getloadavg()[0]
            r = run_session(a.host, case, i, d, a.seg, a.secs)
            t = subprocess.run([sys.executable, "-I", TRUTH, d, "--source", "lines",
                                "--min-fragments", str(a.min_fragments), "--json", os.path.join(d, "truth.json")],
                               capture_output=True, text=True)
            open(os.path.join(d, "truth.txt"), "w").write(t.stdout + t.stderr)
            print("=== %s s%d rc=%s files=%d load=%.1f truth rc=%d" % (case, i, r["rc"], len(r["files"]), load, t.returncode))
            print(t.stdout, end="")
            truth = json.load(open(os.path.join(d, "truth.json"))) if os.path.exists(os.path.join(d, "truth.json")) else []
            summary.append({"case": case, "session": i, "load1": round(load, 2), "host_rc": r["rc"],
                            "files": r["files"], "truth_rc": t.returncode, "truth": truth})
            ok = ok and t.returncode == 0
    json.dump(summary, open(os.path.join(a.out, "summary.json"), "w"), indent=1)
    print("ALL PASS" if ok else "FAILURES")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
