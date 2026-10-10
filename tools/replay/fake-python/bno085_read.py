#!/usr/bin/env python3
"""Rig stand-in for python/bno085_read.py (record/replay rig, tools/replay).

The agent spawns `python3 <XORGATE_PYTHON_DIR>/bno085_read.py --address 0x4a`
exactly as on the device; this helper connects to the player's unix socket
($RIG_IMU_SOCK, one per agent) and relays the tape's IMU stdout lines verbatim.
`__EXIT__ <code>` from the player ends it with that exit code (a tape
`imu_exit`). SIGINT ends it with 0 like the real helper (KeyboardInterrupt).
"""
import json
import os
import socket
import sys


def main():
    path = os.environ.get("RIG_IMU_SOCK")
    if not path:
        print(json.dumps({"type": "error", "error": "init failed: RIG_IMU_SOCK unset (rig fake)"}), flush=True)
        return 3
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.connect(path)
    except OSError as exc:
        print(json.dumps({"type": "error", "error": f"init failed: rig socket {exc}"}), flush=True)
        return 3
    try:
        for line in s.makefile("r", encoding="utf-8", newline="\n"):
            if line.startswith("__EXIT__"):
                parts = line.split()
                return int(parts[1]) if len(parts) > 1 else 1
            sys.stdout.write(line)
            sys.stdout.flush()
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
