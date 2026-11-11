"""Phase 7c WP2: the C4 differential scenarios (plan §WP2, rows 1-29).

Each scenario drives the ORACLE's model backend (`oracle.py ModelBackend`):
a scripted SHTP sensor (`Sensor`) behind a fake i2c-dev, plus per-scenario
hooks that inject errors, glitches and signals. The oracle records the whole
op stream, the clock calls and the helper's stdout as an imutx transcript; the
Rust client is then run in lockstep against THAT transcript
(`xorgate-core::imu_lockstep`). Because the bus model is a deterministic
function of the op prefix, replaying its recorded answers to an op-identical
client is the same as running the model, and the first differing op is where
a divergence shows.

All data here is SYNTHETIC: ramps and small integers; no sensor was involved.
Row 15 (self-reset) uses a synthetic advertisement + reset-complete until a
W0 transcript supplies the real post-reset packets (plan WP0b step 5).

The sensor model is deliberately simple (and is NOT what C4 proves; see
`wp2.md` "What C4 does NOT prove"): a FIFO of SHTP frames; a read of fewer
bytes than the head frame PEEKS (returns its prefix; the two 4-byte header
reads), a read of at least the frame's length POPS it (zero-padded); an empty
queue reads as zeros (count 0). A write is one host frame: a soft reset on
channel 1 queues an advertisement (channel 0) and a reset-complete (channel 1);
a product-id request queues a product-id response; a set-feature queues the
get-feature response (`0xFC`) and starts input-report batches every 50 ms of
virtual time. `glitch()` scripts the next read responses verbatim.
"""

import struct
from collections import deque

MS = 1_000_000  # ns
I2C_SLAVE = 0x0703
ADDR = 0x4A
PART = 0x01234567  # synthetic part number

# A synthetic SHTP advertisement payload (channel 0, starts with tag 0x00).
ADVERT = bytes([0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x80, 0x06, 0x31, 0x2E, 0x30, 0x2E, 0x30, 0x00,
                0x02, 0x02, 0x00, 0x01, 0x03, 0x02, 0xFF, 0x7F, 0x04, 0x02, 0x00, 0x01, 0x05, 0x02, 0xFF, 0x7F])


def hdr(count, channel, seq):
    return struct.pack("<H", count) + bytes([channel, seq])


def rv(seq, words, acc=0x0123, status=0x03, delay=0x00):
    return bytes([0x05, seq & 0xFF, status, delay]) + struct.pack("<4h", *words) + struct.pack("<h", acc)


def la(seq, words, status=0x03, delay=0x00):
    return bytes([0x04, seq & 0xFF, status, delay]) + struct.pack("<3h", *words)


def base_ts(delta=0x10):
    return bytes([0xFB]) + struct.pack("<I", delta)


def rebase(delta=0x20):
    return bytes([0xFA]) + struct.pack("<I", delta)


def fc(fid, interval=50_000):
    return bytes([0xFC, fid, 0, 0, 0]) + struct.pack("<I", interval) + bytes(8)


def f8(part=PART):
    return bytes([0xF8, 0x00, 0x03, 0x02]) + struct.pack("<I", part) + struct.pack("<I", 0x1F2) + struct.pack(
        "<H", 0x0011) + bytes(2)


def f1(command, status, seq=0):
    return bytes([0xF1, seq, command, 0, 0, status]) + bytes(10)


def rv_words(b):
    return (37 * b + 11, -(23 * b) - 5, 13 * b + 7, 16000 - 3 * b)


def la_words(b):
    return (5 + b, -7 - b, 9 + 2 * b)


class Sensor:
    def __init__(self, be):
        self.be = be
        self.queue = deque()
        self.glitches = deque()
        self.seq = [0] * 6
        self.rseq = 0
        self.enabled = set()
        self.next_report_ns = None
        self.period_ns = 50 * MS
        self.batches = 0
        self.reports = True  # emit input batches once a feature is enabled
        self.answer_features = {0x05: True, 0x04: True}
        self.answer_id = True
        self.part = PART
        self.on_feature = None  # hook(fid) -> frames queued BEFORE the 0xFC
        self.batch_override = None  # hook(b) -> payload or None

    # -- frames ------------------------------------------------------------
    def frame(self, channel, payload, cont=False):
        count = len(payload) + 4
        if cont:
            count |= 0x8000
        f = hdr(count, channel, self.seq[channel]) + payload
        self.seq[channel] = (self.seq[channel] + 1) % 256
        return f

    def push(self, channel, payload, cont=False):
        self.queue.append(self.frame(channel, payload, cont))

    def glitch(self, *responses):
        """The next len(responses) reads return these verbatim (bytes, or an
        errno name), ahead of the queue."""
        self.glitches.extend(responses)

    def reset(self):
        self.queue.clear()
        self.enabled.clear()
        self.next_report_ns = None
        self.push(0, ADVERT)
        self.push(1, bytes([0x01]))

    def default_batch(self, b):
        p = base_ts(0x10 + b)
        if 0x05 in self.enabled:
            p += rv(self.rseq, rv_words(b))
            self.rseq += 1
        if 0x04 in self.enabled:
            p += la(self.rseq, la_words(b))
            self.rseq += 1
        return p

    def tick(self):
        now = self.be.ns
        if not self.reports or self.next_report_ns is None or now < self.next_report_ns:
            return
        payload = None
        if self.batch_override:
            payload = self.batch_override(self.batches)
        if payload is None:
            payload = self.default_batch(self.batches)
        if payload:
            self.push(3, payload)
        self.batches += 1
        while self.next_report_ns <= now:
            self.next_report_ns += self.period_ns

    # -- bus ---------------------------------------------------------------
    def read(self, n):
        if self.glitches:
            r = self.glitches.popleft()
            return r if isinstance(r, str) else r[:n]
        self.tick()
        if not self.queue:
            return bytes(n)
        head = self.queue[0]
        if n >= len(head):
            self.queue.popleft()
            return head + bytes(n - len(head))
        return head[:n]

    def host_write(self, data):
        if len(data) < 4:
            return
        count = struct.unpack_from("<H", data)[0] & 0x7FFF
        channel = data[2]
        payload = data[4:count]
        if channel == 1 and payload == b"\x01":
            self.reset()
        elif channel == 2 and payload[:1] == b"\xf9":
            if self.answer_id:
                self.push(2, f8(self.part))
        elif channel == 2 and payload[:1] == b"\xfd" and len(payload) >= 2:
            fid = payload[1]
            if self.on_feature:
                self.on_feature(fid)
            if self.answer_features.get(fid, True):
                self.push(2, fc(fid))
                self.enabled.add(fid)
                if self.next_report_ns is None:
                    self.next_report_ns = self.be.ns + self.period_ns


class Scenario:
    name = ""
    row = 0
    title = ""
    interval = 0.05
    stop_after = 3  # SIGINT at the interval sleep after this many samples (None: never)
    expect_exit = {"code": 0}
    expect_in = ()  # stdout lines that must appear
    expect_not_in = ()

    def attach(self, be):
        self.be = be
        self.sensor = Sensor(be)
        self._it = None
        self.setup()

    def setup(self):
        pass

    # -- per-iteration helpers (an iteration = the loop body between two
    # interval sleeps, numbered by the sleeps already taken) --------------
    @property
    def it(self):
        return self.be.interval_sleeps if self.be.ready else None

    def first_of_iteration(self):
        """True once per loop iteration (the first call in it)."""
        it = self.it
        if it is None or it == self._it:
            return False
        self._it = it
        return True

    # -- hooks -------------------------------------------------------------
    def cost(self, be, kind, err):
        if err == "ETIMEDOUT":
            return 1000 * MS  # DesignWare adapter timeout per failed op (review F19)
        return {"open": 100_000, "ioctl": 50_000, "close": 0}.get(kind, 10 * MS)

    def kill_before(self, be, req):
        return None

    def sigint_at_sleep(self, be, secs):
        return self.stop_after is not None and secs == self.interval and be.samples >= self.stop_after

    def on_open(self, be, path):
        return None

    def on_ioctl(self, be, req, arg):
        return None

    def on_read(self, be, n):
        return self.sensor.read(n)

    def on_write(self, be, data):
        self.sensor.host_write(data)
        return len(data)

    # -- self-validation: the scenario exercised what it claims ------------
    def check(self, be, exit_, lines):
        problems = []
        if exit_ != self.expect_exit:
            problems.append(f"exit {exit_} != expected {self.expect_exit}")
        for want in self.expect_in:
            if not any(want in ln for ln in lines):
                problems.append(f"missing stdout {want!r}")
        for bad in self.expect_not_in:
            if any(bad in ln for ln in lines):
                problems.append(f"unexpected stdout {bad!r}")
        problems += self.extra_check(be, lines)
        return problems

    def extra_check(self, be, lines):
        return []


def samples_of(lines):
    import json  # noqa: PLC0415

    return [json.loads(ln) for ln in lines if ln.startswith('{"type": "sample"')]


READY = '{"type": "ready", "address": "0x4a"}'
INIT = '{"type": "error", "error": "init failed at 0x4a: '
BYTE_COUNT = "Byte count is 0x7FFF/0xFFFF; Error?"
SEQ_FF = "Sequence number is 0xFF; Error?"


# ---------------------------------------------------------------- rows 1-16
class S01CleanInit(Scenario):
    name = "s01_clean_init"
    row = 1
    title = "clean init, ready, samples"
    stop_after = 4
    expect_in = (READY, '"type": "sample"')


class S02IdSecondAttempt(Scenario):
    name = "s02_id_second_attempt"
    row = 2
    title = "product id read on the 2nd attempt"
    stop_after = 2
    expect_in = (READY,)

    def setup(self):
        self.f9 = 0

    def on_write(self, be, data):
        if len(data) >= 5 and data[2] == 2 and data[4] == 0xF9:
            self.f9 += 1
            if self.f9 == 1:
                return len(data)  # lost: the sensor never answers it
        return super().on_write(be, data)


class S03IdNever(Scenario):
    name = "s03_id_never"
    row = 3
    title = "product id never answered -> exit 3"
    expect_exit = {"code": 3}
    expect_in = (INIT + 'Could not read ID"}',)

    def setup(self):
        self.sensor.answer_id = False


class S04EnableTimeout(Scenario):
    name = "s04_enable_timeout"
    row = 4
    title = "feature 0x05 never acknowledged -> exit 3"
    expect_exit = {"code": 3}
    expect_in = (INIT + "('Was not able to enable feature', 5)\"}",)

    def setup(self):
        self.sensor.answer_features[0x05] = False


class S05ProbeNack(Scenario):
    name = "s05_probe_nack"
    row = 5
    title = "probe: zero-length write AND 1-byte read fail -> exit 3"
    expect_exit = {"code": 3}
    expect_in = (INIT + 'No I2C device at address: 0x4a"}',)

    def on_write(self, be, data):
        return "EREMOTEIO" if not data else super().on_write(be, data)

    def on_read(self, be, n):
        return "ENXIO" if n == 1 else super().on_read(be, n)


class S06SoftResetEio(Scenario):
    name = "s06_softreset_eio"
    row = 6
    title = "EIO on the soft-reset write escapes init -> exit 3"
    expect_exit = {"code": 3}
    expect_in = (INIT + '[Errno 5] Input/output error"}',)

    def on_write(self, be, data):
        return "EIO" if data else super().on_write(be, data)


class S07ZeroFirstSample(Scenario):
    name = "s07_zero_first_sample"
    row = 7
    title = "zero seeds: all-zero, then quat-only, then full samples"
    stop_after = 4
    expect_in = (READY,)

    def setup(self):
        self.sensor.reports = False
        self.sent = set()

    def on_read(self, be, n):
        it = self.it
        if it == 1 and 1 not in self.sent:
            self.sent.add(1)
            self.sensor.push(3, base_ts(1) + rv(0, rv_words(1)))
        elif it is not None and it >= 2 and not self.sensor.reports:
            self.sensor.reports = True
            self.sensor.next_report_ns = be.ns
        return super().on_read(be, n)

    def extra_check(self, be, lines):
        s = samples_of(lines)
        p = []
        if not s or any(v != 0.0 for g in ("quat", "linaccel") for v in s[0][g].values()):
            p.append("first sample is not all zero")
        if len(s) < 3 or s[1]["quat"]["r"] == 0.0 or any(v != 0.0 for v in s[1]["linaccel"].values()):
            p.append("second sample is not quat-only")
        return p


class S08LifoDuplicateRv(Scenario):
    name = "s08_lifo_duplicate_rv"
    row = 8
    title = "two RVs in one batch: the OLDEST wins (LIFO)"
    stop_after = 4

    def setup(self):
        def batch(b):
            if b == 3:
                return base_ts(3) + rv(1, (101, 102, 103, 104)) + rv(2, (201, 202, 203, 204)) + la(3, (1, 2, 3))
            if b > 3:  # later batches carry no RV, so the result stays visible
                return base_ts(b) + la(b, la_words(b))
            return None

        self.sensor.batch_override = batch

    def extra_check(self, be, lines):
        want = 104 / 16384
        return [] if any(s["quat"]["r"] == want for s in samples_of(lines)) else ["oldest RV did not win"]


class S09BaseTimestamp(Scenario):
    name = "s09_base_timestamp"
    row = 9
    title = "FB/FA-prefixed batches, several reports, a continuation-bit header"
    stop_after = 5

    def setup(self):
        s = self.sensor

        def batch(b):
            if b == 2:
                s.queue.append(s.frame(3, base_ts(2) + rebase(5) + rv(9, rv_words(2)) + la(10, la_words(2))
                                       + la(11, la_words(3)), cont=True))
                return b""
            if b >= 1:
                return rebase(b) + base_ts(b) + rv(b, rv_words(b)) + la(b, la_words(b))
            return None

        s.batch_override = batch


class S10Unprocessable(Scenario):
    name = "s10_unprocessable"
    row = 10
    title = "'Unprocessable Batch bytes' mid-stream (#49), then recovery"
    stop_after = 5

    def setup(self):
        def batch(b):
            if b == 3:
                return base_ts(3) + rv(30, rv_words(30)) + la(31, la_words(30)) + bytes([0x04, 0x01, 0x02])
            return None

        self.sensor.batch_override = batch

    def extra_check(self, be, lines):
        return [] if any("Packet ***" in ln for ln in lines) else ["no D80 dump: the batch error did not happen"]


class S11UnknownReportId(Scenario):
    name = "s11_unknown_report_id"
    row = 11
    title = "unknown report id -> KeyError; command responses (clock reads, SAVE_DCD fail)"
    stop_after = 6

    def setup(self):
        s = self.sensor

        def batch(b):
            if b == 3:
                return base_ts(3) + bytes([0x07]) + bytes(9)
            if b == 4:
                s.push(2, f1(7, 0) + f1(6, 0))  # ME_CALIBRATE ok, SAVE_DCD ok: two clock reads
                return None
            if b == 5:
                s.push(2, f1(6, 1))  # SAVE_DCD failed -> RuntimeError
                return None
            return None

        s.batch_override = batch


class S12ChannelGt5(Scenario):
    name = "s12_channel_gt5"
    row = 12
    title = "a header on channel 6 -> 'list assignment index out of range'"
    stop_after = 3

    def on_read(self, be, n):
        if self.it == 1 and self.first_of_iteration():
            self.sensor.glitch(hdr(20, 6, 0), hdr(20, 6, 0))
        return super().on_read(be, n)


class S13FfffHeader(Scenario):
    name = "s13_ffff_header"
    row = 13
    title = "0xFFFF / 0x7FFF headers: the two _data_ready lines, stale sample"
    stop_after = 5
    expect_in = (BYTE_COUNT, SEQ_FF)

    def on_read(self, be, n):
        if self.it in (1, 3) and self.first_of_iteration():
            self.sensor.glitch(bytes([0xFF, 0xFF, 0x03, 0xFF]) if self.it == 1 else bytes([0xFF, 0x7F, 0x03, 0x00]))
        return super().on_read(be, n)

    def extra_check(self, be, lines):
        n = sum(1 for ln in lines if ln == BYTE_COUNT)
        return [] if n == 2 else [f"{n} byte-count lines, expected 2"]


class S14Errors39Then40(Scenario):
    name = "s14_errors_39_then_40"
    row = 14
    title = "39 errors, a success, then 40 errors -> exit 4"
    stop_after = None
    expect_exit = {"code": 4}
    expect_in = ('{"type": "error", "error": "read failed 40x: [Errno 5] Input/output error"}',)
    FAIL = set(range(2, 41)) | set(range(42, 200))

    def on_read(self, be, n):
        if self.it in self.FAIL and self.first_of_iteration():
            return "EIO"
        return super().on_read(be, n)


class S14bErrorsBetweenCachedSamples(Scenario):
    name = "s14b_errors_between_cached"
    row = 14
    title = "45 errors, each followed by a CACHED sample (no new report): the count resets anyway"
    stop_after = 50

    def setup(self):
        self.failed = 0

    def on_read(self, be, n):
        it = self.it
        if it is not None and it >= 3:
            self.sensor.reports = False  # from here on every sample is the cached one
            if it % 2 == 1 and self.failed < 45 and self.first_of_iteration():
                self.failed += 1
                return "EIO"
        return super().on_read(be, n)

    def extra_check(self, be, lines):
        return [] if self.failed == 45 else [f"{self.failed} errors injected"]


class S15SelfResetFrozen(Scenario):
    name = "s15_self_reset_frozen"
    row = 15
    title = "sensor resets itself: KeyError(0), Unprocessable(1), then frozen (SYNTHETIC; W0 pending)"
    stop_after = 7

    def on_read(self, be, n):
        if self.it == 3 and self.first_of_iteration():
            self.sensor.reset()
        return super().on_read(be, n)

    def extra_check(self, be, lines):
        s = samples_of(lines)
        return [] if len(s) >= 6 and s[-1] == s[-2] == s[-3] else ["not frozen"]


class S16SigintAtSleep(Scenario):
    name = "s16_sigint_at_sleep"
    row = 16
    title = "SIGINT at the loop's error-path interval sleep -> exit 0"
    stop_after = None

    def setup(self):
        self.failed = False

    def on_read(self, be, n):
        if self.it == 2 and self.first_of_iteration():
            self.failed = True
            return "EIO"
        return super().on_read(be, n)

    def sigint_at_sleep(self, be, secs):
        return self.failed and secs == self.interval


# ---------------------------------------------------------------- rows 17-29
class S17LeftoverSlices(Scenario):
    name = "s17_leftover_slices"
    row = 17
    title = "FB+05+04+1-byte remainder, then a good 05: the STALE RV wins (F1)"
    stop_after = 5

    def setup(self):
        def batch(b):
            if b == 3:
                return base_ts(3) + rv(50, (51, 52, 53, 54)) + la(51, (5, 6, 7)) + bytes([0x05])
            if b == 4:
                return rv(52, (61, 62, 63, 64))
            if b > 4:  # later batches carry no RV, so the stale RV stays visible
                return base_ts(b) + la(b, la_words(b))
            return None

        self.sensor.batch_override = batch

    def extra_check(self, be, lines):
        return [] if any(s["quat"]["r"] == 54 / 16384 for s in samples_of(lines)) else ["stale RV did not win"]


class S17bEmptyPacketRaisingLeftover(Scenario):
    name = "s17b_empty_packet_raising_leftover"
    row = 17
    title = (
        "a leftover stability report (class 5) from a failed batch raises on an EMPTY packet: "
        "print(packet) raises first and its text wins (review-wp1 F2)"
    )
    expect_exit = {"code": 3}
    expect_in = (INIT + 'bytearray index out of range"}',)

    def setup(self):
        self.f9 = 0

    def on_write(self, be, data):
        s = self.sensor
        if len(data) >= 5 and data[2] == 2 and data[4] == 0xF9:
            self.f9 += 1
            if self.f9 == 1:  # attempt 1: a batch whose leftover raises later; no id answer
                s.push(3, bytes([0x13, 0, 0, 0, 5, 0]) + bytes([0x05]))
                return len(data)
        if len(data) >= 6 and data[2] == 2 and data[4] == 0xFD and data[5] == 0x05:
            s.glitch(hdr(20, 3, 9), hdr(4, 3, 9), hdr(4, 3, 9))  # an empty channel-3 packet
        return super().on_write(be, data)


class S18aHeadersDisagreeInit(Scenario):
    name = "s18a_headers_disagree_init"
    row = 18
    title = "2nd/3rd headers disagree (count, channel, seq) on the product-id packet"
    stop_after = 2

    def on_write(self, be, data):
        if len(data) >= 5 and data[2] == 2 and data[4] == 0xF9:
            third = hdr(26, 2, 0x31) + f8()
            self.sensor.glitch(hdr(20, 2, 3), hdr(20, 3, 4), third)
            return len(data)
        return super().on_write(be, data)


class S18bOver512(Scenario):
    name = "s18b_over_512"
    row = 18
    title = "packets > 512: buffer replaced (600, then 520)"
    stop_after = 5

    def setup(self):
        def batch(b):
            if b in (2, 3):
                n = 42 if b == 2 else 36  # 4+5+42*14 = 597; 4+5+36*14 = 513
                return base_ts(b) + b"".join(rv(i, (b * 100 + i, -i, i, 1000 + i)) for i in range(n))
            return None

        self.sensor.batch_override = batch


class S18cClamp8192(Scenario):
    name = "s18c_clamp_8192"
    row = 18
    title = "2nd header 0xFFFF on channel 3 -> read(32767) clamped to 8192 -> 'index out of range'"
    stop_after = 4

    def on_read(self, be, n):
        if self.it == 1 and self.first_of_iteration():
            self.sensor.glitch(hdr(20, 3, 1), bytes([0xFF, 0xFF, 0x03, 0x05]), bytes(range(256)) * 32)
        return super().on_read(be, n)


class S18dAllOnes(Scenario):
    name = "s18d_all_ones"
    row = 18
    title = "2nd header FF FF FF FF -> channel 255 -> error before any full read"
    stop_after = 3

    def on_read(self, be, n):
        if self.it == 1 and self.first_of_iteration():
            self.sensor.glitch(hdr(20, 3, 1), bytes([0xFF] * 4))
        return super().on_read(be, n)


class S18eStaleTail(Scenario):
    name = "s18e_stale_tail"
    row = 18
    title = "loop: 3rd header claims more than was read -> stale buffer bytes parsed"
    stop_after = 5

    def on_read(self, be, n):
        if self.it == 2 and self.first_of_iteration():
            body = base_ts(7) + la(70, (70, 71, 72))
            self.sensor.glitch(hdr(19, 3, 1), hdr(19, 3, 2), hdr(40, 3, 3) + body)
        return super().on_read(be, n)


class S19Eagain(Scenario):
    name = "s19_eagain"
    row = 19
    title = (
        "EAGAIN: probe write and soft-reset write treated as sent (seq still +1); a loop read -> "
        "TypeError; a short write count is ignored"
    )
    stop_after = 3

    def setup(self):
        self.writes = 0

    def on_write(self, be, data):
        self.writes += 1
        if self.writes in (1, 2):  # the probe, then the first soft reset (never reaches the sensor)
            return "EAGAIN"
        if len(data) >= 6 and data[2] == 2 and data[4] == 0xFD and data[5] == 0x04:
            super().on_write(be, data)
            return 7  # write(2) reported a short count: PureIO ignores it (no retry)
        return super().on_write(be, data)

    def on_read(self, be, n):
        if self.it == 1 and self.first_of_iteration():
            return "EAGAIN"
        return super().on_read(be, n)


class S20aMixedErrors(Scenario):
    name = "s20a_mixed_errors"
    row = 20
    title = "EREMOTEIO/ETIMEDOUT/ENXIO on ioctl and read; the LAST text wins -> exit 4"
    stop_after = None
    expect_exit = {"code": 4}
    expect_in = ('{"type": "error", "error": "read failed 40x: [Errno 110] Connection timed out"}',)

    def setup(self):
        self.mode = None

    PLAN = (("ioctl", "EREMOTEIO"), ("read", "ETIMEDOUT"), ("read", "ENXIO"), ("ioctl", "ETIMEDOUT"))

    def on_ioctl(self, be, req, arg):
        if self.it is not None and self.it >= 2 and self.first_of_iteration():
            kind, err = self.PLAN[(self.it - 2) % 4]
            self.mode = err if kind == "read" else None
            if kind == "ioctl":
                return err
        return None

    def on_read(self, be, n):
        if self.mode:
            err, self.mode = self.mode, None
            return err
        return super().on_read(be, n)


class S20bEnodev(Scenario):
    name = "s20b_enodev"
    row = 20
    title = "bus gone mid-stream: ENODEV on every ioctl -> exit 4"
    stop_after = None
    expect_exit = {"code": 4}
    expect_in = ('{"type": "error", "error": "read failed 40x: [Errno 19] No such device"}',)

    def on_ioctl(self, be, req, arg):
        return "ENODEV" if self.it is not None and self.it >= 2 else None


class S20cWriteErrors(Scenario):
    name = "s20c_write_errors"
    row = 20
    title = "write errors in init: id request EREMOTEIO (retried), set-feature ENXIO -> exit 3"
    expect_exit = {"code": 3}
    expect_in = (INIT + '[Errno 6] No such device or address"}',)

    def setup(self):
        self.f9 = 0

    def on_write(self, be, data):
        if len(data) >= 5 and data[2] == 2 and data[4] == 0xF9:
            self.f9 += 1
            if self.f9 == 1:
                return "EREMOTEIO"
        if len(data) >= 6 and data[2] == 2 and data[4] == 0xFD and data[5] == 0x04:
            return "ENXIO"
        return super().on_write(be, data)


class S21aEbusyProbe(Scenario):
    name = "s21a_ebusy_probe"
    row = 21
    title = "EBUSY on I2C_SLAVE at the probe -> 'No I2C device' (busdevice catches it)"
    expect_exit = {"code": 3}
    expect_in = (INIT + 'No I2C device at address: 0x4a"}',)

    def on_ioctl(self, be, req, arg):
        return "EBUSY"


class S21bEbusyAfterProbe(Scenario):
    name = "s21b_ebusy_after_probe"
    row = 21
    title = "EBUSY on I2C_SLAVE after the probe (soft reset) -> '[Errno 16]' exit 3"
    expect_exit = {"code": 3}
    expect_in = (INIT + '[Errno 16] Device or resource busy"}',)

    def setup(self):
        self.n = 0

    def on_ioctl(self, be, req, arg):
        self.n += 1
        return "EBUSY" if self.n >= 2 else None


class S21cEbusyLoop(Scenario):
    name = "s21c_ebusy_loop"
    row = 21
    title = "EBUSY on I2C_SLAVE mid-stream -> 40x -> exit 4"
    stop_after = None
    expect_exit = {"code": 4}
    expect_in = ('{"type": "error", "error": "read failed 40x: [Errno 16] Device or resource busy"}',)

    def on_ioctl(self, be, req, arg):
        return "EBUSY" if self.it is not None and self.it >= 2 else None


class S22aOpenEnoent(Scenario):
    name = "s22a_open_enoent"
    row = 22
    title = "open ENOENT -> the BCM2712 bus-missing constant -> exit 3"
    expect_exit = {"code": 3}
    expect_in = (INIT + "No Hardware I2C on (scl,sda)=(3, 2)\\nValid I2C ports: ((1, 3, 2), (0, 1, 0), (10, 45, 44))."
                 "\\nMake sure I2C is enabled.\"}",)

    def on_open(self, be, path):
        return "ENOENT"


class S22bOpenEacces(Scenario):
    name = "s22b_open_eacces"
    row = 22
    title = "open EACCES -> '[Errno 13] Permission denied: '/dev/i2c-1'' -> exit 3"
    expect_exit = {"code": 3}
    expect_in = (INIT + "[Errno 13] Permission denied: '/dev/i2c-1'\"}",)

    def on_open(self, be, path):
        return "EACCES"


class S23aProbeZeroWrite(Scenario):
    name = "s23a_probe_zero_write"
    row = 23
    title = "probe: the zero-length write is accepted (a real write op)"
    stop_after = 1


class S23bProbeFallbackRead(Scenario):
    name = "s23b_probe_fallback_read"
    row = 23
    title = "probe: zero-length write EOPNOTSUPP -> 1-byte read OK -> init continues"
    stop_after = 2
    expect_in = (READY,)

    def on_write(self, be, data):
        return "EOPNOTSUPP" if not data else super().on_write(be, data)

    def on_read(self, be, n):
        return b"\x00" if n == 1 else super().on_read(be, n)


class S23cProbeFallbackEagain(Scenario):
    name = "s23c_probe_fallback_eagain"
    row = 23
    title = "probe: write EOPNOTSUPP, fallback read EAGAIN -> TypeError escapes the probe"
    expect_exit = {"code": 3}
    expect_in = (INIT + "'NoneType' object is not subscriptable\"}",)

    def on_write(self, be, data):
        return "EOPNOTSUPP" if not data else super().on_write(be, data)

    def on_read(self, be, n):
        return "EAGAIN" if n == 1 else super().on_read(be, n)


class S23dProbeFallbackEmpty(Scenario):
    name = "s23d_probe_fallback_empty"
    row = 23
    title = "probe: write EOPNOTSUPP, fallback read returns 0 bytes -> 'index out of range'"
    expect_exit = {"code": 3}
    expect_in = (INIT + 'index out of range"}',)

    def on_write(self, be, data):
        return "EOPNOTSUPP" if not data else super().on_write(be, data)

    def on_read(self, be, n):
        return b"" if n == 1 else super().on_read(be, n)


class S24aCount0And1to4(Scenario):
    name = "s24a_count0_and_short"
    row = 24
    title = "loop: 2nd header count 0 -> PacketError -> continue; count 3 -> short read, empty packet"
    stop_after = 5

    def on_read(self, be, n):
        if self.it == 1 and self.first_of_iteration():
            self.sensor.glitch(hdr(20, 3, 1), hdr(0, 3, 2))
        elif self.it == 2 and self.first_of_iteration():
            self.sensor.glitch(hdr(20, 3, 1), hdr(3, 3, 2), hdr(3, 3, 2))
        return super().on_read(be, n)


class S24bCount0Init(Scenario):
    name = "s24b_count0_init"
    row = 24
    title = "init: count 0 in _wait_for_packet -> PacketError propagates -> attempt retried"
    stop_after = 2
    expect_in = (READY,)

    def setup(self):
        self.f9 = 0

    def on_write(self, be, data):
        if len(data) >= 5 and data[2] == 2 and data[4] == 0xF9:
            self.f9 += 1
            if self.f9 == 1:
                self.sensor.glitch(hdr(20, 2, 1), hdr(0, 2, 2))
                return len(data)
        return super().on_write(be, data)


class S25SeqWrap(Scenario):
    name = "s25_seq_wrap"
    row = 25
    title = "sequence 0xFE/0xFF received on channel 2 -> sends use them and wrap 255 -> 0"
    stop_after = 3

    def setup(self):
        self.sensor.seq[2] = 0xFE
        self.sensor.seq[3] = 0xFD

    def extra_check(self, be, lines):
        sends = [bytes.fromhex(r["hex"]) for r in be.rec.records if r["op"] == "write" and r.get("len", 0) >= 4]
        seqs = [s[3] for s in sends if s[2] == 2]
        return [] if 0xFF in seqs else [f"no 0xFF send on channel 2 ({seqs})"]


class S26Queued12(Scenario):
    name = "s26_queued_12"
    row = 26
    title = ">= 12 packets queued during an enable: 11 handled per call"
    stop_after = 3

    def setup(self):
        s = self.sensor

        def on_feature(fid):
            if fid == 0x05:
                for i in range(12):
                    s.push(3, base_ts(0x40 + i))

        s.on_feature = on_feature


class S27ZeroPartNumber(Scenario):
    name = "s27_zero_part_number"
    row = 27
    title = "_check_id: part number 0 -> waits out the 5 s budget on other traffic -> retry"
    stop_after = 2
    expect_in = (READY,)

    def setup(self):
        s = self.sensor
        self.f9 = 0
        s.part = 0

    def on_write(self, be, data):
        s = self.sensor
        if len(data) >= 5 and data[2] == 2 and data[4] == 0xF9:
            self.f9 += 1
            if self.f9 == 1:  # zero part number, then a stream of channel-3 batches
                s.enabled = {0x04}
                s.next_report_ns = be.ns + s.period_ns
            else:
                s.part = PART
        return super().on_write(be, data)


class S27bCheckIdSkipsChannels01(Scenario):
    name = "s27b_check_id_skips_ch0_ch1"
    row = 27
    title = "_check_id: channel 0/1 packets before the product id are skipped, not handled"
    stop_after = 2
    expect_in = (READY,)

    def on_write(self, be, data):
        if len(data) >= 5 and data[2] == 2 and data[4] == 0xF9:
            self.sensor.push(0, ADVERT)  # would raise KeyError(0) if handled
            self.sensor.push(1, bytes([0x01]))  # would raise ('Unprocessable Batch bytes', 1)
        return super().on_write(be, data)


class S28UnsolicitedFc(Scenario):
    name = "s28_unsolicited_fc"
    row = 28
    title = "an unsolicited 0xFC mid-stream re-zeros linaccel"
    stop_after = 5

    def setup(self):
        s = self.sensor

        def batch(b):
            if b == 3:
                p = s.default_batch(b)
                s.push(3, p)
                s.push(2, fc(0x04))
                return b""
            if b > 3:  # later batches carry no LA, so the re-zeroed LA stays visible
                return base_ts(b) + rv(b, rv_words(b))
            return None

        s.batch_override = batch

    def extra_check(self, be, lines):
        return [] if any(all(v == 0.0 for v in x["linaccel"].values()) and x["quat"]["r"] != 0.0
                         for x in samples_of(lines)) else ["no re-zeroed linaccel sample"]


class S29SigtermMidLoop(Scenario):
    name = "s29_sigterm_midloop"
    row = 29
    title = "SIGTERM mid-loop: death by signal; the buffered _data_ready lines are LOST"
    stop_after = None
    expect_exit = {"signal": "SIGTERM"}
    expect_not_in = (BYTE_COUNT,)

    def setup(self):
        self.armed = False

    def on_read(self, be, n):
        if self.it == 3 and self.first_of_iteration():
            self.armed = True
            self.sensor.glitch(bytes([0xFF, 0xFF, 0x03, 0xFF]))
        return super().on_read(be, n)

    def kill_before(self, be, req):
        # the ioctl of the 2nd drain's header read, right after the glitched one
        return 15 if self.armed and req["op"] == "ioctl" else None


ALL = {cls.name: cls for cls in (
    S01CleanInit, S02IdSecondAttempt, S03IdNever, S04EnableTimeout, S05ProbeNack, S06SoftResetEio,
    S07ZeroFirstSample, S08LifoDuplicateRv, S09BaseTimestamp, S10Unprocessable, S11UnknownReportId,
    S12ChannelGt5, S13FfffHeader, S14Errors39Then40, S14bErrorsBetweenCachedSamples, S15SelfResetFrozen,
    S16SigintAtSleep,
    S17LeftoverSlices, S17bEmptyPacketRaisingLeftover, S18aHeadersDisagreeInit, S18bOver512, S18cClamp8192,
    S18dAllOnes, S18eStaleTail,
    S19Eagain, S20aMixedErrors, S20bEnodev, S20cWriteErrors, S21aEbusyProbe, S21bEbusyAfterProbe,
    S21cEbusyLoop, S22aOpenEnoent, S22bOpenEacces, S23aProbeZeroWrite, S23bProbeFallbackRead,
    S23cProbeFallbackEagain, S23dProbeFallbackEmpty, S24aCount0And1to4, S24bCount0Init, S25SeqWrap,
    S26Queued12, S27ZeroPartNumber, S27bCheckIdSkipsChannels01, S28UnsolicitedFc, S29SigtermMidLoop,
)}
