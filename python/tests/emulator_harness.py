"""Launch ELM327-emulator 4.0.0 on a pty with a scenario from python/obd/emulator.

Used by the integration tests and handy for manual runs:
    python3 python/tests/emulator_harness.py [scenario] [seconds]

Finds the `elm` console script via $OBD_ELM_BIN, then <repo>/.venv-obd/bin/elm,
then PATH. Returns None from find_elm() when the emulator is not installed so
tests can skip.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PYTHON_DIR = os.path.dirname(HERE)
REPO = os.path.dirname(PYTHON_DIR)
EMULATOR_DIR = os.path.join(PYTHON_DIR, "obd", "emulator")
HELPER = os.path.join(PYTHON_DIR, "obd_read.py")


def find_elm():
    cand = os.environ.get("OBD_ELM_BIN")
    if cand and os.access(cand, os.X_OK):
        return cand
    cand = os.path.join(REPO, ".venv-obd", "bin", "elm")
    if os.access(cand, os.X_OK):
        return cand
    return shutil.which("elm")


class Emulator:
    """Context manager: `with Emulator("taos-2022") as emu: emu.pty`."""

    def __init__(self, scenario="taos-2022", module="taos_2022", elm=None, out_file=None):
        self.scenario, self.module = scenario, module
        self.elm = elm or find_elm()
        self.out_file = out_file or os.path.join(
            os.environ.get("TMPDIR", "/tmp"), "elm-%s-%d.out" % (scenario, os.getpid()))
        self.proc = None
        self.pty = None
        self.workdir = None

    def __enter__(self):
        if not self.elm:
            raise RuntimeError("ELM327-emulator not installed")
        if os.path.exists(self.out_file):
            os.remove(self.out_file)
        # Each emulator gets its own cwd: `merge` imports the module from cwd
        # and the emulator writes elm.log there, so parallel runs never collide.
        self.workdir = tempfile.mkdtemp(prefix="elm-%s-" % self.scenario)
        shutil.copy(os.path.join(EMULATOR_DIR, self.module + ".py"), self.workdir)
        env = dict(os.environ, PYTHONPATH=self.workdir)
        self.proc = subprocess.Popen(
            [self.elm, "-b", self.out_file], cwd=self.workdir, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            text=True)
        deadline = time.time() + 20
        while time.time() < deadline:
            if os.path.exists(self.out_file):
                with open(self.out_file) as f:
                    first = f.readline().strip()
                if first.startswith("/dev/"):
                    self.pty = first
                    break
            if self.proc.poll() is not None:
                raise RuntimeError("emulator exited early (%s)" % self.proc.returncode)
            time.sleep(0.1)
        if not self.pty:
            self.close()
            raise RuntimeError("emulator did not print a pty name")
        self.command("merge %s" % self.module)
        self.command("scenario %s" % self.scenario)
        time.sleep(0.5)
        with open(self.out_file) as f:
            text = f.read()
        if "switched to '%s'" % self.scenario not in text:
            self.close()
            raise RuntimeError("scenario %s not loaded:\n%s" % (self.scenario, text))
        return self

    def command(self, line):
        """Send an interpreter command (e.g. 'scenario engineoff')."""
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def close(self):
        if self.proc and self.proc.poll() is None:
            try:
                self.proc.stdin.write("quit\n")
                self.proc.stdin.flush()
            except (BrokenPipeError, OSError):
                pass
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=3)
        if self.proc and self.proc.stdin:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
        try:
            os.remove(self.out_file)
        except OSError:
            pass
        if self.workdir:
            shutil.rmtree(self.workdir, ignore_errors=True)
            self.workdir = None

    def __exit__(self, *exc):
        self.close()
        return False


def run_helper(args, seconds, python=None, stop_signal=signal.SIGTERM, until=None, env=None):
    """Run obd_read.py with `args` for `seconds` (or until `until(lines)` is
    true), then send `stop_signal`. Returns (exit_code, parsed_lines, stderr).
    `env` adds to the inherited environment (e.g. OBD_TEST_TIMESCALE).
    """
    python = python or sys.executable
    proc_env = dict(os.environ, **(env or {}))
    proc = subprocess.Popen([python, HELPER] + list(args), stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, env=proc_env)
    lines, raw_err = [], []

    def reader():
        for ln in proc.stdout:
            ln = ln.strip()
            if not ln:
                continue
            try:
                lines.append(json.loads(ln))
            except ValueError:
                lines.append({"type": "_unparsed", "line": ln})

    def err_reader():
        for ln in proc.stderr:
            raw_err.append(ln)

    threads = [threading.Thread(target=reader, daemon=True),
               threading.Thread(target=err_reader, daemon=True)]
    for t in threads:
        t.start()
    deadline = time.time() + seconds
    while time.time() < deadline and proc.poll() is None:
        if until and until(lines):
            break
        time.sleep(0.1)
    if proc.poll() is None:
        signals = list(stop_signal) if isinstance(stop_signal, (list, tuple)) else [stop_signal]
        for i, sig in enumerate(signals):
            if i:
                time.sleep(0.1)
            if proc.poll() is not None:
                break
            proc.send_signal(sig)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    for t in threads:
        t.join(timeout=2)
    for stream in (proc.stdout, proc.stderr):
        try:
            stream.close()
        except OSError:
            pass
    return proc.returncode, lines, "".join(raw_err)


if __name__ == "__main__":
    scenario = sys.argv[1] if len(sys.argv) > 1 else "taos-2022"
    secs = float(sys.argv[2]) if len(sys.argv) > 2 else 12
    with Emulator(scenario) as emu:
        print("emulator on", emu.pty, file=sys.stderr)
        audit = os.path.join(os.environ.get("TMPDIR", "/tmp"), "obd-audit-manual")
        code, out, err = run_helper(["--serial", emu.pty, "--poll-hz", "1", "--audit-dir", audit], secs)
        for o in out:
            print(json.dumps(o))
        print("exit", code, file=sys.stderr)
        print(err, file=sys.stderr)
