#!/usr/bin/env python3
"""Phase 7c WP2 oracle self-test (plan §WP2: "a harness self-test pins the
`FileIO` half against a real `io.FileIO` on an `O_NONBLOCK` pipe").

Part 1 compares the shim's `FakeI2cFile` / `fake_open` / `fake_ioctl`
(`shim/sitecustomize.py`) with REAL CPython objects put in the same situation:
the observation (return value and type, or exception class, errno, str,
filename) must be identical, and a zero-length write must be exactly ONE
request on the fake and exactly ONE write(2) on the real FileIO
(`/proc/self/io` `syscw`).

Part 2 runs a probe script under the shim with the real stack and checks the
forced board (RASPBERRY_PI_5 / BCM2XXX; see oracle.helper_env), `board.SCL/SDA` = pins 3/2, that `busio.I2C`
takes the `generic_linux` (PureIO) path, that PureIO's `open`/`ioctl` and
`time.monotonic`/`time.sleep` are the fakes, and that lgpio's import-time call
was `gpiochip_open(0)` (D81), with an empty stderr.

Run as a NON-root user in the oracle image (EACCES needs it). Exit 0 = pass.
"""
import fcntl
import io
import json
import os
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import oracle  # noqa: E402


def _load_shim():
    # Load shim/sitecustomize.py under its own name: `site` already imported
    # Debian's `sitecustomize` into this process. Inactive here (no P7C_ORACLE_FD).
    import importlib.util  # noqa: PLC0415

    spec = importlib.util.spec_from_file_location("p7c_shim", os.path.join(HERE, "shim", "sitecustomize.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


shim = _load_shim()

RESULTS = []


class Local:
    """An in-process transport with canned replies; counts requests."""

    def __init__(self, reply):
        self.reply = reply
        self.requests = []

    def request(self, msg):
        self.requests.append(msg)
        return self.reply


def obs(fn):
    try:
        r = fn()
        return ["ret", type(r).__name__, r if not isinstance(r, bytes) else r.hex()]
    except BaseException as e:  # noqa: BLE001
        return ["exc", type(e).__name__, getattr(e, "errno", None), str(e), getattr(e, "filename", None)]


def case(name, real, fake, extra=None):
    ok = real == fake and (extra is None or extra[0])
    RESULTS.append({"case": name, "ok": ok, "real": real, "fake": fake, **({"note": extra[1]} if extra else {})})


def syscw():
    with open("/proc/self/io") as f:
        for line in f:
            if line.startswith("syscw:"):
                return int(line.split()[1])
    raise RuntimeError("no syscw")


def nb_pipe():
    r, w = os.pipe()
    os.set_blocking(r, False)
    os.set_blocking(w, False)
    return r, w


def part1():
    # A. read EAGAIN -> None
    r, w = nb_pipe()
    case("read EAGAIN", obs(lambda: io.FileIO(r, "r", closefd=False).read(4)),
         obs(lambda: shim.FakeI2cFile(Local({"eagain": True}), "/dev/i2c-1").read(4)))
    # C. short read -> the bytes read, type bytes
    os.write(w, b"xyz")
    case("short read", obs(lambda: io.FileIO(r, "r", closefd=False).read(8)),
         obs(lambda: shim.FakeI2cFile(Local({"hex": "78797a"}), "/dev/i2c-1").read(8)))
    # D. write -> the count
    case("write count", obs(lambda: io.FileIO(w, "w", closefd=False).write(b"abcd")),
         obs(lambda: shim.FakeI2cFile(Local({"ret": 4}), "/dev/i2c-1").write(b"abcd")))
    os.read(r, 100)
    # E. zero-length write: returns 0, ONE syscall / ONE request
    f = io.FileIO(w, "w", closefd=False)
    before = syscw()
    real = obs(lambda: f.write(b""))
    delta = syscw() - before
    t = Local({"ret": 0})
    fake = obs(lambda: shim.FakeI2cFile(t, "/dev/i2c-1").write(b""))
    case("zero-length write", real, fake,
         (delta == 1 and len(t.requests) == 1 and t.requests[0] == {"op": "write", "hex": ""},
          f"real write(2) count {delta}, fake requests {len(t.requests)}"))
    # F. a memoryview slice (what PureIO passes) is written as its bytes
    t = Local({"ret": 3})
    mv = memoryview(bytearray(b"hello"))[1:4]
    real = obs(lambda: io.FileIO(w, "w", closefd=False).write(mv))
    got = os.read(r, 100)
    fake = obs(lambda: shim.FakeI2cFile(t, "/dev/i2c-1").write(mv))
    case("memoryview write", real, fake, (got == b"ell" and t.requests[0]["hex"] == b"ell".hex(), "bytes on the wire"))
    # B. write EAGAIN -> None (pipe full)
    try:
        while True:
            os.write(w, b"\0" * 65536)
    except BlockingIOError:
        pass
    case("write EAGAIN", obs(lambda: io.FileIO(w, "w", closefd=False).write(b"abcd")),
         obs(lambda: shim.FakeI2cFile(Local({"eagain": True}), "/dev/i2c-1").write(b"abcd")))
    os.close(r)
    # H. write EPIPE -> BrokenPipeError
    case("write EPIPE", obs(lambda: io.FileIO(w, "w", closefd=False).write(b"x")),
         obs(lambda: shim.FakeI2cFile(Local({"errno": 32}), "/dev/i2c-1").write(b"x")))
    os.close(w)
    # G. read EIO -> OSError(5)
    mem = open("/proc/self/mem", "rb", buffering=0)  # noqa: SIM115
    case("read EIO", obs(lambda: mem.read(1)),
         obs(lambda: shim.FakeI2cFile(Local({"errno": 5}), "/dev/i2c-1").read(1)))
    mem.close()
    # I/J. open ENOENT / EACCES (builtins.open semantics, filename kept)
    missing = "/nonexistent-p7c/i2c-1"
    case("open ENOENT", obs(lambda: open(missing, "r+b", buffering=0)),
         obs(lambda: shim.make_open(Local({"errno": 2}))(missing, "r+b", buffering=0)))
    d = tempfile.mkdtemp()
    locked = os.path.join(d, "i2c-1")
    with open(locked, "wb"):
        pass
    os.chmod(locked, 0)
    case("open EACCES", obs(lambda: open(locked, "r+b", buffering=0)),
         obs(lambda: shim.make_open(Local({"errno": 13}))(locked, "r+b", buffering=0)),
         (os.geteuid() != 0, "must run non-root"))
    # K. ioctl error (I2C_SLAVE on a regular file -> ENOTTY)
    with open(os.path.join(d, "plain"), "wb") as pf:
        case("ioctl ENOTTY", obs(lambda: fcntl.ioctl(pf.fileno(), 0x0703, 0x4A)),
             obs(lambda: shim.make_ioctl(Local({"errno": 25}))(3, 0x0703, 0x4A)))
    # L. the open the shim answers is a FileIO-like object with fileno()
    fobj = shim.make_open(Local({"ok": True}))("/dev/i2c-1", "r+b", buffering=0)
    case("open ok", ["fileno", isinstance(fobj.fileno(), int)], ["fileno", True])


PROBE = r'''
import json, sys, time
import board, busio
import Adafruit_PureIO.smbus as smbus
from adafruit_blinka.agnostic import detector
i2c = busio.I2C(board.SCL, board.SDA, frequency=100000)
import lgpio
print(json.dumps({
    "board": detector.board.id, "chip": detector.chip.id,
    "scl": repr(board.SCL), "sda": repr(board.SDA),
    "i2c_module": type(i2c._i2c).__module__,
    "open_is_fake": smbus.open.__module__ == "sitecustomize",
    "ioctl_is_fake": smbus.ioctl.__module__ == "sitecustomize",
    "monotonic_is_fake": time.monotonic.__module__ == "sitecustomize",
    "sleep_is_fake": time.sleep.__module__ == "sitecustomize",
    "lgpio_calls": lgpio.CALLS,
}), flush=True)
'''


class ProbeBackend:
    def __init__(self):
        self.out = bytearray()
        self.requests = []

    def stdout(self, data):
        self.out += data

    def request(self, req):
        self.requests.append(req)
        return {"reply": {"ok": True} if req["op"] in ("open", "ioctl") else {}}

    def finish(self, exit_):
        self.exit = exit_


def part2():
    d = tempfile.mkdtemp()
    p = os.path.join(d, "probe.py")
    with open(p, "w") as f:
        f.write(PROBE)
    be = ProbeBackend()
    exit_, stderr = oracle.run_helper(be, helper=p, args=())
    try:
        got = json.loads(bytes(be.out).decode())
    except ValueError:
        got = {"stdout": bytes(be.out).decode(errors="replace")}
    want = {"board": "RASPBERRY_PI_5", "chip": "BCM2XXX", "scl": "3", "sda": "2",
            "i2c_module": "adafruit_blinka.microcontroller.generic_linux.i2c",
            "open_is_fake": True, "ioctl_is_fake": True, "monotonic_is_fake": True, "sleep_is_fake": True,
            "lgpio_calls": [["gpiochip_open", 0]]}
    RESULTS.append({"case": "real stack under the shim", "ok": got == want and exit_ == {"code": 0} and not stderr,
                    "got": got, "exit": exit_, "stderr": stderr.decode(errors="replace"),
                    "requests": [r["op"] for r in be.requests]})


def main():
    part1()
    part2()
    for r in RESULTS:
        print(("PASS " if r["ok"] else "FAIL ") + json.dumps(r))
    bad = [r for r in RESULTS if not r["ok"]]
    print(f"selftest: {len(RESULTS) - len(bad)}/{len(RESULTS)} pass")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
