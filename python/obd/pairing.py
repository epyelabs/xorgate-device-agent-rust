"""Bluetooth bonding via bluetoothctl driven over a pipe (Linux/BlueZ only).

Rules: the Pi is never discoverable or pairable. Registering an agent makes
bluetoothctl flip Pairable=yes, so it is forced off right after, and again at
the end. BlueZ drops unbonded devices from its cache ~30 s after a scan stops,
so discovery runs while pairing. Only the MAC given by the operator is paired.
"""
import re
import shutil
import subprocess
import threading
import time

_ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
_MAC = re.compile(r"^(?:[0-9A-F]{2}:){5}[0-9A-F]{2}$")


def strip_ansi(s):
    return _ANSI.sub("", s)


def valid_mac(mac):
    return bool(_MAC.match(str(mac).upper()))


def _btctl(cmds, wait=1.0, total=15.0):
    p = subprocess.Popen(["bluetoothctl"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, text=True)
    out = []

    def reader():
        for line in p.stdout:
            out.append(line)

    th = threading.Thread(target=reader, daemon=True)
    th.start()
    try:
        for c in cmds:
            p.stdin.write(c + "\n")
            p.stdin.flush()
            time.sleep(wait)
        p.stdin.write("quit\n")
        p.stdin.flush()
    except (BrokenPipeError, OSError):
        pass
    try:
        p.wait(timeout=total)
    except subprocess.TimeoutExpired:
        p.kill()
    th.join(timeout=2)
    return strip_ansi("".join(out))


def have_bluetoothctl():
    return shutil.which("bluetoothctl") is not None


def controller_present():
    if not have_bluetoothctl():
        return False
    try:
        out = subprocess.run(["bluetoothctl", "list"], capture_output=True, text=True,
                             timeout=10).stdout
    except (OSError, subprocess.SubprocessError):
        return False
    return "Controller" in strip_ansi(out)


def is_bonded(mac):
    try:
        return "Paired: yes" in _btctl(["info %s" % mac], wait=1.5)
    except OSError:
        return False


def pair(mac, pin, log=None):
    """Pair `mac` with a KeyboardOnly agent; return (ok, transcript)."""
    log = log or (lambda *_: None)
    p = subprocess.Popen(["bluetoothctl"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, text=True)
    out = []
    done = threading.Event()

    def write(line):
        try:
            p.stdin.write(line + "\n")
            p.stdin.flush()
        except (BrokenPipeError, OSError):
            pass

    def reader():
        for line in p.stdout:
            line = strip_ansi(line)
            out.append(line)
            low = line.lower()
            if "enter pin code" in low or "enter passkey" in low:
                write(pin)
            elif "confirm passkey" in low or "authorize service" in low or "accept pairing" in low:
                write("yes")
            elif ("pairing successful" in low or "failed to pair" in low
                  or "already exists" in low or "authenticationfailed" in low
                  or "authentication failed" in low):
                done.set()

    th = threading.Thread(target=reader, daemon=True)
    th.start()
    for c in ("agent KeyboardOnly", "default-agent", "pairable off", "discoverable off", "scan on"):
        write(c)
        time.sleep(0.8)
    t0 = time.time()
    while time.time() - t0 < 30:
        if any(mac in ln and "Device" in ln for ln in out):
            break
        time.sleep(0.5)
    else:
        log("pair: %s not seen during 30 s scan" % mac)
    time.sleep(1.5)
    write("pair %s" % mac)
    done.wait(timeout=45)
    time.sleep(1)
    for c in ("trust %s" % mac, "scan off", "pairable off", "discoverable off", "quit"):
        write(c)
        time.sleep(0.8)
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    transcript = "".join(out)
    ok = "pairing successful" in transcript.lower() or "already exists" in transcript.lower()
    if not ok:
        ok = is_bonded(mac)
    # belt and braces: the Pi must never stay pairable/discoverable
    _btctl(["pairable off", "discoverable off"], wait=0.5, total=5)
    return ok, transcript
