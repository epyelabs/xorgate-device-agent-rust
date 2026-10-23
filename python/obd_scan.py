#!/usr/bin/env python3
"""xorgate OBD2: list nearby Bluetooth devices (never pairs).

Usage: obd_scan.py [--seconds N]
Prints ONE line of JSON: [{"mac": "AA:BB:...", "name": "OBDII", "rssi": -58}, ...]
strongest first, unnamed devices dropped. Exit 2 when there is no Bluetooth
controller / bluetoothctl. Spawned by the agent for the `obd_scan` command and
by `xorgate obd scan`. Stdlib only (Python 3.11).
"""
import argparse
import json
import sys

from obd.scan import discover


def main(argv=None):
    ap = argparse.ArgumentParser(prog="obd_scan.py")
    ap.add_argument("--seconds", type=float, default=25.0, help="discovery window (default 25)")
    args = ap.parse_args(argv)
    try:
        candidates = discover(min(120.0, max(1.0, args.seconds)),
                              log=lambda m: sys.stderr.write("[obd_scan] %s\n" % m))
    except RuntimeError as exc:
        sys.stderr.write("[obd_scan] %s\n" % exc)
        return 2
    sys.stdout.write(json.dumps(candidates, separators=(",", ":")) + "\n")
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
