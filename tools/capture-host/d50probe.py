"""D50 probe (rev2 reviewer): is the 2b tail difference caused by the order of
watch removal vs the print (the WP's D50 claim), or by the time between
`handling interrupt.` and `Interrupt: Stopping pipeline ...` (latency)?

Per run: the record argv of scenario 2/2b, SIGINT 3 s after `New clock:`;
mode "single" sends nothing else, mode "double" sends a second SIGINT the
moment `handling interrupt.` is read. Every stdout line gets the monotonic
ns time of the os.read() that delivered it.

  python3 -I d50probe.py PROGRAM ARG0 PREFIX|- MODE N
"""
import json
import os
import select
import signal
import subprocess
import sys
import time

sys.path.insert(0, "/repo/tools/capture-host")
import transcript as tr  # noqa: E402

prog, arg0, prefix, mode, n = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
prefix = [] if prefix == "-" else [prefix]
m, cases = tr.load_matrix()
argv = ["-e"] + tr.swap_source(m, cases["record/720p15-1mbps/sw-h264"])
results = []
for i in range(n):
    tr.reset_work()
    p = subprocess.Popen([arg0] + prefix + argv, executable=prog, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
    fd = p.stdout.fileno()
    buf, lines, chunk = b"", [], 0
    t_int, t_kill, t_kill2 = None, None, None
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        if t_int is None and any(l[2].startswith("New clock:") for l in lines):
            t_int = time.monotonic_ns() + 3_000_000_000
        if t_int and t_kill is None and time.monotonic_ns() >= t_int:
            os.kill(p.pid, signal.SIGINT)
            t_kill = time.monotonic_ns()
        r, _, _ = select.select([fd], [], [], 0.01)
        if not r:
            if p.poll() is not None:
                break
            continue
        data = os.read(fd, 65536)
        now = time.monotonic_ns()
        if not data:
            break
        chunk += 1
        buf += data
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            s = line.decode("utf-8", "replace")
            lines.append((now, chunk, s))
            if mode == "double" and s == "handling interrupt." and t_kill2 is None:
                os.kill(p.pid, signal.SIGINT)
                t_kill2 = time.monotonic_ns()
    try:
        p.wait(timeout=20)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()
    rc = p.returncode
    hi = next((k for k, l in enumerate(lines) if l[2] == "handling interrupt."), None)
    row = {"run": i, "rc": rc, "exit": ("sig " + signal.Signals(-rc).name) if rc < 0 else rc}
    if hi is not None:
        t0, c0 = lines[hi][0], lines[hi][1]
        row["tail"] = [l[2] for l in lines[hi + 1:]][:3]
        row["after_us"] = [(round((l[0] - t0) / 1000), l[1] == c0) for l in lines[hi + 1:hi + 3]]
        if t_kill:
            row["sigint_to_line_us"] = round((t0 - t_kill) / 1000)
        if t_kill2:
            row["line_to_kill2_us"] = round((t_kill2 - t0) / 1000)
    results.append(row)
    print(json.dumps(row), flush=True)
