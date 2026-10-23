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

Gate file ($RIG_OBD_GATE): "1" open / "0" closed, rewritten by the player at
least every 500 ms. Missing, unreadable or older than 1.5 s = OPEN (the player
stopped gating: tape done, --no-gate), so the helper can never be wedged.

Signals: SIGTERM/SIGINT/SIGHUP are forwarded to the helper once (execa's
`kill()` and the Rust port signal the pid, not the group); a helper killed by a
signal re-raises it here, so the agent sees the same `helper stopped (signal)`.
"""
import os
import signal
import subprocess
import sys
import time

GATE = os.environ.get("RIG_OBD_GATE")
STALE_S = 1.5


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


def wait_open():
    while not is_open():
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
    wait_open()
    try:
        out.write(line)
        out.flush()
    except BrokenPipeError:
        break
rc = child.wait()
wait_open()
if rc < 0:
    signal.signal(-rc, signal.SIG_DFL)
    os.kill(os.getpid(), -rc)
sys.exit(rc)
