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
    ("C3", "imutx.py", '        self.t_rel_ns += _nsec(m.group("rel"))\n',
     '        self.t_rel_ns += _nsec(m.group("rel")) // 1000 * 1000\n',
     "converter sums us-truncated deltas (review F2 drift)"),
    ("C4", "imutx.py", "        os.unlink(a.raw)\n", "        pass\n", "converter keeps the raw strace (F18)"),
    ("C5", "imutx.py", '                self.counts["other_pid_stdout_writes"] += 1\n                return\n',
     '                self.counts["other_pid_stdout_writes"] += 1\n',
     "converter keeps another pid's fd-1 writes as stdout"),
    ("C6", "imutx.py", "            self.pending[pid] = (mu.group(\"name\"), mu.group(\"head\"), t_us)\n",
     "            self.pending[pid] = (mu.group(\"name\"), mu.group(\"head\"), t_us + 1)\n",
     "converter stamps a split syscall with a wrong (non-entry) time"),
    ("C7", "imutx.py", r'''    r"\(\+\s*(?P<rel>\d+\.\d{9})\)\s+"''', r'''    r"\(\+\s*(?P<rel>\d+\.\d{6}(?:\d{3})?)\)\s+"''',
     "converter accepts us-precision deltas (review F5b)"),
    ("C8", "imutx.py", r'''    r"(?P<wall>\d{2}:\d{2}:\d{2}\.\d{6})\s+"''', r'''    r"(?P<wall>\d{2}:\d{2}:\d{2}\.\d{6}|\d+\.\d{6})\s+"''',
     "converter accepts a -ttt / -r-style float leader (review F5b)"),
    ("C9", "imutx.py", "            if self._is_device_fd(pid, fd):\n                self.i2c_fds[pid].discard(fd)\n",
     "            if False:\n                self.i2c_fds[pid].discard(fd)\n",
     "converter ignores close() of the device fd (review Q3)"),
    ("C10", "imutx.py", '                self._emit({"op": "exit", "code": int(me.group("code")), "t_us": t_us})\n',
     "                pass\n", "converter drops the normal exit code (review F3)"),
    ("R10", "imutx.py", "                if pid == self.main_pid:\n                    if fd is not None:\n",
     "                if True:\n                    if fd is not None:\n",
     "reviewer R10: converter keeps a FOREIGN pid's device open"),
    ("R5", "scrub_imutx.py", "            and ret == req\n", "",
     "reviewer R5: full-read detection ignores ret == req"),
    ("R9", "scrub_imutx.py", "                cand = bytes(gen.byte() for _ in cand)\n",
     "                cand = bytes(gen.byte() for _ in range(2))\n",
     "reviewer R9: the redraw of a 1-byte unit is broken"),
    ("S8", "scrub_imutx.py", '    meta["scrubbed"] = {"tool": TOOL, "seed": "secret", "stdout": stdout_state}\n',
     '    meta["scrubbed"] = {"tool": TOOL, "seed": seed, "stdout": stdout_state}\n',
     "scrubber records the seed in meta (review F6)"),
    ("W1", "capture/bno085_read.py", '    "--relative-timestamps=ns",\n', '    "-r",\n',
     "wrapper uses us-precision -r (review F2)"),
    ("W2", "capture/bno085_read.py", '    "-q",\n', "", "wrapper drops the quiet flag"),
    ("W4", "capture/bno085_read.py", '    "-q",\n', '    "-qq",\n', "wrapper uses -qq (loses the exit line, review F3)"),
    ("W5", "capture/bno085_read.py", '    "trace=openat,ioctl,read,write,close",\n', '    "trace=openat,ioctl,read,write",\n',
     "wrapper stops tracing close (review Q3)"),
    ("W3", "capture/bno085_read.py", "    if stat.S_IMODE(st.st_mode) != 0o700:\n", "    if False:\n",
     "wrapper accepts a non-0700 output dir"),
    ("R6", "capture/bno085_read.py", "    if st.st_uid != os.getuid():\n", "    if False:\n",
     "reviewer R6: wrapper accepts an output dir owned by another uid"),
    # Reviewer R4 (the -r origin offset not subtracted) is NOT listed: strace
    # prints the first delta as 0, so the mutant is equivalent on real data.
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
