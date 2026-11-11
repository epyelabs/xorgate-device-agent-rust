#!/usr/bin/env python3
"""strace -> `*.imutx` converter (Phase 7c WP0a; plan §WP0a, review F18).

    imutx.py convert RAW.strace -o OUT.imutx [--device /dev/i2c-1] [--keep-raw]

Reads a transcript written by `tools/imu/capture/bno085_read.py`
(`strace -qq --seccomp-bpf -f -tt -r -T -xx -s 4096
-e trace=openat,ioctl,read,write`) and writes JSON lines:

    {"op": "meta", "format": "imutx/1", "device": "/dev/i2c-1", "clock": "monotonic", ...}
    {"op": "open",  "path": "/dev/i2c-1", "flags": "O_RDWR|O_CLOEXEC", "ret": 3, "errno": null, "t_us": .., "dur_us": ..}
    {"op": "ioctl", "req": 1795, "arg": 74, "ret": 0, "errno": null, "t_us": .., "dur_us": ..}
    {"op": "write", "len": 5, "hex": "0500010001", "ret": 5, "errno": null, "t_us": .., "dur_us": ..}
    {"op": "read",  "len": 4, "hex": "....", "ret": 4, "errno": null, "t_us": .., "dur_us": ..}
    {"op": "read",  "len": 4, "hex": "", "ret": -1, "errno": "EAGAIN", "t_us": .., "dur_us": ..}
    {"op": "stdout", "line": "{\"type\": \"ready\", \"address\": \"0x4a\"}", "t_us": ..}
    {"op": "signal", "sig": "SIGTERM", "t_us": ..}
    {"op": "exit", "signal": "SIGTERM", "t_us": ..}

- KEPT: every op on a file descriptor that the MAIN tracee obtained by opening
  the device (failed opens of the device too: ENOENT/EACCES are scenarios),
  and the main tracee's writes to fd 1 reassembled into `stdout` lines.
  The main tracee is the first pid in the trace (strace launched it).
- DROPPED: everything else (Python's startup reads of /proc/cpuinfo,
  device-tree, site-packages; other fds; other pids). Only counts survive.
- `len` is the requested byte count; `ret` the syscall return (-1 on error,
  None for `= ?`); `hex` the bytes strace showed (`ret` bytes for a read, the
  buffer for a write); `trunc: true` marks a string strace cut at `-s 4096`.
- `t_us`: microseconds since the first traced line, from the CUMULATIVE `-r`
  (monotonic) deltas, at syscall ENTRY (a split `<unfinished ...>` /
  `<... resumed>` pair takes the entry line's time). A wall-clock step during
  the capture therefore cannot move `t_us`. Without `-r` the wall clock is
  used (midnight-wrapped) and meta says `"clock": "wall"`.
- `dur_us`: the `-T` duration.
- `exit` records exist only for a death by signal: `-qq` suppresses
  `+++ exited with N +++` (verified, strace 6.1), so an exit code is not in the
  transcript; the stdout records (and the oracle) carry the exit class.

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
TRACED = ("openat", "ioctl", "read", "write")

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
    r"(?P<wall>\d{2}:\d{2}:\d{2}\.\d{6}|\d+\.\d{6})\s+"
    r"(?:\(\+\s*(?P<rel>\d+\.\d{6})\)\s+)?"
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


def _wall_usec(text):
    if ":" in text:
        h, m, s = text.split(":")
        return (int(h) * 3600 + int(m) * 60) * 1_000_000 + _usec(s)
    return _usec(text)


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
        self.t_rel = 0
        self.saw_rel = False
        self.origin = None
        self.wall_prev = None
        self.wall_wrap = 0
        self.counts = {
            "lines": 0,
            "dropped_syscalls": 0,
            "foreign_pid_device_ops": 0,
            "fd_reuse": 0,
            "truncated_strings": 0,
            "other_pid_stdout_writes": 0,
        }

    # -- time -----------------------------------------------------------
    def _time(self, m):
        if m.group("rel") is not None:
            self.saw_rel = True
            self.t_rel += _usec(m.group("rel"))
            # The first line's delta is measured from strace's own start;
            # t_us counts from the first traced line.
            if self.origin is None:
                self.origin = self.t_rel
            return self.t_rel - self.origin
        if self.saw_rel:
            raise ParseError("a line without -r after lines with -r")
        w = _wall_usec(m.group("wall"))
        if self.wall_prev is not None and w + self.wall_wrap < self.wall_prev - 12 * 3600 * 1_000_000:
            self.wall_wrap += 24 * 3600 * 1_000_000
        w += self.wall_wrap
        self.wall_prev = w
        if self.origin is None:
            self.origin = w
        return w - self.origin

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
            "clock": "monotonic" if self.saw_rel else "wall",
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
            raise ParseError(f"line {self.counts['lines']}: no strace leader")
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
        if op in ("open", "ioctl", "read", "write"):
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
    a = ap.parse_args(argv)

    if a.cmd == "summary":
        print(json.dumps(summarize(read_records(a.imutx), {}), sort_keys=True))
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
