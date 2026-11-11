#!/usr/bin/env python3
"""Phase 7c WP1: generate `fixtures/imu_codec/*.json` from CPython and the
bench's Adafruit wheels. Every expected value in those fixtures was printed
by the REAL code: CPython's `repr`/`json.dumps`/exception `str`, and the real
`adafruit_bno08x` 1.3.3, `adafruit_bus_device` 5.2.17, Blinka 9.1.0
`generic_linux/i2c.py` (+ the two `busio.I2C` methods compiled from the
9.1.0 source) and `Adafruit_PureIO` 1.1.11 `smbus.SMBus`. Only the i2c-dev
FILE object below PureIO is faked (a scripted list of `read(2)` results).

Synthetic inputs only: no transcript bytes, no sensor values.

Usage (labelled container on the homelab, plan phase-7c WP1):

    python3 -I gen_codec_fixtures.py --adafruit <unpacked-wheels-dir> --out <dir>

`--adafruit` holds the unpacked wheels (`adafruit_bno08x/`,
`adafruit_bus_device/`, `adafruit_blinka/`, `Adafruit_PureIO/`, `busio.py`,
`micropython.py` and the `*.dist-info/` dirs).
"""

import argparse
import ast
import contextlib
import hashlib
import io
import json
import os
import struct
import sys

GENERATED_BY = "tools/imu/codec-fixtures/gen_codec_fixtures.py"


def sha256_file(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def exc_pair(e):
    return [type(e).__name__, str(e)]


def hx(b):
    return bytes(b).hex()


# --------------------------------------------------------------------------
# The mini stack: real bno08x -> real I2CDevice -> busio.I2C methods compiled
# from the 9.1.0 source -> real generic_linux.I2C -> real PureIO SMBus ->
# FakeFile (the only fake).
# --------------------------------------------------------------------------


class FakeFile:
    """Stands in for `open("/dev/i2c-1", "r+b", buffering=0)`. Each `read`
    pops one scripted result: bytes, None (CPython FileIO on EAGAIN) or
    ("oserr", errno)."""

    def __init__(self, script):
        self.script = list(script)
        self.log = []

    def fileno(self):
        return 99

    def close(self):
        pass

    def read(self, n):
        self.log.append(["read", n])
        r = self.script.pop(0)
        if isinstance(r, tuple):
            raise OSError(r[1], os.strerror(r[1]))
        return r

    def write(self, b):
        self.log.append(["write", bytes(b).hex()])
        return len(b)


def build_stack(adafruit_dir):
    sys.path.insert(0, adafruit_dir)
    import types  # noqa: PLC0415

    from adafruit_blinka import Lockable  # noqa: PLC0415
    from adafruit_blinka.microcontroller.generic_linux.i2c import I2C as LinuxI2C  # noqa: PLC0415
    import Adafruit_PureIO.smbus as smbus  # noqa: PLC0415

    # busio.I2C imports platformdetect at module import; compile just its
    # `readfrom_into` / `writeto` methods from the real 9.1.0 source.
    busio_path = os.path.join(adafruit_dir, "busio.py")
    tree = ast.parse(open(busio_path).read())
    i2c_cls = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "I2C")
    methods = [
        n for n in i2c_cls.body
        if isinstance(n, ast.FunctionDef) and n.name in ("readfrom_into", "writeto")
    ]
    assert len(methods) == 2
    cls_def = ast.ClassDef(
        name="BusioI2C", bases=[ast.Name("Lockable", ast.Load())], keywords=[],
        body=methods, decorator_list=[], type_params=[],
    ) if sys.version_info >= (3, 12) else ast.ClassDef(
        name="BusioI2C", bases=[ast.Name("Lockable", ast.Load())], keywords=[],
        body=methods, decorator_list=[],
    )
    mod = ast.Module(body=[cls_def], type_ignores=[])
    ast.fix_missing_locations(mod)
    ns = {"Lockable": Lockable}
    exec(compile(mod, busio_path, "exec"), ns)  # noqa: S102
    BusioI2C = ns["BusioI2C"]

    # i2c_device.py evaluates `i2c: I2C` / `buf: WriteableBuffer` annotations
    # at def time; on the bench they come from `busio` and
    # `circuitpython_typing`. Provide them (annotations only).
    busio_mod = types.ModuleType("busio")
    busio_mod.I2C = BusioI2C
    sys.modules["busio"] = busio_mod
    typing_mod = types.ModuleType("circuitpython_typing")
    typing_mod.ReadableBuffer = typing_mod.WriteableBuffer = object
    sys.modules["circuitpython_typing"] = typing_mod
    import adafruit_bno08x as B  # noqa: PLC0415
    from adafruit_bno08x.i2c import BNO08X_I2C  # noqa: PLC0415
    from adafruit_bus_device.i2c_device import I2CDevice  # noqa: PLC0415

    ioctl_log = []
    smbus.ioctl = lambda fd, req, arg: ioctl_log.append([req, arg])

    def make(script, address=0x4A):
        f = FakeFile(script)
        bus = smbus.SMBus.__new__(smbus.SMBus)
        bus._device = f
        lin = LinuxI2C.__new__(LinuxI2C)
        lin._i2c_bus = bus
        busio = BusioI2C.__new__(BusioI2C)
        busio._i2c = lin
        dev = I2CDevice(busio, address, probe=False)
        bno = BNO08X_I2C.__new__(BNO08X_I2C)
        bno.bus_device_obj = dev
        bno._debug = False
        bno._data_buffer = bytearray(B.DATA_BUFFER_SIZE)
        bno._packet_slices = []
        bno._sequence_number = [0, 0, 0, 0, 0, 0]
        bno._readings = {}
        bno._magnetometer_accuracy = 0
        bno._me_calibration_started_at = -1.0
        bno._dcd_saved_at = -1
        return bno, f

    srcs = {
        "adafruit_bno08x/__init__.py": os.path.join(adafruit_dir, "adafruit_bno08x/__init__.py"),
        "adafruit_bno08x/i2c.py": os.path.join(adafruit_dir, "adafruit_bno08x/i2c.py"),
        "adafruit_bus_device/i2c_device.py": os.path.join(adafruit_dir, "adafruit_bus_device/i2c_device.py"),
        "busio.py": busio_path,
        "adafruit_blinka/microcontroller/generic_linux/i2c.py": os.path.join(
            adafruit_dir, "adafruit_blinka/microcontroller/generic_linux/i2c.py"),
        "Adafruit_PureIO/smbus.py": os.path.join(adafruit_dir, "Adafruit_PureIO/smbus.py"),
    }
    versions = {}
    for d in sorted(os.listdir(adafruit_dir)):
        if d.endswith(".dist-info"):
            versions[d[: -len(".dist-info")]] = True
    meta = {
        "generatedBy": GENERATED_BY,
        "python": sys.version.split()[0],
        "wheels": sorted(versions),
        "sourceSha256": {k: sha256_file(v) for k, v in srcs.items()},
        "synthetic": True,
    }
    return B, make, meta


# --------------------------------------------------------------------------
# float_repr.json
# --------------------------------------------------------------------------


def gen_float_repr(B, meta):
    out = dict(meta)
    for name, q, scalar in (("q14", 14, B._Q_POINT_14_SCALAR), ("q8", 8, B._Q_POINT_8_SCALAR)):
        reprs = [repr(k * scalar) for k in range(-32768, 32768)]
        stream = "\n".join(reprs).encode()
        pick = sorted(set(
            [-32768, -32767, -16385, -16384, -16383, -256, -255, -3, -2, -1, 0, 1, 2, 3,
             255, 256, 16383, 16384, 16385, 32766, 32767] + list(range(-32768, 32768, 997))
        ))
        out[name] = {
            "qPoint": q,
            "count": len(reprs),
            "sha256": hashlib.sha256(stream).hexdigest(),
            "samples": [[k, reprs[k + 32768]] for k in pick],
        }
    general = [0.1, 0.5, 1.0, -2.0, 100.0, 1e16, 1e15, 9999999999999998.0, 123456789012345678.0,
               1e-4, 1e-5, 0.00012, 1.5e-7, 5e-324, 1.7976931348623157e308, 2.5e-05,
               9007199254740993.0, 0.30000000000000004, -0.0, 1e22, 1e21, 12345.678,
               float("nan"), float("inf"), float("-inf"), 0.0001220703125, 6.103515625e-05]
    out["general"] = [[struct.pack(">d", x).hex(), repr(x), json.dumps(x)] for x in general]
    return out


# --------------------------------------------------------------------------
# py_text.json
# --------------------------------------------------------------------------


def gen_py_text(B, meta):
    out = dict(meta)
    strings = [chr(c) for c in range(0, 0x80)] + [
        "No Hardware I2C on (scl,sda)=(3, 2)\nValid I2C ports: ((1, 3, 2), (0, 1, 0), (10, 45, 44)).\nMake sure I2C is enabled.",
        'quote " and \\ backslash',
        "café",
        "\U0001F600",
        " ",
        "tab\tcr\rff\fbs\b",
        "\x00\x1f\x7f\x80",
        "",
    ]
    out["jsonStrings"] = [[s, json.dumps(s)] for s in strings]

    ready = []
    for a in (0x4A, 0x4B, 75, 0x14A, 0):
        ready.append([a, json.dumps({"type": "ready", "address": hex(a)})])
    out["readyLines"] = ready

    raws = [
        [0, 0, 0, 0, 0, 0, 0],
        [1, -1, 2, 16384, 1, -1, 256],
        [-32768, 32767, -16384, 16383, -32768, 32767, -256],
        [123, -4567, 8901, 12345, 77, -1234, 2561],
        [-1, -2, -3, -4, -5, -6, -7],
        [16384, 0, 0, 0, 32767, -32767, 0],
    ]
    q14, q8 = B._Q_POINT_14_SCALAR, B._Q_POINT_8_SCALAR
    samples = []
    for r in raws:
        qi, qj, qk, qr = (r[0] * q14, r[1] * q14, r[2] * q14, r[3] * q14)
        ax, ay, az = (r[4] * q8, r[5] * q8, r[6] * q8)
        line = json.dumps({
            "type": "sample",
            "quat": {"r": qr, "i": qi, "j": qj, "k": qk},
            "linaccel": {"x": ax, "y": ay, "z": az},
        })
        samples.append([r, line])
    # The FC zero seed: plain float literals, not raw * scalar.
    seed = json.dumps({"type": "sample", "quat": {"r": 0.0, "i": 0.0, "j": 0.0, "k": 0.0},
                       "linaccel": {"x": 0.0, "y": 0.0, "z": 0.0}})
    out["sampleLines"] = samples
    out["zeroSeedLine"] = seed

    exc_args = [
        ("RuntimeError", ["Unprocessable Batch bytes", 3]),
        ("RuntimeError", ["Unprocessable Batch bytes", 1]),
        ("RuntimeError", ["Was not able to enable feature", 5]),
        ("RuntimeError", ["Was not able to enable feature", 4]),
        ("RuntimeError", ["Timed out waiting for a packet on channel", 2]),
        ("RuntimeError", ["Could not read ID"]),
        ("RuntimeError", ["Timed out waiting for a packet"]),
        ("RuntimeError", ["Unable to save calibration data"]),
        ("RuntimeError", []),
        ("RuntimeError", ["it's", 1]),
        ("RuntimeError", ["both ' and \"", 2]),
        ("RuntimeError", ["line\nbreak\ttab\\", -7]),
        ("RuntimeError", ["\x01\x7f", 0]),
        ("ValueError", ["No I2C device at address: 0x%x" % 0x4A]),
        ("KeyError", [0]),
        ("KeyError", [7]),
        ("KeyError", [250]),
        ("PacketError", ["No packet available"]),
        ("IndexError", ["index out of range"]),
    ]
    excs = []
    for name, args in exc_args:
        cls = {"RuntimeError": RuntimeError, "ValueError": ValueError, "KeyError": KeyError,
               "IndexError": IndexError, "PacketError": B.PacketError}[name]
        e = cls(*args)
        excs.append([name, args, str(e)])
    out["excStr"] = excs

    raised = []

    def catch(label, fn):
        try:
            fn()
        except Exception as e:  # noqa: BLE001
            raised.append([label] + exc_pair(e))
        else:
            raise AssertionError(label)

    def list_assign():
        lst = [0, 0, 0, 0, 0, 0]
        lst[6] = 1

    catch("list_assignment", list_assign)
    catch("bytes_index", lambda: b"ab"[5])
    catch("bytearray_index", lambda: bytearray(2)[5])
    catch("list_index", lambda: ["a"][3])
    def none_subscript():
        readin = None
        return readin[0]

    catch("none_subscript", none_subscript)
    out["raised"] = raised

    structs = []
    for fmt, n, off in [("<B", 2, 2), ("<B", 0, 2), ("<h", 5, 4), ("<h", 4, 4), ("<h", 2, 4),
                        ("<h", 13, 12), ("<H", 9, 8), ("<H", 5, 4), ("<BBBHIII", 16, 0),
                        ("<BBBHIII", 0, 0), ("<BBBBB", 4, 0), ("<BBBBBBBBBBB", 15, 5),
                        ("<BBBBBBBBB", 14, 6), ("<I", 7, 4), ("<I", 11, 8), ("<H", 13, 12),
                        ("<B", 4, 4), ("<B", 5, 5)]:
        try:
            struct.unpack_from(fmt, bytes(n), offset=off)
        except struct.error as e:
            structs.append([fmt, n, off, str(e)])
        else:
            raise AssertionError((fmt, n, off))
    out["structErrors"] = structs

    errnos = [1, 2, 5, 6, 11, 13, 16, 19, 22, 95, 110, 121]
    out["strerror"] = [[e, os.strerror(e)] for e in errnos]
    out["osErrorStr"] = [[e, str(OSError(e, os.strerror(e)))] for e in errnos]
    out["osErrorPathStr"] = [[e, str(OSError(e, os.strerror(e), "/dev/i2c-1"))] for e in errnos]
    try:
        open("/nonexistent-p7c/i2c-1", "r+b", buffering=0)
    except OSError as e:
        out["openEnoent"] = exc_pair(e)

    helper = []
    for addr, text in [(0x4A, "Could not read ID"), (0x4B, "[Errno 5] Input/output error"),
                       (0x4A, str(RuntimeError("Was not able to enable feature", 5)))]:
        helper.append(["init", addr, text, f"init failed at {hex(addr)}: {text}"])
    for n, text in [(40, "'NoneType' object is not subscriptable"), (40, "250"),
                    (40, str(RuntimeError("Unprocessable Batch bytes", 3)))]:
        helper.append(["read", n, text, f"read failed {n}x: {text}"])
    out["helperTexts"] = helper
    return out


# --------------------------------------------------------------------------
# shtp.json
# --------------------------------------------------------------------------

HEADER_COUNTS = [0, 1, 2, 3, 4, 5, 0x10, 0x12, 0x8010, 0x8000, 0x8003, 0x8004, 0x8005,
                 0x7FFF, 0xFFFF, 0x7FFE, 0xFFFE, 512, 513, 516, 517, 0x0400]


def gen_shtp(B, make, meta):
    out = dict(meta)
    headers = []
    for count in HEADER_COUNTS:
        for ch, seq in ((0, 0), (1, 1), (2, 0x7F), (3, 0xFE), (5, 0xFF), (6, 0xFF), (7, 3), (255, 0)):
            buf = bytearray(struct.pack("<H", count) + bytes([ch, seq]))
            h = B.Packet.header_from_buffer(buf)
            headers.append([hx(buf), h.channel_number, h.sequence_number, h.data_length,
                            h.packet_byte_count])
    out["headers"] = headers

    ready = []
    for count in HEADER_COUNTS:
        for ch, seq in ((3, 0), (3, 0xFF), (6, 0xFF)):
            hdr = struct.pack("<H", count) + bytes([ch, seq])
            bno, f = make([hdr])
            cap = io.StringIO()
            with contextlib.redirect_stdout(cap):
                r = bno._data_ready
            ready.append({"read": hx(hdr), "ready": r, "stdout": cap.getvalue(),
                          "log": f.log, "bufferHead": hx(bno._data_buffer[:8])})
    for label, script in (("eagain", [None]), ("short2", [b"\x05\x00"]),
                          ("eio", [("oserr", 5)])):
        bno, f = make(script)
        try:
            bno._data_ready
        except Exception as e:  # noqa: BLE001
            ready.append({"label": label, "exc": exc_pair(e), "log": f.log,
                          "bufferHead": hx(bno._data_buffer[:8])})
    out["dataReady"] = ready

    sends = []
    rpt = B.BNO08X._get_feature_enable_report
    cases = [
        (1, [1], [0, 0, 0, 0, 0, 0]),
        (1, [1], [0, 254, 0, 0, 0, 0]),
        (1, [1], [0, 255, 0, 0, 0, 0]),
        (2, [0xF9, 0], [0, 2, 9, 0, 0, 0]),
        (2, list(rpt(0x05, 50000)), [0, 2, 10, 0, 0, 0]),
        (2, list(rpt(0x04, 50000)), [0, 2, 255, 0, 0, 0]),
        (2, list(rpt(0x1E, 50000, 0x1FF)), [0, 0, 0, 0, 0, 0]),
        (0, [], [7, 0, 0, 0, 0, 0]),
    ]
    for ch, data, seqs in cases:
        bno, f = make([])
        bno._sequence_number = list(seqs)
        stale = bytes(range(256)) * 2
        bno._data_buffer[:] = stale
        ret = bno._send_packet(ch, bytearray(data))
        sends.append({"channel": ch, "data": hx(data), "seqBefore": seqs,
                      "written": [e[1] for e in f.log if e[0] == "write"], "ret": ret,
                      "seqAfter": list(bno._sequence_number),
                      "bufferHead": hx(bno._data_buffer[:32]), "bufferLen": len(bno._data_buffer)})
    out["sends"] = sends

    def payload(n, seed):
        return bytes((seed + i * 7) & 0xFF for i in range(n))

    stale = bytes((0xA0 + i) & 0xFF for i in range(512))
    reads = []

    def read_case(label, script, seqs=None, buf=None):
        bno, f = make(script)
        if seqs:
            bno._sequence_number = list(seqs)
        bno._data_buffer = bytearray(buf if buf is not None else stale)
        res = {"label": label,
               "script": [hx(x) if isinstance(x, bytes) else (list(x) if isinstance(x, tuple) else None)
                          for x in script],
               "start": "zeros" if buf is not None else "ramp",
               "bufferBefore": hx(bno._data_buffer[:16]),
               "bufferLenBefore": len(bno._data_buffer), "seqBefore": list(bno._sequence_number)}
        try:
            p = bno._read_packet()
            res["packet"] = {"channel": p.header.channel_number, "seq": p.header.sequence_number,
                             "dataLength": p.header.data_length,
                             "count": p.header.packet_byte_count, "data": hx(p.data)}
        except Exception as e:  # noqa: BLE001
            res["exc"] = exc_pair(e)
        res["reads"] = [e[1] for e in f.log if e[0] == "read"]
        res["seqAfter"] = list(bno._sequence_number)
        res["bufferLen"] = len(bno._data_buffer)
        res["bufferSha256"] = hashlib.sha256(bno._data_buffer).hexdigest()
        res["bufferHead"] = hx(bno._data_buffer[:40])
        reads.append(res)

    def frame(count, ch, seq, body):
        return struct.pack("<H", count) + bytes([ch, seq]) + body

    h18 = frame(18, 3, 7, b"")
    read_case("normal", [h18, frame(18, 3, 7, payload(14, 1))])
    read_case("continuation_bit", [frame(0x8012, 3, 8, b""), frame(0x8012, 3, 8, payload(14, 2))])
    read_case("count_zero", [frame(0, 2, 9, b"")], seqs=[1, 2, 3, 4, 5, 6])
    read_case("channel_6", [frame(18, 6, 1, b"")])
    read_case("third_disagrees", [h18, frame(30, 2, 9, payload(14, 3))])
    read_case("third_channel_6", [h18, frame(18, 6, 9, payload(14, 3))])
    read_case("third_beyond_buffer", [h18, frame(0x0400, 3, 9, payload(14, 4))])
    read_case("third_count_zero", [h18, frame(0, 3, 9, payload(14, 4))])
    read_case("over_512", [frame(600, 3, 4, b""), frame(600, 3, 4, payload(596, 5))])
    read_case("over_512_then_smaller_buffer", [frame(520, 3, 4, b""), frame(520, 3, 4, payload(516, 6))],
              buf=bytes(1000))
    read_case("clamp_7fff", [frame(0x7FFF, 3, 1, b""), payload(8192, 7)])
    read_case("clamp_ffff", [frame(0xFFFF, 3, 0xFF, b""), payload(8192, 8)])
    read_case("eagain_header", [None])
    read_case("eagain_full", [h18, None])
    read_case("eio_full", [h18, ("oserr", 5)])
    read_case("short_full", [h18, frame(18, 3, 7, payload(6, 9))])
    for c in (1, 2, 3, 4, 5):
        read_case(f"count_{c}", [frame(c, 3, 2, b""), frame(c, 3, 2, payload(16, 10))[:c]])
    out["readPackets"] = reads
    return out


# --------------------------------------------------------------------------
# sh2.json
# --------------------------------------------------------------------------


def reading_entry(v):
    if isinstance(v, tuple) and all(isinstance(x, float) for x in v):
        kind = "floats"
    elif isinstance(v, tuple) and all(isinstance(x, int) for x in v):
        kind = "ints"
    elif isinstance(v, int) and not isinstance(v, bool):
        kind = "int"
    else:
        kind = "other"
    return {"kind": kind, "repr": repr(v)}


def gen_sh2(B, make, meta):
    out = dict(meta)
    lengths = []
    for rid in range(256):
        try:
            lengths.append([rid, B._report_length(rid)])
        except KeyError as e:
            lengths.append([rid, None, str(e)])
    out["reportLengths"] = lengths
    out["sensorTable"] = [[rid, repr(s), c, n, rid in B._RAW_REPORTS]
                          for rid, (s, c, n) in sorted(B._AVAIL_SENSOR_REPORTS.items())]
    out["setFeature"] = [
        [0x05, 50000, 0, hx(B.BNO08X._get_feature_enable_report(0x05, 50000))],
        [0x04, 50000, 0, hx(B.BNO08X._get_feature_enable_report(0x04, 50000))],
        [0x1E, 50000, 0x1FF, hx(B.BNO08X._get_feature_enable_report(0x1E, 50000, 0x1FF))],
        [0x05, 0xDEADBEEF, 0x01020304,
         hx(B.BNO08X._get_feature_enable_report(0x05, 0xDEADBEEF, 0x01020304))],
    ]

    def le16(v):
        return struct.pack("<h", v)

    def sensor(rid, seq, status, delay, words, extra=b"", unsigned=False):
        body = bytes([rid, seq, status, delay])
        for w in words:
            body += struct.pack("<H" if unsigned else "<h", w)
        return body + extra

    def rv(words, acc=0x1234, seq=1, status=3, delay=0):
        return sensor(0x05, seq, status, delay, words, le16(acc))

    def la(words, seq=1, status=2, delay=0):
        return sensor(0x04, seq, status, delay, words)

    fb = bytes([0xFB, 0x10, 0x20, 0x30, 0x40])
    fa = bytes([0xFA, 0x01, 0x02, 0x03, 0x04])

    def fc(fid, interval=50000):
        return struct.pack("<BBBHIII", 0xFC, fid, 0, 0, interval, 0, 0)

    def f8(part=0x0098A2F1):
        return bytes([0xF8, 0x00, 3, 2]) + struct.pack("<II", part, 0x175) + struct.pack("<H", 0x0C) + b"\x00\x00"

    def f1(cmd, status):
        return bytes([0xF1, 0, cmd, 0, 0, status]) + bytes(10)

    def pkt(ch, body, seq=0, count=None, buf_len=None):
        n = len(body) + 4 if count is None else count
        b = struct.pack("<H", n) + bytes([ch, seq]) + body
        if buf_len is not None:
            b = b[:buf_len]
        return b

    seqs = {
        "rv_with_base_timestamp": [pkt(3, fb + rv([100, -200, 300, 16000]))],
        "fb_la_rv": [pkt(3, fb + la([256, -512, 1]) + rv([1, 2, 3, 4]))],
        "duplicate_rv_lifo": [pkt(3, fb + rv([11, 12, 13, 14]) + rv([21, 22, 23, 24]))],
        "duplicate_la_lifo": [pkt(3, la([1, 1, 1]) + la([2, 2, 2]) + la([3, 3, 3]))],
        "unprocessable_then_good": [
            pkt(3, fb + rv([31, 32, 33, 34]) + bytes([0x05, 0, 0])),
            pkt(3, fb + rv([41, 42, 43, 44])),
        ],
        "f1_leftover_scenario17": [
            pkt(3, fb + rv([51, 52, 53, 54]) + la([5, 6, 7]) + bytes([0x05])),
            pkt(3, rv([61, 62, 63, 64])),
        ],
        "unknown_id_then_good": [
            pkt(3, fb + la([9, 9, 9]) + bytes([0x07]) + bytes(9)),
            pkt(3, la([8, 8, 8])),
        ],
        "unknown_control_id": [pkt(2, bytes([0xF9, 0x00]))],
        "advertisement_ch0": [pkt(0, bytes([0x00, 0x01, 0x04]) + bytes(20))],
        "reset_complete_ch1": [pkt(1, bytes([0x01]))],
        "zero_seed_then_value_then_reseed": [
            pkt(2, fc(0x05)), pkt(2, fc(0x04)),
            pkt(3, fb + rv([71, 72, 73, 74]) + la([75, 76, 77])),
            pkt(2, fc(0x05)),
        ],
        "fc_for_other_ids": [pkt(2, fc(0x1E) + fc(0x13) + fc(0x14) + fc(0x11) + fc(0x99))],
        "product_id": [pkt(2, f8())],
        "command_responses": [pkt(2, f1(7, 0)), pkt(2, f1(6, 0)), pkt(2, f1(6, 1)), pkt(2, f1(9, 1))],
        "rebase_then_rv": [pkt(3, fa + rv([-1, -2, -3, -4]))],
        "empty_packet": [pkt(3, b"")],
        "other_sensors": [
            pkt(3, sensor(0x01, 1, 0, 0, [1, -2, 3])),
            pkt(3, sensor(0x02, 1, 0, 0, [512, -512, 7])),
            pkt(3, sensor(0x03, 1, 2, 0, [16, -16, 33])),
            pkt(3, sensor(0x06, 1, 0, 0, [2509, 0, -2509])),
            pkt(3, sensor(0x08, 1, 0, 0, [1, 2, 3, 4])),
            pkt(3, sensor(0x09, 1, 0, 0, [4096, 0, 0, 0], le16(5))),
            pkt(3, sensor(0x11, 1, 0, 0, [0, 0, 1234], le16(0))),
            pkt(3, sensor(0x19, 1, 0, 0, [0x0001])),
            pkt(3, sensor(0x13, 1, 0, 0, [0x0002])),
            pkt(3, sensor(0x1E, 1, 0, 0, [0x0304]) + bytes(range(1, 11))),
            pkt(3, sensor(0x14, 1, 0, 0, [65535, 1, 32768], bytes(6), unsigned=True)),
            pkt(3, sensor(0x15, 1, 0, 0, [1, 2, 3], bytes(6), unsigned=True)),
            pkt(3, sensor(0x16, 1, 0, 0, [4, 5, 6], bytes(6), unsigned=True)),
        ],
        "stability_bad_index": [pkt(3, sensor(0x13, 1, 0, 0, [0x0005]))],
        "activity_bad_index": [pkt(3, sensor(0x1E, 1, 0, 0, [0x0901]) + bytes(10))],
        "process_error_mid_pop": [
            pkt(3, rv([81, 82, 83, 84]) + sensor(0x13, 1, 0, 0, [0x0009]) + la([85, 86, 87])),
            pkt(3, la([91, 92, 93])),
        ],
        "truncated_data_index": [pkt(3, rv([1, 2, 3, 4]), count=40)],
        "truncated_slice_struct": [pkt(3, fb + rv([1, 2, 3, 4]), count=4 + 5 + 14, buf_len=4 + 5 + 9)],
        "truncated_slice_accuracy": [pkt(3, la([1, 2, 3]), count=14, buf_len=6)],
        "truncated_fc": [pkt(2, fc(0x05), count=21, buf_len=12)],
        "truncated_f8": [pkt(2, f8(), count=20, buf_len=14)],
        "f8_zero_part": [pkt(2, f8(0))],
    }
    batches = []
    for name, packets in seqs.items():
        bno, _f = make([])
        steps = []
        for buf in packets:
            p = B.Packet(bytearray(buf))
            cap = io.StringIO()
            exc = None
            with contextlib.redirect_stdout(cap):
                try:
                    bno._handle_packet(p)
                except Exception as e:  # noqa: BLE001
                    exc = exc_pair(e)
            steps.append({
                "packet": hx(buf),
                "exc": exc,
                "dumped": cap.getvalue() != "",
                "readings": {str(k): reading_entry(v) for k, v in sorted(bno._readings.items())},
                "pending": [hx(s[1]) for s in bno._packet_slices],
            })
        batches.append({"name": name, "steps": steps})
    out["batches"] = batches

    decode = []
    for rid, words in ((0x05, [1, -1, 16384, -32768]), (0x04, [256, -256, 32767]),
                       (0x05, [0, 0, 0, 0]), (0x04, [-1, 1, 0])):
        body = sensor(rid, 1, 3, 0, words, le16(0) if rid == 0x05 else b"")
        vals, acc = B._parse_sensor_report_data(bytearray(body))
        decode.append([hx(body), [repr(v) for v in vals], acc])
    out["decode"] = decode
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--adafruit", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    B, make, meta = build_stack(os.path.abspath(args.adafruit))
    os.makedirs(args.out, exist_ok=True)
    for name, fn in (("float_repr", lambda: gen_float_repr(B, meta)),
                     ("py_text", lambda: gen_py_text(B, meta)),
                     ("shtp", lambda: gen_shtp(B, make, meta)),
                     ("sh2", lambda: gen_sh2(B, make, meta))):
        doc = fn()
        path = os.path.join(args.out, name + ".json")
        with open(path, "w") as f:
            json.dump(doc, f, indent=1, sort_keys=False)
            f.write("\n")
        print(path, os.path.getsize(path))


if __name__ == "__main__":
    main()
