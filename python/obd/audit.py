"""Append-only audit log: one JSONL file per day, 0600, size-capped directory.

Every line is written synchronously (os.write on an O_APPEND fd) so the
record of a request exists before the next request is sent; `sync=True`
fsyncs (session open/close, every non-ok status, forbidden attempts).
"""
import glob
import json
import os
import time

DEFAULT_CAP_BYTES = 50 * 1024 * 1024
_CHECK_EVERY = 1024 * 1024


class AuditLog:
    def __init__(self, directory, cap_bytes=DEFAULT_CAP_BYTES):
        self.dir = directory
        self.cap = cap_bytes
        self.fd = None
        self.path = None
        self.day = None
        self.size = 0
        self._since_check = 0
        os.makedirs(self.dir, mode=0o700, exist_ok=True)
        self._open_for(self._today())

    @staticmethod
    def _today():
        return time.strftime("%Y-%m-%d", time.gmtime())

    def _open_for(self, day):
        if self.fd is not None:
            self.close_fd()
        self.day = day
        self.path = os.path.join(self.dir, "%s.jsonl" % day)
        self.fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        try:
            self.size = os.fstat(self.fd).st_size
        except OSError:
            self.size = 0
        self._enforce_cap()

    def write(self, obj, sync=False):
        if self.fd is None:
            return
        day = self._today()
        if day != self.day:
            self._open_for(day)
        line = (json.dumps(obj, separators=(",", ":"), default=str) + "\n").encode("utf-8")
        try:
            os.write(self.fd, line)
            if sync:
                os.fsync(self.fd)
        except OSError:
            return
        self.size += len(line)
        self._since_check += len(line)
        if self._since_check >= _CHECK_EVERY:
            self._since_check = 0
            self._enforce_cap()

    def _files(self):
        paths = glob.glob(os.path.join(self.dir, "*.jsonl"))
        info = []
        for p in paths:
            try:
                st = os.stat(p)
            except OSError:
                continue
            info.append((st.st_mtime, p, st.st_size))
        info.sort()
        return info

    def _enforce_cap(self):
        files = self._files()
        total = sum(s for _, _, s in files)
        while total > self.cap and files:
            _, oldest, size = files[0]
            if os.path.abspath(oldest) == os.path.abspath(self.path):
                if len(files) > 1:
                    files.pop(0)
                    continue
                # The current file alone exceeds the cap: rotate it out so the
                # next pass can drop it, and start fresh.
                rotated = self.path.replace(".jsonl", ".%d.part.jsonl" % int(time.time()))
                try:
                    os.close(self.fd)
                    os.rename(self.path, rotated)
                except OSError:
                    pass
                self.fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
                self.size = 0
                files = self._files()
                total = sum(s for _, _, s in files)
                continue
            try:
                os.unlink(oldest)
            except OSError:
                pass
            files.pop(0)
            total -= size

    def sync(self):
        if self.fd is not None:
            try:
                os.fsync(self.fd)
            except OSError:
                pass

    def close_fd(self):
        if self.fd is not None:
            try:
                os.fsync(self.fd)
            except OSError:
                pass
            try:
                os.close(self.fd)
            except OSError:
                pass
            self.fd = None

    def close(self):
        self.close_fd()
