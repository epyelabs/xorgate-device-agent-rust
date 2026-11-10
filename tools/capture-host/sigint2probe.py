"""ci-sigint-race probe: what does a SECOND SIGINT do, by arrival time?

Run 42845761508 failed `second_sigint_and_sigterm_take_the_default_action`:
the host exited 0 after a clean EOS although a second SIGINT was sent on
reading `handling interrupt.`. This probe sweeps the second SIGINT's arrival
time over the whole `-e` stop (drain, after EOS, before exit) for a program
(the host, or the real gst-launch-1.0), and classifies every run by the
program's disposition AT THE MOMENT the second SIGINT is delivered:

  --freeze  SIGSTOP the program at the trigger, wait until every task is
            stopped, read the process state and SigCgt, queue the SIGINT,
            SIGCONT. The SIGINT is then handled with exactly the disposition
            read (nothing ran in between), so the class is exact:
              W = SigCgt has SIGINT (a handler is installed: the watch is
                  still there, or being removed; GLib/the host swallow or
                  coalesce it, or it races the removal)
              D = SigCgt lacks SIGINT (SIG_DFL): the default action MUST kill
              Z = already a zombie / gone (the SIGINT can do nothing)
  (default) read SigCgt, then kill(2) at once: the test's own timing, the
            class is a lower bound (W may already be D when it lands).

A host defect is a D run that does not die by SIGINT.

  python3 -I sigint2probe.py --program P [--arg0 A] [--prefix __capture-host]
      --pipeline '-e fakesrc ! identity sleep-time=1000000 ! fakesink'
      --trigger 'handling interrupt.' --delay-us 0 [--freeze] -n 20 [--tag T]
      [--inherit clean|ignore|block]

One JSON row per run on stdout, then a summary row.
"""
import argparse
import json
import os
import select
import signal
import subprocess
import sys
import time

ap = argparse.ArgumentParser()
ap.add_argument("--program", required=True)
ap.add_argument("--arg0")
ap.add_argument("--prefix", action="append", default=[])
ap.add_argument("--pipeline", required=True)
ap.add_argument("--first-trigger", default="New clock:")
ap.add_argument("--trigger", required=True)
ap.add_argument("--delay-us", type=int, default=0)
ap.add_argument("--freeze", action="store_true")
ap.add_argument("-n", type=int, default=10)
ap.add_argument("--tag", default="")
ap.add_argument("--inherit", choices=["clean", "ignore", "block"], default="clean",
                help="the program starts with SIGINT+SIGHUP+SIGQUIT SIG_IGN, or blocked")
a = ap.parse_args()


def proc_state(pid):
    """(process state letter, SIGINT caught?, every task stopped?) or None."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            st = f.read().rsplit(")", 1)[1].split()[0]
        with open(f"/proc/{pid}/status") as f:
            cgt = next(l for l in f if l.startswith("SigCgt:")).split()[1]
        stopped = True
        for t in os.listdir(f"/proc/{pid}/task"):
            try:
                with open(f"/proc/{pid}/task/{t}/stat") as f:
                    ts = f.read().rsplit(")", 1)[1].split()[0]
                stopped &= ts in ("T", "t", "Z", "X")
            except OSError:
                pass
        return st, (int(cgt, 16) >> (signal.SIGINT - 1)) & 1 == 1, stopped
    except (OSError, StopIteration):
        return None


def spin_us(us):
    end = time.monotonic_ns() + us * 1000
    while time.monotonic_ns() < end:
        pass


def classify_and_kill(pid):
    if a.freeze:
        os.kill(pid, signal.SIGSTOP)
        deadline = time.monotonic() + 2
        s = proc_state(pid)
        while s is not None and s[0] != "Z" and not s[2] and time.monotonic() < deadline:
            s = proc_state(pid)
    else:
        s = proc_state(pid)
    t = time.monotonic_ns()
    if s is None or s[0] in ("Z", "X"):
        cls = "Z"
    elif s[1]:
        cls = "W"
    else:
        cls = "D"
    try:
        os.kill(pid, signal.SIGINT)
    except ProcessLookupError:
        cls = "Z"
    if a.freeze:
        try:
            os.kill(pid, signal.SIGCONT)
        except ProcessLookupError:
            pass
    return cls, t


def pre():
    sigs = (signal.SIGINT, signal.SIGHUP, signal.SIGQUIT)
    if a.inherit == "ignore":
        for s in sigs:
            signal.signal(s, signal.SIG_IGN)
    elif a.inherit == "block":
        signal.pthread_sigmask(signal.SIG_BLOCK, sigs)


argv = [a.arg0 or a.program] + a.prefix + a.pipeline.split(" ")
rows = []
for i in range(a.n):
    p = subprocess.Popen(argv, executable=a.program, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         start_new_session=True, preexec_fn=pre)
    fd = p.stdout.fileno()
    buf, lines = b"", []
    t_k1 = t_trig = t_k2 = None
    cls = None
    deadline = time.monotonic() + 30
    eof = False
    while not eof and time.monotonic() < deadline:
        r, _, _ = select.select([fd], [], [], 0.05)
        if not r:
            continue
        data = os.read(fd, 65536)
        now = time.monotonic_ns()
        if not data:
            break
        buf += data
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            s = line.decode("utf-8", "replace")
            lines.append((now, s))
            if t_k1 is None and s.startswith(a.first_trigger):
                os.kill(p.pid, signal.SIGINT)
                t_k1 = time.monotonic_ns()
            elif t_k1 is not None and t_k2 is None and s.startswith(a.trigger):
                t_trig = now
                spin_us(a.delay_us)
                cls, t_k2 = classify_and_kill(p.pid)
    try:
        p.wait(timeout=30)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()
    err = p.stderr.read().decode("utf-8", "replace").splitlines()
    rc = p.returncode
    texts = [l[1] for l in lines]
    t_eos = next((l[0] for l in lines if l[1].startswith("Got EOS")), None)
    row = {
        "tag": a.tag, "run": i, "class": cls or "-",
        "exit": f"sig {signal.Signals(-rc).name}" if rc < 0 else rc,
        "got_eos": t_eos is not None,
        "k2_after_trigger_us": round((t_k2 - t_trig) / 1000) if t_k2 else None,
        "k2_after_k1_ms": round((t_k2 - t_k1) / 1e6, 3) if t_k2 and t_k1 else None,
        "eos_after_k2_ms": round((t_eos - t_k2) / 1e6, 3) if t_eos and t_k2 else None,
        "last": texts[-1] if texts else "",
        "err": err[:2],
    }
    rows.append(row)
    print(json.dumps(row), flush=True)

summ = {}
for r in rows:
    k = f'{r["class"]}->{r["exit"]}'
    summ[k] = summ.get(k, 0) + 1
defects = sum(1 for r in rows if r["class"] == "D" and r["exit"] != "sig SIGINT")
print(json.dumps({"tag": a.tag, "summary": summ, "n": len(rows), "D_not_killed": defects,
                  "program": os.path.basename(a.program), "pipeline": a.pipeline,
                  "trigger": a.trigger, "delay_us": a.delay_us, "freeze": a.freeze,
                  "inherit": a.inherit}), flush=True)
