"""Phase 7c WP2 oracle shim: the ONLY fake under the unmodified helper.

Loaded by CPython's `site` (via PYTHONPATH) into the helper process
`python3 python/bno085_read.py --address 0x4a` when `P7C_ORACLE_FD` is set.
Everything above it is the real bench stack (Blinka 9.1.0 busio +
generic_linux, PureIO 1.1.11, busdevice 5.2.17, bno08x 1.3.3; plan WP2,
review F3). It replaces, below PureIO:

- `Adafruit_PureIO.smbus.open` (a module global; `smbus.py:140`
  `open(f"/dev/i2c-{bus}", "r+b", buffering=0)`) with `fake_open`, which
  returns a `FakeI2cFile` that behaves like CPython's `FileIO` on i2c-dev:
  `read(n)`/`write(b)` are ONE request each (one `read(2)`/`write(2)`, a
  zero-length write included); EAGAIN -> `None` (FileIO's contract); any
  other errno -> `OSError(errno, strerror)` (CPython builds the subclass);
  the bytes a read returns may be short (i2c-dev's 8192 clamp is modelled by
  the parent);
- `Adafruit_PureIO.smbus.ioctl` (`from fcntl import ioctl`, `smbus.py:154`)
  with `fake_ioctl`: `OSError(errno, strerror)` or `0`, like `fcntl.ioctl`;
- `time.monotonic` / `time.sleep` with the parent's VIRTUAL clock: the parent
  returns the exact doubles (IEEE bits) and decides when a SIGINT lands in a
  sleep (it sends a real SIGINT while this process waits for the reply, so
  CPython raises KeyboardInterrupt out of `time.sleep`, as on the bench).

Every op is a synchronous JSON-line request to the parent over the socket in
`P7C_ORACLE_FD` (`tools/imu/oracle/oracle.py`), which models the bus (scripted
scenarios) or replays a `*.imutx` transcript, records the op stream and
drains this process's stdout between requests (emission order).

`FakeI2cFile` takes a transport so `selftest.py` can pin it against a real
`io.FileIO` without a parent. Nothing here reads a clock or prints.
"""

import os
import struct

_DEBIAN_SITECUSTOMIZE = "/etc/python3.11/sitecustomize.py"


class RpcTransport:
    """One JSON line out, one JSON line back, over an inherited socket fd."""

    def __init__(self, fd):
        import json  # noqa: PLC0415 - keep the module import cheap when inactive

        self._json = json
        self.fd = fd
        self._buf = b""

    def send(self, msg):
        data = (self._json.dumps(msg, separators=(",", ":")) + "\n").encode()
        view = memoryview(data)
        while view:
            n = os.write(self.fd, view)
            view = view[n:]

    def request(self, msg):
        self.send(msg)
        while b"\n" not in self._buf:
            chunk = os.read(self.fd, 65536)  # a SIGINT here raises KeyboardInterrupt
            if not chunk:
                raise SystemExit("p7c oracle: parent closed the channel")
            self._buf += chunk
        line, self._buf = self._buf.split(b"\n", 1)
        return self._json.loads(line)


def _oserror(errno_):
    # CPython maps errno -> subclass (FileNotFoundError, PermissionError, ...)
    # exactly as FileIO / fcntl do; the text is glibc's strerror.
    return OSError(errno_, os.strerror(errno_))


class FakeI2cFile:
    """CPython `FileIO` on `/dev/i2c-1` as PureIO uses it (`r+b`, unbuffered)."""

    FAKE_FD = 3

    def __init__(self, transport, name):
        self._t = transport
        self.name = name
        self.closed = False

    def fileno(self):
        return self.FAKE_FD

    def read(self, n):
        rep = self._t.request({"op": "read", "n": int(n)})
        if "errno" in rep:
            raise _oserror(rep["errno"])
        if rep.get("eagain"):
            return None  # FileIO.read on EAGAIN (CPython _io_FileIO_read_impl)
        return bytes.fromhex(rep["hex"])

    def write(self, b):
        data = bytes(b)  # PureIO passes a memoryview slice of the driver buffer
        rep = self._t.request({"op": "write", "hex": data.hex()})
        if "errno" in rep:
            raise _oserror(rep["errno"])
        if rep.get("eagain"):
            return None  # FileIO.write on EAGAIN
        return rep["ret"]

    def close(self):
        # PureIO closes in SMBus.close/__del__ (interpreter teardown): ONE
        # close(2), reported so a transcript that traces `close` lines up.
        if not self.closed:
            self.closed = True
            self._t.request({"op": "close"})


def make_open(transport, notes=None):
    def fake_open(file, mode="r", buffering=-1, *args, **kwargs):
        if mode != "r+b" or buffering != 0 or args or kwargs:
            raise AssertionError(f"p7c oracle: unexpected open({file!r}, {mode!r}, {buffering!r})")
        msg = {"op": "open", "path": str(file)}
        if notes is not None:
            msg["notes"] = notes()
        rep = transport.request(msg)
        if "errno" in rep:
            # builtins.open raises OSError(errno, strerror, filename).
            raise OSError(rep["errno"], os.strerror(rep["errno"]), str(file))
        return FakeI2cFile(transport, str(file))

    return fake_open


def make_ioctl(transport):
    def fake_ioctl(fd, request, arg=0, mutate_flag=True):
        rep = transport.request({"op": "ioctl", "req": int(request), "arg": int(arg)})
        if "errno" in rep:
            raise _oserror(rep["errno"])
        return 0

    return fake_ioctl


def _bits(x):
    return struct.unpack("<Q", struct.pack("<d", float(x)))[0]


def _float(bits):
    return struct.unpack("<d", struct.pack("<Q", bits))[0]


def make_clock(transport):
    def monotonic():
        return _float(transport.request({"op": "monotonic"})["bits"])

    def sleep(secs):
        transport.request({"op": "sleep", "bits": _bits(secs)})

    return monotonic, sleep


def _lgpio_calls():
    import sys  # noqa: PLC0415

    mod = sys.modules.get("lgpio")
    return list(getattr(mod, "CALLS", [])) if mod is not None else None


def install(fd):
    import time  # noqa: PLC0415

    import Adafruit_PureIO.smbus as smbus  # noqa: PLC0415

    t = RpcTransport(fd)
    smbus.open = make_open(t, notes=lambda: {"lgpio": _lgpio_calls()})
    smbus.ioctl = make_ioctl(t)
    time.monotonic, time.sleep = make_clock(t)
    return t


_fd = os.environ.get("P7C_ORACLE_FD")
if _fd:
    install(int(_fd))

# Chain Debian's own sitecustomize (this module shadows it on sys.path).
if os.path.exists(_DEBIAN_SITECUSTOMIZE):
    with open(_DEBIAN_SITECUSTOMIZE) as _f:
        exec(compile(_f.read(), _DEBIAN_SITECUSTOMIZE, "exec"), {"__name__": "sitecustomize"})  # noqa: S102
