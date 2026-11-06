#!/usr/bin/env python3
"""Process-contract transcripts for the capture host (Phase 7a WP0, K4).

Runs a capture program (the REAL `gst-launch-1.0` 1.22, or the agent binary
as `xorgate-capture __capture-host`) through fixed scenarios inside the rig
image and records, per scenario:

- every stdout/stderr line with its fd and arrival time (ms since spawn);
- the exit code / terminating signal;
- `/proc/<pid>/status` SigCgt/SigIgn/SigBlk/SigPnd/ShdPnd + Threads of the
  main thread at PLAYING (1 s after `New clock:`), the per-thread SigBlk set,
  and again right after the first `handling interrupt.` line;
- the actions the driver took (signals) with their times.

    transcript.py record --program /usr/bin/gst-launch-1.0 --outdir DIR [--only NAME]
    transcript.py record --program /repo/.../xorgate-rs --arg0 xorgate-capture \
        --prefix __capture-host --outdir DIR
    transcript.py compare REF_DIR GOT_DIR      # exit 0 iff every row matches

The argv of every scenario comes from fixtures/capture-host/argv-matrix.json
(the real core builders), with `libcamerasrc camera-name=<cam>` swapped for
`videotestsrc is-live=true pattern=ball` where the scenario needs a running
source (the rig shim's rule). Runs as root in the rig container; scenario 6
drops to uid/gid 65534 so a 0555 directory is really unwritable.
"""
import argparse
import datetime
import json
import os
import platform
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
MATRIX = os.path.join(REPO, "fixtures", "capture-host", "argv-matrix.json")
WORK = "/work"
NOBODY = 65534

SIGNAMES = {getattr(signal, n): n for n in dir(signal) if n.startswith("SIG") and not n.startswith("SIG_") and isinstance(getattr(signal, n), int)}


def load_matrix():
    with open(MATRIX) as f:
        m = json.load(f)
    return m, {c["name"]: c["argv"] for c in m["cases"]}


def swap_source(m, argv):
    frm, to = m["sourceSwap"]["from"], m["sourceSwap"]["to"]
    out, i = [], 0
    while i < len(argv):
        if argv[i : i + len(frm)] == frm:
            out += to
            i += len(frm)
            continue
        out.append(argv[i])
        i += 1
    return out


def scenarios():
    """name -> dict(desc, argv(m, cases), actions, user, setup, exit_timeout)."""
    def bridge(m, c):
        return swap_source(m, c["bridge"])

    def record(m, c):
        return swap_source(m, c["record/720p15-1mbps/sw-h264"])

    playing = [("line", r"^New clock:", 15), ("sleep", 1.0), ("proc", "playing")]
    return {
        "1-bridge-sigint": dict(
            desc="bridge-only start, SIGINT at PLAYING (no -e)",
            argv=bridge,
            actions=playing + [("sleep", 1.0), ("signal", "SIGINT"), ("line", r"^handling interrupt\.", 5), ("proc", "after_intr")],
        ),
        "2-record-sigint": dict(
            desc="record start (-e), SIGINT after ~5 s (EOS finalises the fragment)",
            argv=record,
            actions=playing + [("sleep", 4.0), ("signal", "SIGINT"), ("line", r"^handling interrupt\.", 5), ("proc", "after_intr")],
            # The snapshot races the shutdown (SIGHUP's watch is removed at
            # `end:`): 4 gst-launch runs gave 3 different values.
            racy=["SigCgt@after_intr"],
        ),
        "2b-record-sigint-twice": dict(
            desc="record start (-e), SIGINT, then a second SIGINT right after 'handling interrupt.'",
            argv=record,
            actions=playing + [("sleep", 4.0), ("signal", "SIGINT"), ("line", r"^handling interrupt\.", 5), ("signal", "SIGINT")],
            # Death by the second SIGINT holds only because this driver reacts
            # ~1 ms after the line: gst-launch SWALLOWS a second SIGINT that
            # lands within ~0.1 ms of `handling interrupt.` (its watch is still
            # installed, with no source left; review-wp0-r2 µs probe), the host
            # never does (D50). How many tail lines print before death is
            # timing-only in both, so only the stdout AFTER the line is
            # waived (R-WP0-8); everything through it is compared exactly.
            racy_tail={"1": r"^handling interrupt\.$"},
        ),
        "3-record-sigkill": dict(
            desc="record start (-e), SIGKILL after ~5 s",
            argv=record,
            actions=playing + [("sleep", 4.0), ("signal", "SIGKILL")],
        ),
        "3b-record-sigterm": dict(
            desc="record start (-e), SIGTERM after ~3 s (default action expected)",
            argv=record,
            actions=playing + [("sleep", 2.0), ("signal", "SIGTERM")],
        ),
        "4-missing-element": dict(
            desc="bridge-only argv as built (libcamerasrc is not installed in the rig)",
            argv=lambda m, c: list(c["bridge"]),
            actions=[],
        ),
        "5a-parse-error-syntax": dict(
            desc="bridge-only argv with a doubled '!' after the source",
            argv=lambda m, c: (lambda a: a[:3] + ["!"] + a[3:])(bridge(m, c)),
            actions=[],
        ),
        "5b-parse-error-no-property": dict(
            desc="bridge-only argv with an unknown property on the source",
            argv=lambda m, c: (lambda a: a[:1] + ["nosuchprop=1"] + a[1:])(bridge(m, c)),
            actions=[],
        ),
        "5c-parse-error-empty": dict(
            desc="one empty token (parse returns NULL: 'could not be constructed')",
            argv=lambda m, c: [""],
            actions=[],
        ),
        "5d-parse-error-link": dict(
            desc="bridge-only argv whose caps filter cannot link (audio caps)",
            argv=lambda m, c: [("audio/x-raw" if t.startswith("video/x-raw") else t) for t in bridge(m, c)],
            actions=[],
        ),
        "6-runtime-error-shm-unwritable": dict(
            desc="bridge-only, shmsink socket dir 0555 and the program runs as uid 65534",
            argv=lambda m, c: [t.replace("/work/shm/", "/work/ro/") for t in bridge(m, c)],
            user=NOBODY,
            actions=[],
        ),
        "6b-runtime-error-record-shm-unwritable": dict(
            desc="record (-e), shmsink socket dir 0555, uid 65534",
            argv=lambda m, c: [t.replace("/work/shm/", "/work/ro/") for t in record(m, c)],
            user=NOBODY,
            actions=[],
        ),
        "7-finite-eos": dict(
            desc="record (-e) with a finite head (num-buffers=45): natural EOS",
            argv=lambda m, c: list(c["record-finite/720p15-1mbps/sw-h264"]),
            actions=playing,
        ),
        "9-sigint-before-watch": dict(
            desc="SIGINT while set_state(PAUSED) blocks (filesrc on a FIFO with no writer), "
                 "i.e. before g_unix_signal_add: the default action",
            argv=lambda m, c: ["filesrc", "location=/work/in.fifo", "!", "fakesink"],
            setup=lambda: os.mkfifo("/work/in.fifo"),
            actions=[("line", r"^Setting pipeline to PAUSED", 10), ("sleep", 0.5), ("proc", "pre_arm"),
                     ("signal", "SIGINT")],
        ),
        "8-bridge-sighup": dict(
            desc="bridge-only, SIGHUP at PLAYING (no GST_DEBUG_DUMP_DOT_DIR), then SIGINT",
            argv=bridge,
            actions=playing + [("signal", "SIGHUP"), ("sleep", 1.0), ("signal", "SIGINT")],
        ),
    }


def reset_work():
    if os.path.isdir(WORK):
        for e in os.listdir(WORK):
            if e in ("home-nobody",):
                continue
            p = os.path.join(WORK, e)
            shutil.rmtree(p) if os.path.isdir(p) and not os.path.islink(p) else os.unlink(p)
    os.makedirs(WORK, exist_ok=True)
    os.chmod(WORK, 0o755)
    for d in ("out", "shm"):
        os.makedirs(os.path.join(WORK, d), exist_ok=True)
        os.chmod(os.path.join(WORK, d), 0o777)
    os.makedirs(os.path.join(WORK, "ro"), exist_ok=True)
    os.chmod(os.path.join(WORK, "ro"), 0o555)
    home = os.path.join(WORK, "home-nobody")
    os.makedirs(home, exist_ok=True)
    os.chown(home, NOBODY, NOBODY)


def proc_snapshot(pid):
    snap = {}
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                k, _, v = line.partition(":")
                if k in ("State", "Threads", "SigQ", "SigPnd", "ShdPnd", "SigBlk", "SigIgn", "SigCgt", "Name"):
                    snap[k] = v.strip()
        blk = {}
        for tid in sorted(os.listdir(f"/proc/{pid}/task"), key=int):
            try:
                with open(f"/proc/{pid}/task/{tid}/status") as f:
                    st = dict(l.split(":", 1) for l in f if ":" in l)
                name = st.get("Name", "").strip()
                b = st.get("SigBlk", "").strip()
                blk.setdefault(b, []).append(name)
            except OSError:
                pass
        snap["taskSigBlk"] = {k: sorted(v) for k, v in blk.items()}
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            snap["cmdline0"] = f.read().split(b"\0")[0].decode("utf-8", "replace")
        with open(f"/proc/{pid}/comm") as f:
            snap["comm"] = f.read().strip()
    except OSError as e:
        snap["error"] = str(e)
    for k in ("SigCgt", "SigIgn", "SigBlk"):
        if k in snap:
            snap[k + "Names"] = decode_mask(snap[k])
    return snap


def decode_mask(hexmask):
    v = int(hexmask, 16)
    return [SIGNAMES.get(i + 1, f"SIG{i + 1}") for i in range(64) if v >> i & 1]


def run(program, arg0, prefix, sc_name, sc, m, cases, extra_env):
    reset_work()
    if "setup" in sc:
        sc["setup"]()
    argv = sc["argv"](m, cases)
    env = dict(os.environ)
    env.update(extra_env)
    user = sc.get("user")
    if user is not None:
        env["HOME"] = os.path.join(WORK, "home-nobody")
        # Warm the registry for this user so the scenario never spawns the
        # plugin scanner (keeps its own stderr out of the transcript).
        subprocess.run(["gst-inspect-1.0", "videotestsrc"], env=env, user=user, group=user,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    t0 = time.monotonic()
    lines, events, procs = [], [], {}
    cond = threading.Condition()

    def now():
        return int((time.monotonic() - t0) * 1000)

    p = subprocess.Popen(
        [arg0] + prefix + argv,
        executable=program,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        user=user,
        group=user,
        start_new_session=True,  # own process group, like the agent's spawn
    )

    def pump(stream, fd):
        buf = b""
        while True:
            chunk = stream.read1(65536)
            if not chunk:
                break
            buf += chunk
            while b"\n" in buf:
                ln, buf = buf.split(b"\n", 1)
                with cond:
                    lines.append({"fd": fd, "t": now(), "line": ln.decode("utf-8", "replace")})
                    cond.notify_all()
        if buf:  # unterminated tail: the agent drops it (child.rs); keep it visible
            with cond:
                lines.append({"fd": fd, "t": now(), "line": buf.decode("utf-8", "replace"), "unterminated": True})
                cond.notify_all()

    th = [threading.Thread(target=pump, args=(p.stdout, 1)), threading.Thread(target=pump, args=(p.stderr, 2))]
    for t in th:
        t.start()

    def wait_line(rx, timeout):
        r = re.compile(rx)
        deadline = time.monotonic() + timeout
        with cond:
            while True:
                if any(r.search(l["line"]) for l in lines):
                    return True
                left = deadline - time.monotonic()
                if left <= 0 or p.poll() is not None:
                    return any(r.search(l["line"]) for l in lines)
                cond.wait(min(left, 0.05))

    for act in sc["actions"]:
        kind = act[0]
        if kind == "line":
            ok = wait_line(act[1], act[2])
            events.append({"t": now(), "action": f"wait-line {act[1]}", "ok": ok})
        elif kind == "sleep":
            time.sleep(act[1])
        elif kind == "proc":
            procs[act[1]] = proc_snapshot(p.pid) if p.poll() is None else {"error": "exited"}
            procs[act[1]]["t"] = now()
        elif kind == "signal":
            if p.poll() is None:
                os.kill(p.pid, getattr(signal, act[1]))
                events.append({"t": now(), "action": f"signal {act[1]}"})
            else:
                events.append({"t": now(), "action": f"signal {act[1]} skipped (exited)"})

    timeout_killed = False
    try:
        p.wait(timeout=sc.get("exit_timeout", 25))
    except subprocess.TimeoutExpired:
        timeout_killed = True
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()
        events.append({"t": now(), "action": "timeout SIGKILL"})
    for t in th:
        t.join()
    rc = p.returncode
    # shmsink formats the pid with %5d ("shmpipe.  123.    0").
    shm_rx = re.compile(r"^shmpipe\.\s*%d\." % p.pid)
    shm_left = sorted(n for n in os.listdir("/dev/shm") if shm_rx.match(n))
    for n in shm_left:
        try:
            os.unlink(os.path.join("/dev/shm", n))
        except OSError:
            pass
    frags = sorted(os.listdir(os.path.join(WORK, "out"))) if os.path.isdir(os.path.join(WORK, "out")) else []
    return {
        "scenario": sc_name,
        "description": sc["desc"],
        "argv": argv,
        "user": "nobody(65534)" if user is not None else "root",
        "lines": lines,
        "exit": {
            "code": rc if rc >= 0 else None,
            "signal": SIGNAMES.get(-rc) if rc < 0 else None,
            "timeoutKilled": timeout_killed,
        },
        "proc": procs,
        "events": events,
        "after": {"shmLeft": shm_left, "fragments": frags},
        "racy": sc.get("racy", []),
        "racyTail": sc.get("racy_tail", {}),
    }


def stamp(program):
    def sh(cmd):
        try:
            return subprocess.run(cmd, capture_output=True, text=True, timeout=10).stdout.strip()
        except Exception as e:  # noqa: BLE001
            return f"error: {e}"

    with open("/etc/debian_version") as f:
        deb = f.read().strip()
    return {
        "program": program,
        "gstLaunchVersion": sh(["gst-launch-1.0", "--version"]).splitlines()[:2],
        "debianVersion": deb,
        "arch": platform.machine(),
        "kernel": platform.release(),
        "recordedAt": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "recorder": "tools/capture-host/transcript.py",
        "packages": sh(["dpkg-query", "-W", "-f", "${Package} ${Version}\\n",
                        "libgstreamer1.0-0", "gstreamer1.0-tools", "gstreamer1.0-plugins-base",
                        "gstreamer1.0-plugins-good", "gstreamer1.0-plugins-bad", "gstreamer1.0-plugins-ugly"]).splitlines(),
        "env": {k: os.environ.get(k) for k in ("LANG", "LC_ALL", "GST_DEBUG", "GST_DEBUG_DUMP_DOT_DIR")},
    }


def cmd_record(a):
    m, cases = load_matrix()
    scs = scenarios()
    os.makedirs(a.outdir, exist_ok=True)
    st = stamp(a.program)
    for name, sc in scs.items():
        if a.only and name not in a.only:
            continue
        print(f"== {name}", file=sys.stderr, flush=True)
        t = run(a.program, a.arg0 or os.path.basename(a.program), a.prefix, name, sc, m, cases, {})
        t["stamp"] = st
        with open(os.path.join(a.outdir, f"{name}.json"), "w") as f:
            json.dump(t, f, indent=1)
            f.write("\n")
        ex = t["exit"]
        print(f"   exit={ex}  lines={len(t['lines'])}  cgt={t['proc'].get('playing', {}).get('SigCgtNames')}", file=sys.stderr, flush=True)


# ---------------------------------------------------------------------------
# compare
# ---------------------------------------------------------------------------

VOLATILE = [
    (re.compile(r"^Execution ended after \d+:\d{2}:\d{2}\.\d{9}$"), "Execution ended after <duration>"),
    (re.compile(r"'gdb gst-launch-1\.0 \d+'"), "'gdb gst-launch-1.0 <pid>'"),
]


def norm(line):
    for rx, rep in VOLATILE:
        if rx.search(line):
            return rx.sub(rep, line)
    return line


# R-WP0-3: the agent DROPS an unterminated tail (xorgate-hal child.rs), so a
# missing final newline is a real difference: it is part of the line.
SETXID_BIT = 1 << 32  # signal 33 (glibc SIGSETXID) in a /proc mask
UNTERMINATED = " \u27e8no newline\u27e9"


# WP1: the host's own timing lines (xorgate-core capture_timing). They are
# host-only by design (gst-launch never prints them) and are validated by
# their own row; every OTHER line is compared byte for byte.
TIMING_RX = re.compile(r"^XG-FRAGMENT(-END)? \{")


def fd_lines(t, fd):
    return [norm(l["line"]) + (UNTERMINATED if l.get("unterminated") else "")
            for l in t["lines"] if l["fd"] == fd and not (fd == 1 and TIMING_RX.match(l["line"]))]


def timing_row(r, g):
    """The host's XG-FRAGMENT lines: v1 JSON, one start per fragment file
    (id == the %05d sequence), an END only after a clean EOS and only for the
    last id, startTs ascending. The reference (gst-launch) must have none."""
    ref_t = [l for l in r["lines"] if l["fd"] == 1 and TIMING_RX.match(l["line"])]
    got = [l for l in g["lines"] if TIMING_RX.match(l["line"])]
    errs = []
    if ref_t:
        errs.append(f"reference has timing lines: {ref_t}")
    starts, ends = [], []
    for l in got:
        if l["fd"] != 1:
            errs.append(f"timing line on fd{l['fd']}")
        prefix, _, body = l["line"].partition(" ")
        try:
            j = json.loads(body)
        except ValueError:
            errs.append(f"not JSON: {l['line']}")
            continue
        if j.get("v") != 1:
            errs.append(f"v != 1: {l['line']}")
        (ends if prefix == "XG-FRAGMENT-END" else starts).append(j)
    seqs = sorted(int(f[:5]) for f in g.get("after", {}).get("fragments", []) if f[:5].isdigit())
    ids = [j.get("id") for j in starts]
    if g["proc"].get("playing", {}).get("cmdline0") == "xorgate-capture" and seqs and ids != seqs:
        errs.append(f"start ids {ids} != fragment sequences {seqs}")
    ts = [j.get("startTs") for j in starts]
    if ts != sorted(ts):
        errs.append(f"startTs not ascending: {ts}")
    clean_eos = g["exit"]["signal"] is None and any(
        l["line"] == 'Got EOS from element "pipeline0".' for l in g["lines"])
    if ends and (not clean_eos or len(ends) != 1 or not ids or ends[0].get("id") != ids[-1]):
        errs.append(f"END {ends} (clean EOS {clean_eos}, last id {ids[-1:] or None})")
    if clean_eos and ids and not ends:
        errs.append("clean EOS without an XG-FRAGMENT-END")
    detail = f"starts={ids} lagMs={[j.get('lagMs') for j in starts]} ends={[j.get('id') for j in ends]}"
    return ("host timing lines (XG-FRAGMENT*)", not errs, detail + ("" if not errs else f" ERRORS {errs}"))


def split_at(lines, rx):
    """(lines through the first match of rx, the rest); no match = (all, [])."""
    r = re.compile(rx)
    for i, l in enumerate(lines):
        if r.search(l):
            return lines[: i + 1], lines[i + 1:]
    return lines, []


def sigint_latency(t):
    """ms from each driver SIGINT to the next `handling interrupt.` line."""
    out = []
    for e in t.get("events", []):
        if e["action"] == "signal SIGINT":
            nxt = [l["t"] for l in t["lines"] if l["line"] == "handling interrupt." and l["t"] >= e["t"]]
            if nxt:
                out.append(nxt[0] - e["t"])
    return out


def host_mask_row(g):
    """R-WP0-4, host only: the mask was set before ANY thread existed, so
    every thread but the wait thread blocks SIGINT|SIGHUP at PLAYING."""
    p = g["proc"].get("playing", {})
    if p.get("cmdline0") != "xorgate-capture" or "taskSigBlk" not in p:
        return None
    need = (1 << (signal.SIGINT - 1)) | (1 << (signal.SIGHUP - 1))
    bad = {m: n for m, n in p["taskSigBlk"].items()
           if int(m, 16) & need != need and [x for x in n if x != "xg-sigwait"]}
    return ("host per-thread SigBlk (SIGINT|SIGHUP on every thread but xg-sigwait)", not bad,
            f"unblocked: {bad}")


def cmd_compare(a):
    ok_all = True
    names = sorted(n[:-5] for n in os.listdir(a.ref) if n.endswith(".json"))
    for name in names:
        with open(os.path.join(a.ref, name + ".json")) as f:
            r = json.load(f)
        gp = os.path.join(a.got, name + ".json")
        if not os.path.exists(gp):
            print(f"[MISSING] {name}")
            ok_all = False
            continue
        with open(gp) as f:
            g = json.load(f)
        rows = []
        known = set()
        rows.append(("exit", r["exit"]["code"] == g["exit"]["code"] and r["exit"]["signal"] == g["exit"]["signal"],
                     f"ref={r['exit']} got={g['exit']}"))
        for key in ("SigCgt", "SigIgn"):
            for when in ("pre_arm", "playing", "after_intr"):
                rv = r["proc"].get(when, {}).get(key)
                gv = g["proc"].get(when, {}).get(key)
                if rv is None and gv is None:
                    continue
                label = f"{key}@{when}"
                detail = f"ref={r['proc'].get(when, {}).get(key + 'Names')} got={g['proc'].get(when, {}).get(key + 'Names')}"
                ok = rv == gv
                if (not ok and key == "SigCgt" and when == "pre_arm" and rv and gv
                        and int(gv, 16) == int(rv, 16) | SETXID_BIT):
                    # Known, exact: glibc installs its internal SIGSETXID (33)
                    # handler on the process's first pthread_create. Before
                    # its watch, gst-launch has no thread yet; the host's
                    # signal-wait thread exists from startup by design (R3).
                    # Invisible to every consumer (D-row candidate, wp0.md).
                    known.add(label)
                rows.append((label, ok, detail))
        rt = r["proc"].get("playing", {}).get("Threads")
        gt = g["proc"].get("playing", {}).get("Threads")
        if rt is not None or gt is not None:
            rows.append(("Threads@playing", rt == gt, f"ref={rt} got={gt}"))
        hm = host_mask_row(g)
        if hm:
            rows.append(hm)
        rows.append(timing_row(r, g))
        ra, ga = r.get("after", {}), g.get("after", {})
        rows.append(("shm segments left (count)", len(ra.get("shmLeft", [])) == len(ga.get("shmLeft", [])),
                     f"ref={ra.get('shmLeft')} got={ga.get('shmLeft')}"))
        rows.append(("fragments", ra.get("fragments") == ga.get("fragments"),
                     f"ref={ra.get('fragments')} got={ga.get('fragments')}"))
        racy = set(r.get("racy", [])) | known
        tails = r.get("racyTail", {})

        def diff_row(label, rl, gl):
            same = rl == gl
            detail = ""
            if not same:
                import difflib
                detail = "\n" + "\n".join("      " + d for d in difflib.unified_diff(rl, gl, "ref", "got", lineterm="", n=1))
            rows.append((label, same, detail))

        for fd in (1, 2):
            rl, gl = fd_lines(r, fd), fd_lines(g, fd)
            anchor = tails.get(str(fd))
            if anchor:
                rh, rtail = split_at(rl, anchor)
                gh, gtail = split_at(gl, anchor)
                diff_row(f"fd{fd} lines through {anchor}", rh, gh)
                diff_row(f"fd{fd} tail after {anchor}", rtail, gtail)
                racy.add(f"fd{fd} tail after {anchor}")
            else:
                diff_row(f"fd{fd} lines", rl, gl)
        bad = [x for x in rows if not x[1] and x[0] not in racy]
        ok_all &= not bad
        print(f"[{'PASS' if not bad else 'DIFF'}] {name}")
        rl_, gl_ = sigint_latency(r), sigint_latency(g)
        if rl_ or gl_:
            print(f"    info: SIGINT -> 'handling interrupt.' ms: ref={rl_} got={gl_}")
        for label, ok, detail in rows:
            if label in known and not ok:
                print(f"    !! {label} (known divergence: glibc SIGSETXID installed by the host's startup thread): {detail}")
            elif label in racy and not ok:
                print(f"    ~~ {label} (racy, informational): {detail}")
            elif not ok or a.verbose:
                print(f"    {'ok ' if ok else 'XX '}{label}: {detail}")
    return 0 if ok_all else 1


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("record")
    r.add_argument("--program", required=True)
    r.add_argument("--arg0")
    r.add_argument("--prefix", action="append", default=[])
    r.add_argument("--outdir", required=True)
    r.add_argument("--only", action="append")
    c = sub.add_parser("compare")
    c.add_argument("ref")
    c.add_argument("got")
    c.add_argument("-v", "--verbose", action="store_true")
    a = ap.parse_args()
    if a.cmd == "record":
        cmd_record(a)
        return 0
    return cmd_compare(a)


if __name__ == "__main__":
    sys.exit(main())
