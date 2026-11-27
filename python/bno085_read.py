#!/usr/bin/env python3
"""Stream BNO08x (BNO085/BNO086) motion data as line-delimited JSON for the
Xorgate device agent.

The device agent spawns this and reads stdout. Each line is one JSON object: a
"ready" marker, a "sample", or an "error". I2C runs at 100 kHz to avoid the
BNO08x's clock-stretching issues on the Pi (matches the verified test script).
Requires `adafruit-circuitpython-bno08x` + `adafruit-blinka`.

The "ready" line carries the chip's self-reported identity when the driver
read it (the first product-ID entry, the main firmware): "part", "version",
"build". It comes from the product-ID response the driver reads anyway; no
extra bus traffic.
"""
import argparse
import json
import os
import sys
import tempfile
import time


def emit(obj):
    print(json.dumps(obj), flush=True)


def parse_address(value):
    return int(value, 16) if str(value).lower().startswith("0x") else int(value)


# The driver internals the SHTP fix below overrides or reads. They are the same
# in every adafruit-circuitpython-bno08x release from 1.0.0 to 1.3.3. When one
# is missing (a later release), the stock driver is used and stderr says so.
SHTP_FIX_NEEDS = ("_send_packet", "_handle_packet", "_parse_sensor_id", "_get_data")


def shtp_fixed(base):
    """`base` (BNO08X_I2C) with two SHTP fixes the BNO086 needs, harmless on a
    BNO085, or None when `base` lacks an internal they rely on.

    1. The host keeps its OWN per-channel write sequence numbers. The library
       overwrites them with the hub's read sequence numbers, so the host's
       channel-2 writes jump (e.g. 1 -> 4 -> 10). A BNO086 answers every such
       jump with an SHTP error list on channel 0 (`01 0e`); a BNO085 does not.
    2. Packets on channel 0 (SHTP command) and 1 (executable) are never sensor
       reports: they are not handed to the report parser, which would read the
       error list `01 0e` as a truncated accelerometer report and raise
       RuntimeError('Unprocessable Batch bytes', 2).
    It also keeps the main firmware's identity from the product-ID response.
    """
    missing = [n for n in SHTP_FIX_NEEDS if not callable(getattr(base, n, None))]
    if missing:
        return None

    class ShtpI2C(base):
        def __init__(self, *a, **kw):
            self._tx_seq = [0] * 6  # set before super().__init__: it already writes
            self._identity = None
            super().__init__(*a, **kw)

        def _send_packet(self, channel, data):
            seqs = getattr(self, "_sequence_number", None)
            if isinstance(seqs, list) and channel < len(seqs):
                seqs[channel] = self._tx_seq[channel]
            result = super()._send_packet(channel, data)
            self._tx_seq[channel] = (self._tx_seq[channel] + 1) % 256
            return result

        def _handle_packet(self, packet):
            if packet.channel_number in (0, 1):
                return
            super()._handle_packet(packet)

        def _parse_sensor_id(self):
            part = super()._parse_sensor_id()
            if part:
                self._identity = (part, "%d.%d.%d" % (self._get_data(2, "<B"),
                                                      self._get_data(3, "<B"),
                                                      self._get_data(12, "<H")),
                                  self._get_data(8, "<I"))
            return part

    return ShtpI2C


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--address", default="0x4a", help="I2C address, e.g. 0x4a or 0x4b")
    ap.add_argument("--interval", type=float, default=0.05, help="seconds between samples")
    args = ap.parse_args()
    addr = parse_address(args.address)

    # Adafruit Blinka's GPIO backend (lgpio on Pi 5) creates `.lgd-nfy*` notify
    # files in the current working directory during `import board`. When launched
    # from a non-writable CWD (e.g. a systemd service's default "/"), that import
    # fails. chdir to a writable temp dir first so the reader works regardless of
    # how it was started.
    try:
        os.chdir(tempfile.gettempdir())
    except OSError:
        pass

    try:
        import board
        import busio
        from adafruit_bno08x import (
            BNO_REPORT_ROTATION_VECTOR,
            BNO_REPORT_LINEAR_ACCELERATION,
        )
        from adafruit_bno08x.i2c import BNO08X_I2C
    except Exception as exc:  # noqa: BLE001
        emit({"type": "error", "error": f"import failed: {exc}. "
              "Install adafruit-circuitpython-bno08x and adafruit-blinka."})
        return 2

    driver = shtp_fixed(BNO08X_I2C)
    if driver is None:
        print("adafruit_bno08x lacks the internals of the SHTP sequence fix; "
              "using the stock driver (a BNO086 may fail to start)", file=sys.stderr, flush=True)
        driver = BNO08X_I2C

    try:
        i2c = busio.I2C(board.SCL, board.SDA, frequency=100000)
        bno = driver(i2c, address=addr)
        bno.enable_feature(BNO_REPORT_ROTATION_VECTOR)
        bno.enable_feature(BNO_REPORT_LINEAR_ACCELERATION)
    except Exception as exc:  # noqa: BLE001
        emit({"type": "error", "error": f"init failed at {hex(addr)}: {exc}"})
        return 3

    ready = {"type": "ready", "address": hex(addr)}
    identity = getattr(bno, "_identity", None)
    if identity:
        ready["part"], ready["version"], ready["build"] = identity
    emit(ready)

    # The BNO08x over I2C throws transient RuntimeErrors (clock stretching,
    # "unprocessable batch" SHTP frames), and i2cdetect probes on the same bus
    # can bump a read. A single bad read must NOT end the stream: the agent would
    # null its cached sample, re-detect, and respawn us (~2s), so the monitor
    # flashes "no samples yet" each time. Skip the bad sample and keep going;
    # only give up after a sustained run of failures, which means the sensor is
    # actually gone (unplugged / wedged bus) rather than a one-off glitch.
    max_consecutive_errors = int(2.0 / args.interval) if args.interval > 0 else 40
    consecutive_errors = 0

    try:
        while True:
            try:
                # Adafruit returns the quaternion as (i, j, k, real).
                qi, qj, qk, qr = bno.quaternion
                ax, ay, az = bno.linear_acceleration
            except Exception as exc:  # noqa: BLE001
                consecutive_errors += 1
                if consecutive_errors >= max_consecutive_errors:
                    emit({"type": "error",
                          "error": f"read failed {consecutive_errors}x: {exc}"})
                    return 4
                time.sleep(args.interval)
                continue

            consecutive_errors = 0
            emit({
                "type": "sample",
                "quat": {"r": qr, "i": qi, "j": qj, "k": qk},
                "linaccel": {"x": ax, "y": ay, "z": az},
            })
            time.sleep(args.interval)
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    sys.exit(main())
