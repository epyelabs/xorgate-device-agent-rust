#!/usr/bin/env python3
"""Stream OBD2 vehicle data as line-delimited JSON for the Xorgate device agent.

Entry point for the `obd` package next to this file (stdlib only, Python 3.11).
See python/obd/__init__.py and the P1 brief for the contract. Usage:

  obd_read.py --adapter AA:BB:CC:DD:EE:FF [--channel 1] [--pin 1234] [--poll-hz 1]
              [--fields rpm,speed,...] [--power-source switched|constant]
              [--audit-dir DIR]
  obd_read.py --serial /dev/pts/N ...   (ELM327 emulator / USB adapter)
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from obd.main import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
