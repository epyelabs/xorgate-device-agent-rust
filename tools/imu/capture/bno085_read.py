#!/usr/bin/env python3
"""Phase 7c W0 capture wrapper: run the REAL BNO085 helper under strace.

Installed ONLY for a bench capture window (`phase-7c/bench-brief-w0.md`), as
`/var/lib/xorgate-rs/p7c/python/bno085_read.py`, by a runtime drop-in that
points `XORGATE_PYTHON_DIR` at that directory. The agent runs
`python3 <XORGATE_PYTHON_DIR>/bno085_read.py --address 0x4a`, so this file
must be Python. It never touches the bus itself: it `execv`s

    strace -q --seccomp-bpf -f -tt --relative-timestamps=ns -T -xx -s 4096
           -e trace=openat,ioctl,read,write,close -o <out>/w0-<n>.strace
           python3 /opt/xorgate-rs/python/bno085_read.py <args>

- `-q`: no attach/detach chatter. With `-o FILE` nothing reaches the helper's
  stderr (the agent logs every stderr line as an `[imu]` error), and unlike
  `-qq` the FILE keeps `+++ exited with N +++`, so the transcript carries the
  exit code (review F3; verified on strace 6.1: stderr empty).
- `--seccomp-bpf` (needs `-f`): the tracee stops only on the traced syscalls.
- `-tt` wall clock (orientation only) AND `--relative-timestamps=ns`
  monotonic deltas: the converter derives `t_us` from the summed ns deltas,
  so an NTP step cannot corrupt the replay clock (plan fix-check) and µs
  truncation does not accumulate (review F2: `-r` lost ~0.57 µs per line).
- `close` is traced so the converter can end the device-fd mapping exactly
  (review Q3).
- `-T` syscall durations, `-xx` every string as hex, `-s 4096` up to 4 KiB.

Process contract (verified with strace 6.1, the bookworm build):
- exec keeps the pid, the process group and fds 0/1/2: the agent still owns
  the helper's stdout, and nothing is printed by this wrapper on success.
- strace exits with the tracee's exit code; when the tracee dies by a signal,
  strace re-raises that signal on itself, so the agent sees the same
  exit code / signal it would see from the bare helper.
- `-o FILE PROG` implies `-I3`: strace BLOCKS fatal signals. A SIGTERM sent to
  strace alone is ignored. The agent's `stop()` signals the whole process
  group, so the TRACEE gets the SIGTERM directly, dies, and strace follows it.
  An operator who wants to stop the helper must therefore kill the traced
  `python3 /opt/xorgate-rs/python/bno085_read.py` pid, never strace.
- A SIGKILL of strace alone (the agent's kill-on-drop) leaves the tracee
  running untraced under its seccomp filter, where every traced syscall fails
  with ENOSYS; the helper then dies on its own. The unit's control-group kill
  covers that on restart.

Fail-safe: if the capture cannot be set up safely (output dir missing, not a
0700 directory owned by us, strace missing, the real helper missing), the
wrapper writes ONE stderr line `p7c-capture: <reason>; running the helper
untraced` (the journal shows it as an `[imu]` line, which the runbook checks
for) and execs the real helper without strace, so the IMU keeps working.
"""
import os
import stat
import sys

REAL_HELPER = "/opt/xorgate-rs/python/bno085_read.py"
OUT_DIR = "/run/xorgate-p7c"
STRACE = "/usr/bin/strace"
PREFIX = "w0-"
MAX_RUNS = 999

STRACE_FLAGS = [
    "-q",
    "--seccomp-bpf",
    "-f",
    "-tt",
    "--relative-timestamps=ns",
    "-T",
    "-xx",
    "-s",
    "4096",
    "-e",
    "trace=openat,ioctl,read,write,close",
]

# Test seams only. The bench drop-in sets none of these, so the defaults above
# apply there.
_ENV_OUT = "XORGATE_P7C_CAPTURE_OUT"
_ENV_REAL = "XORGATE_P7C_REAL_HELPER"
_ENV_STRACE = "XORGATE_P7C_STRACE"


def unsafe_reason(out_dir, strace, real_helper):
    """Why a traced run must not start, or None when it is safe."""
    try:
        st = os.lstat(out_dir)
    except OSError as e:
        return f"output dir {out_dir} unavailable ({e.strerror})"
    if not stat.S_ISDIR(st.st_mode):
        return f"output dir {out_dir} is not a directory"
    if st.st_uid != os.getuid():
        return f"output dir {out_dir} is not owned by uid {os.getuid()}"
    if stat.S_IMODE(st.st_mode) != 0o700:
        return f"output dir {out_dir} has mode {stat.S_IMODE(st.st_mode):o}, want 700"
    if not os.access(strace, os.X_OK):
        return f"{strace} is not executable"
    if not os.path.isfile(real_helper):
        return f"{real_helper} is missing"
    return None


def reserve_output(out_dir):
    """Create the next free `w0-<n>.strace` (O_EXCL, 0600) and return its path.

    Reserving with O_EXCL means two helper starts can never share or overwrite
    a transcript; strace then reopens the same file with O_TRUNC.
    """
    for n in range(1, MAX_RUNS + 1):
        path = os.path.join(out_dir, f"{PREFIX}{n}.strace")
        try:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600)
        except FileExistsError:
            continue
        os.close(fd)
        return path
    raise OSError(f"no free {PREFIX}<n>.strace slot in {out_dir}")


def build_argv(strace, out_path, real_helper, args):
    return [strace, *STRACE_FLAGS, "-o", out_path, "python3", real_helper, *args]


def main(argv=None):
    args = list(sys.argv[1:] if argv is None else argv)
    out_dir = os.environ.get(_ENV_OUT) or OUT_DIR
    real_helper = os.environ.get(_ENV_REAL) or REAL_HELPER
    strace = os.environ.get(_ENV_STRACE) or STRACE

    reason = unsafe_reason(out_dir, strace, real_helper)
    out_path = None
    if reason is None:
        try:
            out_path = reserve_output(out_dir)
        except OSError as e:
            reason = f"cannot reserve a transcript file ({e})"
    if reason is not None:
        sys.stderr.write(f"p7c-capture: {reason}; running the helper untraced\n")
        sys.stderr.flush()
        os.execvp("python3", ["python3", real_helper, *args])
    os.execv(strace, build_argv(strace, out_path, real_helper, args))


if __name__ == "__main__":
    main()
