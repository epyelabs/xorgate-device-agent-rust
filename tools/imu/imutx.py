#!/usr/bin/env python3
"""strace -> `*.imutx` converter (Phase 7c WP0a; plan §WP0a, review F18).

    imutx.py convert RAW.strace -o OUT.imutx [--device /dev/i2c-1] [--keep-raw]
    imutx.py summary FILE.imutx      # counts only
    imutx.py stats FILE.imutx        # counts only: probe branch, packets/channel, init, zero seeds, cadence, clamps

Reads a transcript written by `tools/imu/capture/bno085_read.py`
(`strace -q --seccomp-bpf -f -tt --relative-timestamps=ns -T -xx -s 4096
-e trace=openat,ioctl,read,write,close`) and writes JSON lines:

    {"op": "meta", "format": "imutx/1", "device": "/dev/i2c-1", "clock": "monotonic-ns", "counts": {..}}
    {"op": "open", "path": "/dev/i2c-1", "flags": "O_RDWR|O_CLOEXEC", "ret": 3, "errno": null, "t_us": .., ...}
    {"op": "ioctl", "req": 1795, "arg": 74, "ret": 0, "errno": null, "t_us": .., "dur_us": ..}
    {"op": "write", "len": 5, "hex": "0500010001", "ret": 5, "errno": null, "t_us": .., "dur_us": ..}
    {"op": "read",  "len": 4, "hex": "....", "ret": 4, "errno": null, "t_us": .., "dur_us": ..}
    {"op": "read",  "len": 4, "hex": "", "ret": -1, "errno": "EAGAIN", "t_us": .., "dur_us": ..}
    {"op": "close", "ret": 0, "errno": null, "t_us": .., "dur_us": ..}   (device fd only)
    {"op": "stdout", "line": "{\"type\": \"ready\", \"address\": \"0x4a\"}", "t_us": ..}
    {"op": "signal", "sig": "SIGTERM", "t_us": ..}
    {"op": "exit", "signal": "SIGTERM", "t_us": ..}      (death by signal)
    {"op": "exit", "code": 3, "t_us": ..}                 (normal exit, kept by -q)

- KEPT: every op on a file descriptor that the MAIN tracee obtained by opening
  the device (failed opens of the device too: ENOENT/EACCES are scenarios),
  and the main tracee's writes to fd 1 reassembled into `stdout` lines.
  The main tracee is the first pid in the trace (strace launched it).
- DROPPED: everything else (Python's startup reads of /proc/cpuinfo,
  device-tree, site-packages; other fds; other pids). Only counts survive.
- `len` is the requested byte count; `ret` the syscall return (-1 on error,
  None for `= ?`); `hex` the bytes strace showed (`ret` bytes for a read, the
  buffer for a write); `trunc: true` marks a string strace cut at `-s 4096`.
- `t_us`: microseconds since the first traced line: the CUMULATIVE
  `--relative-timestamps=ns` (monotonic) deltas, summed in ns and rounded ONCE
  per record (review F2: summing µs-truncated `-r` deltas loses ~0.57 µs per
  line), at syscall ENTRY (a split `<unfinished ...>` / `<... resumed>` pair
  takes the entry line's time). A wall-clock step cannot move `t_us`.
- STRICT leader (review F5b): every line must carry BOTH the `-tt` wall time
  and the ns `(+ delta)`. A `-r`-only, µs-delta, `-ttt` or `-tt`-only
  transcript is REFUSED (exit 2), because it would yield wrong or
  non-monotonic `t_us`.
- `dur_us`: the `-T` duration.
- `exit {code}` comes from `+++ exited with N +++` (kept by `-q`, review F3);
  `exit {signal}` from `+++ killed by SIG +++`.
- `close` of the device fd is an op record `{op: close, ret, errno, t_us,
  dur_us}` in sequence (the WP2 lockstep treats it as an op), ends the device
  mapping and is counted as `device_closes`; a close of any other fd is
  count-only. `fd_reuse` counts a device fd number that a later `openat`
  reused without a traced close. Both counts land in meta.

F18: after the output is written (temp file, fsync, rename, mode 0600) the RAW
strace is DELETED unless `--keep-raw`. A parse failure keeps the raw file and
exits 2. The summary printed on stdout holds counts only (safe for evidence).
"""
import argparse
import json
import os
import re
import sys
import tempfile

FORMAT = "imutx/1"
TRACED = ("openat", "ioctl", "read", "write", "close")

I2C_IOCTLS = {
    "I2C_RETRIES": 0x0701,
    "I2C_TIMEOUT": 0x0702,
    "I2C_SLAVE": 0x0703,
    "I2C_TENBIT": 0x0704,
    "I2C_FUNCS": 0x0705,
    "I2C_SLAVE_FORCE": 0x0706,
    "I2C_RDWR": 0x0707,
    "I2C_PEC": 0x0708,
    "I2C_SMBUS": 0x0720,
}
_IOC_DIRS = {"_IOC_NONE": 0, "_IOC_WRITE": 1, "_IOC_READ": 2, "_IOC_READ|_IOC_WRITE": 3}

LEADER = re.compile(
    r"^(?:(?P<pid>\d+)\s+)?"
    r"(?P<wall>\d{2}:\d{2}:\d{2}\.\d{6})\s+"
    r"\(\+\s*(?P<rel>\d+\.\d{9})\)\s+"
    r"(?P<body>.*)$"
)
UNFINISHED = re.compile(r"^(?P<name>\w+)\((?P<head>.*?)\s*<unfinished \.\.\.>$")
RESUMED = re.compile(r"^<\.\.\. (?P<name>\w+) resumed>(?P<tail>.*)$")
CALL = re.compile(r"^(?P<name>\w+)\((?P<rest>.*)$")
RESULT = re.compile(
    r"^\s*=\s*(?P<ret>-?\d+|0x[0-9a-fA-F]+|\?)"
    r"(?:\s+(?P<errno>E[A-Z0-9]+|ERESTART\w*)\s+\([^)]*\))?"
    r"(?:\s+\([^)]*\))?"
    r"(?:\s+<(?P<dur>\d+\.\d{6})>|\s+<unavailable>)?\s*$"
)
SIGNAL_LINE = re.compile(r"^--- (?P<sig>SIG\w+) .*---$")
KILLED = re.compile(r"^\+\+\+ killed by (?P<sig>SIG\w+)(?: \(core dumped\))? \+\+\+$")
EXITED = re.compile(r"^\+\+\+ exited with (?P<code>\d+) \+\+\+$")


class ParseError(ValueError):
    pass


def _usec(text):
    sec, frac = text.split(".")
    return int(sec) * 1_000_000 + int(frac)


def _nsec(text):
    sec, frac = text.split(".")
    return int(sec) * 1_000_000_000 + int(frac)


def split_args(text):
    """Split a strace argument list at top-level commas.

    Returns (args, rest) where rest is the text after the closing paren.
    Quotes, (), [] and {} nest. `rest` is None when the list is not closed.
    """
    args, cur, depth, i, n = [], [], 0, 0, len(text)
    while i < n:
        c = text[i]
        if c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            cur.append(text[i : j + 1])
            i = j + 1
            continue
        if c in "([{":
            depth += 1
        elif c in ")]}":
            if depth == 0:
                args.append("".join(cur).strip())
                return [a for a in args if a != ""], text[i + 1 :]
            depth -= 1
        elif c == "," and depth == 0:
            args.append("".join(cur).strip())
            cur = []
            i += 1
            continue
        cur.append(c)
        i += 1
    return None, None


_ESC = {"n": 10, "t": 9, "r": 13, "v": 11, "f": 12, "a": 7, "b": 8, "\\": 92, '"': 34, "'": 39}


def decode_string(arg):
    """strace C-string literal -> (bytes, truncated). Accepts -xx output and
    the plain escapes strace uses without -xx."""
    trunc = False
    if arg.endswith("..."):
        arg, trunc = arg[:-3], True
    if len(arg) < 2 or arg[0] != '"' or arg[-1] != '"':
        raise ParseError(f"not a string literal: {arg[:40]!r}")
    s, out, i = arg[1:-1], bytearray(), 0
    while i < len(s):
        c = s[i]
        if c != "\\":
            out += c.encode("latin-1")
            i += 1
            continue
        nxt = s[i + 1]
        if nxt == "x":
            out.append(int(s[i + 2 : i + 4], 16))
            i += 4
        elif nxt in "01234567":
            j = i + 1
            while j < len(s) and j < i + 4 and s[j] in "01234567":
                j += 1
            out.append(int(s[i + 1 : j], 8))
            i = j
        else:
            out.append(_ESC[nxt])
            i += 2
    return bytes(out), trunc


def parse_int(text):
    t = text.strip()
    if re.fullmatch(r"-?0x[0-9a-fA-F]+", t):
        return int(t, 16)
    if re.fullmatch(r"-?\d+", t):
        return int(t)
    return None


def parse_ioctl_request(text):
    """`I2C_SLAVE`, `_IOC(_IOC_NONE, 0x7, 0x3, 0)`, `0x703` or `A or B`."""
    for alt in text.split(" or "):
        alt = alt.strip()
        if alt in I2C_IOCTLS:
            return I2C_IOCTLS[alt]
        m = re.fullmatch(r"_IOC\((?P<dir>[^,]+),\s*(?P<type>[^,]+),\s*(?P<nr>[^,]+),\s*(?P<size>[^)]+)\)", alt)
        if m and m.group("dir").strip() in _IOC_DIRS:
            d = _IOC_DIRS[m.group("dir").strip()]
            t, nr, size = (parse_int(m.group(k)) for k in ("type", "nr", "size"))
            if None not in (t, nr, size):
                return (d << 30) | (size << 16) | (t << 8) | nr
        v = parse_int(alt)
        if v is not None:
            return v
    return None


class Converter:
    def __init__(self, device="/dev/i2c-1"):
        self.device = device
        self.records = []
        self.main_pid = None
        self.i2c_fds = {}  # pid -> set(fd)
        self.pending = {}  # pid -> (name, head, t_us)
        self.stdout_buf = bytearray()
        self.stdout_t = None
        self.t_rel_ns = 0
        self.origin_ns = None
        self.counts = {
            "lines": 0,
            "dropped_syscalls": 0,
            "foreign_pid_device_ops": 0,
            "fd_reuse": 0,
            "device_closes": 0,
            "truncated_strings": 0,
            "other_pid_stdout_writes": 0,
        }

    # -- time -----------------------------------------------------------
    def _time(self, m):
        # Sum in ns and round once: the first line's delta (0 on strace 6.1)
        # is part of the origin, so t_us counts from the first traced line.
        self.t_rel_ns += _nsec(m.group("rel"))
        if self.origin_ns is None:
            self.origin_ns = self.t_rel_ns
        return round((self.t_rel_ns - self.origin_ns) / 1000)

    # -- records --------------------------------------------------------
    def _emit(self, rec):
        self.records.append(rec)

    def _is_device_fd(self, pid, fd):
        return fd is not None and fd in self.i2c_fds.get(pid, set())

    def _stdout(self, data, t_us):
        self.stdout_buf += data
        while True:
            k = self.stdout_buf.find(b"\n")
            if k < 0:
                break
            line = bytes(self.stdout_buf[:k]).decode("utf-8", "backslashreplace")
            del self.stdout_buf[: k + 1]
            self._emit({"op": "stdout", "line": line, "t_us": t_us})

    def finish(self):
        if self.stdout_buf:
            line = bytes(self.stdout_buf).decode("utf-8", "backslashreplace")
            self._emit({"op": "stdout", "line": line, "t_us": self.records[-1]["t_us"] if self.records else 0,
                        "partial": True})
            self.stdout_buf.clear()
        if self.pending:
            # A syscall still <unfinished ...> when the trace ended (the
            # process was killed inside it): keep it if it is ours, unresolved.
            for pid, (name, head, t_us) in sorted(self.pending.items()):
                self._syscall(pid, name, head + ") = ?", t_us)
            self.pending.clear()
        meta = {
            "op": "meta",
            "format": FORMAT,
            "device": self.device,
            "clock": "monotonic-ns",
            "counts": dict(self.counts),
        }
        return [meta] + self.records

    # -- line handling --------------------------------------------------
    def feed(self, raw_line):
        line = raw_line.rstrip("\n")
        if not line.strip():
            return
        self.counts["lines"] += 1
        m = LEADER.match(line)
        if not m:
            raise ParseError(
                f"line {self.counts['lines']}: no `PID HH:MM:SS.uuuuuu (+ S.nnnnnnnnn)` leader "
                "(the capture must use -tt --relative-timestamps=ns)"
            )
        pid = int(m.group("pid")) if m.group("pid") else 0
        if self.main_pid is None:
            self.main_pid = pid
        t_us = self._time(m)
        body = m.group("body")

        mu = UNFINISHED.match(body)
        if mu:
            if pid in self.pending:
                raise ParseError(f"line {self.counts['lines']}: pid {pid} has two unfinished syscalls")
            self.pending[pid] = (mu.group("name"), mu.group("head"), t_us)
            return
        mr = RESUMED.match(body)
        if mr:
            if pid not in self.pending or self.pending[pid][0] != mr.group("name"):
                raise ParseError(f"line {self.counts['lines']}: resumed {mr.group('name')} without its start")
            name, head, t0 = self.pending.pop(pid)
            tail = mr.group("tail")
            joined = head + (", " if head and not tail.startswith(")") and not head.endswith(",") else "")
            self._syscall(pid, name, joined + tail, t0)
            return
        ms = SIGNAL_LINE.match(body)
        if ms:
            if pid == self.main_pid:
                self._emit({"op": "signal", "sig": ms.group("sig"), "t_us": t_us})
            return
        mk = KILLED.match(body)
        if mk:
            if pid == self.main_pid:
                self._emit({"op": "exit", "signal": mk.group("sig"), "t_us": t_us})
            return
        me = EXITED.match(body)
        if me:
            if pid == self.main_pid:
                self._emit({"op": "exit", "code": int(me.group("code")), "t_us": t_us})
            return
        mc = CALL.match(body)
        if mc and mc.group("name") in TRACED:
            self._syscall(pid, mc.group("name"), mc.group("rest"), t_us)
            return
        if mc:
            # A syscall outside the trace set cannot appear with -e trace=...
            raise ParseError(f"line {self.counts['lines']}: untraced syscall {mc.group('name')}")
        raise ParseError(f"line {self.counts['lines']}: unrecognised body")

    def _syscall(self, pid, name, rest, t_us):
        args, after = split_args(rest)
        if args is None:
            raise ParseError(f"line {self.counts['lines']}: unterminated {name} arguments")
        r = RESULT.match(after)
        if not r:
            raise ParseError(f"line {self.counts['lines']}: unparsed {name} result {after.strip()[:40]!r}")
        ret_text = r.group("ret")
        ret = None if ret_text == "?" else parse_int(ret_text)
        errno = r.group("errno")
        dur_us = _usec(r.group("dur")) if r.group("dur") else None
        base = {"ret": ret, "errno": errno, "t_us": t_us, "dur_us": dur_us}

        if name == "openat":
            path, _ = decode_string(args[1])
            path = path.decode("utf-8", "backslashreplace")
            fd = ret if (ret is not None and ret >= 0) else None
            if path == self.device:
                if pid == self.main_pid:
                    if fd is not None:
                        self.i2c_fds.setdefault(pid, set()).add(fd)
                    self._emit({"op": "open", "path": path, "flags": args[2], **base})
                else:
                    self.counts["foreign_pid_device_ops"] += 1
                return
            if fd is not None and self._is_device_fd(pid, fd):
                # The device fd number was closed (close is not traced) and
                # reused for another file: stop attributing it to the device.
                self.i2c_fds[pid].discard(fd)
                self.counts["fd_reuse"] += 1
            self.counts["dropped_syscalls"] += 1
            return

        fd = parse_int(args[0]) if args else None
        if name == "close":
            if self._is_device_fd(pid, fd):
                self.i2c_fds[pid].discard(fd)
                self.counts["device_closes"] += 1
                self._emit({"op": "close", **base})
            else:
                self.counts["dropped_syscalls"] += 1
            return
        if name == "write" and fd == 1:
            if pid != self.main_pid:
                self.counts["other_pid_stdout_writes"] += 1
                return
            data, trunc = decode_string(args[1])
            if trunc:
                self.counts["truncated_strings"] += 1
            if ret is not None and ret > 0:
                self._stdout(data[:ret], t_us)
            return

        if not self._is_device_fd(pid, fd):
            if pid != self.main_pid and any(fd in s for s in self.i2c_fds.values()):
                self.counts["foreign_pid_device_ops"] += 1
            self.counts["dropped_syscalls"] += 1
            return

        if name == "ioctl":
            rec = {"op": "ioctl", "req": parse_ioctl_request(args[1])}
            arg = parse_int(args[2]) if len(args) > 2 else None
            rec["arg"] = arg if arg is not None else (args[2] if len(args) > 2 else None)
            rec.update(base)
            self._emit(rec)
            return
        # read / write on the device
        length = parse_int(args[2]) if len(args) > 2 else None
        rec = {"op": name, "len": length}
        if len(args) > 1 and args[1].startswith('"'):
            data, trunc = decode_string(args[1])
            if trunc:
                self.counts["truncated_strings"] += 1
                rec["trunc"] = True
        else:
            data = b""  # an address: the read failed, nothing was transferred
        if name == "read" and ret is not None and ret >= 0:
            data = data[:ret]
        rec["hex"] = data.hex()
        rec.update(base)
        self._emit(rec)


def convert_lines(lines, device="/dev/i2c-1"):
    c = Converter(device)
    for line in lines:
        c.feed(line)
    return c.finish(), c.counts


def summarize(records, counts):
    """Counts only: safe to paste into evidence."""
    s = {"ops": {}, "errnos": {}, "stdout": {"ready": 0, "sample": 0, "error": 0, "non_json": 0, "partial": 0}}
    for r in records:
        op = r["op"]
        if op in ("open", "ioctl", "read", "write", "close"):
            s["ops"][op] = s["ops"].get(op, 0) + 1
            if r.get("errno"):
                s["errnos"][r["errno"]] = s["errnos"].get(r["errno"], 0) + 1
        elif op == "stdout":
            if r.get("partial"):
                s["stdout"]["partial"] += 1
            try:
                kind = json.loads(r["line"]).get("type")
            except (ValueError, AttributeError):
                kind = None
            s["stdout"][kind if kind in ("ready", "sample", "error") else "non_json"] += 1
        elif op in ("signal", "exit"):
            s[op] = s.get(op, 0) + 1
    ts = [r["t_us"] for r in records if "t_us" in r]
    s["span_s"] = round((max(ts) - min(ts)) / 1e6, 3) if ts else 0
    s["counts"] = counts
    return s


def stats(records):
    """The WP0b `wp0-capture.md` numbers, COUNTS ONLY (no value leaves this):
    the probe branch, packets per channel, init duration, zero-seed samples,
    cadence, plus the `summarize` counts."""
    from scrub_imutx import classify, ops_of  # same directory; lazy to keep imutx standalone

    ops = ops_of(records)
    out = summarize(records, {})
    writes = [o for o in ops if o["op"] == "write"]
    probe = "none"
    if writes and writes[0].get("len") == 0:
        w0 = writes[0]
        if w0.get("errno") is None:
            probe = "zero-length write accepted"
        else:
            i = ops.index(w0)
            nxt = next((o for o in ops[i + 1 :] if o["op"] in ("read", "write")), None)
            tail = "no 1-byte read"
            if nxt is not None and nxt["op"] == "read" and nxt.get("len") == 1:
                tail = "1-byte read " + (nxt["errno"] or "ok")
            probe = f"zero-length write {w0['errno']}, then {tail}"
    out["probe"] = probe
    # D3: reads the i2c-dev 8192-byte cap cut short (a 2nd header of 0x7FFF).
    out["clamp_reads"] = sum(
        1 for o in ops if o["op"] == "read" and (o.get("len") or 0) > 8192 and o.get("ret") == 8192
    )
    ex = next((r for r in records if r["op"] == "exit"), None)
    out["exit_record"] = None if ex is None else (f"code {ex['code']}" if "code" in ex else f"signal {ex['signal']}")
    meta_counts = (records[0].get("counts") or {}) if records and records[0].get("op") == "meta" else {}
    out["converter_counts"] = {k: meta_counts.get(k) for k in ("fd_reuse", "device_closes", "truncated_strings",
                                                               "foreign_pid_device_ops", "other_pid_stdout_writes")}
    per_channel = {}
    for _idx, _count, channel in classify(ops).full_reads:
        per_channel[str(channel)] = per_channel.get(str(channel), 0) + 1
    out["packets_per_channel"] = per_channel
    t_first = ops[0]["t_us"] if ops else None
    ready_t = None
    samples = []
    zero_quat = zero_lin = 0
    for r in records:
        if r["op"] != "stdout":
            continue
        try:
            obj = json.loads(r["line"])
        except ValueError:
            continue
        if not isinstance(obj, dict):
            continue
        if obj.get("type") == "ready" and ready_t is None:
            ready_t = r["t_us"]
        if obj.get("type") == "sample":
            samples.append(r["t_us"])
            zero_quat += all(v == 0 for v in (obj.get("quat") or {}).values())
            zero_lin += all(v == 0 for v in (obj.get("linaccel") or {}).values())
    out["init_ms"] = round((ready_t - t_first) / 1000, 1) if ready_t is not None and t_first is not None else None
    out["zero_seed_samples"] = {"quat_all_zero": zero_quat, "linaccel_all_zero": zero_lin}
    if len(samples) > 1:
        out["cadence_hz"] = round((len(samples) - 1) / ((samples[-1] - samples[0]) / 1e6), 2)
    else:
        out["cadence_hz"] = None
    return out


def write_records(records, out_path):
    d = os.path.dirname(os.path.abspath(out_path))
    fd, tmp = tempfile.mkstemp(prefix=".imutx-", dir=d)
    try:
        with os.fdopen(fd, "w") as f:
            for r in records:
                f.write(json.dumps(r, separators=(",", ":")) + "\n")
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp, 0o600)
        os.replace(tmp, out_path)
    except BaseException:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise


def read_records(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def main(argv=None):
    ap = argparse.ArgumentParser(prog="imutx")
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("convert", help="strace -> imutx; deletes the raw strace on success")
    c.add_argument("raw")
    c.add_argument("-o", "--out", required=True)
    c.add_argument("--device", default="/dev/i2c-1")
    c.add_argument("--keep-raw", action="store_true", help="do NOT delete the raw strace (tests only)")
    s = sub.add_parser("summary", help="counts-only summary of an imutx file")
    s.add_argument("imutx")
    t = sub.add_parser("stats", help="counts-only WP0b numbers (probe, channels, init, zero seeds, cadence)")
    t.add_argument("imutx")
    a = ap.parse_args(argv)

    if a.cmd == "summary":
        print(json.dumps(summarize(read_records(a.imutx), {}), sort_keys=True))
        return 0
    if a.cmd == "stats":
        print(json.dumps(stats(read_records(a.imutx)), sort_keys=True))
        return 0
    try:
        with open(a.raw, encoding="latin-1") as f:
            records, counts = convert_lines(f, a.device)
    except ParseError as e:
        print(f"imutx: {a.raw}: {e}; raw file KEPT", file=sys.stderr)
        return 2
    write_records(records, a.out)
    if not a.keep_raw:
        os.unlink(a.raw)
    print(json.dumps(summarize(records, counts), sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
