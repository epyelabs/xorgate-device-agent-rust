"""CLI entry: argument parsing, startup self-test, signal handling."""
import argparse
import json
import os
import signal
import sys

from .allowlist import self_test
from .audit import AuditLog
from .session import Session, Shutdown


def emit(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def default_audit_dir():
    home = os.environ.get("XORGATE_HOME") or os.path.expanduser("~/.xorgate")
    return os.path.join(home, "obd", "audit")


def parse_args(argv):
    ap = argparse.ArgumentParser(prog="obd_read.py",
                                 description="ELM327/OBD2 helper: JSON lines on stdout")
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--adapter", help="adapter Bluetooth MAC (RFCOMM)")
    src.add_argument("--serial", help="serial/pty path (emulator, USB adapter)")
    ap.add_argument("--channel", type=int, default=1, help="RFCOMM channel (default 1)")
    ap.add_argument("--pin", default="1234", help="legacy pairing PIN (default 1234)")
    ap.add_argument("--poll-hz", type=float, default=1.0, help="poll cycles per second")
    ap.add_argument("--fields", default="", help="csv of field names (default all)")
    ap.add_argument("--power-source", choices=("switched", "constant"), default="switched")
    ap.add_argument("--audit-dir", default=default_audit_dir())
    return ap.parse_args(argv)


def make_signal_handler(session):
    """SIGINT/SIGTERM -> Shutdown, except once the session has run its
    shutdown close: the agent SIGINTs the helper and systemd then SIGTERMs
    the whole cgroup ~100 ms later, and that late signal must not turn the
    final status/audit lines into a traceback (bench finding, WP3)."""
    def on_signal(signum, _frame):
        if session.closed:
            return
        raise Shutdown(signum)
    return on_signal


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if args.adapter:
        args.adapter = args.adapter.strip().upper()
        from .pairing import valid_mac
        if not valid_mac(args.adapter):
            emit({"type": "error", "code": 6, "error": "invalid adapter MAC %r" % args.adapter})
            return 6

    failures = self_test()
    if failures:
        emit({"type": "error", "code": 6,
              "error": "allowlist self-test failed: %s" % "; ".join(failures[:5])})
        return 6

    try:
        audit = AuditLog(args.audit_dir)
    except OSError as exc:
        emit({"type": "error", "code": 6, "error": "cannot open audit dir %s: %s"
              % (args.audit_dir, exc)})
        return 6

    session = Session(args, audit, emit)

    on_signal = make_signal_handler(session)
    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)

    session.emit_ready()
    try:
        code = session.run()
    finally:
        audit.close()
    return code
