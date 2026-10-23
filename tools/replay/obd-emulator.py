"""Rig (Phase 5b WP4): hold one ELM327-emulator pty open until SIGTERM and
write its device path to <pty_file>. One per agent (an ELM327 link is
single-client). Reuses the vendored test harness (python/tests/
emulator_harness.py, ELM327-emulator 4.0.0 from /opt/elm-venv via
$OBD_ELM_BIN), like WP2's tools/obd-e2e/emulator.py.

    python3 obd-emulator.py <python_dir> <scenario> <pty_file>
"""
import os
import signal
import sys
import time

sys.path.insert(0, os.path.join(sys.argv[1], "tests"))
from emulator_harness import Emulator  # noqa: E402

scenario, out = sys.argv[2], sys.argv[3]
stop = []
signal.signal(signal.SIGTERM, lambda *_: stop.append(1))
with Emulator(scenario, module="taos_2022") as emu:
    with open(out + ".tmp", "w") as f:
        f.write(emu.pty)
    os.rename(out + ".tmp", out)
    while not stop:
        time.sleep(0.2)
