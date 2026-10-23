#!/usr/bin/env python3
"""Rig (Phase 5b WP4, orchestrator ruling 2026-10-23): the REAL OBD helper's
stdout through the player's telemetry hold gate.

Installed as `<work>/python/obd_read.py` (the agent spawns
`python3 <XORGATE_PYTHON_DIR>/obd_read.py <argv>`); runs the side's real helper
(`obd_read_real.py`, a symlink to it) with the same argv, stdin and stderr, and
relays each stdout line ONLY while the player's gate is open — so every helper
event the agent reacts to (a sample, `live`, a link move, the exit after a
SIGTERM) reaches BOTH agents outside a tick pair's window, the way the player
releases every other line input (NMEA, IMU). Without it the two independent
emulators' edges landed between agent A's and agent B's tick of the same pair
(stepC/p5b-obd-config.jsrs3: the rev-2 helper exit 7 ms after Rust's tick and
before JS's; the rev-6 `live` 60 ms after Rust's tick).

Before the gate, a cross-side BARRIER ($RIG_OBD_BARRIER, see `barrier`).

Gate file ($RIG_OBD_GATE): "1" open / "0" closed, rewritten by the player at
least every 500 ms. Missing, unreadable or older than 1.5 s = OPEN (the player
stopped gating: tape done, --no-gate), so the helper can never be wedged.

Signals: SIGTERM/SIGINT/SIGHUP are forwarded to the helper once (execa's
`kill()` and the Rust port signal the pid, not the group); a helper killed by a
signal re-raises it here, so the agent sees the same `helper stopped (signal)`.
"""
import json
import os
import signal
import subprocess
import sys
import time

GATE = os.environ.get("RIG_OBD_GATE")
STALE_S = 1.5
# Cross-side barrier (both agents' relays): the k-th helper event of each
# kind (the line's JSON `type`: ready / status / profile / sample / audit /
# error, and the process `exit`) is released on a side only once the OTHER
# side's relay has reached its own k-th event of that kind — so the two
# independent emulators' edges (a `live`, the first sample, the exit after a
# SIGTERM) reach both agents together, and then outside a tick pair (the
# gate). Counts persist across helper restarts in $RIG_OBD_BARRIER/<side>.<kind>.
# A partner that never gets there (diverged sequences) releases after
# BARRIER_S, logged on stderr — a timeout is evidence, never silent.
BARRIER = os.environ.get("RIG_OBD_BARRIER")
SIDE = os.environ.get("RIG_OBD_SIDE", "")
PEERS = [p for p in os.environ.get("RIG_OBD_PEERS", "").split(",") if p and p != SIDE]
BARRIER_S = 3.0


def _count(side, kind):
    try:
        with open(os.path.join(BARRIER, f"{side}.{kind}")) as f:
            return int(f.read() or 0)
    except (OSError, ValueError):
        return 0


def barrier(kind):
    if not (BARRIER and SIDE and PEERS):
        return
    n = _count(SIDE, kind) + 1
    tmp = os.path.join(BARRIER, f".{SIDE}.{kind}.tmp")
    with open(tmp, "w") as f:
        f.write(str(n))
    os.replace(tmp, os.path.join(BARRIER, f"{SIDE}.{kind}"))
    deadline = time.time() + BARRIER_S
    while any(_count(p, kind) < n for p in PEERS):
        if time.time() > deadline:
            sys.stderr.write(f"[rig obd-gate] barrier timeout: {SIDE} {kind} #{n}\n")
            sys.stderr.flush()
            return
        time.sleep(0.005)


def kind_of(line):
    try:
        return str(json.loads(line).get("type") or "line")
    except (ValueError, AttributeError):
        return None


def is_open():
    if not GATE:
        return True
    try:
        st = os.stat(GATE)
        if time.time() - st.st_mtime > STALE_S:
            return True
        with open(GATE, "rb") as f:
            return f.read(1) != b"0"
    except OSError:
        return True


# Report hold: a `status` line (a link move) makes each agent request a config
# report on its next publish tick (ts:src/worker.ts:1399-1404 `obdReportDue`),
# built 2 s later (ts:src/agent/config-sync.ts:1604-1617) — i.e. within
# [r + 2000, r + 3000] of the line's release r, at each agent's own instant.
# Any helper line released in that span could land between the two builds
# (stepC-H9 p5b-obd-crank.rp.cal09: the `profile` between them). So after a
# status line, hold from r + 2000 - REPORT_MARGIN_S to r + 3000 + REPORT_MARGIN_S.
# Both sides release their status lines together (the barrier), so the hold is
# symmetric.
REPORT_MARGIN_S = 0.25
report_hold = None  # (start, end) epoch seconds


def in_report_hold():
    if report_hold is None or not GATE:
        return False
    now = time.time()
    return report_hold[0] <= now < report_hold[1]


def wait_open(report=True):
    while not is_open() or (report and in_report_hold()):
        time.sleep(0.005)


real = os.path.join(os.path.dirname(os.path.abspath(sys.argv[0])), "obd_read_real.py")
child = subprocess.Popen([sys.executable, "-u", real] + sys.argv[1:], stdout=subprocess.PIPE)


def forward(sig, _frame):
    try:
        child.send_signal(sig)
    except ProcessLookupError:
        pass


for s in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
    signal.signal(s, forward)

out = sys.stdout.buffer
for line in iter(child.stdout.readline, b""):
    k = kind_of(line)
    if k is not None:
        barrier(k)
    wait_open()
    try:
        out.write(line)
        out.flush()
    except BrokenPipeError:
        break
    if k == "status":
        r = time.time()
        report_hold = (r + 2.0 - REPORT_MARGIN_S, r + 3.0 + REPORT_MARGIN_S)
rc = child.wait()
barrier("exit")
# The exit is not held for a report: the agent's stop waits only a bounded
# time for the helper (SIGKILL after), which a hold could change.
wait_open(report=False)
if rc < 0:
    signal.signal(-rc, signal.SIG_DFL)
    os.kill(os.getpid(), -rc)
sys.exit(rc)
