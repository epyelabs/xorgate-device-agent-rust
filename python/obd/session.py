"""Session state machine: connect, handshake, probe, poll, recover.

States (status lines): connecting, pairing, probing, live, ecu_silent,
backoff, stopped, adapter_not_found. link: 0 none, 1 adapter only,
2 ECU answering. Exit codes: 2 no Bluetooth controller, 3 adapter unreachable
after 10 min of backoff, 4 protocol refused, 5 repeated stop conditions,
6 software bug (forbidden/budget). SIGINT/SIGTERM: ATZ, close, exit 0.

Start-stop awareness (2026-10-14 Taos drive, fix/obd-startstop-voltage):
- rpm 0 with the ECU answering is a normal `live` state (engine auto-stop at
  a light). Ignition off is ONLY three consecutive misses once the ECU has
  answered; the RFCOMM link then stays open and ONE probe (010C1) goes out on
  the ECU_SILENT_PROBE_SCHEDULE until the first answer resumes polling. The
  link is torn down only on transport errors, stop strings or a signal.
- Voltage (rule 5) depends on --power-source. `switched` (the socket dies
  with the ignition): never stop, count the episode (`voltage_low_events`),
  log it, refresh the status detail. `constant`: stop only after a SUSTAINED
  low (>= VOLTAGE_SUSTAIN_S and >= VOLTAGE_SUSTAIN_CYCLES consecutive cycles
  below V_STOP) while rpm is 0 or unknown; a single low sample (a crank dip)
  never stops. Stopped = link kept open, status `stopped` link 1, ONE request
  per VOLTAGE_RECHECK_S (PID 42 while the ECU answers, else ATRV, alternating
  back so a returning ECU is seen), resume at >= V_RESUME. On the Taos the
  old rule fired twice in one 24 min drive (11.72 V after 50 s of auto-stop,
  then a 9.82 V crank dip) and each stop cost a 600 s blind window.
"""
import os
import re
import sys
import time
import uuid

from . import HELPER_VERSION
from .allowlist import ALLOWLIST_SHA256
from .decoders import FIELDS, DEFAULT_FIELDS, apply_derived, decode_field, plan_fields
from .gate import Gate, Forbidden, BudgetExceeded, StopCondition, LinkLost, new_counters
from .isotp import isotp_reassemble, supported_bitmap, find_payload
from .transport import TransportError, NoBluetooth, RfcommTransport, SerialTransport

# voltage policy (D6); ATRV reads ~0.85 V below PID 42 on the Vgate, corrected
# only when PID 42 is not available.
V_STOP = 11.8
V_RESUME = 12.4
ATRV_OFFSET_V = 0.85

# Test-only timescale (python/tests): scales every policy interval below so
# the emulator tests run in seconds. Never set on a device; default 1.0.
_TIMESCALE = float(os.environ.get("OBD_TEST_TIMESCALE", "1") or "1")
if not (0.001 <= _TIMESCALE <= 1.0):
    _TIMESCALE = 1.0

# constant power: low for this long AND this many consecutive cycles, with
# rpm 0/unknown, before polling stops
VOLTAGE_SUSTAIN_S = 30.0 * _TIMESCALE
VOLTAGE_SUSTAIN_CYCLES = 3
# stopped on voltage: one request per re-check, link kept open
VOLTAGE_RECHECK_S = 60.0 * _TIMESCALE
# reconnect cap while a voltage hold loses the link (was 600 s)
VOLTAGE_BACKOFF_CAP_S = 300.0 * _TIMESCALE

# ECU silent: (until elapsed s, probe interval s); the last entry is forever
ECU_SILENT_PROBE_SCHEDULE = (
    (600.0 * _TIMESCALE, 10.0 * _TIMESCALE),
    (3600.0 * _TIMESCALE, 60.0 * _TIMESCALE),
    (None, 300.0 * _TIMESCALE),
)
ECU_PROBE_CMD = "010C1"

BACKOFF_CAP_S = 300.0 * _TIMESCALE
STOP_BACKOFF_BASE_S = 60.0 * _TIMESCALE
ADAPTER_UNREACHABLE_S = 600.0
STOP_SESSIONS_LIMIT = 3
ECU_MISS_LIMIT = 3
IDENTITY_EVERY_S = 60.0 * _TIMESCALE
ATRV_EVERY_S = 60.0 * _TIMESCALE
AUDIT_EVERY_S = 60.0 * _TIMESCALE
RETRIES = 3

_VOLT = re.compile(r"(\d+(?:\.\d+)?)\s*V")


def ecu_silent_probe_interval(elapsed_s):
    """Probe cadence while the ECU is silent: 10 s for 10 min, 60 s for the
    rest of the hour, then 300 s. Pure; unit-tested."""
    for until, interval in ECU_SILENT_PROBE_SCHEDULE:
        if until is None or elapsed_s < until:
            return interval
    return ECU_SILENT_PROBE_SCHEDULE[-1][1]


class Shutdown(BaseException):
    """Raised from the signal handler; unwinds to a clean ATZ + close + exit 0."""


class ProtocolRefused(Exception):
    pass


class EcuSilent(Exception):
    pass


class VoltageLow(Exception):
    pass


class IdentityChanged(Exception):
    pass


class HelperExit(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


class Session:
    def __init__(self, args, audit, emit):
        self.args = args
        self.audit = audit
        self.emit = emit
        self.counters = new_counters()
        self.inventory = {}
        self.gate = Gate(audit, self.counters, self.inventory, mac=args.adapter or "")
        self.poll_hz = max(0.05, float(args.poll_hz))
        self.requested = self._parse_fields(args.fields)
        self.power_source = getattr(args, "power_source", "switched") or "switched"
        self.state = None
        self.link = 0
        self.identity = {}
        self.protocol = {}
        self.profile = None
        self.plan = {}
        self.last_audit_emit = time.monotonic()
        self._stop_sessions = 0
        self._connect_fail_since = None
        self._backoff_n = 0
        # voltage episode tracking (both power sources)
        self._low_episode = False      # below V_STOP, not yet back above V_RESUME
        self._low_since = None         # monotonic start of the consecutive-low run
        self._low_cycles = 0
        self._voltage_hold = False     # constant power: polling stopped on voltage
        # per-session poll bookkeeping (survives an ecu_silent / voltage resume)
        self._answered = set()
        self._dropped = set()
        # True once the shutdown close has run: later signals are ignored
        # (the agent SIGINTs us and systemd SIGTERMs the cgroup ~100 ms apart).
        self.closed = False
        self.stop_requested = False

    # ---------------------------------------------------------- utilities
    def _parse_fields(self, csv):
        if not csv:
            return list(DEFAULT_FIELDS)
        names = [n.strip() for n in csv.split(",") if n.strip()]
        unknown = [n for n in names if n not in FIELDS]
        if unknown:
            self.log("ignoring unknown fields: %s" % ",".join(unknown))
        names = [n for n in names if n in FIELDS]
        if "link" not in names:
            names.append("link")
        return names

    @staticmethod
    def log(msg):
        sys.stderr.write("[obd_read] %s\n" % msg)
        sys.stderr.flush()

    def status(self, state, link, detail=""):
        self.state, self.link = state, link
        self.emit({"type": "status", "state": state, "link": link,
                   "detail": detail[:160], "ts": round(time.time(), 3)})

    def emit_audit(self):
        self.last_audit_emit = time.monotonic()
        self.emit({"type": "audit", "ts": round(time.time(), 3),
                   "counters": dict(self.counters), "inventory": dict(self.inventory),
                   "last_stop": self.gate.last_stop})

    def emit_ready(self):
        self.emit({"type": "ready", "helper": "obd_read", "version": HELPER_VERSION,
                   "allowlist_sha256": ALLOWLIST_SHA256})

    def _sleep(self, seconds):
        """Interruptible sleep: SIGTERM raises Shutdown out of time.sleep."""
        end = time.monotonic() + seconds
        while True:
            left = end - time.monotonic()
            if left <= 0:
                return
            time.sleep(min(left, 1.0))
            if time.monotonic() - self.last_audit_emit >= AUDIT_EVERY_S:
                self.emit_audit()

    # ---------------------------------------------------------- main loop
    def run(self):
        """Return the process exit code."""
        try:
            while True:
                try:
                    self._session()
                except (LinkLost, TransportError) as exc:
                    self._after_link_loss(exc)
                except StopCondition as exc:
                    self._after_stop(exc)
                except IdentityChanged as exc:
                    self._after_identity(exc)
        except Shutdown:
            self._close("shutdown")
            self.closed = True
            self.status("stopped", 0, "signal")
            self.emit_audit()
            return 0
        except ProtocolRefused as exc:
            self._fail(4, str(exc))
            return 4
        except (Forbidden, BudgetExceeded) as exc:
            self._fail(6, "software bug: %s" % exc)
            return 6
        except NoBluetooth as exc:
            self._fail(2, str(exc))
            return 2
        except HelperExit as exc:
            self._fail(exc.code, str(exc))
            return exc.code

    def _fail(self, code, message):
        self._close("exit %d" % code)
        self.emit_audit()
        self.emit({"type": "error", "code": code, "error": message})

    def _close(self, reason):
        try:
            self.gate.close(reset=True, reason=reason)
        except Shutdown:
            # a second signal while closing: finish closing without the reset
            try:
                self.gate.close(reset=False, reason=reason)
            except Exception:  # noqa: BLE001
                pass
        except Exception:  # noqa: BLE001
            pass

    # ---------------------------------------------------------- recovery
    def _backoff_delay(self, n, base=1.0, cap=BACKOFF_CAP_S):
        return min(cap, base * (2 ** n))

    def _after_link_loss(self, exc):
        self.counters["reconnects"] += 1
        self._close("link lost")
        cap = VOLTAGE_BACKOFF_CAP_S if self._voltage_hold else BACKOFF_CAP_S
        delay = self._backoff_delay(self._backoff_n, cap=cap)
        self._backoff_n += 1
        self.status("backoff", 0, "link: %s; retry in %.0f s" % (str(exc)[:80], delay))
        self._sleep(delay)

    def _after_connect_failure(self, exc):
        now = time.monotonic()
        if self._connect_fail_since is None:
            self._connect_fail_since = now
        self.counters["reconnects"] += 1
        self.status("adapter_not_found", 0, str(exc)[:120])
        if now - self._connect_fail_since >= ADAPTER_UNREACHABLE_S:
            raise HelperExit(3, "adapter unreachable after %.0f s of backoff: %s"
                             % (now - self._connect_fail_since, exc))
        delay = self._backoff_delay(self._backoff_n)
        self._backoff_n += 1
        self.status("backoff", 0, "adapter unreachable; retry in %.0f s" % delay)
        self._sleep(delay)

    def _after_stop(self, exc):
        self._stop_sessions += 1
        self._close("stop condition")
        if self._stop_sessions >= STOP_SESSIONS_LIMIT:
            raise HelperExit(5, "stop condition in %d consecutive sessions: %s"
                             % (self._stop_sessions, exc))
        delay = self._backoff_delay(self._stop_sessions - 1, base=STOP_BACKOFF_BASE_S)
        self.status("stopped", 0, str(exc)[:120])
        self.status("backoff", 0, "after stop condition; retry in %.0f s" % delay)
        self._sleep(delay)

    def _after_identity(self, exc):
        self.counters["err_identity"] += 1
        self.gate.last_stop = {"ts": time.time(), "reason": "identity: %s" % str(exc)[:80]}
        self._close("identity changed")
        self.status("stopped", 0, str(exc)[:120])
        self.status("backoff", 0, "adapter identity changed; reconnecting in 1 s")
        self._sleep(1.0)

    # ---------------------------------------------------------- one session
    def _session(self):
        transport = self._connect()
        if transport is None:
            return
        self.gate.open(transport, uuid.uuid4().hex[:12])
        self._backoff_n = 0
        self._connect_fail_since = None
        self._answered = set()
        self._dropped = set()
        self._handshake()
        self._wait_for_ecu()
        self._probe()
        self._stop_sessions = 0
        # The link stays open across ECU silence and voltage holds: both
        # return here and polling resumes with the same plan. Transport
        # errors, stop strings and identity changes still unwind to run().
        while True:
            try:
                self._poll_loop()
            except EcuSilent as exc:
                self._ecu_silent_wait(exc)
            except VoltageLow as exc:
                self._voltage_hold_wait(exc)

    def _connect(self):
        if self.args.serial:
            self.status("connecting", 0, "serial %s" % self.args.serial)
            try:
                return SerialTransport(self.args.serial)
            except TransportError as exc:
                self._after_connect_failure(exc)
                return None
        from . import pairing
        mac = self.args.adapter
        self.status("connecting", 0, "rfcomm %s ch %d" % (mac, self.args.channel))
        if not pairing.controller_present():
            raise NoBluetooth("no Bluetooth controller (bluetoothctl list is empty)")
        if not pairing.is_bonded(mac):
            self.status("pairing", 0, "bonding %s" % mac)
            ok, transcript = pairing.pair(mac, self.args.pin, self.log)
            self.audit.write({"kind": "pair", "ts": time.time(), "mac": mac, "ok": ok,
                              "transcript": transcript[-1500:]}, sync=True)
            if not ok:
                self._after_connect_failure(TransportError("pairing %s failed" % mac))
                return None
        try:
            return RfcommTransport(mac, self.args.channel)
        except NoBluetooth:
            raise
        except TransportError as exc:
            self._after_connect_failure(exc)
            return None

    # ---------------------------------------------------------- handshake
    def _expect_ok(self, cmd):
        lines, status = self._send(cmd)
        if status != "ok":
            raise LinkLost("%s answered %s %r" % (cmd, status, lines))
        return lines

    def _send(self, cmd, timeout=2.0, settle=0.0):
        """Gate.send with the retry policy for transport-level timeouts."""
        delay = 0.1
        for attempt in range(RETRIES + 1):
            lines, status = self.gate.send(cmd, timeout=timeout, settle=settle)
            if status != "timeout":
                return lines, status
            if attempt < RETRIES:
                time.sleep(delay)
                delay *= 2
        raise LinkLost("no prompt after %d retries of %s" % (RETRIES, cmd))

    def _read_atrv(self):
        lines, status = self._send("ATRV")
        for ln in lines:
            m = _VOLT.search(ln.upper())
            if m:
                return float(m.group(1))
        return None

    def _read_pid42(self):
        """One PID 42 request: (volts or None, ecu_answered)."""
        lines, status = self._send("01421")
        if status != "ok":
            return None, False
        _, payload = find_payload(isotp_reassemble(lines), "4142")
        if payload is None:
            return None, False
        try:
            return decode_field("module_voltage", "42", payload[4:]), True
        except (ValueError, IndexError):
            return None, True

    def _handshake(self):
        self.status("connecting", 0, "handshake")
        self._send("ATZ", timeout=6.0, settle=1.0)
        self._expect_ok("ATE0")
        self._expect_ok("ATL0")
        self._expect_ok("ATS0")
        self._expect_ok("ATH1")
        ati = " ".join(self._send("ATI")[0]).strip()
        at1 = " ".join(self._send("AT@1")[0]).strip()
        if not ati:
            raise LinkLost("empty ATI")
        self.identity = {"ati": ati, "at1": at1}
        self.last_identity_check = time.monotonic()
        self.status("probing", 1, "adapter %s" % ati)
        self.adapter_voltage = self._read_atrv()
        self.last_atrv = time.monotonic()
        self._voltage_gate(self.adapter_voltage, corrected=True, context="handshake")
        self._expect_ok("ATSP0")

    def _wait_for_ecu(self):
        """Send 0100 until some ECU answers. A silent bus at connect time
        (constant power, ignition off) is `ecu_silent` link 1 with the link
        kept open and the probe schedule, exactly like silence after live."""
        t0 = time.monotonic()
        n = 0
        while True:
            # 15 s: the ATSP0 search tries every protocol (K-line inits included)
            lines, status = self._send("0100", timeout=15.0)
            per = isotp_reassemble(lines)
            responders = sorted(h for h, p in per.items() if p.upper().startswith("4100"))
            if status == "ok" and responders:
                break
            if n == 0:
                detail = "0100 -> %s %r" % (status, lines[-2:])
                self.status("ecu_silent", 1, "%s; probing 0100" % detail[:100])
                self.audit.write({"kind": "ecu_silent", "ts": time.time(),
                                  "session": self.gate.session_id, "detail": detail[:160],
                                  "probe": "0100"}, sync=True)
            n += 1
            elapsed = time.monotonic() - t0
            interval = ecu_silent_probe_interval(elapsed)
            if n % 6 == 0:
                self.status("ecu_silent", 1, "no ECU for %.0f s; probing 0100 every %.0f s"
                            % (elapsed, interval))
            self._sleep(interval)
        if n:
            self.audit.write({"kind": "ecu_resumed", "ts": time.time(),
                              "session": self.gate.session_id, "probes": n}, sync=True)
            self.status("probing", 1, "ECU answering after %d probe(s)" % n)
        dpn = " ".join(self._send("ATDPN")[0]).strip().upper()
        dp = " ".join(self._send("ATDP")[0]).strip()
        self.protocol = {"dpn": dpn, "dp": dp}
        if not dpn or dpn[-1] not in "67":
            self.counters["err_protocol"] += 1
            self.gate.last_stop = {"ts": time.time(), "reason": "protocol %s" % dpn}
            raise ProtocolRefused("protocol %r (%s) is not ISO 15765-4 CAN (ATDPN 6/7)"
                                  % (dpn, dp))
        self._first_0100 = (lines, per, responders)

    # ---------------------------------------------------------- probe
    def _probe(self):
        t0 = time.monotonic()
        n0 = self.counters["requests"]
        first, per, responders = self._first_0100
        pids_by_ecu = {h: set() for h in responders}
        base = 0x00
        current_per = per
        while True:
            more = False
            for hdr, payload in current_per.items():
                if payload.upper().startswith("41%02X" % base):
                    pids = supported_bitmap(payload, base)
                    pids_by_ecu.setdefault(hdr, set()).update(pids)
                    if (base + 0x20) in pids and base + 0x20 <= 0xE0:
                        more = True
            if not more:
                break
            base += 0x20
            lines, status = self._send("01%02X" % base)
            if status != "ok":
                break
            current_per = isotp_reassemble(lines)
        mode09 = {}
        lines, status = self._send("0900")
        if status == "ok":
            for hdr, payload in isotp_reassemble(lines).items():
                if payload.upper().startswith("4900"):
                    mode09[hdr] = sorted(supported_bitmap(payload, 0))
        elapsed = max(1e-3, time.monotonic() - t0)
        rate = (self.counters["requests"] - n0) / elapsed

        supported_pids = set()
        for s in pids_by_ecu.values():
            supported_pids.update(s)
        supported_hex = {"%02X" % p for p in supported_pids}
        supported, unsupported, plan = plan_fields(self.requested, supported_hex)
        # PID 42 feeds the voltage policy even when module_voltage is not requested
        if "42" in supported_hex and "42" not in plan:
            plan["42"] = []
        # constant power: PID 0C feeds the policy too (never stop while the
        # engine runs), one extra request per cycle only when rpm is not polled
        if self.power_source == "constant" and "0C" in supported_hex and "0C" not in plan:
            plan["0C"] = []
        self.plan = plan
        self.supported_fields = supported
        powertrain = "unknown"
        if "0C" in supported_hex:
            powertrain = "hybrid" if ("5B" in supported_hex or "9A" in supported_hex) else "ice"
        elif "5B" in supported_hex or "9A" in supported_hex:
            powertrain = "ev"
        pids_by_ecu_hex = {h: ["%02X" % p for p in sorted(s)] for h, s in pids_by_ecu.items()}
        self.profile = {
            "type": "profile", "ts": round(time.time(), 3),
            "adapter": {"mac": self.args.adapter or self.args.serial or "",
                        "ati": self.identity["ati"], "at1": self.identity["at1"]},
            "protocol": dict(self.protocol),
            "responders": responders,
            "pids_by_ecu": pids_by_ecu_hex,
            "supported_fields": supported,
            "unsupported_fields": unsupported,
            "powertrain": powertrain,
            "rate_req_s": round(rate, 1),
        }
        self.audit.write({"kind": "session", "ts": time.time(), "session": self.gate.session_id,
                          "mac": self.args.adapter or "", "ati": self.identity["ati"],
                          "at1": self.identity["at1"], "dpn": self.protocol["dpn"],
                          "dp": self.protocol["dp"], "mode01_bitmaps": pids_by_ecu_hex,
                          "mode09_bitmaps": mode09, "vin_hash": None,
                          "power_source": self.power_source,
                          "helper_version": HELPER_VERSION,
                          "allowlist_sha256": ALLOWLIST_SHA256}, sync=True)
        self.emit(self.profile)

    # ---------------------------------------------------------- voltage
    def _voltage_gate(self, volts, corrected, context, rpm=None):
        """Rule 5, start-stop aware. Tracks a low EPISODE (below V_STOP until
        back above V_RESUME) on both power sources; raises VoltageLow only on
        constant power after a sustained low with rpm 0/unknown."""
        if volts is None:
            return
        v = volts + (ATRV_OFFSET_V if corrected else 0.0)
        now = time.monotonic()
        if v >= V_STOP:
            self._low_since = None
            self._low_cycles = 0
            if self._low_episode and v >= V_RESUME:
                self._low_episode = False
                self.log("voltage recovered: %.2f V (%s)" % (v, context))
            return
        # below V_STOP
        if not self._low_episode:
            self._low_episode = True
            self.counters["voltage_low_events"] += 1
            self.audit.write({"kind": "voltage_low", "ts": time.time(),
                              "session": self.gate.session_id, "volts": round(v, 2),
                              "source": context, "rpm": rpm,
                              "power_source": self.power_source}, sync=True)
            if self.power_source == "switched":
                self.log("voltage low %.2f V (%s), rpm %s: ignored on switched power (event %d)"
                         % (v, context, rpm, self.counters["voltage_low_events"]))
                if self.state == "live":
                    self.status("live", 2, "voltage low %.2f V (%s) ignored: switched power"
                                % (v, context))
            else:
                self.log("voltage low %.2f V (%s), rpm %s: watching for %.0f s"
                         % (v, context, rpm, VOLTAGE_SUSTAIN_S))
        if self.power_source != "constant":
            return
        if rpm is not None and rpm > 0:
            # alternator running: a low reading is never a reason to stop
            self._low_since = None
            self._low_cycles = 0
            return
        if self._low_since is None:
            self._low_since = now
        self._low_cycles += 1
        if (now - self._low_since >= VOLTAGE_SUSTAIN_S
                and self._low_cycles >= VOLTAGE_SUSTAIN_CYCLES):
            raise VoltageLow("%.2f V (%s) below %.1f V for %.0f s, rpm %s"
                             % (v, context, V_STOP, now - self._low_since,
                                "0" if rpm == 0 else "unknown"))

    def _voltage_hold_wait(self, exc):
        """Constant power, sustained low: stop polling, keep the link, one
        request per VOLTAGE_RECHECK_S until >= V_RESUME."""
        self.counters["err_voltage"] += 1
        self._voltage_hold = True
        self._low_since = None
        self._low_cycles = 0
        self.gate.last_stop = {"ts": time.time(), "reason": "voltage: %s" % str(exc)[:80]}
        self.audit.write({"kind": "voltage_hold", "ts": time.time(),
                          "session": self.gate.session_id, "detail": str(exc)[:160]}, sync=True)
        self.status("stopped", 1, "%s; re-check every %.0f s" % (str(exc)[:100], VOLTAGE_RECHECK_S))
        self.emit_audit()
        ecu_answers = True
        n = 0
        try:
            while True:
                self._sleep(VOLTAGE_RECHECK_S)
                n += 1
                if ecu_answers:
                    v, ecu_answers = self._read_pid42()
                    context = "PID 42"
                else:
                    raw = self._read_atrv()
                    if raw is not None:
                        self.adapter_voltage = raw
                    v = raw + ATRV_OFFSET_V if raw is not None else None
                    context = "ATRV"
                    ecu_answers = True   # try PID 42 again next time
                if v is not None and v >= V_RESUME:
                    self.log("voltage resume: %.2f V (%s) after %d re-check(s)" % (v, context, n))
                    self.audit.write({"kind": "voltage_resume", "ts": time.time(),
                                      "session": self.gate.session_id, "volts": round(v, 2),
                                      "source": context, "rechecks": n}, sync=True)
                    self._low_episode = False
                    return
                self.status("stopped", 1, "voltage %s (%s); re-check every %.0f s"
                            % ("%.2f V" % v if v is not None else "unknown", context,
                               VOLTAGE_RECHECK_S))
        finally:
            self._voltage_hold = False

    # ---------------------------------------------------------- ECU silent
    def _ecu_silent_wait(self, exc):
        """Three misses after the ECU had answered: keep the link, send ONE
        010C1 probe on the schedule, resume on the first answer."""
        t0 = time.monotonic()
        self.status("ecu_silent", 1, "%s; probing %s every %.0f s"
                    % (str(exc)[:70], ECU_PROBE_CMD, ecu_silent_probe_interval(0)))
        self.audit.write({"kind": "ecu_silent", "ts": time.time(),
                          "session": self.gate.session_id, "detail": str(exc)[:160],
                          "probe": ECU_PROBE_CMD}, sync=True)
        n = 0
        while True:
            elapsed = time.monotonic() - t0
            interval = ecu_silent_probe_interval(elapsed)
            self._sleep(interval)
            lines, status = self._send(ECU_PROBE_CMD)
            n += 1
            if status == "ok":
                _, payload = find_payload(isotp_reassemble(lines), "410C")
                if payload is not None:
                    self.log("ECU answering again after %d probe(s), %.0f s" % (n, elapsed))
                    self.audit.write({"kind": "ecu_resumed", "ts": time.time(),
                                      "session": self.gate.session_id, "probes": n,
                                      "silent_s": round(time.monotonic() - t0, 1)}, sync=True)
                    return
            if n % 6 == 0:
                self.status("ecu_silent", 1, "silent %.0f s; probing %s every %.0f s"
                            % (time.monotonic() - t0, ECU_PROBE_CMD, interval))

    # ---------------------------------------------------------- poll loop
    def _poll_loop(self):
        period = 1.0 / self.poll_hz
        answered = self._answered
        dropped = self._dropped
        consecutive_misses = 0
        cycle_budget = 4 * max(1, len(self.plan)) + 8
        self.status("live", 2, "polling %d PIDs at %.2f Hz" % (len(self.plan), self.poll_hz))
        while True:
            cycle_t0 = time.monotonic()
            req0 = self.counters["requests"]
            fields = {}
            module_voltage = None
            policy_rpm = None
            for pid, names in self.plan.items():
                if pid in dropped:
                    continue
                lines, status = self._send("01%s1" % pid)
                value_ok = False
                if status == "ok":
                    _, payload = find_payload(isotp_reassemble(lines), "41" + pid)
                    if payload is not None:
                        data = payload[4:]
                        for name in names:
                            try:
                                fields[name] = decode_field(name, pid, data)
                                value_ok = True
                            except (ValueError, IndexError):
                                pass
                        if pid == "42":
                            try:
                                module_voltage = decode_field("module_voltage", "42", data)
                                value_ok = True
                            except (ValueError, IndexError):
                                pass
                        if pid == "0C":
                            try:
                                policy_rpm = decode_field("rpm", "0C", data)
                                value_ok = True
                            except (ValueError, IndexError):
                                pass
                if value_ok:
                    answered.add(pid)
                    consecutive_misses = 0
                    continue
                if pid not in answered:
                    dropped.add(pid)
                    self.audit.write({"kind": "drop_pid", "ts": time.time(), "pid": pid,
                                      "status": status, "session": self.gate.session_id},
                                     sync=True)
                    self.log("PID %s answered %s: dropped for this session" % (pid, status))
                else:
                    consecutive_misses += 1
                    if consecutive_misses >= ECU_MISS_LIMIT:
                        raise EcuSilent("%d consecutive misses (last PID %s)"
                                        % (consecutive_misses, pid))
            if not self.plan or all(p in dropped for p in self.plan):
                # nothing pollable: keep the link alive via ATRV only
                if time.monotonic() - self.last_atrv >= ATRV_EVERY_S:
                    self.adapter_voltage = self._read_atrv()
                    self.last_atrv = time.monotonic()

            # voltage policy: PID 42 while the ECU answers, else corrected ATRV.
            # Emit the sample FIRST so a stop never swallows the cycle's data.
            now = time.monotonic()
            atrv_fresh = False
            if now - self.last_atrv >= ATRV_EVERY_S:
                self.adapter_voltage = self._read_atrv()
                self.last_atrv = now
                atrv_fresh = True
            if now - self.last_identity_check >= IDENTITY_EVERY_S:
                ati = " ".join(self._send("ATI")[0]).strip()
                self.last_identity_check = now
                if ati != self.identity["ati"]:
                    raise IdentityChanged("ATI %r != session %r" % (ati, self.identity["ati"]))
            if "adapter_voltage" in self.requested and self.adapter_voltage is not None:
                fields["adapter_voltage"] = self.adapter_voltage
            apply_derived(fields, self.requested)
            fields["link"] = 2
            self.emit({"type": "sample", "ts": round(time.time(), 3), "fields": fields})
            if self.counters["requests"] - req0 > cycle_budget:
                self.gate._budget_violation("per-cycle budget %d exceeded" % cycle_budget, "cycle")
            if time.monotonic() - self.last_audit_emit >= AUDIT_EVERY_S:
                self.emit_audit()
            if module_voltage is not None:
                self._voltage_gate(module_voltage, corrected=False, context="PID 42", rpm=policy_rpm)
            elif atrv_fresh and self.adapter_voltage is not None:
                self._voltage_gate(self.adapter_voltage, corrected=True, context="ATRV", rpm=policy_rpm)
            left = period - (time.monotonic() - cycle_t0)
            if left > 0:
                self._sleep(left)
