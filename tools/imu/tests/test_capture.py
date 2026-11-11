"""Capture-wrapper tests. The integration class needs a real /usr/bin/strace
(the bookworm test container); it is skipped elsewhere."""
import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
IMU = os.path.dirname(HERE)
WRAPPER = os.path.join(IMU, "capture", "bno085_read.py")
sys.path.insert(0, os.path.join(IMU, "capture"))
sys.path.insert(0, IMU)

import bno085_read as W  # noqa: E402  (the wrapper, not the real helper)
import imutx  # noqa: E402

PLAN_FLAGS = ["-q", "--seccomp-bpf", "-f", "-tt", "--relative-timestamps=ns", "-T", "-xx", "-s", "4096",
              "-e", "trace=openat,ioctl,read,write,close"]


class Argv(unittest.TestCase):
    def test_exact_strace_argv(self):
        argv = W.build_argv("/usr/bin/strace", "/run/xorgate-p7c/w0-1.strace", W.REAL_HELPER, ["--address", "0x4a"])
        self.assertEqual(
            argv,
            ["/usr/bin/strace", *PLAN_FLAGS, "-o", "/run/xorgate-p7c/w0-1.strace", "python3",
             "/opt/xorgate-rs/python/bno085_read.py", "--address", "0x4a"],
        )

    def test_bench_defaults(self):
        self.assertEqual(W.OUT_DIR, "/run/xorgate-p7c")
        self.assertEqual(W.REAL_HELPER, "/opt/xorgate-rs/python/bno085_read.py")
        self.assertEqual(W.STRACE, "/usr/bin/strace")


class Safety(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.out = os.path.join(self.d, "out")
        os.mkdir(self.out, 0o700)
        os.chmod(self.out, 0o700)
        self.helper = os.path.join(self.d, "helper.py")
        open(self.helper, "w").close()
        self.strace = os.path.join(self.d, "strace")
        with open(self.strace, "w") as f:
            f.write("#!/bin/sh\n")
        os.chmod(self.strace, 0o755)

    def tearDown(self):
        shutil.rmtree(self.d)

    def test_safe(self):
        self.assertIsNone(W.unsafe_reason(self.out, self.strace, self.helper))

    def test_missing_dir(self):
        self.assertIn("unavailable", W.unsafe_reason(self.out + "x", self.strace, self.helper))

    def test_wrong_mode(self):
        os.chmod(self.out, 0o755)
        self.assertIn("mode 755", W.unsafe_reason(self.out, self.strace, self.helper))

    def test_symlinked_dir_refused(self):
        link = os.path.join(self.d, "link")
        os.symlink(self.out, link)
        self.assertIn("not a directory", W.unsafe_reason(link, self.strace, self.helper))

    @unittest.skipUnless(os.getuid() == 0, "needs root to chown (the test container)")
    def test_dir_owned_by_another_uid(self):
        # review F9 (R6)
        os.chown(self.out, 65534, 65534)
        self.assertIn("not owned by uid", W.unsafe_reason(self.out, self.strace, self.helper))

    def test_strace_not_executable(self):
        os.chmod(self.strace, 0o644)
        self.assertIn("not executable", W.unsafe_reason(self.out, self.strace, self.helper))

    def test_helper_missing(self):
        os.unlink(self.helper)
        self.assertIn("missing", W.unsafe_reason(self.out, self.strace, self.helper))

    def test_reserve_is_exclusive_and_private(self):
        a = W.reserve_output(self.out)
        b = W.reserve_output(self.out)
        self.assertEqual([os.path.basename(a), os.path.basename(b)], ["w0-1.strace", "w0-2.strace"])
        self.assertEqual(stat.S_IMODE(os.stat(a).st_mode), 0o600)


FAKE_HELPER = textwrap.dedent(
    """
    import fcntl, os, sys, time
    dev = os.environ["FAKE_DEV"]
    f = open(dev, "r+b", buffering=0)
    try:
        fcntl.ioctl(f.fileno(), 0x0703, 0x4A)
    except OSError:
        pass
    f.write(b"")
    f.write(bytes([5, 0, 1, 0, 1]))
    f.seek(0)
    f.read(4)
    if sys.argv[1:] != ["--loop"]:
        f.close()  # a deterministic device close (otherwise left to interpreter shutdown)
    print('{"type": "ready", "address": "0x4a"}', flush=True)
    if sys.argv[1:] == ["--loop"]:
        while True:
            time.sleep(0.05)
            print('{"type": "sample"}', flush=True)
    print('{"type": "error", "error": "init failed at 0x4a: synthetic"}', flush=True)
    sys.exit(3)
    """
)


class FakeStraceRun(unittest.TestCase):
    """The exec path without ptrace: a fake strace that records its argv and
    execs the tracee command. Checks stdout pass-through and the exit code."""

    def test_exec_contract(self):
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "out")
            os.mkdir(out, 0o700)
            os.chmod(out, 0o700)
            helper = os.path.join(d, "helper.py")
            with open(helper, "w") as f:
                f.write("import sys\nprint('{\"type\": \"ready\"}', flush=True)\nsys.exit(4)\n")
            argv_log = os.path.join(d, "argv.json")
            fake = os.path.join(d, "strace")
            with open(fake, "w") as f:
                f.write(
                    "#!" + sys.executable + "\nimport json, os, sys\n"
                    f"json.dump(sys.argv, open({argv_log!r}, 'w'))\n"
                    "i = sys.argv.index('python3')\n"
                    f"os.execv({sys.executable!r}, [{sys.executable!r}] + sys.argv[i + 1:])\n"
                )
            os.chmod(fake, 0o755)
            env = dict(os.environ, XORGATE_P7C_CAPTURE_OUT=out, XORGATE_P7C_REAL_HELPER=helper, XORGATE_P7C_STRACE=fake)
            p = subprocess.run([sys.executable, WRAPPER, "--address", "0x4a"], env=env, capture_output=True, text=True)
            self.assertEqual(p.returncode, 4)
            self.assertEqual(p.stdout, '{"type": "ready"}\n')
            self.assertEqual(p.stderr, "")
            with open(argv_log) as f:
                argv = json.load(f)
            self.assertEqual(argv[1:-4], [*PLAN_FLAGS, "-o", os.path.join(out, "w0-1.strace")])
            self.assertEqual(argv[-4:], ["python3", helper, "--address", "0x4a"])

    def test_unsafe_dir_runs_untraced_with_one_stderr_line(self):
        with tempfile.TemporaryDirectory() as d:
            helper = os.path.join(d, "helper.py")
            with open(helper, "w") as f:
                f.write("import sys\nprint('{\"type\": \"ready\"}', flush=True)\nsys.exit(0)\n")
            env = dict(os.environ, XORGATE_P7C_CAPTURE_OUT=os.path.join(d, "absent"), XORGATE_P7C_REAL_HELPER=helper)
            p = subprocess.run([sys.executable, WRAPPER], env=env, capture_output=True, text=True)
            self.assertEqual(p.returncode, 0)
            self.assertEqual(p.stdout, '{"type": "ready"}\n')
            self.assertEqual(len(p.stderr.splitlines()), 1)
            self.assertTrue(p.stderr.startswith("p7c-capture: "))


@unittest.skipUnless(os.access("/usr/bin/strace", os.X_OK), "needs /usr/bin/strace (test container)")
class RealStrace(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.out = os.path.join(self.d, "out")
        os.mkdir(self.out, 0o700)
        os.chmod(self.out, 0o700)
        self.dev = os.path.join(self.d, "fake-i2c")
        with open(self.dev, "wb") as f:
            f.write(bytes([0x14, 0x00, 0x03, 0x07, 0, 0, 0, 0]))
        self.helper = os.path.join(self.d, "helper.py")
        with open(self.helper, "w") as f:
            f.write(FAKE_HELPER)
        self.env = dict(os.environ, XORGATE_P7C_CAPTURE_OUT=self.out, XORGATE_P7C_REAL_HELPER=self.helper,
                        FAKE_DEV=self.dev)

    def tearDown(self):
        shutil.rmtree(self.d)

    def test_exit_code_stdout_and_conversion(self):
        p = subprocess.run([sys.executable, WRAPPER, "--address", "0x4a"], env=self.env, capture_output=True,
                           text=True, cwd=self.d)
        self.assertEqual(p.returncode, 3, p.stderr)
        self.assertEqual(p.stderr, "", "-qq: strace must say nothing on stderr")
        self.assertEqual(len(p.stdout.splitlines()), 2)
        raw = os.path.join(self.out, "w0-1.strace")
        out = os.path.join(self.out, "w0-1.imutx")
        with open(os.devnull, "w") as null:
            sys_stdout, sys.stdout = sys.stdout, null
            try:
                rc = imutx.main(["convert", raw, "-o", out, "--device", self.dev])
            finally:
                sys.stdout = sys_stdout
        self.assertEqual(rc, 0)
        self.assertFalse(os.path.exists(raw))
        recs = imutx.read_records(out)
        self.assertEqual(recs[0]["clock"], "monotonic-ns")
        ops = [(r["op"], r.get("req"), r.get("len"), r.get("hex"), r.get("errno")) for r in recs[1:]]
        self.assertEqual(
            ops,
            [
                ("open", None, None, None, None),
                ("ioctl", 0x0703, None, None, "ENOTTY"),
                ("write", None, 0, "", None),
                ("write", None, 5, "0500010001", None),
                ("read", None, 4, "05000100", None),  # reads back the bytes it wrote
                ("close", None, None, None, None),
                ("stdout", None, None, None, None),
                ("stdout", None, None, None, None),
                ("exit", None, None, None, None),
            ],
        )
        self.assertEqual(recs[-1]["code"], 3, "-q keeps `+++ exited with N +++` (review F3)")
        ts = [r["t_us"] for r in recs[1:]]
        self.assertEqual(ts, sorted(ts))

    def test_relative_ns_drift_is_bounded(self):
        # review F2: >= 50k traced lines over a few seconds; the converter's
        # t_us (summed ns deltas) must track the -tt wall span within 1 ms.
        loop = os.path.join(self.d, "loop.py")
        with open(loop, "w") as f:
            f.write("import os, sys\nfd = os.open(os.environ['FAKE_DEV'], os.O_RDONLY)\n"
                    "for _ in range(250000):\n    os.read(fd, 1)\nsys.exit(0)\n")
        env = dict(self.env, XORGATE_P7C_REAL_HELPER=loop)
        p = subprocess.run([sys.executable, WRAPPER], env=env, capture_output=True, text=True, cwd=self.d)
        self.assertEqual(p.returncode, 0, p.stderr)
        raw = os.path.join(self.out, "w0-1.strace")
        with open(raw) as f:
            lines = f.read().splitlines()
        self.assertGreaterEqual(len(lines), 50000)

        def wall_us(line):
            h, m, sec = line.split()[1].split(":")
            return (int(h) * 3600 + int(m) * 60) * 1_000_000 + round(float(sec) * 1_000_000)

        span = wall_us(lines[-1]) - wall_us(lines[0])
        self.assertGreater(span, 1_000_000, "the run should take more than a second")
        recs, _ = imutx.convert_lines(lines, device=self.dev)
        self.assertEqual(recs[-1]["op"], "exit")
        self.assertLess(abs(recs[-1]["t_us"] - span), 1000, f"t_us {recs[-1]['t_us']} vs wall span {span}")

    def _start_loop(self):
        p = subprocess.Popen([sys.executable, WRAPPER, "--loop"], env=self.env, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, cwd=self.d, start_new_session=True)
        self.assertTrue(p.stdout.readline())  # ready
        tracee = None
        for _ in range(50):
            q = subprocess.run(["pgrep", "-f", "^python3 " + self.helper + " --loop"], capture_output=True, text=True)
            if q.stdout.strip():
                tracee = int(q.stdout.split()[0])
                break
            time.sleep(0.05)
        self.assertIsNotNone(tracee, "the tracee must match ^python3 <real helper>")
        return p, tracee

    def test_sigterm_to_strace_alone_is_ignored_tracee_kill_propagates(self):
        p, tracee = self._start_loop()
        os.kill(p.pid, signal.SIGTERM)  # strace (the wrapper's pid after exec)
        time.sleep(0.5)
        self.assertIsNone(p.poll(), "-o FILE PROG blocks fatal signals in strace (-I3)")
        os.kill(tracee, signal.SIGTERM)
        self.assertEqual(p.wait(timeout=10), -signal.SIGTERM)
        p.stdout.close()
        p.stderr.close()

    def test_group_sigterm_like_the_agent_stop(self):
        p, _ = self._start_loop()
        os.killpg(p.pid, signal.SIGTERM)
        self.assertEqual(p.wait(timeout=10), -signal.SIGTERM)
        p.stdout.close()
        p.stderr.close()


if __name__ == "__main__":
    unittest.main()
