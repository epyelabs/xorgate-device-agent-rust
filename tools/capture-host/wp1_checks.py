#!/usr/bin/env python3
"""Phase 7a WP1 rig checks for the capture host (run in the replay-rig image,
the REAL gst-launch-1.0 1.22 next to the host). Mac Docker only.

    wp1_checks.py timing --host BIN [--fps 15|30] [--seg 4] [--secs 22]
        Record argv (core builder tokens from argv-matrix.json, camera swapped
        for `videotestsrc is-live=true pattern=ball`, max-size-time = --seg),
        run the host and gst-launch on it, SIGINT after --secs. Checks: the
        XG-FRAGMENT ids == the %05d file sequences; both programs write the
        same file names; lag within the guard; startTs spacing = seg ± 70 ms;
        fragment 0 vs the driver's PLAYING wall; XG-FRAGMENT-END vs the last
        file's duration (gst-discoverer-1.0).
    wp1_checks.py nice --host BIN
        Both programs, record argv; the driver renices the pid to 10 right
        after spawn (as the supervisor does); every thread's nice at PLAYING+3 s.
    wp1_checks.py bridge-stop --host BIN
        Both programs, bridge-only argv; SIGINT at PLAYING+2 s; what is left in
        /dev/shm (shmpipe.<pid>.*) and the socket path.
    wp1_checks.py rss --host BIN [--secs 600]
        Both programs side by side on the 720p15-1mbps record argv for --secs,
        sampling /proc/<pid>/stat (utime+stime) and VmRSS every 10 s.

Every subcommand prints one JSON summary and exits 0 iff its checks pass.
"""
import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import transcript as tr  # noqa: E402

GST = "/usr/bin/gst-launch-1.0"
WORK = "/work"
TIMING_RX = re.compile(r"^(XG-FRAGMENT|XG-FRAGMENT-END) (\{.*\})$")
CLK_TCK = os.sysconf("SC_CLK_TCK")


def record_argv(fps=15, seg=None, out=None, sock=None):
    m, cases = tr.load_matrix()
    argv = tr.swap_source(m, cases["record/720p15-1mbps/sw-h264"])
    res, caps_done = [], False
    for t in argv:
        if fps != 15 and not caps_done and t.startswith("video/x-raw,format=I420,"):
            # The RECORD caps only (the first caps token); the bridge caps
            # stay 15/1 by design.
            t = t.replace("framerate=15/1", f"framerate={fps}/1")
            caps_done = True
        if fps != 15 and t.startswith("key-int-max="):
            t = f"key-int-max={fps * 2}"
        if seg is not None and t.startswith("max-size-time="):
            t = f"max-size-time={seg * 1_000_000_000}"
        if out and t.startswith("location="):
            t = f"location={out}/%05d.mp4"
        if sock and t.startswith("socket-path="):
            t = f"socket-path={sock}"
        res.append(t)
    return res


def bridge_argv(sock=None):
    m, cases = tr.load_matrix()
    argv = tr.swap_source(m, cases["bridge"])
    return [f"socket-path={sock}" if sock and t.startswith("socket-path=") else t for t in argv]


class Proc:
    """A capture program with both pipes drained line by line (fd, wall ms)."""

    def __init__(self, program, argv, renice=None):
        is_host = program != GST
        a0 = ["xorgate-capture", "__capture-host"] if is_host else ["gst-launch-1.0"]
        self.lines = []
        self.cond = threading.Condition()
        self.p = subprocess.Popen(a0 + argv, executable=program, stdin=subprocess.DEVNULL,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        if renice is not None:
            os.setpriority(os.PRIO_PROCESS, self.p.pid, renice)
        self.th = [threading.Thread(target=self._pump, args=(self.p.stdout, 1), daemon=True),
                   threading.Thread(target=self._pump, args=(self.p.stderr, 2), daemon=True)]
        for t in self.th:
            t.start()

    def _pump(self, stream, fd):
        buf = b""
        while True:
            c = stream.read1(65536)
            if not c:
                break
            buf += c
            while b"\n" in buf:
                ln, buf = buf.split(b"\n", 1)
                with self.cond:
                    self.lines.append((fd, time.time() * 1000.0, ln.decode("utf-8", "replace")))
                    self.cond.notify_all()

    def wait_line(self, rx, timeout):
        r = re.compile(rx)
        end = time.monotonic() + timeout
        with self.cond:
            while True:
                for l in self.lines:
                    if r.search(l[2]):
                        return l
                left = end - time.monotonic()
                if left <= 0 or self.p.poll() is not None:
                    return None
                self.cond.wait(min(left, 0.05))

    def stop(self, sig=signal.SIGINT, timeout=25):
        if self.p.poll() is None:
            os.kill(self.p.pid, sig)
        try:
            self.p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(self.p.pid, signal.SIGKILL)
            self.p.wait()
        for t in self.th:
            t.join(timeout=5)
        return self.p.returncode


def fresh(d):
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
    os.chmod(d, 0o777)


def discover_duration_ms(path):
    """The media span of an mp4 (first PTS to last PTS + duration), read with
    the real demuxer: `qtdemux ! fakesink silent=false -v` prints one
    `chain` line per sample (the rig image has no gst-discoverer)."""
    out = subprocess.run([GST, "-v", "filesrc", f"location={path}", "!", "qtdemux", "!", "fakesink", "silent=false"],
                         capture_output=True, text=True, timeout=60).stdout
    rx = re.compile(r"pts: (\d+):(\d+):(\d+)\.(\d+), duration: (\d+):(\d+):(\d+)\.(\d+)")

    def ns(h, m, s_, f):
        return ((int(h) * 60 + int(m)) * 60 + int(s_)) * 1_000_000_000 + int(f.ljust(9, "0")[:9])
    pts = []
    for m in rx.finditer(out):
        g = m.groups()
        pts.append((ns(*g[:4]), ns(*g[4:])))
    if not pts:
        return None
    first = min(p for p, _ in pts)
    last = max(p + d for p, d in pts)
    return (last - first) / 1e6


def cmd_timing(a):
    errs, res = [], {"fps": a.fps, "segSecs": a.seg, "secs": a.secs}
    frame_ms = 1000.0 / a.fps
    runs = {}
    for who, prog in (("host", a.host), ("gst", GST)):
        out = f"{WORK}/{who}-out"
        fresh(out)
        fresh(f"{WORK}/shm")
        argv = record_argv(a.fps, a.seg, out, f"{WORK}/shm/{who}.sock")
        p = Proc(prog, argv)
        clk = p.wait_line(r"^New clock:", 20)
        time.sleep(a.secs)
        rc = p.stop()
        files = sorted(os.listdir(out))
        runs[who] = dict(rc=rc, files=files, clock_wall=clk[1] if clk else None, lines=p.lines, out=out)
    h, g = runs["host"], runs["gst"]
    res["files"] = {"host": h["files"], "gst": g["files"]}
    want = [f"{i:05d}.mp4" for i in range(len(h["files"]))]
    if h["files"] != want:
        errs.append(f"host files are not %05d from 0: {h['files']}")
    if [f for f in h["files"]] != g["files"]:
        # Count can differ by one at the stop instant; the NAMES must follow %05d.
        common = min(len(h["files"]), len(g["files"]))
        if h["files"][:common] != g["files"][:common]:
            errs.append(f"file names differ host {h['files']} gst {g['files']}")
        res["fileCountNote"] = "counts differ at the stop instant" if len(h["files"]) != len(g["files"]) else None
    starts, ends = [], []
    for fd, wall, ln in h["lines"]:
        m = TIMING_RX.match(ln)
        if m:
            if fd != 1:
                errs.append(f"timing line on fd{fd}")
            (ends if m.group(1) == "XG-FRAGMENT-END" else starts).append(json.loads(m.group(2)))
    if any(TIMING_RX.match(ln) for _, _, ln in g["lines"]):
        errs.append("gst-launch printed a timing line?")
    ids = [s["id"] for s in starts]
    seqs = [int(f[:5]) if f[:5].isdigit() else f for f in h["files"]]
    if ids != seqs:
        errs.append(f"XG-FRAGMENT ids {ids} != %05d sequences {seqs}")
    lags = [s["lagMs"] for s in starts]
    if any(not (0 <= x <= 7000) for x in lags):
        errs.append(f"lag outside guard {lags}")
    st = [s["startTs"] for s in starts]
    spacing = [st[i + 1] - st[i] for i in range(len(st) - 1)]
    if any(abs(d - a.seg * 1000) > 70 for d in spacing):
        errs.append(f"startTs spacing {spacing} not {a.seg * 1000} ± 70")
    res["xg"] = {"ids": ids, "lagMs": lags, "startTs": st, "spacingMs": spacing}
    if h["clock_wall"] and st:
        # The driver's PLAYING reference: the wall time the `New clock:` line
        # was READ (an upper bound on the host's PLAYING instant).
        dev = [round(st[n] - (h["clock_wall"] + n * a.seg * 1000), 1) for n in range(len(st))]
        res["startTs_minus_driverPlayingWall_plus_NxSeg_ms"] = dev
        if any(abs(x) > 300 for x in dev):
            errs.append(f"startTs vs the driver's PLAYING wall + N·seg outside ±300 ms: {dev}")
    # END: exactly one, for the last id, after a clean EOS.
    clean = any(ln == 'Got EOS from element "pipeline0".' for _, _, ln in h["lines"])
    if not clean:
        errs.append("host: no clean EOS")
    if len(ends) != 1 or not ids or ends[0]["id"] != ids[-1]:
        errs.append(f"END {ends} for ids {ids}")
    elif h["files"]:
        last = os.path.join(h["out"], h["files"][-1])
        dur = discover_duration_ms(last)
        xg_dur = ends[0]["endTs"] - st[-1]
        res["lastFragment"] = {"file": h["files"][-1], "discovererMs": dur, "endTsMinusStartTs": xg_dur,
                               "endMinusStartMono_ms": (ends[0]["endMonoNs"] - starts[-1]["frameMonoNs"]) / 1e6}
        if dur is None or abs(xg_dur - dur) > frame_ms + 20:
            errs.append(f"END duration {xg_dur} ms vs file {dur} ms (> 1 frame + 20 ms)")
    # Non-final fragments: spacing vs the file's own media duration.
    durs = [discover_duration_ms(os.path.join(h["out"], f)) for f in h["files"]]
    res["fileDurationsMs"] = durs
    for i, d in enumerate(spacing):
        if i >= len(durs):
            errs.append(f"fragment {i}: no file for its startTs delta {d}")
        elif durs[i] is not None and abs(d - durs[i]) > frame_ms + 20:
            errs.append(f"fragment {i}: startTs delta {d} vs file duration {durs[i]}")
    res["exit"] = {"host": h["rc"], "gst": g["rc"]}
    res["hostStderr"] = [ln for fd, _, ln in h["lines"] if fd == 2]
    res["errors"] = errs
    res["pass"] = not errs
    print(json.dumps(res, indent=1))
    return 0 if not errs else 1


def task_nices(pid):
    out = []
    for tid in sorted(os.listdir(f"/proc/{pid}/task"), key=int):
        try:
            with open(f"/proc/{pid}/task/{tid}/stat") as f:
                stat = f.read()
            with open(f"/proc/{pid}/task/{tid}/comm") as f:
                comm = f.read().strip()
        except OSError:
            continue
        out.append((comm, int(stat.rsplit(")", 1)[1].split()[16])))
    return out


def cmd_nice(a):
    res, errs = {}, []
    for who, prog in (("host", a.host), ("gst", GST)):
        fresh(f"{WORK}/out")
        fresh(f"{WORK}/shm")
        p = Proc(prog, record_argv(out=f"{WORK}/out"), renice=10)
        p.wait_line(r"^New clock:", 20)
        time.sleep(3)
        n = task_nices(p.p.pid)
        p.stop()
        res[who] = {"threads": len(n), "nices": sorted({v for _, v in n}),
                    "not10": [x for x in n if x[1] != 10]}
        if who == "host" and res[who]["not10"]:
            errs.append(f"host threads not at nice 10: {res[who]['not10']}")
    res["errors"], res["pass"] = errs, not errs
    print(json.dumps(res, indent=1))
    return 0 if not errs else 1


def cmd_bridge_stop(a):
    res, errs = {}, []
    for who, prog in (("host", a.host), ("gst", GST)):
        for i in range(a.runs):
            fresh(f"{WORK}/shm")
            sock = f"{WORK}/shm/cam0.sock"
            p = Proc(prog, bridge_argv(sock))
            p.wait_line(r"^New clock:", 20)
            time.sleep(2)
            sock_live = os.path.exists(sock)
            rx = re.compile(r"^shmpipe\.\s*%d\." % p.p.pid)
            shm_live = sorted(n for n in os.listdir("/dev/shm") if rx.match(n))
            rc = p.stop()
            shm_left = sorted(n for n in os.listdir("/dev/shm") if rx.match(n))
            row = {"exit": rc, "whileRunning": {"socket": sock_live, "shm": shm_live},
                   "after": {"socket": os.path.exists(sock), "shm": shm_left}}
            res.setdefault(who, []).append(row)
            for n in shm_left:
                os.unlink(os.path.join("/dev/shm", n))
            if who == "host" and (row["after"]["socket"] or shm_left):
                errs.append(f"host run {i} left {row['after']}")
            if who == "host" and not (sock_live and shm_live):
                errs.append(f"host run {i}: nothing to clean while running? {row}")
    res["errors"], res["pass"] = errs, not errs
    print(json.dumps(res, indent=1))
    return 0 if not errs else 1


def proc_sample(pid):
    with open(f"/proc/{pid}/stat") as f:
        st = f.read().rsplit(")", 1)[1].split()
    ticks = int(st[11]) + int(st[12])  # utime + stime (fields 14, 15)
    rss = hwm = None
    with open(f"/proc/{pid}/status") as f:
        for l in f:
            if l.startswith("VmRSS:"):
                rss = int(l.split()[1])
            elif l.startswith("VmHWM:"):
                hwm = int(l.split()[1])
    return ticks, rss, hwm


def cmd_rss(a):
    procs = {}
    for who, prog in (("host", a.host), ("gst", GST)):
        out = f"{WORK}/{who}-out"
        fresh(out)
        os.makedirs(f"{WORK}/shm", exist_ok=True)
        procs[who] = Proc(prog, record_argv(out=out, sock=f"{WORK}/shm/{who}.sock"), renice=10)
    for p in procs.values():
        p.wait_line(r"^New clock:", 30)
    t0 = time.monotonic()
    base = {w: proc_sample(p.p.pid) for w, p in procs.items()}
    series = {w: [] for w in procs}
    last = dict(base)
    lt = t0
    while time.monotonic() - t0 < a.secs:
        time.sleep(10)
        now = time.monotonic()
        for w, p in procs.items():
            if p.p.poll() is not None:
                continue
            s = proc_sample(p.p.pid)
            cpu = (s[0] - last[w][0]) / CLK_TCK / (now - lt) * 100
            series[w].append({"t": round(now - t0), "cpuPct": round(cpu, 2), "rssKiB": s[1], "hwmKiB": s[2]})
            last[w] = s
        lt = now
    el = time.monotonic() - t0
    res = {"secs": round(el), "argv": "record/720p15-1mbps/sw-h264 (videotestsrc is-live pattern=ball), nice 10",
           "loadavg": open("/proc/loadavg").read().strip()}
    for w, p in procs.items():
        s = proc_sample(p.p.pid)
        rss = [x["rssKiB"] for x in series[w]]
        res[w] = {
            "meanCpuPct": round((s[0] - base[w][0]) / CLK_TCK / el * 100, 2),
            "rssKiB": {"first": rss[0] if rss else None, "last": rss[-1] if rss else None,
                       "max": max(rss) if rss else None, "min": min(rss) if rss else None},
            "hwmKiB": s[2],
            "series": series[w],
        }
        p.stop()
    res["pass"] = True
    print(json.dumps(res, indent=1))
    return 0


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    t = sub.add_parser("timing")
    t.add_argument("--host", required=True)
    t.add_argument("--fps", type=int, default=15)
    t.add_argument("--seg", type=int, default=4)
    t.add_argument("--secs", type=float, default=22)
    n = sub.add_parser("nice")
    n.add_argument("--host", required=True)
    b = sub.add_parser("bridge-stop")
    b.add_argument("--host", required=True)
    b.add_argument("--runs", type=int, default=3)
    r = sub.add_parser("rss")
    r.add_argument("--host", required=True)
    r.add_argument("--secs", type=int, default=600)
    a = ap.parse_args()
    os.makedirs(WORK, exist_ok=True)
    return {"timing": cmd_timing, "nice": cmd_nice, "bridge-stop": cmd_bridge_stop, "rss": cmd_rss}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
