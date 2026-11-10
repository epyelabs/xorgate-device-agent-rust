"""ci-sigint-race probe: a SECOND SIGHUP sent the moment the first one's
`SIGHUP: ` line is read (`the_sighup_watch_stays_armed`'s timing). GLib's
unix-signal source clears its `pending` flag BEFORE it runs the callback
(`g_unix_signal_watch_dispatch`, GLib 2.74), so a SIGHUP that lands while
gst-launch's `hup_handler` runs is dispatched again: two lines. Counts the
`SIGHUP: ` lines per run, then stops the program with SIGINT.

  python3 -I sighupprobe.py --program P [--arg0 A] [--prefix __capture-host]
      [--dot-dir DIR] [--delay-us N] [--inherit clean|ignore|block] -n 20 [--tag T]

--dot-dir sets GST_DEBUG_DUMP_DOT_DIR, so the handler also writes a dot
snapshot after its line (a wider window, same mechanism).
"""
import argparse
import json
import os
import select
import signal
import subprocess
import time

ap = argparse.ArgumentParser()
ap.add_argument("--program", required=True)
ap.add_argument("--arg0")
ap.add_argument("--prefix", action="append", default=[])
ap.add_argument("--pipeline", default="fakesrc ! identity sleep-time=10000 ! fakesink")
ap.add_argument("--dot-dir")
ap.add_argument("--delay-us", type=int, default=0)
ap.add_argument("--inherit", choices=["clean", "ignore", "block"], default="clean")
ap.add_argument("-n", type=int, default=10)
ap.add_argument("--tag", default="")
a = ap.parse_args()


def pre():
    sigs = (signal.SIGINT, signal.SIGHUP, signal.SIGQUIT)
    if a.inherit == "ignore":
        for s in sigs:
            signal.signal(s, signal.SIG_IGN)
    elif a.inherit == "block":
        signal.pthread_sigmask(signal.SIG_BLOCK, sigs)


env = dict(os.environ)
env.pop("GST_DEBUG_DUMP_DOT_DIR", None)
if a.dot_dir:
    os.makedirs(a.dot_dir, exist_ok=True)
    env["GST_DEBUG_DUMP_DOT_DIR"] = a.dot_dir
argv = [a.arg0 or a.program] + a.prefix + a.pipeline.split(" ")
rows = []
for i in range(a.n):
    p = subprocess.Popen(argv, executable=a.program, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         start_new_session=True, preexec_fn=pre, env=env)
    fd = p.stdout.fileno()
    buf, hups, t_h1, t_h2, t_int = b"", 0, None, None, None
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if t_h2 and t_int is None and time.monotonic_ns() - t_h2 > 1_000_000_000:
            os.kill(p.pid, signal.SIGINT)
            t_int = time.monotonic_ns()
        r, _, _ = select.select([fd], [], [], 0.05)
        if not r:
            if p.poll() is not None:
                break
            continue
        data = os.read(fd, 65536)
        if not data:
            break
        buf += data
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            s = line.decode("utf-8", "replace")
            if t_h1 is None and s.startswith("New clock:"):
                os.kill(p.pid, signal.SIGHUP)
                t_h1 = time.monotonic_ns()
            elif s.startswith("SIGHUP: "):
                hups += 1
                if t_h2 is None:
                    end = time.monotonic_ns() + a.delay_us * 1000
                    while time.monotonic_ns() < end:
                        pass
                    os.kill(p.pid, signal.SIGHUP)
                    t_h2 = time.monotonic_ns()
    try:
        p.wait(timeout=20)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()
    rc = p.returncode
    row = {"tag": a.tag, "run": i, "hup_lines": hups,
           "exit": f"sig {signal.Signals(-rc).name}" if rc < 0 else rc}
    rows.append(row)
    print(json.dumps(row), flush=True)
summ = {}
for r in rows:
    k = f'{r["hup_lines"]} lines->{r["exit"]}'
    summ[k] = summ.get(k, 0) + 1
print(json.dumps({"tag": a.tag, "summary": summ, "n": len(rows),
                  "program": os.path.basename(a.program), "dot_dir": bool(a.dot_dir),
                  "delay_us": a.delay_us, "inherit": a.inherit}), flush=True)
