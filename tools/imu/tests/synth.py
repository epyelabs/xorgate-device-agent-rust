"""Synthetic imutx transcripts in the Adafruit driver's I/O pattern.

Every value here is invented (no sensor was involved). The op pattern follows
bno08x 1.3.3: probe (zero-length write), sends as one write, and per packet a
`_data_ready` header read, a `_read_packet` header read and one full read,
each preceded by `ioctl(I2C_SLAVE)`.
"""
import struct

I2C_SLAVE = 0x0703


def rv_report(seq, words, acc, delay=0x21, status=0x03):
    return bytes([0x05, seq, status, delay]) + struct.pack("<4h", *words) + struct.pack("<h", acc)


def la_report(seq, words, delay=0x22, status=0x03):
    return bytes([0x04, seq, status, delay]) + struct.pack("<3h", *words)


def base_ts(delta=0x00000010):
    return bytes([0xFB]) + struct.pack("<I", delta)


def header(count, channel, seq):
    return struct.pack("<H", count) + bytes([channel, seq])


class Tx:
    def __init__(self):
        self.recs = [{"op": "meta", "format": "imutx/1", "device": "/dev/i2c-1", "clock": "monotonic"}]
        self.t = 0
        self.seq = [0] * 6

    def _op(self, rec):
        rec.setdefault("errno", None)
        rec["t_us"] = self.t
        rec["dur_us"] = 30
        self.t += 100
        self.recs.append(rec)
        return len(self.ops()) - 1  # the op index

    def ops(self):
        return [r for r in self.recs if r["op"] not in ("meta", "stdout")]

    def open(self):
        return self._op({"op": "open", "path": "/dev/i2c-1", "flags": "O_RDWR|O_CLOEXEC", "ret": 3})

    def ioctl(self):
        return self._op({"op": "ioctl", "req": I2C_SLAVE, "arg": 0x4A, "ret": 0})

    def write(self, data, ret=None, errno=None):
        self.ioctl()
        return self._op({"op": "write", "len": len(data), "hex": data.hex(), "ret": len(data) if ret is None else ret,
                         "errno": errno})

    def read(self, n, data=None, errno=None):
        self.ioctl()
        if errno:
            return self._op({"op": "read", "len": n, "hex": "", "ret": -1, "errno": errno})
        return self._op({"op": "read", "len": n, "hex": data.hex(), "ret": len(data)})

    def probe(self):
        self.open()
        self.write(b"")

    def send(self, channel, payload):
        self.write(header(len(payload) + 4, channel, self.seq[channel]) + payload)
        self.seq[channel] = (self.seq[channel] + 1) % 256

    def packet(self, channel, payload, full_header=None, read_len=None):
        """H1 + H2 + full read. Returns the full read's op index."""
        h = header(len(payload) + 4, channel, 7)
        self.read(4, h)
        self.read(4, h)
        body = (full_header or h) + payload
        if read_len is not None:
            body = body[:read_len]
        return self.read(len(body), body)

    def idle(self):
        self.read(4, header(0, 0, 0))

    def stdout(self, line):
        self.recs.append({"op": "stdout", "line": line, "t_us": self.t})


def basic():
    """Init-ish traffic + three input batches. Returns (Tx, full-read indices)."""
    tx = Tx()
    tx.probe()
    tx.send(1, bytes([0x01]))  # soft reset
    tx.packet(0, bytes([0x00, 0x01, 0x02, 0x03]))  # an advertisement-like payload (KeyError(0))
    tx.packet(2, bytes([0xF8]) + bytes(range(1, 16)))  # product id response
    tx.stdout('{"type": "ready", "address": "0x4a"}')
    fulls = []
    for n in range(3):
        payload = (
            base_ts(0x10 + n)
            + rv_report(n, (1000 + n, -2000, 3000, 12000 - n), 0x0123)
            + la_report(n, (5 + n, -7, 9))
        )
        fulls.append(tx.packet(3, payload))
        tx.idle()
        tx.stdout('{"type": "sample", "quat": {"r": 0.5, "i": 0.0, "j": 0.0, "k": 0.0}, '
                  '"linaccel": {"x": 0.0, "y": 0.0, "z": 0.0}}')
    return tx, fulls
