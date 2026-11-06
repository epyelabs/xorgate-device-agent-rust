#!/usr/bin/env python3
"""Bench overlay (Phase 5b WP5): drain the ELM327 emulator's pty, then exec
the installed OBD helper.

`scripts/bench/p5b-overlay.sh apply` installs this file as
`$P5B_OBD_PY/obd_read.py`. The agent spawns
`python3 <XORGATE_PYTHON_DIR>/obd_read.py <argv>`, so every helper start runs
this first, with the helper's own argv.

Why: the overlay's emulator outlives each helper. A reply to the PREVIOUS
helper's last command (its shutdown close, cut by the agent's stop signal) can
still sit in the pty's input queue, and the next helper then reads it as the
answer to its own first command; every later reply shifts by one. This is the
rig's fix, mirrored (tools/replay/obd-gate.py `drain`, orchestrator ruling
2026-10-27): before the helper starts, discard what is queued, then read until
the line has been quiet for DRAIN_QUIET_S (a reply still in flight), at most
DRAIN_MAX_S. A real adapter is not reopened 40 ms after a kill, so this adds no
behaviour a field device has.

Then `exec` the real helper (same pid, so the agent's signals reach it
directly; same argv, stdin, stdout, stderr). Each drain is appended as one JSON
line to $XORGATE_P5B_DRAIN_LOG (default /tmp/xorgate-p5b/obd-drain.ndjson);
a drain that never went quiet is logged `"quiet": false` and the helper still
starts. No `--serial` argument (or an unopenable one) = no drain.
"""
import json
import os
import sys
import time

DRAIN_QUIET_S = 0.3
DRAIN_MAX_S = 2.0
REAL = os.environ.get("XORGATE_P5B_REAL_HELPER", "/opt/xorgate-rs/python/obd_read.py")
LOG = os.environ.get("XORGATE_P5B_DRAIN_LOG", "/tmp/xorgate-p5b/obd-drain.ndjson")


def serial_path(argv):
    for i, a in enumerate(argv):
        if a == "--serial" and i + 1 < len(argv):
            return argv[i + 1]
        if a.startswith("--serial="):
            return a.split("=", 1)[1]
    return None


def drain(path):
    import select
    import termios
    import tty

    t0 = time.time()
    n = 0
    quiet = False
    try:
        fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    except OSError:
        return None
    try:
        try:
            # raw (a queued reply without a newline is readable too), applied
            # NOW: the default TCSAFLUSH would discard the queue uncounted
            tty.setraw(fd, termios.TCSANOW)
        except termios.error:
            pass
        last = time.time()
        while time.time() - t0 < DRAIN_MAX_S:
            r, _, _ = select.select([fd], [], [], 0.02)
            if r:
                try:
                    chunk = os.read(fd, 4096)
                except (BlockingIOError, OSError):
                    chunk = b""
                if chunk:
                    n += len(chunk)
                    last = time.time()
                    continue
            if time.time() - last >= DRAIN_QUIET_S:
                quiet = True
                break
        try:
            termios.tcflush(fd, termios.TCIFLUSH)
        except termios.error:
            pass
    finally:
        os.close(fd)
    return {"w": int(t0 * 1000), "serial": path, "bytes": n,
            "ms": int((time.time() - t0) * 1000), "quiet": quiet}


def main():
    args = sys.argv[1:]
    path = serial_path(args)
    if path:
        rec = drain(path)
        if rec is not None:
            try:
                with open(LOG, "a") as f:
                    f.write(json.dumps(rec) + "\n")
            except OSError:
                pass  # the log is evidence only; never block the helper on it
    os.execv(sys.executable, [sys.executable, REAL] + args)


if __name__ == "__main__":
    main()
