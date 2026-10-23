"""Pure tests of the start-stop aware policies in obd/session.py (no adapter,
no emulator): the voltage gate per power source and the ECU-silent probe
schedule. The emulator tests in test_integration_emulator.py exercise the
same policies end to end over a pty.
"""
import argparse
import unittest
from unittest import mock

import _path  # noqa: F401
from obd import session as S


class FakeAudit:
    def __init__(self):
        self.records = []

    def write(self, obj, sync=False):
        self.records.append(obj)


def make_session(power_source):
    args = argparse.Namespace(adapter=None, serial="/dev/null", channel=1, pin="1234",
                              poll_hz=1.0, fields="", power_source=power_source,
                              audit_dir="/tmp")
    lines = []
    s = S.Session(args, FakeAudit(), lines.append)
    s.gate.session_id = "test"
    return s, lines


class ProbeScheduleTests(unittest.TestCase):
    def test_tiers(self):
        self.assertEqual(S.ecu_silent_probe_interval(0), 10.0 * S._TIMESCALE)
        self.assertEqual(S.ecu_silent_probe_interval(599 * S._TIMESCALE), 10.0 * S._TIMESCALE)
        self.assertEqual(S.ecu_silent_probe_interval(600 * S._TIMESCALE), 60.0 * S._TIMESCALE)
        self.assertEqual(S.ecu_silent_probe_interval(3599 * S._TIMESCALE), 60.0 * S._TIMESCALE)
        self.assertEqual(S.ecu_silent_probe_interval(3600 * S._TIMESCALE), 300.0 * S._TIMESCALE)
        self.assertEqual(S.ecu_silent_probe_interval(10 ** 7), 300.0 * S._TIMESCALE)

    def test_constants_are_the_contract(self):
        # docs/PORTING-obd.md section 7 quotes these
        self.assertEqual(S.V_STOP, 11.8)
        self.assertEqual(S.V_RESUME, 12.4)
        self.assertEqual(S.VOLTAGE_SUSTAIN_CYCLES, 3)
        self.assertEqual(S.ECU_MISS_LIMIT, 3)
        self.assertEqual(S.ECU_PROBE_CMD, "010C1")


class VoltageGateTests(unittest.TestCase):
    def test_switched_never_stops_counts_one_episode(self):
        s, lines = make_session("switched")
        s.state = "live"
        with mock.patch.object(S.time, "monotonic") as mono:
            for i in range(200):
                mono.return_value = 1000.0 + i
                s._voltage_gate(9.8 if i == 50 else 11.0, corrected=False, context="PID 42", rpm=0)
        self.assertEqual(s.counters["voltage_low_events"], 1)
        self.assertEqual(s.counters["err_voltage"], 0)
        kinds = [r["kind"] for r in s.audit.records]
        self.assertEqual(kinds, ["voltage_low"])
        self.assertEqual(s.audit.records[0]["power_source"], "switched")
        # the status detail refresh: same state, no transition for the agent
        statuses = [l for l in lines if l["type"] == "status"]
        self.assertEqual(len(statuses), 1)
        self.assertEqual(statuses[0]["state"], "live")
        self.assertEqual(statuses[0]["link"], 2)
        self.assertIn("switched", statuses[0]["detail"])
        # recovery above V_RESUME closes the episode; the next low is a new one
        s._voltage_gate(12.5, corrected=False, context="PID 42", rpm=900)
        s._voltage_gate(11.0, corrected=False, context="PID 42", rpm=0)
        self.assertEqual(s.counters["voltage_low_events"], 2)

    def test_switched_hysteresis_between_stop_and_resume(self):
        s, _ = make_session("switched")
        s._voltage_gate(11.7, corrected=False, context="PID 42", rpm=0)   # episode 1
        s._voltage_gate(12.0, corrected=False, context="PID 42", rpm=0)   # above stop, below resume
        s._voltage_gate(11.7, corrected=False, context="PID 42", rpm=0)   # still episode 1
        self.assertEqual(s.counters["voltage_low_events"], 1)

    def test_constant_single_low_sample_never_stops(self):
        s, _ = make_session("constant")
        with mock.patch.object(S.time, "monotonic") as mono:
            mono.return_value = 1000.0
            s._voltage_gate(9.8, corrected=False, context="PID 42", rpm=0)   # crank dip
            mono.return_value = 1001.0
            s._voltage_gate(14.4, corrected=False, context="PID 42", rpm=900)
        self.assertEqual(s.counters["voltage_low_events"], 1)
        self.assertEqual(s.counters["err_voltage"], 0)

    def test_constant_low_with_engine_running_never_stops(self):
        s, _ = make_session("constant")
        with mock.patch.object(S.time, "monotonic") as mono:
            for i in range(120):
                mono.return_value = 1000.0 + i
                s._voltage_gate(11.0, corrected=False, context="PID 42", rpm=850)
        self.assertEqual(s.counters["voltage_low_events"], 1)

    def test_constant_sustained_low_with_rpm_zero_stops_after_window(self):
        s, _ = make_session("constant")
        with mock.patch.object(S.time, "monotonic") as mono:
            mono.return_value = 1000.0
            for i in range(int(S.VOLTAGE_SUSTAIN_S)):
                mono.return_value = 1000.0 + i
                s._voltage_gate(11.5, corrected=False, context="PID 42", rpm=0)   # < 30 s: no stop
            mono.return_value = 1000.0 + S.VOLTAGE_SUSTAIN_S
            with self.assertRaises(S.VoltageLow) as cm:
                s._voltage_gate(11.5, corrected=False, context="PID 42", rpm=0)
        self.assertIn("rpm 0", str(cm.exception))
        self.assertIn("11.50 V", str(cm.exception))

    def test_constant_sustained_low_needs_three_cycles_even_at_slow_poll(self):
        s, _ = make_session("constant")
        with mock.patch.object(S.time, "monotonic") as mono:
            mono.return_value = 1000.0
            s._voltage_gate(11.5, corrected=False, context="PID 42", rpm=None)
            mono.return_value = 1000.0 + S.VOLTAGE_SUSTAIN_S + 5   # 2nd cycle, > 30 s
            s._voltage_gate(11.5, corrected=False, context="PID 42", rpm=None)
            mono.return_value = 1000.0 + 2 * S.VOLTAGE_SUSTAIN_S
            with self.assertRaises(S.VoltageLow) as cm:
                s._voltage_gate(11.5, corrected=False, context="PID 42", rpm=None)   # 3rd
        self.assertIn("unknown", str(cm.exception))

    def test_constant_window_resets_on_a_good_sample(self):
        s, _ = make_session("constant")
        with mock.patch.object(S.time, "monotonic") as mono:
            for i in range(200):
                mono.return_value = 1000.0 + i
                # one good sample every 20 s keeps the consecutive-low run short
                v = 12.0 if i % 20 == 0 else 11.5
                s._voltage_gate(v, corrected=False, context="PID 42", rpm=0)
        self.assertEqual(s.counters["err_voltage"], 0)

    def test_atrv_is_corrected_before_the_threshold(self):
        s, _ = make_session("switched")
        s._voltage_gate(11.2, corrected=True, context="ATRV", rpm=None)   # 12.05 V corrected
        self.assertEqual(s.counters["voltage_low_events"], 0)
        s._voltage_gate(10.9, corrected=True, context="ATRV", rpm=None)   # 11.75 V corrected
        self.assertEqual(s.counters["voltage_low_events"], 1)


if __name__ == "__main__":
    unittest.main()
