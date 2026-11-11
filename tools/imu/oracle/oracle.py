#!/usr/bin/env python3
"""Phase 7c WP2: the Python ORACLE (plan `phase-7c-rust-imu.md` §WP2, C4/C5/C10b).

Runs the UNMODIFIED `python/bno085_read.py` on the real bench Python stack
(apt python3 3.11.2, Blinka 9.1.0, PlatformDetect 3.88.0, PureIO 1.1.11,
busdevice 5.2.17, bno08x 1.3.3, a stub lgpio, a forced CM5/BCM2712 board),
faked only BELOW PureIO by `shim/sitecustomize.py`. Every i2c-dev op and every
`time.monotonic`/`time.sleep` of the helper is a synchronous request to THIS
process, which answers it from one of two backends:

- **model** (`scenarios.py`): a scripted SHTP sensor + virtual clock. The
  output transcript (imutx/1 records) carries the op stream with the
  responses given, the clock calls (`clock` records with the exact IEEE bits)
  and the helper's stdout in emission order. That transcript is what the Rust
  lockstep runner (`xorgate-core::imu_lockstep`) replays for C4.
- **replay** (an `*.imutx` transcript): reads return the recorded bytes/errno,
  writes are asserted byte-equal, `time.monotonic()` follows the transcript
  (the `t_us` of the next op), sleeps take no time. Used by C5(b), C10b and the
  WP0a scrubber's `--oracle-cmd` hook.

Commands:

    oracle.py scenario NAME|all --out DIR        # C4 transcripts (+ summary.json)
    oracle.py replay IN.imutx [-o OUT]           # C5(b): replay + compare stdout/exit
    oracle.py stdin                              # the scrubber hook: imutx in, records out
    oracle.py leakcheck RAW.imutx SCRUBBED.imutx # review-wp0-tools F11
    oracle.py errno-table -o FILE                # glibc strerror table for Rust
    oracle.py strip-clock IN.imutx OUT.imutx     # a model transcript as strace would show it

The scrubber contract (`tools/imu/scrub_imutx.py recompute_stdout`, plus
review-wp0-tools item 1):
- (a) op records come back BYTE-FOR-BYTE as given (the input dicts are
  re-emitted; `t_us`, `dur_us`, `flags`, `trunc` untouched);
- (b) ANY op mismatch -> exit status 3 with a one-line reason on stderr;
- (c) a stdout record's `t_us` is the virtual time of the write that emitted
  it: the end (`t_us + dur_us`) of the last op consumed before it (0 before
  the first op); in model mode the virtual clock at that moment;
- (d) `signal` / `exit` records pass through; the `exit` record is MANDATORY
  and the helper's own exit (code or signal) must equal it (review-wp2 F6);
- (e) a transcript whose meta `counts` show a capture defect
  (`truncated_strings`, `other_pid_stdout_writes`, `foreign_pid_device_ops`,
  `fd_reuse`) is refused; `replay` FAILS a transcript with no stdout records
  (review-wp2 F2).

`close` records: one on the device fd is an op (the helper closes the bus at
interpreter teardown via `SMBus.__del__`); a `close` record that names a
different fd than the device's is ignored. A transcript with no `close`
record at all (captured without tracing close) lets the helper's close pass
unrecorded.
"""

import argparse
import errno as errno_mod
import json
import os
import selectors
import signal
import socket
import struct
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
HELPER_DEFAULT = os.environ.get("P7C_HELPER", "/helper/bno085_read.py")
HELPER_SHA256 = "7af7a85627aac526124e3e9e4b0d0b92c711ba44a4bfde158abdd1c525008b3b"
DEVICE = "/dev/i2c-1"
OPS = ("open", "ioctl", "read", "write", "close")
MAX_REQUESTS = 400_000

# The names strace prints (`imutx` records carry names, not numbers).
ERRNO_NAMES = (
    "EPERM", "ENOENT", "EINTR", "EIO", "ENXIO", "EBADF", "EAGAIN", "ENOMEM", "EACCES",
    "EBUSY", "ENODEV", "EINVAL", "ENOTTY", "EPIPE", "EOPNOTSUPP", "ETIMEDOUT",
    "EREMOTEIO", "ESHUTDOWN", "EPROTO", "EMSGSIZE", "ENOSYS",
)


def errno_num(name):
    if name == "EOPNOTSUPP":
        return errno_mod.EOPNOTSUPP
    return getattr(errno_mod, name)


def errno_name(num):
    for n in ERRNO_NAMES:
        if errno_num(n) == num:
            return n
    return errno_mod.errorcode[num]


def f64_bits(x):
    return struct.unpack("<Q", struct.pack("<d", float(x)))[0]


def bits_f64(b):
    return struct.unpack("<d", struct.pack("<Q", b))[0]


# meta `counts` of the WP0a converter that make a capture incomplete
# (review-wp2 F2): any of them > 0 and the transcript is refused.
CAPTURE_DEFECTS = ("truncated_strings", "other_pid_stdout_writes", "foreign_pid_device_ops", "fd_reuse")


class OracleMismatch(Exception):
    """The helper asked for an op the transcript does not have (contract b)."""


# --------------------------------------------------------------------------
# Output records: ops, clock calls and stdout, in emission order.
# --------------------------------------------------------------------------
class Recorder:
    def __init__(self, meta):
        self.records = [meta]
        self.stdout = bytearray()  # every byte the helper wrote to fd 1
        self._partial = b""

    def add(self, rec):
        self.records.append(rec)

    def stdout_bytes(self, data, t_us):
        self.stdout += data
        buf = self._partial + data
        *lines, self._partial = buf.split(b"\n")
        for line in lines:
            self.records.append({"op": "stdout", "line": line.decode("utf-8", "surrogateescape"), "t_us": t_us})

    def finish_stdout(self, t_us):
        if self._partial:
            self.records.append(
                {"op": "stdout", "line": self._partial.decode("utf-8", "surrogateescape"), "partial": True,
                 "t_us": t_us})
            self._partial = b""


# --------------------------------------------------------------------------
# The helper process and the request loop.
# --------------------------------------------------------------------------
def helper_env(fd):
    env = {
        "PATH": "/usr/local/bin:/usr/bin:/bin",
        "HOME": os.environ.get("HOME", "/tmp"),
        "LANG": "C.UTF-8",
        "PYTHONPATH": os.path.join(HERE, "shim"),
        "P7C_ORACLE_FD": str(fd),
        # PlatformDetect 3.88.0 `board.py:61` / `chip.py:58`. The bench is a
        # CM5: there `any_embedded_linux` holds through `any_raspberry_pi`
        # (`/proc/cpuinfo` Revision), which an x86 container lacks; CM5 is
        # not in `_RASPBERRY_PI_40_PIN_IDS`. RASPBERRY_PI_5 is (and is in
        # `_RASPBERRY_PI_5_IDS`), so it reaches the SAME modules as the bench:
        # `board` -> `raspi_5`, `microcontroller` -> `bcm2712`, busio ->
        # `generic_linux` (selftest part 2 pins all three).
        "BLINKA_FORCEBOARD": "RASPBERRY_PI_5",
        "BLINKA_FORCECHIP": "BCM2XXX",
    }
    return env


def run_helper(backend, helper=HELPER_DEFAULT, args=("--address", "0x4a")):
    """Run the unmodified helper against `backend`; returns (exit, stderr)."""
    parent, child = socket.socketpair()
    child_fd = child.fileno()
    os.set_inheritable(child_fd, True)
    proc = subprocess.Popen(
        ["python3", helper, *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        pass_fds=(child_fd,),
        env=helper_env(child_fd),
        cwd="/",
        start_new_session=True,  # the agent spawns the helper in its own process group
    )
    child.close()
    os.set_blocking(proc.stdout.fileno(), False)
    os.set_blocking(proc.stderr.fileno(), False)
    sel = selectors.DefaultSelector()
    sel.register(parent, selectors.EVENT_READ, "rpc")
    sel.register(proc.stdout, selectors.EVENT_READ, "out")
    sel.register(proc.stderr, selectors.EVENT_READ, "err")
    stderr = bytearray()
    rbuf = b""
    open_streams = 3
    requests = 0

    def drain_stdout():
        while True:
            try:
                chunk = os.read(proc.stdout.fileno(), 65536)
            except BlockingIOError:
                return True
            if not chunk:
                return False
            backend.stdout(chunk)

    try:
        while open_streams:
            for key, _ in sel.select():
                if key.data == "out":
                    if not drain_stdout():
                        sel.unregister(proc.stdout)
                        open_streams -= 1
                elif key.data == "err":
                    chunk = os.read(proc.stderr.fileno(), 65536)
                    if chunk:
                        stderr += chunk
                    else:
                        sel.unregister(proc.stderr)
                        open_streams -= 1
                else:
                    chunk = parent.recv(65536)
                    if not chunk:
                        sel.unregister(parent)
                        open_streams -= 1
                        continue
                    rbuf += chunk
                    while b"\n" in rbuf:
                        line, rbuf = rbuf.split(b"\n", 1)
                        # Everything the helper wrote before this request is
                        # already in the pipe (write(2) returned first).
                        drain_stdout()
                        requests += 1
                        if requests > MAX_REQUESTS:
                            raise OracleMismatch(f"more than {MAX_REQUESTS} requests: runaway helper")
                        action = backend.request(json.loads(line))
                        if action.get("signal"):
                            os.kill(proc.pid, action["signal"])
                        if "reply" in action:
                            parent.sendall((json.dumps(action["reply"], separators=(",", ":")) + "\n").encode())
    except OracleMismatch:
        proc.kill()
        proc.wait()
        raise
    finally:
        parent.close()
    rc = proc.wait()
    exit_ = {"code": rc} if rc >= 0 else {"signal": signal.Signals(-rc).name}
    backend.finish(exit_)
    return exit_, bytes(stderr)


# --------------------------------------------------------------------------
# Backend 1: the model (scripted scenarios).
# --------------------------------------------------------------------------
class ModelBackend:
    """Bus + virtual clock driven by a `scenarios.Scenario`. Time is integer
    nanoseconds; `time.monotonic()` returns `ns / 1e9` (CPython's own
    conversion), so the doubles are exact and the same on both sides."""

    def __init__(self, scenario):
        self.sc = scenario
        self.ns = 0
        self.rec = Recorder({"op": "meta", "format": "imutx/1", "device": DEVICE, "clock": "virtual",
                             "scenario": scenario.name, "row": scenario.row, "title": scenario.title,
                             "synthetic": True, "helper_sha256": HELPER_SHA256,
                             "oracle_image": os.environ.get("P7C_ORACLE_IMAGE", "unknown"),
                             "generatedBy": "tools/imu/oracle/oracle.py scenario"})
        self.samples = 0
        self.ready = False
        self.interval_sleeps = 0
        self.counts = {k: 0 for k in OPS}
        self.dead = False
        scenario.attach(self)

    @property
    def now_us(self):
        return self.ns // 1000

    def stdout(self, data):
        for line in data.split(b"\n"):
            if line.startswith(b'{"type": "sample"'):
                self.samples += 1
            elif line.startswith(b'{"type": "ready"'):
                self.ready = True
        self.rec.stdout_bytes(data, self.now_us)

    def _op(self, rec, cost_ns, reply):
        rec["t_us"] = self.now_us
        rec["dur_us"] = cost_ns // 1000
        self.ns += cost_ns
        self.rec.add(rec)
        self.counts[rec["op"]] += 1
        return {"reply": reply}

    def request(self, req):
        op = req["op"]
        if op in OPS:
            sig = self.sc.kill_before(self, req)
            if sig:
                self.rec.add({"op": "signal", "sig": signal.Signals(sig).name, "t_us": self.now_us})
                self.dead = True
                return {"signal": sig}  # no reply: the helper dies in its wait
        if op == "monotonic":
            v = self.ns / 1e9
            self.rec.add({"op": "clock", "fn": "monotonic", "bits": f64_bits(v), "v": repr(v)})
            return {"reply": {"bits": f64_bits(v)}}
        if op == "sleep":
            secs = bits_f64(req["bits"])
            interval = secs == self.sc.interval
            if interval:
                self.interval_sleeps += 1
            sigint = self.sc.sigint_at_sleep(self, secs)
            self.rec.add({"op": "clock", "fn": "sleep", "bits": req["bits"], "secs": repr(secs),
                          "sigint": bool(sigint), "t_us": self.now_us})
            if sigint:
                self.rec.add({"op": "signal", "sig": "SIGINT", "t_us": self.now_us})
                return {"signal": signal.SIGINT}  # KeyboardInterrupt out of time.sleep
            self.ns += int(secs * 1e9 + 0.5)
            return {"reply": {}}
        if op == "open":
            if "notes" in req:
                self.rec.records[0]["notes"] = req["notes"]
            res = self.sc.on_open(self, req["path"])
            rec = {"op": "open", "path": req["path"], "flags": "O_RDWR|O_CLOEXEC"}
            if res:
                rec.update(ret=-1, errno=res)
                return self._op(rec, self.sc.cost(self, "open", res), {"errno": errno_num(res)})
            rec.update(ret=3, errno=None)
            return self._op(rec, self.sc.cost(self, "open", None), {"ok": True})
        if op == "ioctl":
            res = self.sc.on_ioctl(self, req["req"], req["arg"])
            rec = {"op": "ioctl", "req": req["req"], "arg": req["arg"]}
            if res:
                rec.update(ret=-1, errno=res)
                return self._op(rec, self.sc.cost(self, "ioctl", res), {"errno": errno_num(res)})
            rec.update(ret=0, errno=None)
            return self._op(rec, self.sc.cost(self, "ioctl", None), {"ok": True})
        if op == "read":
            n = req["n"]
            res = self.sc.on_read(self, n)
            rec = {"op": "read", "len": n}
            if isinstance(res, str):
                rec.update(hex="", ret=-1, errno=res)
                reply = {"eagain": True} if res == "EAGAIN" else {"errno": errno_num(res)}
                return self._op(rec, self.sc.cost(self, "read", res), reply)
            data = bytes(res)[: min(n, 8192)]  # i2c-dev clamps a read to 8192 bytes
            rec.update(hex=data.hex(), ret=len(data), errno=None)
            return self._op(rec, self.sc.cost(self, "read", None), {"hex": data.hex()})
        if op == "write":
            data = bytes.fromhex(req["hex"])
            res = self.sc.on_write(self, data)
            rec = {"op": "write", "len": len(data), "hex": data.hex()}
            if isinstance(res, str):
                rec.update(ret=-1, errno=res)
                reply = {"eagain": True} if res == "EAGAIN" else {"errno": errno_num(res)}
                return self._op(rec, self.sc.cost(self, "write", res), reply)
            rec.update(ret=res, errno=None)
            return self._op(rec, self.sc.cost(self, "write", None), {"ret": res})
        if op == "close":
            return self._op({"op": "close", "path": DEVICE, "ret": 0, "errno": None}, 0, {})
        raise OracleMismatch(f"unknown request {req!r}")

    def finish(self, exit_):
        self.rec.finish_stdout(self.now_us)
        self.rec.add({"op": "exit", **exit_, "t_us": self.now_us})


# --------------------------------------------------------------------------
# Backend 2: replay of an imutx transcript.
# --------------------------------------------------------------------------
def is_op(rec, dev_fd):
    if rec["op"] == "close":
        fd = rec.get("fd")
        return fd is None or dev_fd is None or fd == dev_fd
    return rec["op"] in ("open", "ioctl", "read", "write")


class ReplayBackend:
    def __init__(self, records):
        if not records or records[0].get("op") != "meta":
            raise OracleMismatch("not an imutx transcript (no meta record)")
        self.meta = records[0]
        counts = self.meta.get("counts") or {}
        bad = {k: counts[k] for k in CAPTURE_DEFECTS if counts.get(k)}
        if bad:
            raise OracleMismatch(f"capture defects in meta counts: {bad} (an incomplete capture is never replayed)")
        self.inp = records[1:]
        self.expected_stdout = [r for r in self.inp if r["op"] == "stdout"]
        self.body = [r for r in self.inp if r["op"] != "stdout"]
        self.dev_fd = next((r.get("ret") for r in self.body if r["op"] == "open" and (r.get("ret") or -1) >= 0), None)
        self.trace_close = any(r["op"] == "close" for r in self.body)
        self.i = 0
        out_meta = dict(self.meta)
        self.rec = Recorder(out_meta)
        self.end_us = 0
        self.past_end = 0
        self.truncated = False
        self.ignored_closes = 0

    # The cursor skips (and passes through) non-device `close` records.
    def _skip_foreign(self):
        while (self.i < len(self.body) and self.body[self.i]["op"] == "close"
               and not is_op(self.body[self.i], self.dev_fd)):
            self.rec.add(self.body[self.i])
            self.i += 1

    def _next(self):
        self._skip_foreign()
        return self.body[self.i] if self.i < len(self.body) else None

    def _consume(self):
        r = self.body[self.i]
        self.i += 1
        self.rec.add(r)
        if "t_us" in r:
            self.end_us = r["t_us"] + (r.get("dur_us") or 0)
        return r

    def stdout(self, data):
        self.rec.stdout_bytes(data, self.end_us)

    def _monotonic(self):
        nxt = None
        for r in self.body[self.i:]:
            if is_op(r, self.dev_fd) or r["op"] in ("signal", "exit"):
                nxt = r
                break
        if nxt is not None and nxt["op"] in OPS:
            return nxt["t_us"] / 1e6
        self.past_end += 1
        return self.end_us / 1e6 + 1000.0 * self.past_end

    def request(self, req):
        op = req["op"]
        if op == "monotonic":
            return {"reply": {"bits": f64_bits(self._monotonic())}}
        nxt = self._next()
        if nxt is not None and nxt["op"] == "signal":
            sig = signal.Signals[nxt["sig"]]
            self._consume()
            if sig == signal.SIGINT:
                return {"signal": sig}  # KeyboardInterrupt in the current call
            return {"signal": sig}
        if op == "sleep":
            return {"reply": {}}
        if op == "close" and not self.trace_close:
            self.ignored_closes += 1
            return {"reply": {}}
        if nxt is None or nxt["op"] == "exit":
            self.truncated = True
            raise OracleMismatch(f"transcript exhausted: the helper asked for {op} at op {self.i}")
        if nxt["op"] != op:
            raise OracleMismatch(f"op {self.i}: the helper asked for {op}, the transcript has {nxt['op']}")
        if op == "open":
            if nxt.get("path") != req["path"]:
                raise OracleMismatch(f"op {self.i}: open {req['path']!r} vs {nxt.get('path')!r}")
            r = self._consume()
            return {"reply": {"errno": errno_num(r["errno"])} if r.get("errno") else {"ok": True}}
        if op == "ioctl":
            if (nxt.get("req"), nxt.get("arg")) != (req["req"], req["arg"]):
                raise OracleMismatch(f"op {self.i}: ioctl {req['req']:#x},{req['arg']:#x} vs "
                                     f"{nxt.get('req')},{nxt.get('arg')}")
            r = self._consume()
            return {"reply": {"errno": errno_num(r["errno"])} if r.get("errno") else {"ok": True}}
        if op == "read":
            if nxt.get("len") != req["n"]:
                raise OracleMismatch(f"op {self.i}: read({req['n']}) vs read({nxt.get('len')})")
            if nxt.get("trunc"):
                raise OracleMismatch(f"op {self.i}: the transcript truncated this read (strace -s)")
            r = self._consume()
            if r.get("errno") == "EAGAIN":
                return {"reply": {"eagain": True}}
            if r.get("errno"):
                return {"reply": {"errno": errno_num(r["errno"])}}
            return {"reply": {"hex": r.get("hex") or ""}}
        if op == "write":
            if nxt.get("hex", "") != req["hex"] or nxt.get("len") != len(req["hex"]) // 2:
                raise OracleMismatch(f"op {self.i}: write bytes differ from the transcript")
            r = self._consume()
            if r.get("errno") == "EAGAIN":
                return {"reply": {"eagain": True}}
            if r.get("errno"):
                return {"reply": {"errno": errno_num(r["errno"])}}
            return {"reply": {"ret": r["ret"]}}
        if op == "close":
            self._consume()
            return {"reply": {}}
        raise OracleMismatch(f"unknown request {req!r}")

    def finish(self, exit_):
        self.rec.finish_stdout(self.end_us)
        # Anything left must be the exit record (or nothing).
        self._skip_foreign()
        rest = self.body[self.i:]
        want = next((r for r in rest if r["op"] == "exit"), None)
        left_ops = [r for r in rest if r["op"] != "exit"]
        if left_ops:
            raise OracleMismatch(f"the helper exited ({exit_}) with {len(left_ops)} transcript records left "
                                 f"(next: {left_ops[0]['op']})")
        if want is None:
            # review-wp2 F6: the exit record is MANDATORY (the W0/B'' capture uses
            # `strace -q`, which keeps `+++ exited with N +++`).
            raise OracleMismatch(f"the transcript has no exit record (the helper exited {exit_})")
        got = {k: exit_[k] for k in exit_}
        exp = {k: want[k] for k in ("code", "signal") if k in want}
        if got != exp:
            raise OracleMismatch(f"exit {got} vs the transcript's {exp}")
        self.rec.add(want)


# --------------------------------------------------------------------------
# D80 filter and comparisons.
# --------------------------------------------------------------------------
_DUMP_HEAD = "\t\t********** Packet *************"
_DUMP_TAIL = "\t\t*******************************"


def strip_d80(lines):
    """Drop the `print(packet)` dumps (`__init__.py:836`; D80) from a list of
    stdout LINES: an empty line, the head line, ..., the tail line, and the
    empty line `print` adds."""
    out, i = [], 0
    while i < len(lines):
        if lines[i] == "" and i + 1 < len(lines) and lines[i + 1] == _DUMP_HEAD:
            j = i + 2
            while j < len(lines) and lines[j] != _DUMP_TAIL:
                j += 1
            if j + 1 < len(lines) and lines[j + 1] == "":
                i = j + 2
                continue
        out.append(lines[i])
        i += 1
    return out


def stdout_lines(records):
    return [r["line"] for r in records if r["op"] == "stdout"]


def stdout_positions(records):
    """(position, line) per stdout record; the position counts the op/clock
    records before it (as `imu_lockstep::expected_lines` does)."""
    pos, out = 0, []
    for r in records:
        op = r["op"]
        if op == "stdout":
            out.append((pos, r["line"]))
        elif op not in ("meta", "signal", "exit"):
            pos += 1
    return out


def read_records(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def write_records(path, records):
    with open(path, "w") as f:
        for r in records:
            f.write(json.dumps(r, separators=(",", ":")) + "\n")


def replay(records, helper=HELPER_DEFAULT):
    be = ReplayBackend(records)
    exit_, stderr = run_helper(be, helper)
    return be, exit_, stderr


# --------------------------------------------------------------------------
# Commands.
# --------------------------------------------------------------------------
def check_helper(helper):
    import hashlib  # noqa: PLC0415

    with open(helper, "rb") as f:
        sha = hashlib.sha256(f.read()).hexdigest()
    if sha != HELPER_SHA256:
        sys.exit(f"oracle: {helper} sha256 {sha} != the vendored helper {HELPER_SHA256}")


def cmd_scenario(a):
    sys.path.insert(0, HERE)
    import scenarios  # noqa: PLC0415

    names = list(scenarios.ALL) if a.name == "all" else [a.name]
    os.makedirs(a.out, exist_ok=True)
    summary = []
    for name in names:
        sc = scenarios.ALL[name]()
        be = ModelBackend(sc)
        exit_, stderr = run_helper(be, a.helper)
        out = os.path.join(a.out, f"{name}.imutx")
        write_records(out, be.rec.records)
        lines = stdout_lines(be.rec.records)
        row = {
            "name": name, "row": sc.row, "title": sc.title, "exit": exit_,
            "records": len(be.rec.records), "ops": sum(be.counts.values()),
            "stdout_lines": len(lines), "samples": be.samples,
            "d80_lines": len(lines) - len(strip_d80(lines)),
            "stderr_bytes": len(stderr),
            "virtual_s": be.ns / 1e9,
        }
        problems = sc.check(be, exit_, lines)
        allowed = getattr(sc, "allow_stderr", None)
        if stderr and not (allowed and allowed in stderr.decode("utf-8", "replace")):
            problems.append("helper wrote to stderr")
        if allowed and allowed not in stderr.decode("utf-8", "replace"):
            problems.append(f"expected {allowed!r} on stderr")
        row["problems"] = problems
        summary.append(row)
        print(f"{name:<34} row {sc.row:>2} exit {exit_} ops {row['ops']:>6} samples {be.samples:>3} "
              f"{'OK' if not problems else 'PROBLEMS: ' + '; '.join(problems)}")
    with open(os.path.join(a.out, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    return 1 if any(r["problems"] for r in summary) else 0


def cmd_replay(a):
    records = read_records(a.transcript)
    if any(r["op"] == "clock" for r in records):
        records = strip_clock(records)  # a model transcript: replay it as strace would show it
    try:
        be, exit_, stderr = replay(records, a.helper)
    except OracleMismatch as e:
        print(f"MISMATCH: {e}", file=sys.stderr)
        return 3
    if a.out:
        write_records(a.out, be.rec.records)
    want = stdout_positions(records)
    got = stdout_positions(be.rec.records)
    res = {"file": os.path.basename(a.transcript), "ops": sum(1 for r in be.body if is_op(r, be.dev_fd)),
           "exit": exit_, "stdout_expected": len(want), "stdout_got": len(got),
           "stdout_equal": [x[1] for x in want] == [x[1] for x in got], "positions_equal": want == got,
           "ignored_closes": be.ignored_closes, "stderr_bytes": len(stderr)}
    if want != got:
        first = next((i for i, (x, y) in enumerate(zip(want, got, strict=False)) if x != y), min(len(want), len(got)))
        res["first_stdout_diff"] = first
    if not want:
        # review-wp2 F2: a transcript without stdout records is a broken capture
        # (fd 1 lost, or written by an untraced syscall), never a pass.
        res["no_stdout_records"] = True
    # counts only: no line content is printed (safe for a raw transcript)
    print(json.dumps(res))
    return 0 if want and want == got and not stderr else 4


def cmd_stdin(a):
    records = [json.loads(line) for line in sys.stdin if line.strip()]
    try:
        be, _exit, stderr = replay(records, a.helper)
    except OracleMismatch as e:
        print(f"oracle: MISMATCH: {e}", file=sys.stderr)
        return 3
    if stderr:
        print("oracle: the helper wrote to stderr", file=sys.stderr)
        return 3
    for r in be.rec.records:
        sys.stdout.write(json.dumps(r, separators=(",", ":")) + "\n")
    return 0


SAMPLE_FIELDS = (("quat", "r"), ("quat", "i"), ("quat", "j"), ("quat", "k"),
                 ("linaccel", "x"), ("linaccel", "y"), ("linaccel", "z"))


INPUT_CHANNELS = (3, 4, 5)


def report_fields(records):
    """Every sensor report in every input-channel (3-5) FULL read of a
    transcript, split by the REAL driver's `_separate_batch` (bno08x 1.3.3,
    imported here, outside the helper process; independent of the scrubber's
    `classify()`), whether or not the helper ever published it: LIFO-losing
    duplicates and reports overwritten in the same drain included
    (review-wp2 F3). A full read = a read of exactly the masked count of the
    4-byte read before it. Returns {key: bytes}, key = (op index, slice index,
    field): for 0x05 the i/j/k/real words, the accuracy word and the delay
    byte; for 0x04 the x/y/z words and the delay byte; for any other sensor
    id every data byte from offset 4 and the delay byte. Also the set of
    feature ids a `0xFC` (get-feature response) seeded."""
    import adafruit_bno08x as drv  # noqa: PLC0415

    fields, seeded, prev = {}, set(), None
    ops = [r for r in records if r["op"] in ("open", "ioctl", "read", "write", "close")]
    for i, r in enumerate(ops):
        if r["op"] != "read":
            continue
        b = bytes.fromhex(r.get("hex") or "") if not r.get("errno") else None
        if b is not None and prev is not None and len(prev) == 4 and len(b) > 4:
            count = struct.unpack_from("<H", prev)[0] & 0x7FFF
            if r.get("len") == count:
                pkt = drv.Packet(bytearray(b))
                slices = []
                try:
                    drv._separate_batch(pkt, slices)  # slices appended before an error stay
                except Exception:  # noqa: BLE001, S110 - the driver raises on #49 remainders
                    pass
                for si, (rid, rb) in enumerate(slices):
                    rb = bytes(rb)
                    if rid == 0xFC and len(rb) >= 2:
                        seeded.add(rb[1])
                    if b[2] not in INPUT_CHANNELS or rid >= 0xF0:
                        continue
                    names = {0x05: ("i", "j", "k", "real", "accuracy"), 0x04: ("x", "y", "z")}.get(rid)
                    if len(rb) > 3:
                        fields[(i, si, "delay")] = rb[3:4]
                    if names:
                        for w, name in enumerate(names):
                            word = rb[4 + 2 * w:6 + 2 * w]
                            if word:
                                fields[(i, si, f"{rid:#04x}.{name}")] = word
                    else:
                        for off in range(4, len(rb)):
                            fields[(i, si, f"{rid:#04x}.byte{off}")] = rb[off:off + 1]
        prev = b
    return fields, seeded


def leakcheck(raw_records, scrubbed_records, helper=HELPER_DEFAULT):
    """review-wp0-tools F11 + review-wp2 F3. Two halves, both independent of
    the scrubber's `classify()`:
    - published: replay RAW and SCRUBBED through the oracle; every published
      sample field must DIFFER at the same sample index. A field 0.0 in BOTH
      passes only when it provably came from a `0xFC` zero seed (the per-report
      half found no equal word AND the raw transcript seeds that feature);
    - per report: every sensor report word in every input-channel full read,
      published or not (LIFO losers, overwritten reports), must differ at the
      same position (`report_fields`).
    Returns a counts-only report (positions are op/slice indexes and field
    names, never values)."""
    out = {}
    samples = {}
    for label, recs in (("raw", raw_records), ("scrubbed", scrubbed_records)):
        be, exit_, _ = replay(recs, helper)
        lines = [ln for ln in stdout_lines(be.rec.records) if ln.startswith('{"type": "sample"')]
        samples[label] = [json.loads(ln) for ln in lines]
        out[f"{label}_exit"] = exit_
    raw, scr = samples["raw"], samples["scrubbed"]
    rep = {"samples_raw": len(raw), "samples_scrubbed": len(scr), "fields": 0, "equal": 0,
           "zero_seed_fields": 0, "equal_positions": []}
    if len(raw) != len(scr):
        rep["sample_count_differs"] = True
    for idx, (a, b) in enumerate(zip(raw, scr, strict=False)):
        for g, k in SAMPLE_FIELDS:
            rep["fields"] += 1
            if a[g][k] == b[g][k]:
                if a[g][k] == 0.0:
                    rep["zero_seed_fields"] += 1
                    rep[f"_zero_{g}"] = rep.get(f"_zero_{g}", 0) + 1
                else:
                    rep["equal"] += 1
                    if len(rep["equal_positions"]) < 20:
                        rep["equal_positions"].append([idx, f"{g}.{k}"])
    # Per-report half (F3): every sensor report word, published or not.
    raw_f, seeded = report_fields(raw_records)
    scr_f, _ = report_fields(scrubbed_records)
    rep["reports_words"] = len(raw_f)
    rep["report_structure_differs"] = sorted(raw_f) != sorted(scr_f)
    eq = [k for k in raw_f if k in scr_f and raw_f[k] == scr_f[k]]
    rep["report_equal"] = len(eq)
    rep["report_equal_positions"] = [[k[0], k[1], k[2]] for k in eq[:20]]
    # A published field 0.0 in BOTH is exempt only when its reading can only
    # have come from a 0xFC seed: every report word was proven to differ
    # above (so no report word supplied an equal 0.0) AND the raw transcript
    # holds a 0xFC for that feature. Otherwise it counts as equal.
    unexplained = 0
    for g, fid in (("quat", 0x05), ("linaccel", 0x04)):
        n = rep.pop(f"_zero_{g}", 0)
        if n and (eq or fid not in seeded):
            unexplained += n
    rep["zero_seed_fields_unexplained"] = unexplained
    rep["pass"] = (rep["equal"] == 0 and unexplained == 0 and rep["report_equal"] == 0
                   and not rep["report_structure_differs"] and not rep.get("sample_count_differs")
                   and rep["samples_raw"] > 0)
    rep.update(out)
    return rep


def cmd_leakcheck(a):
    try:
        rep = leakcheck(read_records(a.raw), read_records(a.scrubbed), a.helper)
    except OracleMismatch as e:
        print(f"oracle: MISMATCH: {e}", file=sys.stderr)
        return 3
    print(json.dumps(rep))
    return 0 if rep["pass"] else 5


def strip_clock(records):
    """A model transcript as strace would have recorded it: no `clock`
    records (strace sees no clock calls), meta `clock: monotonic`, op `t_us`
    kept. Used to exercise the next-op clock rule of both lockstep runners
    on synthetic data (C5 dry run)."""
    out = []
    for r in records:
        if r["op"] == "clock":
            continue
        if r["op"] == "meta":
            r = dict(r, clock="monotonic", derived="strip-clock")
        out.append(r)
    return out


def cmd_strip_clock(a):
    write_records(a.out, strip_clock(read_records(a.src)))
    return 0


def cmd_errno_table(a):
    table = {n: [errno_num(n), os.strerror(errno_num(n))] for n in ERRNO_NAMES}
    with open(a.out, "w") as f:
        json.dump({"generatedBy": "tools/imu/oracle/oracle.py errno-table", "python": sys.version.split()[0],
                   "errno": table}, f, indent=1, sort_keys=True)
        f.write("\n")
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--helper", default=HELPER_DEFAULT)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("scenario")
    s.add_argument("name")
    s.add_argument("--out", required=True)
    r = sub.add_parser("replay")
    r.add_argument("transcript")
    r.add_argument("-o", "--out")
    sub.add_parser("stdin")
    lk = sub.add_parser("leakcheck")
    lk.add_argument("raw")
    lk.add_argument("scrubbed")
    sc = sub.add_parser("strip-clock")
    sc.add_argument("src")
    sc.add_argument("out")
    e = sub.add_parser("errno-table")
    e.add_argument("-o", "--out", required=True)
    a = ap.parse_args(argv)
    if a.cmd not in ("errno-table", "strip-clock"):
        check_helper(a.helper)
    return {"scenario": cmd_scenario, "replay": cmd_replay, "stdin": cmd_stdin,
            "leakcheck": cmd_leakcheck, "errno-table": cmd_errno_table,
            "strip-clock": cmd_strip_clock}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
