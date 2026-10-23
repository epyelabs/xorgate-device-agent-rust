"""Lifecycle + fault-injection tests against ELM327-emulator 4.0.0 on a pty.

Skipped (not failed) when the emulator is not installed; see
python/obd/emulator/README.md for the venv install. Each test launches a fresh
emulator with a scenario from python/obd/emulator/taos_2022.py and runs the
real helper process (python/obd_read.py) with --serial on the pty.
"""
import os
import shutil
import signal
import sys
import tempfile
import time
import unittest

import _path  # noqa: F401
from obd.allowlist import is_allowed
from emulator_harness import Emulator, find_elm, run_helper

ELM = find_elm()


def _by_type(lines, kind):
    return [l for l in lines if l.get("type") == kind]


@unittest.skipUnless(ELM, "ELM327-emulator not installed (see python/obd/emulator/README.md)")
class EmulatorLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.audit_dir = tempfile.mkdtemp(prefix="obd-audit-")

    def tearDown(self):
        shutil.rmtree(self.audit_dir, ignore_errors=True)

    def _run(self, scenario, seconds, extra=(), poll_hz="2", until=None, stop_signal=signal.SIGTERM):
        with Emulator(scenario) as emu:
            self.emu = emu
            args = ["--serial", emu.pty, "--poll-hz", poll_hz, "--audit-dir", self.audit_dir] + list(extra)
            return run_helper(args, seconds, until=until, stop_signal=stop_signal)

    def test_taos_2022_lifecycle(self):
        code, lines, err = self._run("taos-2022", 10.0)
        self.assertEqual(code, 0, err)
        types = [l.get("type") for l in lines]
        self.assertEqual(types[0], "ready")
        ready = lines[0]
        self.assertEqual(ready["helper"], "obd_read")
        self.assertEqual(len(ready["allowlist_sha256"]), 64)
        self.assertIn("profile", types)
        self.assertLess(types.index("profile"), types.index("sample"))
        profile = _by_type(lines, "profile")[0]
        for f in ("rpm", "fuel_level", "odometer", "intake_temp"):
            self.assertIn(f, profile["supported_fields"])
        self.assertEqual(profile["responders"], ["7E8", "7E9"])
        self.assertIn("68", profile["pids_by_ecu"]["7E8"])
        self.assertEqual(profile["protocol"]["dpn"], "A6")
        self.assertEqual(profile["powertrain"], "ice")
        self.assertEqual(profile["unsupported_fields"], [])
        self.assertGreater(profile["rate_req_s"], 5)
        samples = _by_type(lines, "sample")
        self.assertGreaterEqual(len(samples), 3)
        for s in samples:
            self.assertEqual(s["fields"]["link"], 2)
            for k, v in s["fields"].items():
                self.assertIsInstance(v, (int, float), "%s=%r" % (k, v))
                self.assertNotIsInstance(v, bool)
            self.assertEqual(s["fields"]["rpm"], 928)
            self.assertEqual(s["fields"]["intake_temp"], 38)
            self.assertEqual(s["fields"]["odometer"], 47318.8)
            self.assertAlmostEqual(s["fields"]["fuel_level"], 76.86, places=2)
            # torque: PID 62 '41 62 84' + PID 63 '41 63 01 18' (survey frames)
            self.assertEqual(s["fields"]["engine_torque_pct"], 7)
            self.assertEqual(s["fields"]["reference_torque"], 280)
            self.assertEqual(s["fields"]["engine_torque"], 19.6)
        for f in ("engine_torque_pct", "reference_torque", "engine_torque"):
            self.assertIn(f, profile["supported_fields"])
        states = [l["state"] for l in _by_type(lines, "status")]
        self.assertEqual(states[-1], "stopped")
        self.assertIn("live", states)
        audits = _by_type(lines, "audit")
        self.assertGreaterEqual(len(audits), 1)
        audit = audits[-1]
        c = audit["counters"]
        self.assertEqual(c["forbidden_attempts"], 0)
        self.assertEqual(c["budget_violations"], 0)
        self.assertEqual(c["err_adapter"], 0)
        self.assertEqual(c["err_protocol"], 0)
        self.assertEqual(c["timeouts"], 0)
        self.assertGreater(c["ok"], 20)
        self.assertEqual(c["requests"], sum(audit["inventory"].values()))
        for cmd in audit["inventory"]:
            self.assertTrue(is_allowed(cmd), cmd)
        # hint "1" on every Mode 01 poll, no hint on the bitmap walk
        self.assertIn("010C1", audit["inventory"])
        self.assertNotIn("010C", audit["inventory"])
        self.assertEqual(audit["inventory"]["0100"], 1)
        self.assertEqual(audit["inventory"]["ATZ"], 2)   # handshake + close
        self.assertIsNone(audit["last_stop"])
        files = os.listdir(self.audit_dir)
        self.assertEqual(len(files), 1)
        with open(os.path.join(self.audit_dir, files[0])) as f:
            kinds = [l.split('"kind":"')[1].split('"')[0] for l in f if '"kind":"' in l]
        self.assertEqual(kinds[0], "open")
        self.assertIn("session", kinds)
        self.assertEqual(kinds[-1], "close")
        self.assertEqual(kinds.count("io"), c["requests"])

    def test_late_sigterm_during_shutdown_still_exits_zero(self):
        # Bench finding (WP3): the agent SIGINTs the helper and systemd SIGTERMs
        # the whole cgroup ~100 ms later; the late signal used to raise a second
        # Shutdown while the final status line was being written (traceback, exit 1).
        code, lines, err = self._run("taos-2022", 6.0,
                                     stop_signal=[signal.SIGINT] + [signal.SIGTERM] * 30)
        self.assertEqual(code, 0, err)
        self.assertNotIn("Traceback", err)
        states = [l["state"] for l in _by_type(lines, "status")]
        self.assertEqual(states[-1], "stopped")
        self.assertEqual(lines[-1]["type"], "audit")

    def test_sigint_also_exits_zero_with_atz(self):
        code, lines, err = self._run("taos-2022", 6.0, stop_signal=signal.SIGINT,
                                     until=lambda ls: len(_by_type(ls, "sample")) >= 1)
        self.assertEqual(code, 0, err)
        self.assertEqual(_by_type(lines, "audit")[-1]["inventory"]["ATZ"], 2)

    def test_fields_restriction(self):
        code, lines, err = self._run("taos-2022", 8.0, extra=["--fields", "rpm,speed,fuel_level,bogus"],
                                     until=lambda ls: len(_by_type(ls, "sample")) >= 2)
        self.assertEqual(code, 0, err)
        profile = _by_type(lines, "profile")[0]
        self.assertEqual(profile["supported_fields"], ["rpm", "speed", "fuel_level", "link"])
        sample = _by_type(lines, "sample")[0]
        self.assertEqual(set(sample["fields"]), {"rpm", "speed", "fuel_level", "link"})
        inv = _by_type(lines, "audit")[-1]["inventory"]
        self.assertNotIn("01051", inv)
        self.assertIn("01421", inv)   # PID 42 still polled for the voltage policy
        self.assertIn("bogus", err)

    def test_derived_torque_alone_polls_62_63_and_emits_only_nm(self):
        code, lines, err = self._run("taos-2022", 8.0, extra=["--fields", "rpm,engine_torque"],
                                     until=lambda ls: len(_by_type(ls, "sample")) >= 2)
        self.assertEqual(code, 0, err)
        profile = _by_type(lines, "profile")[0]
        self.assertEqual(profile["supported_fields"], ["rpm", "engine_torque", "link"])
        for sample in _by_type(lines, "sample"):
            self.assertEqual(set(sample["fields"]), {"rpm", "engine_torque", "link"})
            self.assertEqual(sample["fields"]["engine_torque"], 19.6)
        inv = _by_type(lines, "audit")[-1]["inventory"]
        self.assertIn("01621", inv)
        self.assertIn("01631", inv)

    def test_unsupported_pid_answering_no_data_is_dropped(self):
        code, lines, err = self._run("taos-2022-nodata2f", 9.0,
                                     until=lambda ls: len(_by_type(ls, "sample")) >= 4)
        self.assertEqual(code, 0, err)
        profile = _by_type(lines, "profile")[0]
        self.assertIn("fuel_level", profile["supported_fields"])   # the bitmap advertised it
        samples = _by_type(lines, "sample")
        self.assertGreaterEqual(len(samples), 4)
        for s in samples:
            self.assertNotIn("fuel_level", s["fields"])
            self.assertIn("rpm", s["fields"])
        audit = _by_type(lines, "audit")[-1]
        self.assertEqual(audit["inventory"]["012F1"], 1)
        self.assertEqual(audit["counters"]["no_data"], 1)
        self.assertEqual(audit["counters"]["forbidden_attempts"], 0)

    def test_protocol_other_than_6_or_7_is_refused_exit_4(self):
        code, lines, err = self._run("taos-2022-badproto", 20.0)
        self.assertEqual(code, 4, err)
        errors = _by_type(lines, "error")
        self.assertEqual(len(errors), 1)
        self.assertEqual(errors[0]["code"], 4)
        self.assertIn("A3", errors[0]["error"])
        self.assertEqual(_by_type(lines, "sample"), [])
        self.assertEqual(_by_type(lines, "profile"), [])
        audit = _by_type(lines, "audit")[-1]
        self.assertEqual(audit["counters"]["err_protocol"], 1)
        self.assertEqual(audit["last_stop"]["reason"], "protocol A3")
        # nothing beyond the handshake reached the car
        self.assertNotIn("010C1", audit["inventory"])
        self.assertNotIn("0120", audit["inventory"])

    # ------------------------------------------------------------------
    # Start-stop awareness (fix/obd-startstop-voltage, 2026-10-14 drive).
    # FAST scales every policy interval by 0.1 (30 s sustain -> 3 s, 60 s
    # re-check -> 6 s, 10 s probe -> 1 s, 60 s audit -> 6 s).
    # ------------------------------------------------------------------
    FAST = {"OBD_TEST_TIMESCALE": "0.1"}

    def _run_env(self, scenario, seconds, extra=(), poll_hz="2", until=None, env=None):
        with Emulator(scenario) as emu:
            self.emu = emu
            args = ["--serial", emu.pty, "--poll-hz", poll_hz, "--audit-dir", self.audit_dir] + list(extra)
            return run_helper(args, seconds, until=until, env=env)

    def _audit_kinds(self):
        files = os.listdir(self.audit_dir)
        with open(os.path.join(self.audit_dir, files[0])) as f:
            return [l.split('"kind":"')[1].split('"')[0] for l in f if '"kind":"' in l]

    def _assert_crank_does_not_stop(self, power_source):
        code, lines, err = self._run_env(
            "taos-2022-crank", 25.0, extra=["--power-source", power_source],
            until=lambda ls: len(_by_type(ls, "sample")) >= 8)
        self.assertEqual(code, 0, err)
        states = [l["state"] for l in _by_type(lines, "status")]
        self.assertNotIn("backoff", states)
        self.assertEqual(states[-1], "stopped")          # only the SIGTERM close
        self.assertEqual(states.count("stopped"), 1)
        samples = _by_type(lines, "sample")
        self.assertGreaterEqual(len(samples), 8)
        rpms = [s["fields"]["rpm"] for s in samples]
        self.assertIn(0, rpms)                            # auto-stop is a live state
        self.assertIn(928, rpms)                          # and the restart is seen
        volts = [s["fields"]["module_voltage"] for s in samples]
        self.assertIn(9.82, volts)                        # the dip sample is published
        self.assertIn(14.4, volts)
        for smp in samples:
            self.assertEqual(smp["fields"]["link"], 2)
        audit = _by_type(lines, "audit")[-1]
        self.assertEqual(audit["counters"]["voltage_low_events"], 1)
        self.assertEqual(audit["counters"]["err_voltage"], 0)
        self.assertEqual(audit["counters"]["reconnects"], 0)
        self.assertEqual(audit["inventory"]["ATZ"], 2)    # handshake + close only
        self.assertIsNone(audit["last_stop"])
        kinds = self._audit_kinds()
        self.assertEqual(kinds.count("voltage_low"), 1)
        self.assertEqual(kinds.count("close"), 1)
        return lines, err

    def test_crank_dip_does_not_stop_on_switched_power(self):
        lines, err = self._assert_crank_does_not_stop("switched")
        self.assertIn("ignored on switched power", err)
        details = [l["detail"] for l in _by_type(lines, "status") if l["state"] == "live"]
        self.assertTrue(any("switched power" in d for d in details), details)

    def test_crank_dip_does_not_stop_on_constant_power(self):
        _, err = self._assert_crank_does_not_stop("constant")
        self.assertIn("watching for", err)

    def test_sustained_low_on_constant_stops_keeps_link_and_resumes(self):
        def until(ls):
            statuses = _by_type(ls, "status")
            st = [l["state"] for l in statuses]
            if "stopped" not in st or st[-1] != "live":
                return False
            resumed = statuses[-1]["ts"]
            return any(s["ts"] > resumed for s in _by_type(ls, "sample"))   # one sample after the resume
        code, lines, err = self._run_env(
            "taos-2022-lowbatt", 40.0, extra=["--power-source", "constant"], until=until, env=self.FAST)
        self.assertEqual(code, 0, err)
        statuses = _by_type(lines, "status")
        states = [l["state"] for l in statuses]
        self.assertNotIn("backoff", states)
        self.assertIn("stopped", states)
        held = statuses[states.index("stopped")]
        self.assertEqual(held["link"], 1)                 # adapter kept, ECU not polled
        self.assertIn("re-check", held["detail"])
        self.assertIn("rpm 0", held["detail"])
        self.assertEqual(states[-2:], ["live", "stopped"]) # resumed, then the SIGTERM close
        # samples stop during the hold and resume after it
        hold_ts = held["ts"]
        live_again = [l for l in statuses if l["state"] == "live" and l["ts"] > hold_ts][0]
        samples = _by_type(lines, "sample")
        self.assertFalse([s for s in samples if hold_ts < s["ts"] < live_again["ts"]])
        self.assertTrue([s for s in samples if s["ts"] > live_again["ts"]])
        audit = _by_type(lines, "audit")[-1]
        c = audit["counters"]
        self.assertEqual(c["err_voltage"], 1)
        self.assertEqual(c["voltage_low_events"], 1)
        self.assertEqual(c["reconnects"], 0)
        self.assertEqual(audit["inventory"]["ATZ"], 2)    # no reconnect: link kept open
        self.assertIn("voltage", audit["last_stop"]["reason"])
        kinds = self._audit_kinds()
        self.assertEqual(kinds.count("voltage_hold"), 1)
        self.assertEqual(kinds.count("voltage_resume"), 1)
        self.assertEqual(kinds.count("close"), 1)
        self.assertIn("voltage resume", err)

    def test_sustained_low_on_switched_never_stops(self):
        code, lines, err = self._run_env(
            "taos-2022-lowbatt", 25.0, extra=["--power-source", "switched"],
            until=lambda ls: len(_by_type(ls, "sample")) >= 12, env=self.FAST)
        self.assertEqual(code, 0, err)
        states = [l["state"] for l in _by_type(lines, "status")]
        self.assertEqual(states.count("stopped"), 1)      # the SIGTERM close only
        self.assertNotIn("backoff", states)
        audit = _by_type(lines, "audit")[-1]
        self.assertEqual(audit["counters"]["err_voltage"], 0)
        self.assertEqual(audit["counters"]["voltage_low_events"], 1)
        self.assertIsNone(audit["last_stop"])

    def test_ecu_silent_keeps_link_probes_010c_and_resumes(self):
        with Emulator("taos-2022") as emu:
            marks = {}

            def until(ls):
                st = [l["state"] for l in _by_type(ls, "status")]
                if len(_by_type(ls, "sample")) == 2 and "off" not in marks:
                    emu.command("scenario taos-2022-ignoff")   # every PID now answers NO DATA
                    marks["off"] = True
                if "ecu_silent" in st and "on" not in marks:
                    marks["on"] = time.time() + 3.0       # let a few probes go out first
                if "on" in marks and marks["on"] is not True and time.time() >= marks["on"]:
                    emu.command("scenario taos-2022")
                    marks["on"] = True
                if "ecu_silent" not in st or st[-1] != "live":
                    return False
                resumed = _by_type(ls, "status")[-1]["ts"]
                return any(s["ts"] > resumed for s in _by_type(ls, "sample"))   # one sample after the resume
            args = ["--serial", emu.pty, "--poll-hz", "2", "--audit-dir", self.audit_dir]
            code, lines, err = run_helper(args, 40.0, until=until, env=self.FAST)
        self.assertEqual(code, 0, err)
        statuses = _by_type(lines, "status")
        states = [l["state"] for l in statuses]
        self.assertNotIn("backoff", states)               # the link was never torn down
        self.assertIn("ecu_silent", states)
        silent = statuses[states.index("ecu_silent")]
        self.assertEqual(silent["link"], 1)
        self.assertIn("010C1", silent["detail"])
        self.assertEqual(states[-2:], ["live", "stopped"])
        audit = _by_type(lines, "audit")[-1]
        c = audit["counters"]
        self.assertGreaterEqual(c["no_data"], 3 + 2)      # three misses + at least two probes
        self.assertEqual(c["reconnects"], 0)
        self.assertEqual(c["forbidden_attempts"], 0)
        self.assertEqual(audit["inventory"]["ATZ"], 2)
        samples = _by_type(lines, "sample")
        self.assertTrue([s for s in samples if s["ts"] > silent["ts"]])   # polling resumed
        for s in samples:
            for v in s["fields"].values():
                self.assertIsInstance(v, (int, float))
        kinds = self._audit_kinds()
        self.assertEqual(kinds.count("ecu_silent"), 1)
        self.assertEqual(kinds.count("ecu_resumed"), 1)
        self.assertEqual(kinds.count("close"), 1)
        self.assertIn("ECU answering again", err)

    def test_ecu_silent_at_connect_probes_0100_without_reconnecting(self):
        with Emulator("taos-2022-ignoff") as emu:
            marks = {}

            def until(ls):
                st = [l["state"] for l in _by_type(ls, "status")]
                if "ecu_silent" in st and "on" not in marks:
                    marks["on"] = time.time() + 2.5
                if "on" in marks and marks["on"] is not True and time.time() >= marks["on"]:
                    emu.command("scenario taos-2022")
                    marks["on"] = True
                return "live" in st and len(_by_type(ls, "sample")) >= 1
            args = ["--serial", emu.pty, "--poll-hz", "2", "--audit-dir", self.audit_dir,
                    "--power-source", "constant"]
            code, lines, err = run_helper(args, 40.0, until=until, env=self.FAST)
        self.assertEqual(code, 0, err)
        types = [l["type"] for l in lines]
        states = [l["state"] for l in _by_type(lines, "status")]
        self.assertNotIn("backoff", states)
        self.assertIn("ecu_silent", states)
        self.assertLess(types.index("status"), types.index("profile"))
        silent = [l for l in _by_type(lines, "status") if l["state"] == "ecu_silent"][0]
        self.assertEqual(silent["link"], 1)
        self.assertIn("0100", silent["detail"])
        self.assertIn("profile", types)
        self.assertGreaterEqual(len(_by_type(lines, "sample")), 1)
        audit = _by_type(lines, "audit")[-1]
        self.assertGreaterEqual(audit["inventory"]["0100"], 2)
        self.assertEqual(audit["inventory"]["ATZ"], 2)
        self.assertEqual(audit["counters"]["reconnects"], 0)
        kinds = self._audit_kinds()
        self.assertEqual(kinds.count("open"), 1)
        self.assertEqual(kinds.count("ecu_silent"), 1)
        self.assertEqual(kinds.count("ecu_resumed"), 1)

    def test_audit_line_cadence(self):
        code, lines, err = self._run_env("taos-2022", 16.0, env=self.FAST)
        self.assertEqual(code, 0, err)
        audits = _by_type(lines, "audit")
        self.assertGreaterEqual(len(audits), 3)           # 6 s cadence + the on-stop line
        reqs = [a["counters"]["requests"] for a in audits]
        self.assertEqual(reqs, sorted(reqs))
        self.assertGreater(reqs[-1], reqs[0])
        gaps = [b["ts"] - a["ts"] for a, b in zip(audits, audits[1:-1])]
        for g in gaps:
            self.assertGreaterEqual(g, 5.0)
            self.assertLess(g, 9.0)
        self.assertEqual(lines[-1]["type"], "audit")
        for a in audits:
            self.assertIn("voltage_low_events", a["counters"])

if __name__ == "__main__":
    unittest.main()
