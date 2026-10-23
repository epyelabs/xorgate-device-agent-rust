"""Gate unit tests with a scripted fake transport (no emulator needed)."""
import json
import os
import shutil
import tempfile
import time
import unittest

import _path  # noqa: F401
from obd.audit import AuditLog
from obd.gate import (Gate, Forbidden, BudgetExceeded, StopCondition, LinkLost,
                      HARD_CAP_REQ_S, new_counters)
from obd.transport import TransportError


class FakeTransport:
    """Answers each command from a dict; '*' is the default; None = no prompt."""

    def __init__(self, table, default="NO DATA"):
        self.table, self.default = table, default
        self.sent = []
        self._pending = b""

    def write(self, data):
        cmd = data.decode().strip()
        self.sent.append(cmd)
        resp = self.table.get(cmd, self.default)
        if callable(resp):
            resp = resp(cmd)
        if resp is None:
            self._pending = b""
        elif resp == "__DROP__":
            raise TransportError("link dropped")
        else:
            self._pending = (resp + "\r\r>").encode()

    def read(self, timeout):
        out, self._pending = self._pending, b""
        if not out:
            time.sleep(min(timeout, 0.01))
        return out

    def close(self):
        pass

    def describe(self):
        return "fake://"


class GateTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.audit = AuditLog(self.dir)
        self.counters = new_counters()
        self.inventory = {}
        self.gate = Gate(self.audit, self.counters, self.inventory, mac="AA:BB", min_interval=0.0)

    def tearDown(self):
        self.audit.close()
        shutil.rmtree(self.dir, ignore_errors=True)

    def _audit_lines(self):
        out = []
        for name in os.listdir(self.dir):
            with open(os.path.join(self.dir, name)) as f:
                out += [json.loads(l) for l in f if l.strip()]
        return out

    def _open(self, table, default="NO DATA"):
        t = FakeTransport(table, default)
        self.gate.open(t, "s1")
        return t

    def test_forbidden_never_written_and_counted(self):
        t = self._open({})
        for cmd in ("04", "ATSH7E0", "ATLP", "2201", "ATSP6"):
            with self.assertRaises(Forbidden):
                self.gate.send(cmd)
        self.assertEqual(t.sent, [])
        self.assertEqual(self.counters["forbidden_attempts"], 5)
        self.assertEqual(self.counters["requests"], 0)
        kinds = [l["kind"] for l in self._audit_lines()]
        self.assertEqual(kinds.count("forbidden"), 5)

    def test_ok_and_inventory_and_audit_order(self):
        t = self._open({"010C1": "7E804410C0E80", "ATRV": "13.2V"})
        lines, status = self.gate.send("010C1")
        self.assertEqual((lines, status), (["7E804410C0E80"], "ok"))
        self.gate.send("ATRV")
        self.gate.send("010C1")
        self.assertEqual(self.inventory, {"010C1": 2, "ATRV": 1})
        self.assertEqual(self.counters["ok"], 3)
        self.assertEqual(self.counters["bytes_tx"], 6 + 5 + 6)
        io = [l for l in self._audit_lines() if l["kind"] == "io"]
        self.assertEqual([l["cmd"] for l in io], ["010C1", "ATRV", "010C1"])
        self.assertTrue(all(l["status"] == "ok" for l in io))

    def test_echo_line_is_stripped(self):
        self._open({"ATE0": "ATE0\rOK"})
        lines, status = self.gate.send("ATE0")
        self.assertEqual(lines, ["OK"])
        self.assertEqual(status, "ok")

    def test_no_data_counted(self):
        self._open({}, default="NO DATA")
        lines, status = self.gate.send("010F1")
        self.assertEqual(status, "no_data")
        self.assertEqual(self.counters["no_data"], 1)
        self._open({"0100": "SEARCHING...\rUNABLE TO CONNECT"})
        _, status = self.gate.send("0100")
        self.assertEqual(status, "no_data")

    def test_timeout_counted(self):
        self._open({"010C1": None})
        lines, status = self.gate.send("010C1", timeout=0.05)
        self.assertEqual(status, "timeout")
        self.assertEqual(self.counters["timeouts"], 1)

    def test_stop_strings_raise(self):
        for text in ("BUS ERROR", "CAN ERROR", "BUFFER FULL", "LV RESET", "DATA ERROR",
                     "BUS BUSY", "FB ERROR", "RX ERROR", "BUS INIT: ...ERROR"):
            self._open({"010C1": text})
            with self.assertRaises(StopCondition):
                self.gate.send("010C1")
            self.assertIsNotNone(self.gate.last_stop)
        self.assertEqual(self.counters["err_adapter"], 9)

    def test_stopped_three_times(self):
        self._open({"010C1": "STOPPED"})
        self.gate.send("010C1")
        self.gate.send("010C1")
        with self.assertRaises(StopCondition):
            self.gate.send("010C1")
        self.assertEqual(self.gate.last_stop["reason"], "STOPPED x3")

    def test_stopped_counter_resets_on_ok(self):
        t = self._open({"010C1": "STOPPED", "010D1": "7E803410D00"})
        self.gate.send("010C1")
        self.gate.send("010C1")
        self.gate.send("010D1")
        self.gate.send("010C1")
        self.gate.send("010C1")   # only 2 in a row again: no stop
        self.assertEqual(self.gate.stopped_count, 2)

    def test_hard_rate_cap_is_a_bug(self):
        self._open({"010C1": "7E804410C0E80"})
        with self.assertRaises(BudgetExceeded):
            for _ in range(HARD_CAP_REQ_S + 1):
                self.gate.send("010C1")
        self.assertEqual(self.counters["budget_violations"], 1)
        self.assertEqual(self.counters["requests"], HARD_CAP_REQ_S)

    def test_pacing_floor_keeps_under_cap(self):
        self.gate.min_interval = 0.05
        self._open({"010C1": "7E804410C0E80"})
        t0 = time.monotonic()
        for _ in range(25):
            self.gate.send("010C1")
        self.assertGreaterEqual(time.monotonic() - t0, 24 * 0.05 - 0.01)
        self.assertEqual(self.counters["budget_violations"], 0)

    def test_link_drop_raises_linklost(self):
        self._open({"010C1": "__DROP__"})
        with self.assertRaises(LinkLost):
            self.gate.send("010C1")
        with self.assertRaises(LinkLost):
            Gate(self.audit, new_counters(), {}).send("ATZ")

    def test_close_sends_atz_and_logs(self):
        t = self._open({"ATZ": "ELM327 v2.3"})
        self.gate.close(reason="test")
        self.assertEqual(t.sent, ["ATZ"])
        self.assertFalse(self.gate.is_open)
        kinds = [l["kind"] for l in self._audit_lines()]
        self.assertEqual(kinds[0], "open")
        self.assertEqual(kinds[-1], "close")


class AuditLogTests(unittest.TestCase):
    def test_cap_drops_oldest_and_rotates_current(self):
        d = tempfile.mkdtemp()
        try:
            old = os.path.join(d, "2020-01-01.jsonl")
            with open(old, "w") as f:
                f.write("x" * 600)
            os.utime(old, (1, 1))
            log = AuditLog(d, cap_bytes=1000)
            self.assertTrue(os.path.exists(old))
            for _ in range(20):
                log.write({"pad": "y" * 100})
            log._enforce_cap()
            self.assertFalse(os.path.exists(old))
            total = sum(os.path.getsize(os.path.join(d, n)) for n in os.listdir(d))
            self.assertLessEqual(total, 1000 + 200)
            log.close()
            mode = os.stat(log.path).st_mode & 0o777
            self.assertEqual(mode, 0o600)
        finally:
            shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
