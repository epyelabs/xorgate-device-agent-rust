"""Byte transports. They never see command strings: only the Gate writes.

RfcommTransport: Bluetooth RFCOMM socket by MAC (stdlib, no root, no rfcomm node).
SerialTransport: a serial/pty path via os.open + termios raw mode (no pyserial),
used with the ELM327 emulator and USB adapters.
"""
import errno
import os
import select
import socket
import termios


class TransportError(Exception):
    """Link-level failure (connect refused, link dropped, device gone)."""


class NoBluetooth(TransportError):
    """This host has no Bluetooth socket support / controller."""


class RfcommTransport:
    def __init__(self, mac, channel=1, connect_timeout=15.0):
        if not hasattr(socket, "AF_BLUETOOTH") or not hasattr(socket, "BTPROTO_RFCOMM"):
            raise NoBluetooth("no AF_BLUETOOTH socket support on this host")
        self.mac, self.channel = mac, int(channel)
        try:
            self.sock = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM,
                                      socket.BTPROTO_RFCOMM)
        except OSError as exc:
            raise NoBluetooth("cannot create RFCOMM socket: %s" % exc) from exc
        try:
            self.sock.settimeout(connect_timeout)
            self.sock.connect((mac, self.channel))
            self.sock.setblocking(False)
        except OSError as exc:
            self.sock.close()
            if exc.errno in (errno.ENODEV, errno.EAFNOSUPPORT, errno.ENETDOWN):
                raise NoBluetooth("Bluetooth controller unavailable: %s" % exc) from exc
            raise TransportError("rfcomm connect %s/%d failed: %s" % (mac, self.channel, exc)) from exc

    def write(self, data):
        try:
            self.sock.setblocking(True)
            self.sock.settimeout(2.0)
            self.sock.sendall(data)
        except OSError as exc:
            raise TransportError("rfcomm write failed: %s" % exc) from exc
        finally:
            self.sock.setblocking(False)

    def read(self, timeout):
        """Return bytes available within `timeout` seconds (b"" on timeout)."""
        try:
            r, _, _ = select.select([self.sock], [], [], max(0.0, timeout))
            if not r:
                return b""
            chunk = self.sock.recv(4096)
        except OSError as exc:
            raise TransportError("rfcomm read failed: %s" % exc) from exc
        if chunk == b"":
            raise TransportError("rfcomm link closed by peer")
        return chunk

    def close(self):
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass

    def describe(self):
        return "rfcomm://%s/%d" % (self.mac, self.channel)


class SerialTransport:
    def __init__(self, path, baud=38400):
        self.path = path
        try:
            self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        except OSError as exc:
            raise TransportError("open %s failed: %s" % (path, exc)) from exc
        try:
            self._set_raw(baud)
        except termios.error as exc:
            os.close(self.fd)
            raise TransportError("termios setup on %s failed: %s" % (path, exc)) from exc

    def _set_raw(self, baud):
        attrs = termios.tcgetattr(self.fd)
        iflag, oflag, cflag, lflag, ispeed, ospeed, cc = attrs
        iflag &= ~(termios.IGNBRK | termios.BRKINT | termios.PARMRK | termios.ISTRIP
                   | termios.INLCR | termios.IGNCR | termios.ICRNL | termios.IXON
                   | termios.IXOFF)
        oflag &= ~termios.OPOST
        lflag &= ~(termios.ECHO | termios.ECHONL | termios.ICANON | termios.ISIG
                   | termios.IEXTEN)
        cflag &= ~(termios.CSIZE | termios.PARENB)
        cflag |= termios.CS8 | termios.CREAD | termios.CLOCAL
        cc = list(cc)
        cc[termios.VMIN] = 0
        cc[termios.VTIME] = 0
        speed = getattr(termios, "B%d" % baud, None)
        if speed is not None:
            ispeed = ospeed = speed
        try:
            termios.tcsetattr(self.fd, termios.TCSANOW,
                              [iflag, oflag, cflag, lflag, ispeed, ospeed, cc])
        except termios.error:
            # A pty may refuse the baud rate; raw mode is what matters.
            termios.tcsetattr(self.fd, termios.TCSANOW,
                              [iflag, oflag, cflag, lflag, attrs[4], attrs[5], cc])

    def write(self, data):
        view = memoryview(data)
        while view:
            try:
                n = os.write(self.fd, view)
            except BlockingIOError:
                select.select([], [self.fd], [], 1.0)
                continue
            except OSError as exc:
                raise TransportError("serial write failed: %s" % exc) from exc
            view = view[n:]

    def read(self, timeout):
        try:
            r, _, _ = select.select([self.fd], [], [], max(0.0, timeout))
            if not r:
                return b""
            return os.read(self.fd, 4096)
        except BlockingIOError:
            return b""
        except OSError as exc:
            if exc.errno == errno.EIO:
                raise TransportError("serial device gone (EIO)") from exc
            raise TransportError("serial read failed: %s" % exc) from exc

    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass

    def describe(self):
        return "serial://%s" % self.path
