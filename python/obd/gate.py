"""The ONE gate. Nothing reaches the vehicle except through ``Gate.send``.

Responsibilities (integration plan section 7):
- allowlist check BEFORE writing; a rejection is a software bug (Forbidden)
- budgets in code: hard cap 20 req/s (trailing 1 s window), 50 ms pacing
  floor, per-session byte cap; exceeding = BudgetExceeded, never a slow-down
- every request/response written to the audit log before the next request
- stop strings (BUS ERROR, CAN ERROR, ...) and 3x STOPPED raise StopCondition
- counters + distinct command inventory for the cloud report
"""
import re
import time
from collections import deque

from .allowlist import is_allowed, normalize
from .transport import TransportError

STOP_STRINGS = ("BUS ERROR", "CAN ERROR", "BUFFER FULL", "LV RESET",
                "DATA ERROR", "BUS BUSY", "FB ERROR", "RX ERROR", "ERR")
STOPPED_LIMIT = 3

HARD_CAP_REQ_S = 20
MIN_INTERVAL_S = 0.05
SESSION_BYTE_CAP = 64 * 1024 * 1024

COUNTER_KEYS = ("requests", "ok", "no_data", "timeouts", "err_adapter",
                "err_protocol", "err_identity", "err_voltage", "reconnects",
                "forbidden_attempts", "budget_violations", "bytes_tx",
                "voltage_low_events")


class Forbidden(Exception):
    """A non-allowlisted command reached the gate. Software bug: exit 6."""


class BudgetExceeded(Exception):
    """A rate/byte budget was exceeded. Software bug: exit 6."""


class StopCondition(Exception):
    """The adapter reported a stop string (rule 8)."""


class LinkLost(Exception):
    """No transport, or the transport failed mid-request."""


def new_counters():
    return {k: 0 for k in COUNTER_KEYS}


class Gate:
    def __init__(self, audit, counters, inventory, mac="", min_interval=MIN_INTERVAL_S):
        self.audit = audit
        self.counters = counters
        self.inventory = inventory
        self.mac = mac
        self.min_interval = min_interval
        self._t = None
        self.session_id = None
        self.session_bytes = 0
        self._sent = deque()
        self._last_send = 0.0
        self.stopped_count = 0
        self.last_stop = None

    # ---- lifecycle
    @property
    def is_open(self):
        return self._t is not None

    def open(self, transport, session_id):
        self._t = transport
        self.session_id = session_id
        self.session_bytes = 0
        self._sent.clear()
        self.stopped_count = 0
        self.audit.write({"kind": "open", "ts": time.time(), "session": session_id,
                          "mac": self.mac, "transport": transport.describe()}, sync=True)

    def close(self, reset=True, reason="close"):
        if self._t is None:
            return
        if reset:
            try:
                self.send("ATZ", timeout=2.0)
            except Exception:  # noqa: BLE001 - best effort on the way out
                pass
        try:
            self._t.close()
        finally:
            self._t = None
            self.audit.write({"kind": "close", "ts": time.time(), "session": self.session_id,
                              "reason": reason}, sync=True)

    # ---- the only write path
    def send(self, cmd, timeout=2.0, settle=0.0):
        """Send one allowlisted command; return (lines, status).

        status: "ok" | "no_data" | "timeout" | "error:<text>".
        Raises Forbidden / BudgetExceeded (bugs), StopCondition (rule 8),
        LinkLost (transport).
        """
        c = normalize(cmd)
        if not is_allowed(c):
            self.counters["forbidden_attempts"] += 1
            self.audit.write({"kind": "forbidden", "ts": time.time(), "mono": time.monotonic(),
                              "session": self.session_id, "cmd": str(cmd)[:64],
                              "status": "forbidden"}, sync=True)
            raise Forbidden("refusing to send non-allowlisted command %r" % cmd)
        if self._t is None:
            raise LinkLost("no transport open for %s" % c)

        # pacing floor, then the hard cap (trailing 1 s window, strict)
        now = time.monotonic()
        wait = self._last_send + self.min_interval - now
        if wait > 0:
            time.sleep(wait)
            now = time.monotonic()
        while self._sent and now - self._sent[0] >= 1.0:
            self._sent.popleft()
        if len(self._sent) >= HARD_CAP_REQ_S:
            self._budget_violation("rate cap %d req/s exceeded" % HARD_CAP_REQ_S, c)
        payload = (c + "\r").encode("ascii")
        if self.session_bytes + len(payload) > SESSION_BYTE_CAP:
            self._budget_violation("session byte cap exceeded", c)

        t0 = time.monotonic()
        self._sent.append(t0)
        self._last_send = t0
        self.counters["requests"] += 1
        self.counters["bytes_tx"] += len(payload)
        self.session_bytes += len(payload)
        self.inventory[c] = self.inventory.get(c, 0) + 1
        try:
            self._t.write(payload)
            buf = self._read_until_prompt(t0 + timeout)
        except TransportError as exc:
            self._log_io(c, [], t0, timed_out=True, status="error:link")
            raise LinkLost(str(exc)) from exc
        ms = (time.monotonic() - t0) * 1000
        text = buf.decode("ascii", "replace").replace("\x00", "")
        lines = [ln.strip() for ln in re.split(r"[\r\n]+", text)]
        lines = [ln for ln in lines if ln and ln != ">" and ln.replace(" ", "").upper() != c]
        timed_out = b">" not in buf
        status = self._classify(lines, timed_out)
        self._log_io(c, lines, t0, timed_out, status, ms)
        if settle:
            time.sleep(settle)
        self._check_stop(c, lines, status)
        return lines, status

    def _read_until_prompt(self, deadline):
        buf = b""
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return buf
            chunk = self._t.read(min(remaining, 0.25))
            if chunk:
                buf += chunk
                if b">" in chunk:
                    return buf

    def _classify(self, lines, timed_out):
        if timed_out and not lines:
            self.counters["timeouts"] += 1
            return "timeout"
        joined = " ".join(lines).upper()
        compact = joined.replace(" ", "")
        for s in STOP_STRINGS:
            if s in joined:
                self.counters["err_adapter"] += 1
                return "error:%s" % s
        if "STOPPED" in joined:
            self.counters["err_adapter"] += 1
            return "error:STOPPED"
        if "NODATA" in compact or "UNABLETOCONNECT" in compact:
            self.counters["no_data"] += 1
            return "no_data"
        if joined.strip() == "?":
            self.counters["err_adapter"] += 1
            return "error:?"
        if timed_out:
            self.counters["timeouts"] += 1
            return "timeout"
        self.counters["ok"] += 1
        return "ok"

    def _log_io(self, c, lines, t0, timed_out, status, ms=None):
        rec = {"kind": "io", "ts": time.time(), "mono": round(t0, 3),
               "session": self.session_id, "mac": self.mac, "cmd": c,
               "lines": lines, "ms": round(ms, 1) if ms is not None else None,
               "timeout": timed_out, "status": status}
        self.audit.write(rec, sync=(status != "ok"))

    def _check_stop(self, c, lines, status):
        if status.startswith("error:") and status != "error:?" and status != "error:link":
            reason = status[6:]
            if reason == "STOPPED":
                self.stopped_count += 1
                if self.stopped_count < STOPPED_LIMIT:
                    return
                reason = "STOPPED x%d" % self.stopped_count
            self.last_stop = {"ts": time.time(), "reason": reason}
            self.audit.write({"kind": "stop", "ts": time.time(), "session": self.session_id,
                              "reason": reason, "cmd": c, "lines": lines}, sync=True)
            raise StopCondition("adapter reported %s after %s: %s" % (reason, c, lines))
        if status != "error:STOPPED":
            self.stopped_count = 0

    def _budget_violation(self, why, c):
        self.counters["budget_violations"] += 1
        self.audit.write({"kind": "budget", "ts": time.time(), "session": self.session_id,
                          "cmd": c, "reason": why}, sync=True)
        raise BudgetExceeded(why)
