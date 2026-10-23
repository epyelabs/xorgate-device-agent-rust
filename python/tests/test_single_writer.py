"""Repository test (plan 7.1): only the gate may send, only the transports may
write bytes. Any other write to a socket/fd in the helper fails this test."""
import os
import re
import unittest

import _path  # noqa: F401

PKG = os.path.join(_path.PYTHON_DIR, "obd")
WRITERS = {"transport.py"}          # the only files allowed to call write/sendall/os.write
SENDERS = {"gate.py"}               # the only file allowed to call transport.write
FILE_LOGS = {"audit.py"}            # writes a regular log file under --audit-dir, never a link
_RAW_WRITE = re.compile(r"\b(sendall\(|sock\.send\(|os\.write\(|\.write\()")
_TRANSPORT_WRITE = re.compile(r"\b(_t|transport|self\.t)\.write\(")


def _py_files():
    for root, _dirs, files in os.walk(PKG):
        if "emulator" in root:
            continue
        for f in files:
            if f.endswith(".py"):
                yield os.path.join(root, f)


class SingleWriterTests(unittest.TestCase):
    def test_only_transports_write_bytes(self):
        offenders = []
        for path in _py_files():
            name = os.path.basename(path)
            if name in WRITERS or name in FILE_LOGS:
                continue
            with open(path) as f:
                for n, line in enumerate(f, 1):
                    code = line.split("#", 1)[0]
                    if _RAW_WRITE.search(code):
                        # the gate hands bytes to its transport; audit/stderr/stdout
                        # writes are JSON lines, not vehicle traffic
                        if name in SENDERS and _TRANSPORT_WRITE.search(code):
                            continue
                        if "audit.write(" in code or "sys.stderr.write(" in code \
                                or "sys.stdout.write(" in code or "stdin.write(" in code \
                                or "self.audit.write(" in code or "os.write(self.fd" in code \
                                or "log.write(" in code:
                            continue
                        offenders.append("%s:%d: %s" % (name, n, code.strip()))
        self.assertEqual(offenders, [])

    def test_only_gate_sends_to_transport(self):
        offenders = []
        for path in _py_files():
            name = os.path.basename(path)
            if name in SENDERS | WRITERS:
                continue
            with open(path) as f:
                for n, line in enumerate(f, 1):
                    code = line.split("#", 1)[0]
                    if _TRANSPORT_WRITE.search(code) or "RfcommTransport(" in code and "write" in code:
                        offenders.append("%s:%d: %s" % (name, n, code.strip()))
                    if re.search(r"\bsocket\.socket\(", code):
                        offenders.append("%s:%d: %s" % (name, n, code.strip()))
                    if re.search(r"\bos\.open\(", code) and name not in FILE_LOGS:
                        offenders.append("%s:%d: %s" % (name, n, code.strip()))
                    if name in FILE_LOGS and re.search(r"transport|socket|\.pts|tty", code, re.I):
                        offenders.append("%s:%d: %s" % (name, n, code.strip()))
        self.assertEqual(offenders, [])

    def test_session_only_uses_gate_send(self):
        with open(os.path.join(PKG, "session.py")) as f:
            src = f.read()
        self.assertNotIn(".sendall(", src)
        self.assertNotIn("os.write(", src)
        self.assertIn("self.gate.send(", src)


if __name__ == "__main__":
    unittest.main()
