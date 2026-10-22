"""Timed Bluetooth discovery via bluetoothctl (Linux/BlueZ only): the
`obd_scan` command and `xorgate obd scan` (obd2-bluetooth-integration P3).

Extracted from the scan half of pairing.pair(). Rules, unchanged: never pair,
never trust a name (the operator picks the MAC), and the Pi is never
discoverable or pairable - both are forced off before and after the scan.
Candidates are what the transcript showed: `[NEW] Device <MAC> <name>` plus
`[CHG] Device <MAC> Name: ...` / `RSSI: -NN` updates. Devices that only ever
show their address as a name (BLE randoms) are dropped by the caller
(normalizeScanCandidates in src/hardware/obd.ts) and by parse_transcript.
"""
import re
import subprocess
import threading
import time

from .pairing import strip_ansi, have_bluetoothctl, _btctl

_MAC_IN_LINE = re.compile(r"Device ((?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2})")
_NEW = re.compile(r"\[NEW\] Device ((?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}) (.+)$")
_NAME = re.compile(r"Device ((?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}) Name: (.+)$")
_RSSI = re.compile(r"Device ((?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}) RSSI: (-?\d+)")
_DEVICES = re.compile(r"^Device ((?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}) (.+)$")


def _mac_like_name(mac, name):
    n = name.strip().upper()
    return n == "" or n == mac or n == mac.replace(":", "-")


def parse_transcript(text):
    """Pure: bluetoothctl output -> [{mac, name, rssi}], unnamed dropped,
    rssi None when never reported, sorted strongest first (None last).

    A candidate must have been SEEN during the discovery window (a `[NEW]` or
    `[CHG] Device` line): the trailing `devices` listing also prints BlueZ's
    cache, including a bonded adapter that is nowhere near (bench finding),
    so that listing only ever supplies names."""
    names = {}
    rssi = {}
    seen = set()
    for raw in text.splitlines():
        line = strip_ansi(raw).strip()
        m = _NEW.search(line)
        if m:
            mac, name = m.group(1).upper(), m.group(2).strip()
            names.setdefault(mac, name)
            seen.add(mac)
        elif line.startswith("[") and _MAC_IN_LINE.search(line) and "Device" in line:
            seen.add(_MAC_IN_LINE.search(line).group(1).upper())
        m = _NAME.search(line)
        if m:
            names[m.group(1).upper()] = m.group(2).strip()
        m = _RSSI.search(line)
        if m:
            rssi[m.group(1).upper()] = int(m.group(2))
        m = _DEVICES.match(line)
        if m and not line.startswith("["):
            names.setdefault(m.group(1).upper(), m.group(2).strip())
    out = []
    for mac, name in names.items():
        if mac not in seen or _mac_like_name(mac, name):
            continue
        out.append({"mac": mac, "name": name[:64], "rssi": rssi.get(mac)})
    out.sort(key=lambda c: (c["rssi"] is None, -(c["rssi"] or 0), c["mac"]))
    return out


def discover(seconds=25.0, log=None):
    """Run `scan on` for `seconds`, then `devices`, and parse. Returns the
    candidate list; raises RuntimeError when there is no bluetoothctl."""
    log = log or (lambda *_: None)
    if not have_bluetoothctl():
        raise RuntimeError("bluetoothctl not found")
    p = subprocess.Popen(["bluetoothctl"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, text=True)
    out = []

    def write(line):
        try:
            p.stdin.write(line + "\n")
            p.stdin.flush()
        except (BrokenPipeError, OSError):
            pass

    def reader():
        for line in p.stdout:
            out.append(line)

    th = threading.Thread(target=reader, daemon=True)
    th.start()
    for c in ("pairable off", "discoverable off", "scan on"):
        write(c)
        time.sleep(0.5)
    time.sleep(max(1.0, float(seconds)))
    for c in ("scan off", "devices", "pairable off", "discoverable off", "quit"):
        write(c)
        time.sleep(0.8)
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
    th.join(timeout=2)
    # belt and braces, exactly as pairing does
    _btctl(["pairable off", "discoverable off"], wait=0.5, total=5)
    transcript = "".join(out)
    log("scan: %d transcript lines" % len(out))
    return parse_transcript(transcript)
