#!/usr/bin/env python3
"""Rig (Phase 5b WP4, orchestrator ruling 2026-10-23): the REAL OBD helper's
stdout through the player's telemetry hold gate.

Installed as `<work>/python/obd_read.py` (the agent spawns
`python3 <XORGATE_PYTHON_DIR>/obd_read.py <argv>`); runs the side's real helper
(`obd_read_real.py`, a symlink to it) with the same argv, stdin and stderr, and
relays each stdout line ONLY while the player's gate is open — so every helper
event the agent reacts to (a sample, `live`, a link move, the exit after a
SIGTERM) reaches BOTH agents outside a tick pair's window, the way the player
releases every other line input (NMEA, IMU). Without it the two independent
emulators' edges landed between agent A's and agent B's tick of the same pair
(stepC/p5b-obd-config.jsrs3: the rev-2 helper exit 7 ms after Rust's tick and
before JS's; the rev-6 `live` 60 ms after Rust's tick).

Before the gate, a cross-side BARRIER ($RIG_OBD_BARRIER, see `barrier`).

Gate file ($RIG_OBD_GATE): "1" open / "0" closed, rewritten by the player at
least every 500 ms. Missing, unreadable or older than 1.5 s = OPEN (the player
stopped gating: tape done, --no-gate), so the helper can never be wedged.

Signals: SIGTERM/SIGINT/SIGHUP are forwarded to the helper once (execa's
`kill()` and the Rust port signal the pid, not the group); a helper killed by a
signal re-raises it here, so the agent sees the same `helper stopped (signal)`.
"""
import json
import os
import signal
import subprocess
import sys
import time

GATE = os.environ.get("RIG_OBD_GATE")
STALE_S = 1.5
# Cross-side barrier (both agents' relays): the k-th helper EDGE (a line whose
# JSON `type` is one of EDGE_KINDS, or the process exit) is released on a side
# only once the OTHER side's relay has reached its own k-th edge — so the two
# independent emulators' edges (`live`, a link move, the profile, the exit
# after a SIGTERM) reach both agents together, and then outside a tick pair
# (the gate). ONE counter over all edge kinds (per-kind counters deadlocked
# when the two streams interleaved kinds differently: stepC
# p5b-obd-config.fin.jsrs2); samples and audits are not edges (each helper
# polls on its own phase; T8/obd-sample-split, T8/obd-audit-cycle). The count
# persists across helper restarts in $RIG_OBD_BARRIER/<side>.edge. A partner
# that never gets there releases after BARRIER_S; every wait (and any
# timeout) goes to the rig-owned $RIG_OBD_WAITLOG, which the diff judges.
EDGE_KINDS = {"ready", "status", "profile", "error", "exit"}
BARRIER = os.environ.get("RIG_OBD_BARRIER")
SIDE = os.environ.get("RIG_OBD_SIDE", "")
PEERS = [p for p in os.environ.get("RIG_OBD_PEERS", "").split(",") if p and p != SIDE]
BARRIER_S = 3.0


def _count(side, kind):
    try:
        with open(os.path.join(BARRIER, f"{side}.{kind}")) as f:
            return int(f.read() or 0)
    except (OSError, ValueError):
        return 0


# The rig-owned wait log (review-wp4 R4-4): every barrier wait, as one JSON
# line in the run dir ($RIG_OBD_WAITLOG) — never the helper's stderr, which
# the agent consumes. The diff FAILS a timeout and a wait above its bound.
WAITLOG = os.environ.get("RIG_OBD_WAITLOG")


def log_wait(rec):
    if not WAITLOG:
        return
    rec.setdefault("pid", os.getpid())  # one relay process = one helper session
    try:
        with open(WAITLOG, "a") as f:
            f.write(json.dumps(rec) + "\n")
    except OSError:
        pass


def _put(name, text):
    tmp = os.path.join(BARRIER, f".{SIDE}.{name}.tmp")
    with open(tmp, "w") as f:
        f.write(text)
    os.replace(tmp, os.path.join(BARRIER, name))


def _arrival(side, kind, n):
    try:
        with open(os.path.join(BARRIER, f"{side}.{kind}.arr.{n}")) as f:
            return float(f.read())
    except (OSError, ValueError):
        return None


def barrier(kind, what):
    """Wait for every peer's k-th edge; return the edge's SHARED release
    instant: the latest arrival among the sides (each side writes its own
    arrival before its count, so a released side can read them all). Both
    sides compute the same value from the same files — unlike each side's
    own wall clock after the wait, which differs by its scheduling (a relay
    descheduled for 0.15 s under load saw the release 0.15 s late). None
    without a barrier."""
    if not (BARRIER and SIDE and PEERS):
        return None
    n = _count(SIDE, kind) + 1
    t0 = time.time()
    _put(f"{SIDE}.{kind}.arr.{n}", repr(t0))
    _put(f"{SIDE}.{kind}", str(n))
    deadline = t0 + BARRIER_S
    timeout = False
    while any(_count(p, kind) < n for p in PEERS):
        if time.time() > deadline:
            timeout = True
            break
        time.sleep(0.005)
    arr = [t0] + [a for a in (_arrival(p, kind, n) for p in PEERS) if a is not None]
    rel = max(arr)
    log_wait({"w": int(t0 * 1000), "side": SIDE, "edge": n, "kind": what,
              "wait_ms": int((time.time() - t0) * 1000), "timeout": timeout,
              "rel": int(rel * 1000)})
    return n, rel


# The tick gate, decided ONCE per edge for both relays (review-wp4 R5-2): the
# first relay to find the player's gate open at/after the edge's shared
# earliest instant publishes that instant as the edge's GO (an exclusive
# hard link — at most one decision per edge); every relay delivers the edge at
# the published GO. Before, each relay read the gate at its own instant after
# the barrier: one found it open and delivered the `stopped` status at once,
# the other found the tick-pair window just opened and held it 270 ms
# (rev4/runs2 obd-config.rev: `metrics.obd.link` JS 2 vs Rust 0) — and the
# same held a helper EXIT on one side only, so one agent learned of the exit
# ~400 ms after the other (stepC p5b-obd-config.r28.rsjs1/.rsjs5/.jsrs4,
# relay waits ~430 ms). The gate closes guard_ms (150) before the lead tick,
# so a delivery a few ms after a GO is still outside the pair; a relay that
# reaches a GO more than GO_LATE_MS late (descheduled) logs it and the diff
# FAILS the run rather than let the two agents see the edge apart.
GO_LATE_MS = 50


def _read_float(path):
    try:
        with open(path) as f:
            return float(f.read())
    except (OSError, ValueError):
        return None


def shared_go(kind, n, earliest):
    if not (BARRIER and SIDE and PEERS):
        while time.time() < earliest:
            time.sleep(0.005)
        wait_open(report=False)
        return time.time()
    path = os.path.join(BARRIER, f"{kind}.go.{n}")
    decided = False
    _held(True)
    while True:
        go = _read_float(path)
        if go is not None:
            break
        now = time.time()
        if now >= earliest and is_open():
            tmp = os.path.join(BARRIER, f".{SIDE}.{kind}.go.{n}.tmp")
            with open(tmp, "w") as f:
                f.write(repr(now))
            try:
                os.link(tmp, path)
                go, decided = now, True
            except FileExistsError:
                pass
            finally:
                os.remove(tmp)
            if go is not None:
                break
            continue
        time.sleep(min(0.005, max(0.0005, earliest - now)))
    late_ms = max(0, int((time.time() - go) * 1000))
    log_wait({"w": int(go * 1000), "side": SIDE, "edge": n, "kind": "go",
              "decider": decided, "late_ms": late_ms})
    return go


def kind_of(line):
    try:
        return str(json.loads(line).get("type") or "line")
    except (ValueError, AttributeError):
        return None


def is_open():
    if not GATE:
        return True
    try:
        st = os.stat(GATE)
        if time.time() - st.st_mtime > STALE_S:
            return True
        with open(GATE, "rb") as f:
            return f.read(1) != b"0"
    except OSError:
        return True


# Report hold: a `status` line (a link move) makes each agent request a config
# report on its next publish tick (ts:src/worker.ts:1399-1404 `obdReportDue`),
# built 2 s later (ts:src/agent/config-sync.ts:1604-1617) — i.e. within
# [r + 2000, r + 3000] of the line's release r, at each agent's own instant.
# Any helper line released in that span could land between the two builds
# (stepC-H9 p5b-obd-crank.rp.cal09: the `profile` between them). So after a
# status line, hold from r + 2000 - REPORT_MARGIN_S to r + 3000 + REPORT_MARGIN_S.
# Both sides release their status lines together (the barrier), so the hold is
# symmetric — and it is decided on SHARED instants (review 2026-10-28): the hold
# window starts from the status edge's shared release instant (`barrier`), and
# whether a later edge is held is decided on THAT edge's shared release instant,
# so both relays always make the same decision. (Before, each relay used its
# own wall clock after the barrier: a relay descheduled for a moment released
# the status ~0.1 s later, its window started later, and a `probing` line due
# ~1.7 s after the connect status — right at the r + 1.75 edge — was held on
# one side and released on the other: stepC p5b-obd-config.r27.rsjs1, relay
# waits 1001-1890 ms. Non-edge lines (samples) use the same shared window.)
REPORT_MARGIN_S = 0.25
report_hold = None  # (start, end) epoch seconds


def in_report_hold():
    if report_hold is None or not GATE:
        return False
    now = time.time()
    return report_hold[0] <= now < report_hold[1]


def gate_epoch():
    """The player's gate-opening counter (`1 <epoch>` / `0 <epoch>`): it
    grows by one at every closed → open transition. None without one."""
    try:
        with open(GATE) as f:
            parts = f.read().split()
        return int(parts[1]) if len(parts) > 1 else None
    except (OSError, ValueError, TypeError):
        return None


# Review-wp4 R7-1: a line HELD here is flagged (`<side>.held`), so the player
# releases no barrier input (config / command / cli / LTE file) while any
# relay still holds a helper line: at a gate opening the held helper lines
# reach both agents BEFORE the input, in the same order on both sides. (Before,
# a pre-SIGINT sample held by the tick gate reached the JS after the config
# that disabled OBD — resurrecting link 2 and 23 fields — and the Rust before
# it: rev4 runs4/cfg.jsrs.)
_holding = False


def _held(on):
    """Set when a line starts to be held; cleared only AFTER it was written
    (the main loop), so the player never releases an input between the
    decision and the delivery."""
    global _holding
    if not (BARRIER and SIDE) or on == _holding:
        return
    _holding = on
    try:
        if on:
            _put(f"{SIDE}.held", repr(time.time()))
        else:
            os.remove(os.path.join(BARRIER, f"{SIDE}.held"))
    except OSError:
        pass


def wait_open(report=True):
    if is_open() and not (report and in_report_hold()):
        return
    _held(True)
    try:
        while not is_open() or (report and in_report_hold()):
            time.sleep(0.005)
    finally:
        _held(False)


def wait_open_line():
    """A held NON-edge line (a sample, an audit): released at the SHARED GO
    of the gate opening that frees it — the first relay to see that opening
    publishes it, the others deliver at it — so the tick-gate decision for a
    held line is made once for both relays (review-wp4 R7-1)."""
    if is_open() and not in_report_hold():
        return
    _held(True)
    try:
        while not is_open() or in_report_hold():
            time.sleep(0.005)
        e = gate_epoch()
        if BARRIER and SIDE and PEERS and e is not None:
            path = os.path.join(BARRIER, f"open.go.{e}")
            if _read_float(path) is None:
                tmp = os.path.join(BARRIER, f".{SIDE}.open.go.{e}.tmp")
                with open(tmp, "w") as f:
                    f.write(repr(time.time()))
                try:
                    os.link(tmp, path)
                except FileExistsError:
                    pass
                finally:
                    os.remove(tmp)
    except BaseException:
        _held(False)
        raise


# Fresh adapter state per helper start (orchestrator ruling 2026-10-27): the
# side's ELM327 emulator outlives each helper, and a reply to the PREVIOUS
# helper's last command (its shutdown close, cut by the agent's stop signal)
# can still sit in the pty's input queue. The next helper then reads it as the
# answer to its own first command and every later reply shifts by one
# (`ECU silent … 0100 -> ok ['OK']`, then ATDPN reading a 0100 answer → exit 4:
# stepB p5b-obd-cmds.cal09 on JS sides, stepC p5b-obd-config.fin2.jsrs on the
# Rust side). A real adapter is not reopened 40 ms after a kill. So before
# spawning, drain: discard what is queued, then read until the line has been
# quiet for DRAIN_QUIET_S (a reply still in flight), at most DRAIN_MAX_S. The
# drain is logged (kind "drain"); one that never went quiet FAILS in the diff.
DRAIN_QUIET_S = 0.3
DRAIN_MAX_S = 2.0


def serial_path(argv):
    for i, a in enumerate(argv):
        if a == "--serial" and i + 1 < len(argv):
            return argv[i + 1]
        if a.startswith("--serial="):
            return a.split("=", 1)[1]
    return None


def drain(path):
    import select
    import termios
    import tty

    t0 = time.time()
    n = 0
    quiet = False
    try:
        fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    except OSError:
        return
    try:
        try:
            # raw (a queued reply without a newline is readable too), applied
            # NOW: the default TCSAFLUSH would discard the queue uncounted
            tty.setraw(fd, termios.TCSANOW)
        except termios.error:
            pass
        last = time.time()
        while time.time() - t0 < DRAIN_MAX_S:
            r, _, _ = select.select([fd], [], [], 0.02)
            if r:
                try:
                    chunk = os.read(fd, 4096)
                except (BlockingIOError, OSError):
                    chunk = b""
                if chunk:
                    n += len(chunk)
                    last = time.time()
                    continue
            if time.time() - last >= DRAIN_QUIET_S:
                quiet = True
                break
        try:
            termios.tcflush(fd, termios.TCIFLUSH)
        except termios.error:
            pass
    finally:
        os.close(fd)
    log_wait({"w": int(t0 * 1000), "side": SIDE, "kind": "drain", "bytes": n,
              "ms": int((time.time() - t0) * 1000), "quiet": quiet})


_serial = serial_path(sys.argv[1:])
if _serial:
    drain(_serial)

real = os.path.join(os.path.dirname(os.path.abspath(sys.argv[0])), "obd_read_real.py")
child = subprocess.Popen([sys.executable, "-u", real] + sys.argv[1:], stdout=subprocess.PIPE)


signalled = False


def forward(sig, _frame):
    global signalled
    signalled = True  # the agent asked this helper to stop (review-wp4 R6-1)
    try:
        child.send_signal(sig)
    except ProcessLookupError:
        pass


for s in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
    signal.signal(s, forward)

out = sys.stdout.buffer
# The FIRST sample after the link goes live is an edge too: it is when the
# obd group appears in telemetry, and each helper's first poll cycle after
# `live` ends at its own instant (stepC p5b-obd-crank.fin.jsrs: the group
# present on one side one tick earlier). Later samples are not edges.
first_sample_due = False
first_sample_rel = None
for line in iter(child.stdout.readline, b""):
    k = kind_of(line)
    if k == "status":
        try:
            first_sample_due = json.loads(line).get("link") == 2
        except ValueError:
            pass
    elif k == "sample" and first_sample_due:
        first_sample_due = False
        first_sample_rel = barrier("edge", "first-sample")
    if k == "profile":
        # The helper's own measured rate (review-wp4 R4-3): the diff checks
        # each agent CARRIES exactly this value (no rounding, no float noise).
        try:
            log_wait({"w": int(time.time() * 1000), "side": SIDE, "kind": "profile-rate",
                      "rate_req_s": json.loads(line).get("rate_req_s")})
        except ValueError:
            pass
    if k == "audit":
        # The helper's audit snapshot (review-wp4 R5-1): the agent's state
        # `obd.reqPerSec` is requestRate() over two consecutive audits of its
        # CURRENT helper (ts:src/agent/collectors/obd.ts:270-278, 666-677),
        # so the diff ties it to these.
        try:
            a = json.loads(line)
            c = a.get("counters") or {}
            log_wait({"w": int(time.time() * 1000), "side": SIDE, "kind": "audit-line",
                      "ts": a.get("ts"), "requests": c.get("requests"), "ok": c.get("ok")})
        except ValueError:
            pass
    rel = None
    if k == "sample" and first_sample_rel is not None:
        rel, first_sample_rel = first_sample_rel, None
    if k in EDGE_KINDS:
        rel = barrier("edge", k)
    held_until = 0.0
    if rel is None:
        wait_open_line()
    else:
        n, rel = rel
        # The shared decisions: held iff the edge's SHARED release instant
        # lies in the report-hold window (then until its shared end), and
        # delivered at the edge's shared GO (the tick gate, decided once).
        if GATE and report_hold is not None and report_hold[0] <= rel < report_hold[1]:
            held_until = report_hold[1]
        go = shared_go("edge", n, max(rel, held_until))
    try:
        out.write(line)
        out.flush()
    except BrokenPipeError:
        _held(False)
        break
    _held(False)
    if k == "status":
        # the window starts at the status's shared delivery instant (its GO)
        r = time.time() if rel is None else go
        report_hold = (r + 2.0 - REPORT_MARGIN_S, r + 3.0 + REPORT_MARGIN_S)
rc = child.wait()
# Review-wp4 R6-1: whether the agent stopped this helper (a forwarded signal),
# so the diff knows its last audit is a stop audit, which may hold one
# request cut in flight (python/obd/gate.py:131-136, :187; session.py:90).
log_wait({"w": int(time.time() * 1000), "side": SIDE, "kind": "helper-exit",
          "rc": rc, "signalled": signalled})
# The exit is not held for a report: the agent's stop waits only a bounded
# time for the helper (SIGKILL after), which a hold could change. Its tick
# gate is the shared GO like every edge.
_x = barrier("edge", "exit")
if _x is None:
    wait_open(report=False)
else:
    shared_go("edge", _x[0], _x[1])
_held(False)
if rc < 0:
    signal.signal(-rc, signal.SIG_DFL)
    os.kill(os.getpid(), -rc)
sys.exit(rc)
