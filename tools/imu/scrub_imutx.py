#!/usr/bin/env python3
"""Shape-preserving scrubber for `*.imutx` transcripts (Phase 7c WP0a; plan
§WP0a "Scrubber", review F18, user decision Q-C3: the repo is assumed PUBLIC).

    scrub_imutx.py scrub RAW.imutx -o SCRUBBED.imutx [--seed-out PRIVATE_FILE]
                   (--oracle-cmd CMD | --stdout-pending)
    scrub_imutx.py check RAW.imutx SCRUBBED.imutx      # the positional self-check

What it changes, and only that (every other byte, length, errno, ret and
timestamp is kept):
- every rotation-vector (RV, 0x05) data word: i, j, k, real (Q14);
- the RV accuracy-estimate word (report bytes 12-13, Q12);
- every linear-acceleration (LA, 0x04) data word: x, y, z (Q8);
- the report `delay` byte (byte 3) of every sensor report;
- the data bytes (offset >= 4) of any OTHER sensor report id (not enabled by
  the helper; scrubbed so a stray motion report cannot leak);
- "tail" bytes the driver never consumes: bytes of a full packet read beyond
  the packet's own byte count, and unconsumed payload of an input-report
  packet (channels 3-5) after a batch error.
The replacements come from a synthetic generator (a slow synthetic rotation
and small synthetic accelerations, with seeded periods, phases and jitter)
re-encoded to Q14/Q8/Q12 words, independent of the original values.

The seed is SECRET (review F6): a fresh `secrets.randbits(64)` per scrub,
never written to the scrubbed file (meta says `"seed": "secret"`); with
`--seed-out` it goes to a 0600 file in the PRIVATE dir only. With a public
seed, anyone could recompute every draw, and a redraw (taken only where the
draw equalled the original) would reveal that original exactly. A redraw is a
fresh draw of the SAME field at the SAME trend index with fresh jitter from a
separate secret stream (review X4): it stays on the trend, so it cannot be
spotted as an outlier against its neighbours (a uniform redraw could be, and
the neighbours' median then estimated the original). `--seed N` exists for tests only.

Where the report bytes are: the scrubber replays the Adafruit driver's view of
the bus (bno08x 1.3.3, `i2c.py:30-105`, `__init__.py:365-392`):
- ONE persistent buffer (512 bytes, replaced by a fresh zeroed buffer of the
  requested size for any read > 512 bytes) that every read fills from offset 0
  and every send overwrites (writes are modelled from the first non-empty
  write, the soft reset; the probe before it uses its own buffer);
- a FULL packet read is a read that follows a successful 4-byte header read,
  requests exactly that header's byte count (`& 0x7FFF`) and returns it all;
- after a full read, the Packet is parsed from the BUFFER: header from bytes
  0-3 (the full read's own header), payload = buffer[4 : 4 + count - 4], so a
  header that claims more than was read slices STALE bytes (review F5); each
  byte keeps its origin (op index, offset), so a stale data byte is scrubbed
  in the op that brought it in;
- the payload is walked as `_separate_batch` does (report lengths from the
  driver's tables), INCLUDING the slices of a batch that later fails (#49
  carry-over, review F1) and the partial remainder (`Unprocessable Batch
  bytes`), whose available data bytes are scrubbed too.
A byte that one context needs as framing (header, report id, seq, status,
control report) and another reads as data is KEPT (it is framing, not motion)
and counted as a conflict.

The positional self-check (`check`): every scrubbed unit (a data word where
both bytes are in the transcript, else the single byte) must differ from the
original AT THE SAME POSITION; every other byte must be unchanged; the op
stream must be identical apart from `hex`. A synthetic value that happens to
equal the original at its position is re-drawn (fresh, from the secret
stream) before writing. The meta must carry no seed value. A multiset
comparison is NOT used: a stationary sensor's near-zero words recur in any
generator (review F18).

stdout: the scrubbed file's `stdout` records are recomputed by the WP2 Python
ORACLE (the unmodified helper on the real bench stack, faked below PureIO),
never by hand: `recompute_stdout()` is the single hook. Until WP2 lands, use
`--stdout-pending`, which writes the scrubbed ops with NO stdout records and
`"stdout": "pending-oracle"` in meta (complete it later with the oracle).
`--oracle-cmd CMD` runs CMD with the scrubbed ops (imutx JSON lines, meta
first) on stdin; CMD prints the replayed records (ops + stdout in emission
order) on stdout. The ops must come back unchanged and the stdout must have
the raw file's SHAPE (same line kinds, identical ready/error texts, identical
non-JSON lines once hex bytes are masked).
"""
import argparse
import copy
import json
import math
import os
import random
import re
import secrets
import struct
import subprocess
import sys

TOOL = "scrub-imutx/1"
DATA_BUFFER_SIZE = 512
HEADER_LEN = 4
INPUT_CHANNELS = (3, 4, 5)

ID_LA = 0x04
ID_RV = 0x05
# bno08x 1.3.3 `__init__.py:140-170`: report id -> length.
SENSOR_REPORT_LEN = {
    0x01: 10, 0x02: 10, 0x03: 10, 0x04: 10, 0x05: 14, 0x06: 10, 0x08: 12, 0x09: 14,
    0x11: 12, 0x13: 6, 0x14: 16, 0x15: 16, 0x16: 16, 0x19: 6, 0x1E: 16,
}
CONTROL_REPORT_LEN = {0xF8: 16, 0xFC: 17, 0xF1: 16, 0xFB: 5, 0xFA: 5}

KEEP = "keep"
REDRAW_TRIES = 8
SENSITIVE_KINDS = ("rv", "rv_acc", "la", "delay", "other", "tail")


class ScrubError(RuntimeError):
    pass


class OracleUnavailable(ScrubError):
    pass


def report_length(rid):
    """`_report_length` (`__init__.py:358-362`); None = KeyError."""
    if rid < 0xF0:
        return SENSOR_REPORT_LEN.get(rid)
    return CONTROL_REPORT_LEN.get(rid)


def ops_of(records):
    return [r for r in records if r["op"] not in ("meta", "stdout")]


def op_bytes(op):
    return bytes.fromhex(op.get("hex") or "")


# --------------------------------------------------------------------------
# Classification
# --------------------------------------------------------------------------
class Field:
    """One sensitive unit group. `units` are tuples of positions: a 2-tuple
    for a little-endian word (lo, hi), a 1-tuple for a byte. A position is
    (op_index, byte_offset) or None (not from the transcript)."""

    __slots__ = ("kind", "units")

    def __init__(self, kind, units):
        self.kind = kind
        self.units = units


class Classification:
    def __init__(self):
        self.fields = []
        self.roles = {}  # position -> KEEP | kind
        self.conflicts = 0
        self.packets = 0
        self.anomalous_reads = 0
        self.full_reads = []  # (op_index, count3, channel)

    def mark_keep(self, pos):
        if pos is None:
            return
        prev = self.roles.get(pos)
        if prev is not None and prev != KEEP:
            self.conflicts += 1
        self.roles[pos] = KEEP

    def add_field(self, kind, units):
        units = [u for u in units if any(p is not None for p in u)]
        if not units:
            return
        for u in units:
            for p in u:
                if p is None:
                    continue
                prev = self.roles.get(p)
                if prev == KEEP:
                    self.conflicts += 1
                elif prev is None:
                    self.roles[p] = kind
        self.fields.append(Field(kind, units))

    def sensitive(self, pos):
        return pos is not None and self.roles.get(pos) in SENSITIVE_KINDS


def _words(positions, start, count):
    out = []
    for w in range(count):
        a, b = start + 2 * w, start + 2 * w + 1
        if a >= len(positions):
            break
        lo = positions[a]
        hi = positions[b] if b < len(positions) else None
        out.append((lo, hi) if hi is not None else (lo,))
    return out


def _classify_slice(cls, rid, pos):
    """`pos` = the slice's positions (possibly a partial remainder)."""
    for p in pos[:1]:
        cls.mark_keep(p)
    if rid >= 0xF0:
        for p in pos[1:]:
            cls.mark_keep(p)
        return
    for p in pos[1:3]:  # sequence, status (2-bit accuracy + delay high bits)
        cls.mark_keep(p)
    if len(pos) > 3:
        cls.add_field("delay", [(pos[3],)])
    if rid == ID_RV:
        cls.add_field("rv", _words(pos, 4, 4))
        cls.add_field("rv_acc", _words(pos, 12, 1))
    elif rid == ID_LA:
        cls.add_field("la", _words(pos, 4, 3))
    else:
        cls.add_field("other", [(p,) for p in pos[4:]])


def _walk_packet(cls, buf, count3, channel):
    """`Packet(buffer)` + `_separate_batch`, on the modelled buffer."""
    data_len = max(0, count3 - HEADER_LEN)
    end = min(HEADER_LEN + data_len, len(buf))
    walked_to = HEADER_LEN
    i = HEADER_LEN
    while i < end:
        rid, origin = buf[i]
        if rid is None:  # a byte strace did not show (truncated string)
            break
        length = report_length(rid)
        if length is None:  # KeyError(id): the walk stops at the id
            cls.mark_keep(origin)
            walked_to = i + 1
            break
        remaining = HEADER_LEN + data_len - i
        take = min(length, remaining, end - i)
        _classify_slice(cls, rid, [buf[k][1] for k in range(i, i + take)])
        walked_to = i + take
        if remaining < length:  # Unprocessable Batch bytes: partial remainder
            break
        i += length
    if channel in INPUT_CHANNELS:
        # Unconsumed payload of an input-report packet after a batch error.
        cls.add_field("tail", [(buf[k][1],) for k in range(walked_to, end) if buf[k][1] is not None])


def classify(ops):
    cls = Classification()
    buf = [(0, None)] * DATA_BUFFER_SIZE
    modelling = False
    prev = None  # the last successful device read (op) with no write since
    for idx, op in enumerate(ops):
        kind = op["op"]
        if kind == "write":
            data = op_bytes(op)
            if not modelling and len(data) == 0:
                continue  # the zero-length probe
            if not modelling:
                modelling = True
                buf = [(0, None)] * DATA_BUFFER_SIZE
            for k, b in enumerate(data):  # `_send_packet` fills the buffer first
                if k < len(buf):
                    buf[k] = (b, None)
            prev = None
            continue
        if kind != "read":
            continue
        if not modelling:
            continue  # the probe's 1-byte read lands in its own buffer
        req = op.get("len") or 0
        if req > DATA_BUFFER_SIZE:
            buf = [(0, None)] * req  # `_read`: a fresh zeroed buffer
        ret = op.get("ret")
        if ret is None or ret < 0:
            prev = None
            continue
        data = op_bytes(op)
        for k in range(min(ret, len(buf))):
            buf[k] = (data[k], (idx, k)) if k < len(data) else (None, None)
        is_full = (
            prev is not None
            and prev.get("len") == 4
            and prev.get("ret") == 4
            and len(op_bytes(prev)) == 4
            and req == (int.from_bytes(op_bytes(prev)[:2], "little") & 0x7FFF)
            and ret == req
        )
        if is_full and ret >= HEADER_LEN and buf[0][0] is not None and buf[1][0] is not None:
            count3 = (buf[0][0] | (buf[1][0] << 8)) & 0x7FFF
            channel = buf[2][0]
            for k in range(HEADER_LEN):
                cls.mark_keep(buf[k][1])
            cls.packets += 1
            cls.full_reads.append((idx, count3, channel))
            _walk_packet(cls, buf, count3, channel)
        elif ret > HEADER_LEN and not is_full:
            cls.anomalous_reads += 1
            cls.add_field("tail", [((idx, k),) for k in range(HEADER_LEN, len(data))])
        prev = op if ret == req else None
    # Bytes of a full read beyond its packet that no context consumed.
    for idx, count3, _channel in cls.full_reads:
        n = len(op_bytes(ops[idx]))
        extra = [((idx, k),) for k in range(max(HEADER_LEN, count3), n) if (idx, k) not in cls.roles]
        cls.add_field("tail", extra)
    return cls


# --------------------------------------------------------------------------
# Synthetic values
# --------------------------------------------------------------------------
def _q(v, bits):
    return max(-32768, min(32767, round(v * (1 << bits))))


class SyntheticGen:
    """Synthetic motion: a slow rotation about a wandering axis and small
    accelerations. Periods, phases and jitter all come from the seeded stream,
    so without the (secret) seed no draw is predictable. Never reads the
    original."""

    def __init__(self, seed):
        self.rng = random.Random(seed)
        # Redraws use their OWN secret stream, so a redraw never shifts the
        # draws that follow it (review X4).
        self.redraw_rng = random.Random(f"redraw:{seed}")
        r = self.rng
        self.p = [r.uniform(20.0, 80.0) for _ in range(4)] + [r.uniform(0.0, 2 * math.pi) for _ in range(6)]
        self.n_rv = 0
        self.n_la = 0

    def rv(self, n=None, rng=None):
        if n is None:
            n = self.n_rv
            self.n_rv += 1
        p = self.p
        ang = 0.8 + 0.5 * math.sin(n / p[0] + p[4]) + (rng or self.rng).uniform(-0.01, 0.01)
        ax = (math.sin(n / p[1] + p[5]), math.cos(n / p[1] + p[5]), 0.4 + 0.1 * math.sin(n / p[2] + p[6]))
        norm = math.sqrt(sum(a * a for a in ax))
        s = math.sin(ang / 2.0) / norm
        q = (ax[0] * s, ax[1] * s, ax[2] * s, math.cos(ang / 2.0))
        return [_q(v, 14) for v in q]

    def rv_acc(self, rng=None):
        return [_q(0.2 + (rng or self.rng).uniform(0.0, 0.3), 12)]

    def la(self, n=None, rng=None):
        if n is None:
            n = self.n_la
            self.n_la += 1
        r = rng or self.rng
        return [
            _q(0.15 * math.sin(n / self.p[3] + ph) + r.uniform(-0.03, 0.03), 8)
            for ph in (self.p[7], self.p[8], self.p[9])
        ]

    def byte(self):
        return self.rng.randrange(256)

    def index(self, kind):
        """The trend index the NEXT draw of `kind` will use (None: no trend)."""
        return {"rv": self.n_rv, "la": self.n_la}.get(kind)

    def redraw(self, kind, n, k):
        """A fresh value for unit `k` of a `kind` field drawn at trend index `n`
        (review X4): the SAME trend with fresh jitter from the redraw stream, so
        a redrawn word has the distribution of any other draw and does not stand
        out from its neighbours. Byte kinds are uniform, like their draws. Uses
        the base formulas even when a subclass overrides the draws (tests)."""
        r = self.redraw_rng
        if kind == "rv":
            return SyntheticGen.rv(self, n or 0, r)[k]
        if kind == "rv_acc":
            return SyntheticGen.rv_acc(self, r)[k]
        if kind == "la":
            return SyntheticGen.la(self, n or 0, r)[k]
        return r.randrange(256)


def _draw_units(field, gen):
    """One synthetic value per unit: ints (words) or bytes."""
    if field.kind == "rv":
        return gen.rv()
    if field.kind == "rv_acc":
        return gen.rv_acc()
    if field.kind == "la":
        return gen.la()
    return [gen.byte() for _ in field.units]


def apply_scrub(ops, cls, gen):
    """Return (new_ops, redraws). Only bytes classified sensitive change."""
    bufs = {i: bytearray(op_bytes(op)) for i, op in enumerate(ops) if op["op"] == "read"}
    done = set()
    redraws = 0
    for field in cls.fields:
        n = gen.index(field.kind)
        values = _draw_units(field, gen)
        units = field.units
        for k, (unit, value) in enumerate(zip(units, values, strict=False)):  # a partial report has fewer units
            live = [(j, p) for j, p in enumerate(unit) if cls.sensitive(p) and p not in done]
            if not live:
                continue
            if len(unit) == 2:
                new = struct.pack("<h", value)
            else:
                new = bytes([value & 0xFF])
            orig = bytes(bufs[p[0]][p[1]] for _, p in live)
            cand = bytes(new[j] for j, _ in live)
            v_cur, tries = value, 0
            while cand == orig:
                # The draw equals the original at THIS position: re-draw it ON
                # the trend (same field, same index, fresh secret jitter), so the
                # redraw is indistinguishable from any other draw (review X4).
                # Where the jitter cannot move this unit (e.g. an axis component
                # near 0), fall back after 8 tries to a +-1 LSB nudge with a
                # secret sign (+-256 when only the high byte is live).
                redraws += 1
                tries += 1
                if tries <= REDRAW_TRIES:
                    v_cur = gen.redraw(field.kind, n, k)
                else:
                    step = 1 if (len(unit) == 1 or any(j == 0 for j, _ in live)) else 256
                    v_cur += gen.redraw_rng.choice((-step, step))
                    if len(unit) == 2:
                        v_cur = max(-32768, min(32767, v_cur))
                new = struct.pack("<h", v_cur) if len(unit) == 2 else bytes([v_cur & 0xFF])
                cand = bytes(new[j] for j, _ in live)
            for (_, p), b in zip(live, cand, strict=True):
                bufs[p[0]][p[1]] = b
                done.add(p)
    new_ops = []
    for i, op in enumerate(ops):
        op = dict(op)
        if i in bufs:
            op["hex"] = bufs[i].hex()
        new_ops.append(op)
    return new_ops, redraws


# --------------------------------------------------------------------------
# The oracle hook (WP2)
# --------------------------------------------------------------------------
def recompute_stdout(scrubbed_ops, oracle):
    """THE single oracle hook. `oracle(ops) -> records` replays the scrubbed op
    stream through the unmodified helper (WP2) and returns ops + stdout
    records in emission order. The ops must come back unchanged."""
    if oracle is None:
        raise OracleUnavailable(
            "the WP2 oracle is not available: pass --oracle-cmd, or --stdout-pending to defer the stdout records"
        )
    records = [r for r in oracle(copy.deepcopy(scrubbed_ops)) if r["op"] != "meta"]
    if ops_of(records) != scrubbed_ops:
        raise ScrubError("the oracle changed the op stream")
    return records


def command_oracle(cmd):
    def run(ops):
        recs = [{"op": "meta", "format": "imutx/1"}] + ops
        payload = "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in recs)
        p = subprocess.run(cmd, shell=True, input=payload, capture_output=True, text=True, check=False)
        if p.returncode != 0:
            raise ScrubError(f"oracle command failed (rc {p.returncode})")
        return [json.loads(line) for line in p.stdout.splitlines() if line.strip()]

    return run


_HEX = re.compile(r"0x[0-9A-Fa-f]{2}\b")


def stdout_shape(records):
    shape = []
    for r in records:
        if r["op"] != "stdout":
            continue
        try:
            obj = json.loads(r["line"])
        except ValueError:
            obj = None
        if isinstance(obj, dict) and obj.get("type") == "sample":
            shape.append(("sample",))
        elif isinstance(obj, dict):
            shape.append(("json", r["line"]))
        else:
            shape.append(("text", _HEX.sub("0x??", r["line"])))
    return shape


def stdout_shape_violations(raw_records, new_records):
    a, b = stdout_shape(raw_records), stdout_shape(new_records)
    if a == b:
        return []
    for i, (x, y) in enumerate(zip(a, b, strict=False)):
        if x != y:
            return [f"stdout line {i}: kind/text differs from the raw transcript ({x[0]} vs {y[0]})"]
    return [f"stdout has {len(b)} lines, the raw transcript {len(a)}"]


# --------------------------------------------------------------------------
# Self-check (positional)
# --------------------------------------------------------------------------
def self_check(raw_records, scrubbed_records):
    v = []
    raw_ops, new_ops = ops_of(raw_records), ops_of(scrubbed_records)
    if len(raw_ops) != len(new_ops):
        return [f"op count {len(new_ops)} != raw {len(raw_ops)}"]
    for i, (a, b) in enumerate(zip(raw_ops, new_ops, strict=True)):
        if {k: x for k, x in a.items() if k != "hex"} != {k: x for k, x in b.items() if k != "hex"}:
            v.append(f"op {i}: fields other than hex differ")
        if len(a.get("hex") or "") != len(b.get("hex") or ""):
            v.append(f"op {i}: byte length differs")
        if a["op"] != "read" and a.get("hex") != b.get("hex"):
            v.append(f"op {i}: a non-read op changed its bytes")
    if v:
        return v
    cls = classify(raw_ops)
    raw_b = {i: op_bytes(op) for i, op in enumerate(raw_ops) if op["op"] == "read"}
    new_b = {i: op_bytes(op) for i, op in enumerate(new_ops) if op["op"] == "read"}
    claimed = set()  # the same first-claim rule as apply_scrub
    for field in cls.fields:
        for unit in field.units:
            live = [p for p in unit if cls.sensitive(p) and p not in claimed]
            claimed.update(live)
            if live and all(raw_b[p[0]][p[1]] == new_b[p[0]][p[1]] for p in live):
                p = live[0]
                v.append(f"{field.kind} unit at op {p[0]} offset {p[1]} keeps its original value")
    for i, data in raw_b.items():
        for k, b in enumerate(data):
            if not cls.sensitive((i, k)) and new_b[i][k] != b:
                v.append(f"op {i} offset {k}: a framing/kept byte changed")
    meta = scrubbed_records[0] if scrubbed_records else {}
    if meta.get("op") != "meta" or not meta.get("scrubbed"):
        v.append("the scrubbed file has no scrubbed meta record")
    elif meta["scrubbed"].get("seed") != "secret":
        v.append("the scrubbed meta carries a seed value (review F6: the seed must stay secret)")
    return v


# --------------------------------------------------------------------------
def scrub(raw_records, seed=None, oracle=None, stdout_pending=False, gen=None):
    """`seed=None` (the default) draws a secret one; it never reaches the output."""
    if not raw_records or raw_records[0].get("op") != "meta" or raw_records[0].get("format") != "imutx/1":
        raise ScrubError("not an imutx/1 file")
    if raw_records[0].get("scrubbed"):
        raise ScrubError("already scrubbed")
    if seed is None:
        seed = secrets.randbits(64)
    ops = ops_of(raw_records)
    cls = classify(ops)
    new_ops, redraws = apply_scrub(ops, cls, gen or SyntheticGen(seed))
    if stdout_pending:
        body, stdout_state = new_ops, "pending-oracle"
    else:
        body, stdout_state = recompute_stdout(new_ops, oracle), "oracle"
    stats = {
        "fields": {k: sum(1 for f in cls.fields if f.kind == k) for k in SENSITIVE_KINDS},
        "packets": cls.packets,
        "conflicts": cls.conflicts,
        "anomalous_reads": cls.anomalous_reads,
        "redraws": redraws,
    }
    meta = dict(raw_records[0])
    meta["scrubbed"] = {"tool": TOOL, "seed": "secret", "stdout": stdout_state}
    out = [meta] + body
    violations = self_check(raw_records, out)
    if not stdout_pending:
        violations += stdout_shape_violations(raw_records, out)
    if violations:
        raise ScrubError("; ".join(violations[:5]) + (f" (+{len(violations) - 5} more)" if len(violations) > 5 else ""))
    return out, stats


def _read(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def _write(path, records):
    with open(path, "w") as f:
        for r in records:
            f.write(json.dumps(r, separators=(",", ":")) + "\n")


def main(argv=None):
    ap = argparse.ArgumentParser(prog="scrub-imutx")
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("scrub")
    s.add_argument("raw")
    s.add_argument("-o", "--out", required=True)
    s.add_argument("--seed", type=int, default=None, help="TESTS ONLY: a fixed seed (default: a secret one)")
    s.add_argument("--seed-out", help="write the secret seed to this file (0600, in the PRIVATE dir only)")
    g = s.add_mutually_exclusive_group(required=True)
    g.add_argument("--oracle-cmd")
    g.add_argument("--stdout-pending", action="store_true")
    c = sub.add_parser("check")
    c.add_argument("raw")
    c.add_argument("scrubbed")
    a = ap.parse_args(argv)
    if a.cmd == "check":
        raw, scr = _read(a.raw), _read(a.scrubbed)
        v = self_check(raw, scr)
        if scr and scr[0].get("scrubbed", {}).get("stdout") == "oracle":
            v += stdout_shape_violations(raw, scr)
        print(json.dumps({"violations": len(v)}))
        for line in v[:20]:
            print(line, file=sys.stderr)
        return 1 if v else 0
    oracle = command_oracle(a.oracle_cmd) if a.oracle_cmd else None
    seed = secrets.randbits(64) if a.seed is None else a.seed
    try:
        out, stats = scrub(_read(a.raw), seed, oracle, a.stdout_pending)
    except ScrubError as e:
        print(f"scrub-imutx: {e}", file=sys.stderr)
        return 3
    _write(a.out, out)
    if a.seed_out:
        fd = os.open(a.seed_out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(f"{seed}\n")
    print(json.dumps(stats, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
