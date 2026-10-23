"""H12 helper: hold an ELM327-emulator pty open until killed; print the pty.

    python3 emulator.py <python_dir> <scenario> <pty_file>
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
