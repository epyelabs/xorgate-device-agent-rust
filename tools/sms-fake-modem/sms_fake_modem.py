#!/usr/bin/env python3
"""A scripted SIM7600 SMS port on a pty (stdlib only) — Phase 5b WP3, for H15.

The semantics of the TS test FakeModem (`test/sms-modem.test.ts:48-120` @
09da38d) and of the rig's FakeModem, seeded with the SAME inbox as the
`p4-sms` tape (`fixtures/replay/p4-sms-modem.json`: SM 10/40 incl. a two-part
UCS2 concat and a stored SUBMIT, ME 23/23, text mode, mem1 SR):

  ATE0 -> ATE0 OK            AT+CMGF? -> +CMGF: <n> OK     AT+CMGF=<d> -> OK
  AT+CPMS? -> +CPMS: "<mem1>",u,t,"ME",u,t,"ME",u,t OK
  AT+CPMS="<X>" -> +CPMS: u,t,23,23,23,23 OK (mem1 := X)
  AT+CMGRO=<i> -> +CMGRO: <stat>,"",<len> / <pdu> / OK, or ERROR (empty slot);
                  in text mode it is a protocol error: logged as a VIOLATION.
  AT+CMGD=0,4  -> ONLY with --allow-clear: empties the store mem1 selects, OK.
                  Without it: ERROR, nothing deleted, logged as a VIOLATION
                  (the rig rule, phase-5b §6.7). Any other delete form is
                  always a violation.
  anything else -> ERROR

The pty slave is published as a symlink (--link, e.g. /run/xorgate-rs-sms-pty)
for `XORGATE_SMS_PORT`; the agent's own `stty raw -echo` configures it. The
command log (commands only, never a message body) goes to --log; on exit
(SIGTERM/SIGINT) the link is removed and the exit code is 3 if any violation
happened, else 0. Nothing here touches a real modem or the real SIM.

CF75: on the bench (shared profile) the JS and Rust agents use different SMS
lock files, so cross-agent exclusion does not hold there; a clear must never
target the real interface-05 port in Phase 5b. H15 points the Rust agent's
XORGATE_SMS_PORT at this pty instead (`lsof` shows the real port unopened).

  python3 sms_fake_modem.py --inbox fixtures/replay/p4-sms-modem.json \\
      --link /run/xorgate-rs-sms-pty --log /tmp/sms-fake.log [--allow-clear]
  python3 sms_fake_modem.py --self-test --inbox fixtures/replay/p4-sms-modem.json
"""

import argparse
import json
import os
import re
import select
import signal
import sys
import termios
import tty

DELETE_ALL = "AT+CMGD=0,4"


class Modem:
    def __init__(self, seed, allow_clear):
        self.cmgf = int(seed.get("cmgf", 0))
        self.mem1 = seed.get("mem1", "SR")
        # stores: {"SM": {"total": n, "slots": {"<i>": {stat, length, pdu}}}}
        self.stores = {k: {"total": v["total"], "slots": dict(v.get("slots", {}))}
                       for k, v in seed.get("stores", {}).items()}
        self.allow_clear = allow_clear
        self.log = []
        self.violations = []
        self.deletes = []

    def used(self, mem):
        return len(self.stores.get(mem, {}).get("slots", {}))

    def total(self, mem):
        return 0 if mem == "SR" else self.stores.get(mem, {}).get("total", 0)

    def triple(self, mem):
        return '"%s",%d,%d' % (mem, self.used(mem), self.total(mem))

    def command(self, cmd):
        """One command (without its CR) -> the reply lines."""
        self.log.append(cmd)
        if cmd == "ATE0":
            return ["ATE0", "OK"]
        if cmd == "AT+CMGF?":
            return ["+CMGF: %d" % self.cmgf, "OK"]
        m = re.fullmatch(r"AT\+CMGF=(\d)", cmd)
        if m:
            self.cmgf = int(m.group(1))
            return ["OK"]
        if cmd == "AT+CPMS?":
            return ["+CPMS: %s,%s,%s" % (self.triple(self.mem1), self.triple("ME"), self.triple("ME")), "OK"]
        m = re.fullmatch(r'AT\+CPMS="(\w+)"', cmd)
        if m:
            self.mem1 = m.group(1)
            return ["+CPMS: %d,%d,23,23,23,23" % (self.used(self.mem1), self.total(self.mem1)), "OK"]
        m = re.fullmatch(r"AT\+CMGRO=(\d+)", cmd)
        if m:
            if self.cmgf != 0:
                self.violations.append("CMGRO sent in text mode: " + cmd)
                return ["ERROR"]
            slot = self.stores.get(self.mem1, {}).get("slots", {}).get(str(int(m.group(1))))
            if slot:
                return ['+CMGRO: %d,"",%d' % (slot["stat"], slot["length"]), slot["pdu"], "OK"]
            return ["ERROR"]
        if re.search(r"CMGR=|CMGL|CMGW|CMSS|CMGS", cmd):
            self.violations.append("forbidden: " + cmd)
            return ["ERROR"]
        if "CMGD" in cmd:
            if cmd != DELETE_ALL or not self.allow_clear:
                self.violations.append("forbidden: " + cmd)
                return ["ERROR"]
            self.deletes.append(self.mem1)
            if self.mem1 in self.stores:
                self.stores[self.mem1]["slots"] = {}
            return ["OK"]
        return ["ERROR"]


def serve(master, modem, log_path, stop):
    pending = b""
    logf = open(log_path, "a", buffering=1) if log_path else None
    try:
        while not stop["now"]:
            r, _, _ = select.select([master], [], [], 0.2)
            if not r:
                continue
            try:
                chunk = os.read(master, 1024)
            except OSError:
                continue  # no slave open yet / closed between clients
            if not chunk:
                continue
            pending += chunk
            while b"\r" in pending:
                raw, pending = pending.split(b"\r", 1)
                cmd = raw.decode("latin-1").lstrip("\n")
                out = modem.command(cmd)
                if logf:
                    logf.write(cmd + "\n")
                os.write(master, b"".join(l.encode("latin-1") + b"\r\n" for l in out))
    finally:
        if logf:
            logf.close()


def self_test(seed):
    """Drive the modem through the read + clear sequences in-process."""
    m = Modem(seed, allow_clear=False)
    assert m.command("AT+CPMS?") == ['+CPMS: "SR",0,0,"ME",23,23,"ME",23,23', "OK"]
    assert m.command('AT+CPMS="SM"')[0] == "+CPMS: 10,40,23,23,23,23"
    assert m.command(DELETE_ALL) == ["ERROR"] and m.violations and m.used("SM") == 10
    m = Modem(seed, allow_clear=True)
    m.command('AT+CPMS="SM"')
    assert m.command(DELETE_ALL) == ["OK"] and m.used("SM") == 0
    assert m.command("AT+CMGD=1,4") == ["ERROR"] and m.violations == ["forbidden: AT+CMGD=1,4"]
    m.command("AT+CMGF=1")
    assert m.command("AT+CMGRO=0") == ["ERROR"] and "text mode" in m.violations[-1]
    print("self-test ok")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--inbox", required=True, help="JSON with a top-level 'modem' {cmgf, mem1, stores}")
    ap.add_argument("--link", help="symlink to create for the pty slave")
    ap.add_argument("--log", help="append every command here")
    ap.add_argument("--allow-clear", action="store_true", help="answer AT+CMGD=0,4 (else a violation)")
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()
    with open(a.inbox) as f:
        seed = json.load(f)["modem"]
    if a.self_test:
        self_test(seed)
        return 0
    master, slave = os.openpty()
    tty.setraw(master, termios.TCSANOW)
    name = os.ttyname(slave)
    if a.link:
        try:
            os.unlink(a.link)
        except FileNotFoundError:
            pass
        os.symlink(name, a.link)
    modem = Modem(seed, a.allow_clear)
    stop = {"now": False}

    def on_signal(_sig, _frm):
        stop["now"] = True

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)
    print("sms-fake-modem: %s%s allowClear=%s" % (name, (" <- " + a.link) if a.link else "", a.allow_clear), flush=True)
    try:
        serve(master, modem, a.log, stop)
    finally:
        if a.link:
            try:
                os.unlink(a.link)
            except FileNotFoundError:
                pass
        os.close(slave)
        os.close(master)
    summary = {"commands": len(modem.log), "deletes": modem.deletes, "violations": modem.violations,
               "end": {"cmgf": modem.cmgf, "mem1": modem.mem1}}
    print("sms-fake-modem: " + json.dumps(summary), flush=True)
    return 3 if modem.violations else 0


if __name__ == "__main__":
    sys.exit(main())
