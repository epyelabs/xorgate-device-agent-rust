#!/usr/bin/env python3
"""Mutation check for the WP0a tooling: every mutant must make the suite FAIL.

    python3 tools/imu/tests/mutations.py        # exit 0 iff the baseline passes and every mutant is killed

Each mutant is a single exact text replacement in a copy of tools/imu (the
pattern must occur exactly once). The suite runs with `unittest discover`.
"""
import os
import shutil
import subprocess
import sys
import tempfile

IMU = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

MUTANTS = [
    # (id, file, old, new, what)
    ("S1", "scrub_imutx.py", "        units = field.units\n",
     '        units = field.units[:-1] if field.kind == "rv" else field.units\n',
     "scrubber skips one RV word (the last of each report)"),
    ("S2", "scrub_imutx.py", "        values = _draw_units(field, gen)\n",
     '        if field.kind == "rv_acc":\n            continue\n        values = _draw_units(field, gen)\n',
     "scrubber skips the RV accuracy word"),
    ("S3", "scrub_imutx.py", "        values = _draw_units(field, gen)\n",
     '        if field.kind == "delay":\n            continue\n        values = _draw_units(field, gen)\n',
     "scrubber skips the delay byte"),
    ("S4", "scrub_imutx.py", "            while cand == orig:\n", "            while False:\n",
     "scrubber keeps a position whose draw equals the original (no redraw)"),
    ("S5", "scrub_imutx.py", '        cls.add_field("rv", _words(pos, 4, 4))\n',
     '        cls.add_field("rv", _words(pos, 4, 3))\n',
     "classifier misses the 4th RV word (caught by the golden-position test, not the self-check)"),
    ("S6", "scrub_imutx.py", "            if live and all(raw_b[p[0]][p[1]] == new_b[p[0]][p[1]] for p in live):\n",
     "            if False:\n",
     "self-check never flags a kept data word"),
    ("S7", "scrub_imutx.py", "    if channel in INPUT_CHANNELS:\n        # Unconsumed",
     "    if False:\n        # Unconsumed", "scrubber leaves unconsumed input-report payload (tail) unscrubbed"),
    ("C1", "imutx.py", '        errno = r.group("errno")\n', "        errno = None\n", "converter drops the errno"),
    ("C2", "imutx.py", "        if not self._is_device_fd(pid, fd):\n",
     '        if not self._is_device_fd(pid, fd) and name != "read":\n',
     "converter keeps reads on a non-i2c fd"),
    ("C3", "imutx.py", '        if m.group("rel") is not None:\n', "        if False:\n",
     "converter ignores -r and uses the wall clock (NTP step breaks t_us)"),
    ("C4", "imutx.py", "        os.unlink(a.raw)\n", "        pass\n", "converter keeps the raw strace (F18)"),
    ("C5", "imutx.py", '                self.counts["other_pid_stdout_writes"] += 1\n                return\n',
     '                self.counts["other_pid_stdout_writes"] += 1\n',
     "converter keeps another pid's fd-1 writes as stdout"),
    ("C6", "imutx.py", "            self.pending[pid] = (mu.group(\"name\"), mu.group(\"head\"), t_us)\n",
     "            self.pending[pid] = (mu.group(\"name\"), mu.group(\"head\"), t_us + 1)\n",
     "converter stamps a split syscall with a wrong (non-entry) time"),
    ("W1", "capture/bno085_read.py", '    "-r",\n', "", "wrapper drops -r (no monotonic deltas)"),
    ("W2", "capture/bno085_read.py", '    "-qq",\n', "", "wrapper drops -qq (strace chatter on the helper's stderr)"),
    ("W3", "capture/bno085_read.py", "    if stat.S_IMODE(st.st_mode) != 0o700:\n", "    if False:\n",
     "wrapper accepts a non-0700 output dir"),
]


def run_suite(root):
    p = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests"], cwd=root,
                       capture_output=True, text=True)
    last = (p.stderr.strip().splitlines() or [""])[-1]
    return p.returncode, last


def main():
    rc, last = run_suite(IMU)
    print(f"baseline: rc={rc} ({last})")
    if rc != 0:
        print("BASELINE FAILS — mutation results meaningless")
        return 2
    survivors = 0
    print("| id | mutant | suite |")
    print("|---|---|---|")
    for mid, rel, old, new, what in MUTANTS:
        with tempfile.TemporaryDirectory() as d:
            root = os.path.join(d, "imu")
            shutil.copytree(IMU, root, ignore=shutil.ignore_patterns("__pycache__"))
            path = os.path.join(root, rel)
            src = open(path).read()
            n = src.count(old)
            if n != 1:
                print(f"| {mid} | {what} | PATTERN FOUND {n}x (broken mutant) |")
                survivors += 1
                continue
            open(path, "w").write(src.replace(old, new))
            mrc, mlast = run_suite(root)
        verdict = f"FAILS ({mlast})" if mrc != 0 else "PASSES — SURVIVED"
        if mrc == 0:
            survivors += 1
        print(f"| {mid} | {what} | {verdict} |")
    print(f"survivors: {survivors}")
    return 1 if survivors else 0


if __name__ == "__main__":
    sys.exit(main())
