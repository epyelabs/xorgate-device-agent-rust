#!/usr/bin/env python3
"""xorgate drive blackbox: a Rust-agent-aware 5 s NDJSON recorder (BENCH ONLY).

Replaces the retired /usr/local/bin/xorgate-blackbox.sh (USER 2026-11-07).
Installed on the bench by scripts/bench/drive-blackbox-ctl.sh as
/usr/local/bin/xorgate-drive-blackbox, run by xorgate-drive-blackbox.service.

  xorgate-drive-blackbox run [--out-dir DIR] [--interval S] [--ticks N]
                             [--max-file-mb MB] [--keep N] [--total-mb MB]
                             [--min-free-mb MB] [--journal-every-ticks N]
                             [--cmd-timeout S] [--journal-timeout S]
  xorgate-drive-blackbox summary PATH...     (files and/or directories)

run: every --interval seconds (5) appends ONE compact JSON line to
  DIR/b<bseq>_<boot-start-utc>_<boot_id[:8]>[.<part>].ndjson (DIR defaults
  to /var/lib/xorgate-rs/drive-blackbox). bseq is a persistent boot sequence
  number (DIR/.boot-seq, +1 per boot): files, pruning, restart continuity and
  summary are ordered by (bseq, part), NEVER by mtime or wall clock, which is
  behind after a power cut until NTP. A recorder restart resumes THIS boot's
  own file (matched by boot_id). A file rotates at --max-file-mb (50); the
  oldest files go only while the total exceeds --total-mb (300) (--keep, a
  file-count cap, defaults to 2000 so boots are never pruned by count); the
  file being written is never deleted. Below --min-free-mb (200) free on
  DIR's filesystem a line is skipped (counted in the next line's
  "skipped"), as is a line whose write fails (ENOSPC, EROFS). Each line is
  fdatasync'ed so a power cut loses at most the line being written; a torn
  last line is newline-terminated before the next append. Every exec has a
  timeout (--cmd-timeout 5 s, --journal-timeout 15 s for the journal count).

  Line keys (any section degrades to null or {"err": ...}, never a crash):
    v, ts (UTC ms), boot (boot_id), bseq, up (uptime s), n (line no. in this
    process), dt (s since the previous tick of this process), ntp (timesyncd
    synced flag), skipped;
    pwr   rail_v (EXT5V_V), core_a (VDD_CORE_A), thr (raw get_throttled hex),
          thr_now / thr_since (decoded bit names: uv, cap, thr, temp), temp_c;
    load  avg [1, 5, 15], mem_avail_kb, mem_total_kb;
    disk  root_free_mb, xorgate_free_mb (the agent's XORGATE_HOME), out_free_mb;
    net   usb0 / wlan0 {present, up, oper}, def (default-route ifaces by
          metric), usb0_default;
    svc   rs {active, sub, pid, nrestarts, rss_kb, ticks, cpu},
          js {active, sub, pid}, lte {active, sub, pid, nrestarts};
    kids  [{comm, pid, rss_kb, ticks, cpu}] = the other pids of the Rust
          agent's cgroup (cpu = % of one core over the tick delta);
    ag    the agent's state.json ($XORGATE_HOME from /proc/<MainPID>/environ):
          src, by, age_s, mqtt, mqtt_for_s, seq, pub, pub_err, pub_age_s,
          tel_age_s, gps {fix, sats, siv, hdop, sky, ttff_s} (NO coordinates),
          lte {q, rssi, gnss_health, resets_24h, modem_temp}, kvs, media_rec,
          cams [{key, state, rec}], rec {en, sess, backlog_b, backlog_n,
          dropped, uploading, upload_age_s, shed, defer, clock_hold, block};
    lte   the modem daemon's telemetry file (read only): csq, reg, status, op,
          act, sim_reg, sim_state, cell, uplink, age_s;
    ev    change markers vs the previous line (also across a recorder restart
          in the same boot): start, boot, nrestarts, mainpid, js_active,
          throttled, mqtt. A failed read (None) never makes a marker: the
          last good value is carried forward and the key is listed in
    read_miss (e.g. ["thr", "rs_nr"]) on that line;
    jr    every --journal-every-ticks (12 = 60 s): {cursor, warn, err, from}
          = the journal's last cursor and the count of xorgate-agent-rs
          warning (4) / error (<= 3) entries since the previous snapshot,
          also across a boot change (from "prev_boot_cursor"; the journal is
          persistent). A timed-out or failed count is {"timeout": true} or
          {"fail": ...} with warn/err null and the cursor NOT advanced, so the
          next window covers it; never a zero;
    recfs with jr: files and bytes under $XORGATE_HOME/recordings per subdir.

READ-ONLY toward the agent: it only reads /proc, /sys, state.json and the
modem daemon's telemetry file, and runs `vcgencmd get_throttled`,
`vcgencmd pmic_read_adc` and one `systemctl show` per tick (plus two
`journalctl` reads per minute). It never writes agent files, signals a
process, opens a modem port, sends AT commands or touches the network.
No latitude/longitude (or any other position) is ever written: the agent
fields are whitelisted and every line is scrubbed of position keys.

summary: a privacy-safe digest of one or more files (or directories): span and
boots, rail min/max, throttled transitions, restarts, gaps > 15 s, MQTT
disconnect windows, GPS fix-loss windows, peak RSS/CPU per process.

Why python3 and not bash + jq: jq is not on Raspberry Pi OS Bookworm Lite,
and one long-lived python3 reading /proc and /sys directly forks 3 times per
tick where a bash loop forks dozens of times.

Tests: DRIVE_BB_ROOT=<dir> prefixes every /proc, /sys, /run and /var path
(scripts/bench/tests/drive-blackbox.sh).
"""

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time

ROOT = os.environ.get("DRIVE_BB_ROOT", "")
SCHEMA = 1
RS_UNIT = "xorgate-agent-rs.service"
JS_UNIT = "xorgate-agent.service"
LTE_UNIT = "sim7600-lte.service"
DEFAULT_OUT = "/var/lib/xorgate-rs/drive-blackbox"
DEFAULT_HOME = "/var/lib/xorgate"
DEFAULT_LTE_FILE = "/run/sim7600-lte/telemetry.json"
CURSOR_FILE = ".journal-cursor"
# Never written, at any depth (belt and braces: the agent fields are already
# whitelisted).
POSITION_KEYS = {"lat", "lon", "lng", "latitude", "longitude", "alt", "altitude",
                 "speed", "course", "heading", "position", "coords", "location"}
THR_BITS = ((0, "uv"), (1, "cap"), (2, "thr"), (3, "temp"))
GAP_S = 15.0
HZ = os.sysconf("SC_CLK_TCK")
PAGE_KB = os.sysconf("SC_PAGE_SIZE") // 1024


def P(path):
    return ROOT + path


def read_text(path, limit=1 << 20):
    try:
        with open(path, "rb") as f:
            return f.read(limit).decode("utf-8", "replace")
    except OSError:
        return None


def read_json(path):
    t = read_text(path, 4 << 20)
    if t is None:
        return None, "missing"
    try:
        return json.loads(t), None
    except ValueError:
        return None, "unparseable"


TIMEOUTS = {"cmd": 5.0, "journal": 15.0}  # set from --cmd-timeout / --journal-timeout


def run_cmd_st(argv, timeout=None):
    """(stdout or None, status): ok | fail (non-zero exit) | timeout | missing."""
    try:
        r = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                           stderr=subprocess.DEVNULL, check=False,
                           timeout=TIMEOUTS["cmd"] if timeout is None else timeout)
    except subprocess.TimeoutExpired:
        return None, "timeout"
    except (OSError, subprocess.SubprocessError):
        return None, "missing"
    if r.returncode != 0:
        return None, "fail"
    return r.stdout.decode("utf-8", "replace"), "ok"


def run_cmd(argv, timeout=None):
    return run_cmd_st(argv, timeout)[0]


def num(v):
    return v if isinstance(v, (int, float)) and not isinstance(v, bool) else None


def leaf(group, key):
    """metrics.<group>.<key>.value, or None."""
    if not isinstance(group, dict):
        return None
    lf = group.get(key)
    return num(lf.get("value")) if isinstance(lf, dict) else None


def dget(d, *keys):
    for k in keys:
        if not isinstance(d, dict):
            return None
        d = d.get(k)
    return d


def age_s(now_ms, then_ms):
    then_ms = num(then_ms)
    return None if then_ms is None else round((now_ms - then_ms) / 1000.0, 1)


def compact(d):
    return {k: v for k, v in d.items() if v is not None}


def scrub(o):
    if isinstance(o, dict):
        return {k: scrub(v) for k, v in o.items() if k.lower() not in POSITION_KEYS}
    if isinstance(o, list):
        return [scrub(v) for v in o]
    return o


def thr_names(mask, shift):
    return [n for b, n in THR_BITS if mask >> (b + shift) & 1]


def uptime():
    t = read_text(P("/proc/uptime"))
    return float(t.split()[0]) if t else 0.0


def boot_id():
    t = read_text(P("/proc/sys/kernel/random/boot_id"))
    return t.strip() if t else "unknown"


def proc_stat(pid):
    """(comm, utime+stime ticks, rss kB, starttime) of /proc/<pid>/stat, or None."""
    s = read_text(P("/proc/%d/stat" % pid))
    if not s or ")" not in s:
        return None
    comm = s[s.find("(") + 1:s.rfind(")")]
    f = s[s.rfind(")") + 2:].split()
    try:
        return comm, int(f[11]) + int(f[12]), int(f[21]) * PAGE_KB, int(f[19])
    except (IndexError, ValueError):
        return None


def proc_environ(pid):
    t = read_text(P("/proc/%d/environ" % pid))
    if not t:
        return {}
    env = {}
    for kv in t.split("\0"):
        k, sep, v = kv.partition("=")
        if sep:
            env[k] = v
    return env


def cgroup_pids(main_pid):
    path = None
    if main_pid:
        for line in (read_text(P("/proc/%d/cgroup" % main_pid)) or "").splitlines():
            if line.startswith("0::"):
                path = line[3:].strip()
    base = P("/sys/fs/cgroup" + (path or "/system.slice/" + RS_UNIT))
    pids = set()
    for d, _subdirs, files in os.walk(base):
        if "cgroup.procs" in files:
            for tok in (read_text(os.path.join(d, "cgroup.procs")) or "").split():
                if tok.isdigit():
                    pids.add(int(tok))
    return sorted(pids)


def statvfs_free_mb(path):
    try:
        st = os.statvfs(path)
    except OSError:
        return None
    return st.f_bavail * st.f_frsize // (1024 * 1024)


def parse_systemctl_show(text):
    units, cur = {}, {}
    for line in (text or "").splitlines() + [""]:
        if not line.strip():
            if cur.get("Id"):
                units[cur["Id"]] = cur
            cur = {}
            continue
        k, _, v = line.partition("=")
        cur[k] = v
    return units


def to_int(v):
    try:
        return int(v)
    except (TypeError, ValueError):
        return None


NAME_RE = re.compile(r"^b(\d+)_(\d{8}T\d{6}Z)_([0-9a-f]{8})(?:\.(\d+))?\.ndjson$")
BSEQ_FILE = ".boot-seq"


def scan_files(d):
    """[(bseq, part, name, boot8)] of d's .ndjson files, oldest first by
    (bseq, part); a foreign name sorts first (bseq -1)."""
    out = []
    try:
        names = os.listdir(d)
    except OSError:
        return out
    for f in names:
        m = NAME_RE.match(f)
        if m:
            out.append((int(m.group(1)), int(m.group(4) or 0), f, m.group(3)))
        elif f.endswith(".ndjson"):
            out.append((-1, 0, f, None))
    out.sort()
    return out


def tail_lines(path, limit=262144):
    """The parsed JSON lines in the last `limit` bytes of path (oldest first)."""
    try:
        with open(path, "rb") as f:
            f.seek(0, 2)
            f.seek(max(0, f.tell() - limit))
            raw = f.read().decode("utf-8", "replace").splitlines()
    except OSError:
        return []
    out = []
    for ln in raw:
        try:
            d = json.loads(ln)
        except ValueError:
            continue
        if isinstance(d, dict) and "ts" in d:
            out.append(d)
    return out


class Out:
    """The NDJSON files: naming (bseq), append, rotation, caps."""

    def __init__(self, out_dir, boot, up, max_file, keep, total, min_free_mb):
        self.dir, self.max_file, self.keep, self.total = out_dir, max_file, keep, total
        self.min_free_mb = min_free_mb
        self.boot8 = boot.replace("-", "")[:8]
        self.fd = None
        self.skipped = 0
        os.makedirs(out_dir, mode=0o755, exist_ok=True)
        files = scan_files(out_dir)
        own = [x for x in files if x[3] == self.boot8]
        if own:  # a recorder restart in the same boot: THIS boot's file, whatever the mtimes say
            self.bseq, self.part, name, _b = own[-1]
            self.base = name.split(".")[0]
            self.resumed = True
        else:
            self.bseq = max([x[0] for x in files] + [self._read_bseq(), 0]) + 1
            start = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(time.time() - up))
            self.base, self.part, self.resumed = "b%06d_%s_%s" % (self.bseq, start, self.boot8), 0, False
            self._write_bseq()
        prev = [x for x in files if x[3] != self.boot8 and x[0] < self.bseq]
        self.prev_file = os.path.join(out_dir, prev[-1][2]) if prev else None
        self.prune()

    def _read_bseq(self):
        return to_int((read_text(os.path.join(self.dir, BSEQ_FILE)) or "").strip()) or 0

    def _write_bseq(self):
        try:
            tmp = os.path.join(self.dir, BSEQ_FILE + ".tmp")
            with open(tmp, "w") as f:
                f.write("%d\n" % self.bseq)
            os.replace(tmp, os.path.join(self.dir, BSEQ_FILE))
        except OSError:
            pass

    def path(self):
        return os.path.join(self.dir, self.base + ("" if self.part == 0 else ".%d" % self.part) + ".ndjson")

    def _open(self):
        p = self.path()
        self.fd = os.open(p, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
        size = os.fstat(self.fd).st_size
        if size:  # a torn last line (power cut mid-write) gets its newline
            with open(p, "rb") as f:
                f.seek(size - 1)
                if f.read(1) != b"\n":
                    os.write(self.fd, b"\n")

    def write(self, line):
        free = statvfs_free_mb(self.dir)
        if free is not None and free < self.min_free_mb:
            self.skipped += 1
            return False
        if self.fd is None or not os.path.exists(self.path()):
            self.close()
            self._open()
        os.write(self.fd, line.encode() + b"\n")
        try:
            os.fdatasync(self.fd)
        except OSError:
            pass
        self.skipped = 0
        if os.fstat(self.fd).st_size >= self.max_file:
            self.close()
            self.part += 1
            self.prune()
        return True

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None

    def prune(self):
        """Oldest (bseq, part) first, never the current file, while the total
        is over --total-mb (or the count over --keep, 2000 by default)."""
        cur = os.path.basename(self.path())
        files = []
        for _bseq, _part, f, _b in scan_files(self.dir):
            try:
                files.append((f, os.stat(os.path.join(self.dir, f)).st_size))
            except OSError:
                continue
        # the current file counts even before its first line (right after a rotation)
        count = len(files) + (0 if any(f == cur for f, _s in files) else 1)
        total = sum(sz for _f, sz in files)
        for f, sz in files:
            if count <= self.keep and total <= self.total:
                break
            if f == cur:
                continue
            try:
                os.unlink(os.path.join(self.dir, f))
            except OSError:
                continue
            count, total = count - 1, total - sz


class Recorder:
    def __init__(self, args):
        self.args = args
        self.boot = boot_id()
        self.out = Out(args.out_dir, self.boot, uptime(), int(args.max_file_mb * 1048576),
                       args.keep, int(args.total_mb * 1048576), args.min_free_mb)
        self.n = 0
        self.last_mono = None
        self.prev_ticks = {}  # pid -> (ticks, uptime)
        self.prev = {}        # change-marker state
        self.adc_fail = 0
        self.vcgencmd = shutil.which("vcgencmd")
        self.cur_ticks = {}
        self.env_pid = None
        self.env = {}
        self.jcursor, self.jrt = self._load_cursor()
        self.start_ev = [{"k": "start", "pid": os.getpid(), "v": SCHEMA}]
        self._restore()

    # -- restart continuity -------------------------------------------------
    def _restore(self):
        if self.out.resumed:  # same boot: continuity from THIS boot's own file only
            own = [ln for ln in tail_lines(self.out.path()) if ln.get("boot") == self.boot]
            if not own:
                return
            ln = own[-1]
            up = num(ln.get("up"))
            rs = dget(ln, "svc", "rs") or {}
            if up is not None:
                if num(rs.get("pid")) and num(rs.get("ticks")) is not None:
                    self.prev_ticks[rs["pid"]] = (rs["ticks"], up, None)
                for k in ln.get("kids") or []:
                    if isinstance(k, dict) and num(k.get("pid")) and num(k.get("ticks")) is not None:
                        self.prev_ticks[k["pid"]] = (k["ticks"], up, None)
            # each marker key: its newest non-null value (a miss line carries nothing)
            for old in own:
                for key, v in self._marker_state(old).items():
                    if v is not None:
                        self.prev[key] = v
            return
        prev = tail_lines(self.out.prev_file)[-1:] if self.out.prev_file else []
        if not prev:
            if not self.out.prev_file:
                self.start_ev[0]["first"] = True
            return
        ln = prev[0]
        self.start_ev.append(compact({"k": "boot", "prev_boot": ln.get("boot"), "prev_bseq": ln.get("bseq"),
                                      "prev_ts": ln.get("ts"), "prev_up": ln.get("up")}))

    @staticmethod
    def _marker_state(ln):
        return {
            "rs_nr": dget(ln, "svc", "rs", "nrestarts"), "rs_pid": dget(ln, "svc", "rs", "pid"),
            "lte_nr": dget(ln, "svc", "lte", "nrestarts"), "lte_pid": dget(ln, "svc", "lte", "pid"),
            "js": dget(ln, "svc", "js", "active"), "thr": dget(ln, "pwr", "thr"),
            "mqtt": dget(ln, "ag", "mqtt"),
        }

    def _load_cursor(self):
        t = read_text(os.path.join(self.args.out_dir, CURSOR_FILE))
        try:
            d = json.loads(t) if t else {}
        except ValueError:
            d = {}
        return d.get("cursor"), d.get("rt")

    # -- sections -----------------------------------------------------------
    def cpu(self, pid, ticks, up, start=None):
        p = self.prev_ticks.get(pid)
        self.cur_ticks[pid] = (ticks, up, start)
        if not p or up <= p[1] or ticks < p[0] or (p[2] is not None and start is not None and p[2] != start):
            return None
        return round((ticks - p[0]) / HZ / (up - p[1]) * 100.0, 1)

    def power(self):
        out = {"rail_v": None, "core_a": None, "thr": None}
        if self.n % 60 == 0 and not self.vcgencmd:  # re-check a missing vcgencmd every 5 min
            self.vcgencmd = shutil.which("vcgencmd")
        thr = run_cmd([self.vcgencmd, "get_throttled"]) if self.vcgencmd else None
        m = re.search(r"throttled=(0x[0-9a-fA-F]+)", thr or "")
        if m:
            mask = int(m.group(1), 16)
            out.update(thr="0x%x" % mask, thr_now=thr_names(mask, 0), thr_since=thr_names(mask, 16))
        if self.vcgencmd and self.adc_fail < 3:  # 3 misses in a row (a Pi 4): stop asking
            adc = run_cmd([self.vcgencmd, "pmic_read_adc"])
            self.adc_fail = 0 if adc and "EXT5V" in adc else self.adc_fail + 1
            for line in (adc or "").splitlines():
                mm = re.match(r"\s*(\S+)_([AV]) (?:current|volt)\(\d+\)=([\d.]+)[AV]", line)
                if mm and mm.group(1) == "EXT5V" and mm.group(2) == "V":
                    out["rail_v"] = round(float(mm.group(3)), 3)
                elif mm and mm.group(1) == "VDD_CORE" and mm.group(2) == "A":
                    out["core_a"] = round(float(mm.group(3)), 3)
        t = read_text(P("/sys/class/thermal/thermal_zone0/temp"))
        out["temp_c"] = round(int(t) / 1000.0, 1) if t and t.strip().lstrip("-").isdigit() else None
        return out

    @staticmethod
    def load():
        la = (read_text(P("/proc/loadavg")) or "").split()
        mem = {}
        for line in (read_text(P("/proc/meminfo")) or "").splitlines():
            k, _, v = line.partition(":")
            if k in ("MemAvailable", "MemTotal"):
                mem[k] = to_int(v.split()[0]) if v.split() else None
        return {"avg": [float(x) for x in la[:3]] if len(la) >= 3 else None,
                "mem_avail_kb": mem.get("MemAvailable"), "mem_total_kb": mem.get("MemTotal")}

    def disk(self):
        return {"root_free_mb": statvfs_free_mb(P("/")),
                "xorgate_free_mb": statvfs_free_mb(P(self.env.get("XORGATE_HOME") or DEFAULT_HOME)),
                "out_free_mb": statvfs_free_mb(self.args.out_dir)}

    @staticmethod
    def net():
        out = {}
        for ifc in ("usb0", "wlan0"):
            d = P("/sys/class/net/" + ifc)
            if not os.path.isdir(d):
                out[ifc] = {"present": False, "up": False}
                continue
            flags = to_int_hex(read_text(d + "/flags"))
            oper = (read_text(d + "/operstate") or "").strip() or None
            out[ifc] = {"present": True, "up": bool(flags is not None and flags & 1), "oper": oper}
        routes = []
        for line in (read_text(P("/proc/net/route")) or "").splitlines()[1:]:
            f = line.split()
            if len(f) >= 8 and f[1] == "00000000" and f[7] == "00000000" and (to_int_hex(f[3]) or 0) & 1:
                routes.append((to_int(f[6]) or 0, f[0]))
        out["def"] = [i for _m, i in sorted(routes)]
        out["usb0_default"] = "usb0" in out["def"]
        return out

    def services(self):
        units = parse_systemctl_show(run_cmd(
            ["systemctl", "show", "--no-pager", "-p", "Id,ActiveState,SubState,MainPID,NRestarts",
             RS_UNIT, JS_UNIT, LTE_UNIT]))

        def base(u):
            s = units.get(u)
            if not s:
                return {"active": None}
            return {"active": s.get("ActiveState"), "sub": s.get("SubState"), "pid": to_int(s.get("MainPID")),
                    "nrestarts": to_int(s.get("NRestarts"))}
        rs, js, lte = base(RS_UNIT), base(JS_UNIT), base(LTE_UNIT)
        js.pop("nrestarts", None)
        if not units:  # systemctl show failed: no MainPID, so no cgroup guess either
            return {"rs": rs, "js": js, "lte": lte}, []
        pid = rs.get("pid") or 0
        if pid:
            st = proc_stat(pid)
            if st:
                rs.update(rss_kb=st[2], ticks=st[1], cpu=self.cpu(pid, st[1], uptime(), st[3]))
        kids = []
        for kp in cgroup_pids(pid):
            if kp == pid:
                continue
            st = proc_stat(kp)
            if st:
                kids.append({"comm": st[0], "pid": kp, "rss_kb": st[2], "ticks": st[1],
                             "cpu": self.cpu(kp, st[1], uptime(), st[3])})
        return {"rs": rs, "js": js, "lte": lte}, kids

    def agent(self, now_ms, rs_pid):
        if rs_pid and rs_pid != self.env_pid:
            env = proc_environ(rs_pid)
            if env:
                self.env, self.env_pid = env, rs_pid
        home = self.env.get("XORGATE_HOME") or DEFAULT_HOME
        src = home.rstrip("/") + "/state.json"
        st, err = read_json(P(src))
        if err or not isinstance(st, dict):
            return {"src": src, "err": err or "not an object"}
        latest = st.get("latest") if isinstance(st.get("latest"), dict) else {}
        metrics = latest.get("metrics") if isinstance(latest.get("metrics"), dict) else {}
        gps = metrics.get("gps")
        lte = metrics.get("lte")
        media = latest.get("media") if isinstance(latest.get("media"), dict) else {}
        rec = st.get("recording") if isinstance(st.get("recording"), dict) else latest.get("recording")
        ag = {
            "src": src, "by": st.get("writtenBy"), "age_s": age_s(now_ms, st.get("lastWriteAt")),
            "mqtt": st.get("connection"), "mqtt_for_s": age_s(now_ms, st.get("connectionSince")),
            "seq": num(st.get("seq")), "pub": num(st.get("published")), "pub_err": num(st.get("publishErrors")),
            "pub_age_s": age_s(now_ms, st.get("lastPublishAt")), "tel_age_s": age_s(now_ms, latest.get("ts")),
            "gps": compact({"fix": leaf(gps, "fix_quality"), "sats": leaf(gps, "satellites"),
                            "siv": leaf(gps, "satellites_in_view"), "hdop": leaf(gps, "hdop"),
                            "sky": leaf(gps, "sky_state"), "ttff_s": leaf(gps, "ttff_s")})
            if isinstance(gps, dict) else None,
            "lte": compact({"q": leaf(lte, "quality"), "rssi": leaf(lte, "rssi"),
                            "gnss_health": leaf(lte, "gnss_health"), "resets_24h": leaf(lte, "resets_24h"),
                            "modem_temp": leaf(lte, "modem_temp")}) if isinstance(lte, dict) else None,
            "kvs": dget(media, "kvs", "value"), "media_rec": dget(media, "recording", "value"),
        }
        streams = media.get("streams")
        if isinstance(streams, list):
            ag["cams"] = [compact({"key": s.get("streamKey"), "state": s.get("state"), "rec": s.get("recording")})
                          for s in streams if isinstance(s, dict)]
        if isinstance(rec, dict):
            ag["rec"] = compact({
                "en": rec.get("enabled"), "sess": rec.get("sessionId"), "backlog_b": num(rec.get("backlogBytes")),
                "backlog_n": num(rec.get("backlogSegments")), "dropped": num(rec.get("droppedSegments")),
                "uploading": rec.get("uploading"), "upload_age_s": age_s(now_ms, rec.get("lastUploadAt")),
                "shed": rec.get("powerShed"), "defer": rec.get("bootDefer"), "clock_hold": rec.get("clockHold"),
                "block": dget(rec, "liveBlock", "reason")})
        return {k: v for k, v in ag.items() if v is not None or k in ("mqtt", "gps")}

    def modem(self, now_ms):
        src = self.env.get("LTE_TELEMETRY_FILE") or DEFAULT_LTE_FILE
        t, err = read_json(P(src))
        if err or not isinstance(t, dict):
            return {"err": err or "not an object"}
        age = None
        ts = t.get("timestamp")
        if isinstance(ts, str):
            try:
                age = round(now_ms / 1000.0 - parse_iso(ts), 1)
            except ValueError:
                age = None
        return compact({"csq": num(t.get("csq")), "reg": num(t.get("reg")), "status": t.get("status"),
                        "op": dget(t, "sim", "carrier"), "act": dget(t, "sim", "act"),
                        "sim_reg": dget(t, "sim", "registration"), "sim_state": dget(t, "sim", "state"),
                        "cell": dget(t, "cellular", "state"), "uplink": dget(t, "uplink", "primary"), "age_s": age})

    def journal(self):
        cur, st = run_cmd_st(["journalctl", "-q", "-n", "1", "-o", "json", "--output-fields=__CURSOR", "--no-pager"])
        if cur is None:  # "err" is the error COUNT, so failures use their own keys
            return {"timeout": True} if st == "timeout" else {"fail": "journalctl " + st}
        cursor = rt = mono = jboot = None
        for line in cur.splitlines():
            try:
                e = json.loads(line)
            except ValueError:
                continue
            cursor, rt = e.get("__CURSOR"), to_int(e.get("__REALTIME_TIMESTAMP"))
            mono, jboot = to_int(e.get("__MONOTONIC_TIMESTAMP")), e.get("_BOOT_ID")
        argv = ["journalctl", "-q", "-u", RS_UNIT, "-p", "warning", "-o", "json",
                "--output-fields=PRIORITY", "--no-pager"]
        tmo = TIMEOUTS["journal"]
        if isinstance(self.jcursor, str):  # also across a boot change: the journal is persistent
            frm = "cursor" if ("b=" + self.boot.replace("-", "")) in self.jcursor else "prev_boot_cursor"
            out, st = run_cmd_st(argv + ["--after-cursor=" + self.jcursor], timeout=tmo)
            if out is None and st == "fail" and self.jrt:  # the cursor was vacuumed away
                frm = "since"
                out, st = run_cmd_st(argv + ["--since=@%d" % (self.jrt // 1000000)], timeout=tmo)
        else:
            frm = "boot"
            out, st = run_cmd_st(argv + ["-b"], timeout=tmo)
        if out is None:  # never a zero window: the cursor stays, the next snapshot covers it
            r = {"cursor": self.jcursor, "warn": None, "err": None, "from": frm}
            r.update({"timeout": True} if st == "timeout" else {"fail": st})
            return r
        warn = err = 0
        for line in out.splitlines():
            try:
                e = json.loads(line)
            except ValueError:
                continue
            em = to_int(e.get("__MONOTONIC_TIMESTAMP"))
            if jboot and mono is not None and e.get("_BOOT_ID") == jboot and em is not None and em > mono:
                continue  # after this snapshot's cursor (boot + monotonic, not the wall clock): next window
            pr = to_int(e.get("PRIORITY"))
            if pr is not None and pr <= 3:
                err += 1
            elif pr == 4:
                warn += 1
        if cursor:
            self.jcursor, self.jrt = cursor, rt
            try:
                tmp = os.path.join(self.args.out_dir, CURSOR_FILE + ".tmp")
                with open(tmp, "w") as f:
                    json.dump({"cursor": cursor, "rt": rt}, f)
                os.replace(tmp, os.path.join(self.args.out_dir, CURSOR_FILE))
            except OSError:
                pass
        return {"cursor": cursor, "warn": warn, "err": err, "from": frm}

    def recfs(self):
        root = P((self.env.get("XORGATE_HOME") or DEFAULT_HOME).rstrip("/") + "/recordings")
        if not os.path.isdir(root):
            return None
        out = {}
        for d, _subdirs, files in os.walk(root):
            rel = os.path.relpath(d, root)
            top = "." if rel == "." else rel.split(os.sep)[0]
            e = out.setdefault(top, {"n": 0, "b": 0})
            for f in files:
                try:
                    e["b"] += os.lstat(os.path.join(d, f)).st_size
                    e["n"] += 1
                except OSError:
                    pass
        return {k: v for k, v in out.items() if v["n"] or k != "."}

    # -- one tick -------------------------------------------------------------
    def events(self, line):
        """(markers, read_miss). A key that reads None keeps its last good
        value (carried forward) and is listed as a miss: no phantom marker on
        the miss tick or on the recovery tick."""
        raw, p = self._marker_state(line), self.prev
        miss = sorted(k for k, v in raw.items() if v is None and p.get(k) is not None)
        cur = {k: (v if v is not None else p.get(k)) for k, v in raw.items()}
        ev = []
        if p:
            for key, unit, kind in (("rs_nr", "rs", "nrestarts"), ("rs_pid", "rs", "mainpid"),
                                    ("lte_nr", "lte", "nrestarts"), ("lte_pid", "lte", "mainpid")):
                if p.get(key) != cur[key] and cur[key] is not None:
                    ev.append({"k": kind, "u": unit, "from": p.get(key), "to": cur[key]})
            if p.get("js") != cur["js"] and cur["js"] is not None:
                ev.append({"k": "js_active", "from": p.get("js"), "to": cur["js"]})
            if p.get("thr") != cur["thr"] and cur["thr"] is not None:
                a, b = to_int_hex(p.get("thr")) or 0, to_int_hex(cur["thr"]) or 0
                bits = [(bb + s, (n if s == 0 else n + "_since")) for s in (0, 16) for bb, n in THR_BITS]
                ev.append({"k": "throttled", "from": p.get("thr"), "to": cur["thr"],
                           "set": [n for bb, n in bits if b >> bb & 1 and not a >> bb & 1],
                           "clr": [n for bb, n in bits if a >> bb & 1 and not b >> bb & 1]})
            if p.get("mqtt") != cur["mqtt"] and cur["mqtt"] is not None:
                ev.append({"k": "mqtt", "from": p.get("mqtt"), "to": cur["mqtt"]})
        self.prev = cur
        return ev, miss

    def tick(self):
        mono = time.monotonic()
        now_ms = int(time.time() * 1000)
        up = uptime()
        self.cur_ticks = {}
        line = {"v": SCHEMA, "ts": now_ms, "boot": self.boot, "bseq": self.out.bseq, "up": round(up, 2), "n": self.n + 1,
                "dt": None if self.last_mono is None else round(mono - self.last_mono, 2),
                "ntp": os.path.exists(P("/run/systemd/timesync/synchronized"))}
        if self.out.skipped:
            line["skipped"] = self.out.skipped
        rs_pid = None

        def section(name, fn, *a):
            try:
                line[name] = fn(*a)
            except Exception as e:  # noqa: BLE001 — one bad section never costs the line
                line[name] = {"err": "%s: %s" % (type(e).__name__, str(e)[:120])}
        section("pwr", self.power)
        section("load", self.load)
        section("disk", self.disk)
        section("net", self.net)
        try:
            line["svc"], line["kids"] = self.services()
            rs_pid = line["svc"]["rs"].get("pid")
        except Exception as e:  # noqa: BLE001
            line["svc"], line["kids"] = {"err": "%s: %s" % (type(e).__name__, str(e)[:120])}, []
        section("ag", self.agent, now_ms, rs_pid)
        section("lte", self.modem, now_ms)
        marks, miss = self.events(line)
        ev = list(self.start_ev) + marks
        self.start_ev = []
        if ev:
            line["ev"] = ev
        if miss:
            line["read_miss"] = miss
        if self.n % max(1, self.args.journal_every_ticks) == 0:
            section("jr", self.journal)
            section("recfs", self.recfs)
        # prune pids that went away
        self.prev_ticks = self.cur_ticks
        try:
            self.out.write(json.dumps(scrub(line), separators=(",", ":"), ensure_ascii=True))
        except OSError:  # disk full / read-only: count it, retry the file next tick
            self.out.close()
            self.out.skipped += 1
        self.last_mono = mono
        self.n += 1


def to_int_hex(v):
    if v is None:
        return None
    try:
        return int(str(v).strip(), 16)
    except ValueError:
        return None


def parse_iso(s):
    """Seconds since the epoch of an ISO-8601 UTC timestamp."""
    import calendar
    m = re.match(r"(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(\.\d+)?(Z|[+-]00:?00)?$", s.strip())
    if not m:
        raise ValueError(s)
    t = calendar.timegm(tuple(int(m.group(i)) for i in range(1, 7)) + (0, 0, 0))
    return t + (float(m.group(7)) if m.group(7) else 0.0)


class Stop(Exception):
    pass


def cmd_run(args):
    def stop(_sig, _frm):
        raise Stop()
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    rec = Recorder(args)
    nxt = time.monotonic()
    try:
        while True:
            rec.tick()
            if args.ticks and rec.n >= args.ticks:
                break
            nxt += args.interval
            delay = nxt - time.monotonic()
            if delay < 0:  # a slow tick (I/O stall): skip ahead, never burst
                nxt, delay = time.monotonic(), 0
            time.sleep(delay)
    except Stop:
        pass
    finally:
        rec.out.close()
    return 0


# --- summary -------------------------------------------------------------------

def load_lines(paths):
    files = []
    for p in paths:
        if not os.path.exists(p):
            raise SystemExit("summary: %s: no such file or directory" % p)
        if os.path.isdir(p):
            files += [os.path.join(p, f) for f in os.listdir(p) if f.endswith(".ndjson")]
        else:
            files.append(p)
    recs, bad, per_file = [], 0, []
    for f in files:
        rs = []
        with open(f, "rb") as fh:
            for ln in fh.read().decode("utf-8", "replace").splitlines():
                if not ln.strip():
                    continue
                try:
                    d = json.loads(ln)
                except ValueError:
                    bad += 1
                    continue
                if isinstance(d, dict) and num(d.get("ts")) is not None:
                    rs.append(d)
                else:
                    bad += 1
        m = NAME_RE.match(os.path.basename(f))
        bseq = int(m.group(1)) if m else (num(rs[0].get("bseq")) if rs and num(rs[0].get("bseq")) is not None else -1)
        part = int(m.group(4) or 0) if m else 0
        # boot sequence, then part: never the wall clock (behind after a power cut)
        per_file.append(((bseq, part, rs[0]["ts"] if rs else 0, f), rs))
    per_file.sort(key=lambda x: x[0])
    for _k, rs in per_file:
        recs += rs
    return recs, bad, len(files)


def fmt_ts(ms):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ms / 1000.0)) if num(ms) is not None else "?"


def windows(recs, bad_fn):
    """[(start_ts, end_ts or None, n)] of consecutive records where bad_fn(r)."""
    out, cur = [], None
    for r in recs:
        if bad_fn(r):
            if cur is None:
                cur = [r["ts"], None, 0]
            cur[2] += 1
        elif cur is not None:
            cur[1] = r["ts"]
            out.append(tuple(cur))
            cur = None
    if cur is not None:
        out.append(tuple(cur))
    return out


def cmd_summary(args):
    recs, bad, nfiles = load_lines(args.paths)
    w = sys.stdout.write
    w("drive-blackbox summary: %d line(s) in %d file(s), %d malformed\n" % (len(recs), nfiles, bad))
    if not recs:
        return 1
    w("span: %s -> %s\n" % (fmt_ts(recs[0]["ts"]), fmt_ts(recs[-1]["ts"])))
    boots = []
    for r in recs:
        if not boots or boots[-1][0] != r.get("boot"):
            boots.append([r.get("boot"), r["ts"], r["ts"], 0, r.get("up"), r.get("up")])
        b = boots[-1]
        b[2], b[3], b[5] = r["ts"], b[3] + 1, r.get("up")
    w("boots: %d\n" % len(boots))
    for b in boots:
        w("  %s  %s -> %s  %d line(s)  uptime %s -> %s s\n" % (str(b[0])[:8], fmt_ts(b[1]), fmt_ts(b[2]), b[3], b[4], b[5]))

    def series(path):
        out = []
        for r in recs:
            v = dget(r, *path)
            if num(v) is not None:
                out.append((v, r["ts"]))
        return out
    w("power:\n")
    for label, path in (("rail_v", ("pwr", "rail_v")), ("core_a", ("pwr", "core_a")), ("temp_c", ("pwr", "temp_c"))):
        s = series(path)
        if s:
            lo, hi = min(s), max(s)
            w("  %s min %s (%s) max %s (%s)\n" % (label, lo[0], fmt_ts(lo[1]), hi[0], fmt_ts(hi[1])))
        else:
            w("  %s: no data\n" % label)
    uv = sum(1 for r in recs if "uv" in (dget(r, "pwr", "thr_now") or []))
    w("  under-voltage NOW in %d line(s)\n" % uv)
    evs = [(r["ts"], e) for r in recs for e in (r.get("ev") or []) if isinstance(e, dict)]
    thr = [(t, e) for t, e in evs if e.get("k") == "throttled"]
    w("throttled transitions: %d\n" % len(thr))
    for t, e in thr[:50]:
        w("  %s %s -> %s set=%s clr=%s\n" % (fmt_ts(t), e.get("from"), e.get("to"),
                                             ",".join(e.get("set") or []) or "-", ",".join(e.get("clr") or []) or "-"))
    rst = [(t, e) for t, e in evs if e.get("k") in ("nrestarts", "mainpid", "js_active", "start", "boot")]
    w("restarts / pid changes / recorder starts / boots: %d\n" % len(rst))
    for t, e in rst[:100]:
        if e.get("k") in ("nrestarts", "mainpid"):
            w("  %s %s %s %s -> %s\n" % (fmt_ts(t), e.get("u"), e.get("k"), e.get("from"), e.get("to")))
        elif e.get("k") == "js_active":
            w("  %s JS agent ActiveState %s -> %s  (must stay inactive)\n" % (fmt_ts(t), e.get("from"), e.get("to")))
        elif e.get("k") == "boot":
            w("  %s new boot (previous boot %s, its last line %s)\n" % (fmt_ts(t), str(e.get("prev_boot"))[:8], fmt_ts(e.get("prev_ts"))))
        else:
            w("  %s recorder start%s\n" % (fmt_ts(t), " (first ever)" if e.get("first") else ""))
    gaps = []
    for a, b in zip(recs, recs[1:]):
        same = a.get("boot") == b.get("boot")
        if same and num(a.get("up")) is not None and num(b.get("up")) is not None:
            g = b["up"] - a["up"]
        else:
            g = (b["ts"] - a["ts"]) / 1000.0
        if g > GAP_S or not same:
            gaps.append((a["ts"], b["ts"], g, same))
    w("gaps > %d s or boot changes: %d\n" % (GAP_S, len(gaps)))
    for a, b, g, same in gaps:
        w("  %s -> %s  %.0f s  %s\n" % (fmt_ts(a), fmt_ts(b), g,
                                        "same boot (recorder stalled or restarted)" if same
                                        else "BOOT CHANGE (power cut or reboot; wall-clock gap, may be off before NTP)"))

    def show_windows(title, wins):
        w("%s: %d\n" % (title, len(wins)))
        for s, e, n in wins:
            dur = "%.0f s" % ((e - s) / 1000.0) if e else "until the end"
            w("  %s -> %s  %s  (%d line(s))\n" % (fmt_ts(s), fmt_ts(e) if e else "end", dur, n))
    have_ag = [r for r in recs if isinstance(r.get("ag"), dict) and "err" not in r["ag"]]
    show_windows("mqtt disconnect windows", windows(have_ag, lambda r: r["ag"].get("mqtt") != "connected"))
    show_windows("gps fix-loss windows", windows(
        have_ag, lambda r: not (isinstance(r["ag"].get("gps"), dict) and (num(r["ag"]["gps"].get("fix")) or 0) > 0)))
    w("peak per process (rss kB / cpu % of one core):\n")
    peaks = {}

    def peak(name, rss, cpu):
        p = peaks.setdefault(name, [0, 0.0, 0])
        p[2] += 1
        if num(rss) is not None:
            p[0] = max(p[0], rss)
        if num(cpu) is not None:
            p[1] = max(p[1], cpu)
    for r in recs:
        rs = dget(r, "svc", "rs")
        if isinstance(rs, dict) and rs.get("pid"):
            peak("xorgate-agent-rs (main)", rs.get("rss_kb"), rs.get("cpu"))
        for k in r.get("kids") or []:
            if isinstance(k, dict):
                peak(str(k.get("comm")), k.get("rss_kb"), k.get("cpu"))
    for name, (rss, cpu, n) in sorted(peaks.items()):
        w("  %-28s peak rss %d kB  peak cpu %.1f %%  (%d sample(s))\n" % (name, rss, cpu, n))
    jrs = [r["jr"] for r in recs if isinstance(r.get("jr"), dict)]
    jr = [j for j in jrs if num(j.get("warn")) is not None]
    w("journal (xorgate-agent-rs): %d warning(s), %d error(s) over %d snapshot(s); %d timed out, %d failed"
      " (not counted; the next window covers them)\n"
      % (sum(j.get("warn") or 0 for j in jr), sum(j.get("err") or 0 for j in jr), len(jr),
         sum(1 for j in jrs if j.get("timeout")), sum(1 for j in jrs if j.get("fail") and not j.get("timeout"))))
    w("  note: the journal tail before a power cut may be lost (journald syncs every 5 min by default);"
      " these 5 s lines are the reliable pre-cut record\n")
    misses = {}
    for r in recs:
        for k in r.get("read_miss") or []:
            misses[k] = misses.get(k, 0) + 1
    w("read misses (value carried forward, no marker): %s\n"
      % (", ".join("%s %d" % kv for kv in sorted(misses.items())) or "none"))
    bl = series(("ag", "rec", "backlog_b"))
    if bl:
        w("upload backlog: max %d bytes, last %d bytes\n" % (max(bl)[0], bl[-1][0]))
    return 0


def main(argv):
    ap = argparse.ArgumentParser(prog="xorgate-drive-blackbox", description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd")
    r = sub.add_parser("run")
    r.add_argument("--out-dir", default=DEFAULT_OUT)
    r.add_argument("--interval", type=float, default=5.0)
    r.add_argument("--ticks", type=int, default=0, help="stop after N lines (tests); 0 = forever")
    r.add_argument("--max-file-mb", type=float, default=50.0)
    r.add_argument("--keep", type=int, default=2000, help="file-count cap (bytes are the real cap)")
    r.add_argument("--total-mb", type=float, default=300.0)
    r.add_argument("--min-free-mb", type=int, default=200)
    r.add_argument("--journal-every-ticks", type=int, default=12)
    r.add_argument("--cmd-timeout", type=float, default=5.0)
    r.add_argument("--journal-timeout", type=float, default=15.0)
    s = sub.add_parser("summary")
    s.add_argument("paths", nargs="+")
    lt = sub.add_parser("latest", help="print the file being written: the highest (boot seq, part)")
    lt.add_argument("dir")
    args = ap.parse_args(argv)
    if args.cmd == "summary":
        return cmd_summary(args)
    if args.cmd == "latest":
        files = scan_files(args.dir)
        if not files:
            return 1
        print(os.path.join(args.dir, files[-1][2]))
        return 0
    if args.cmd in (None, "run"):
        if args.cmd is None:
            args = ap.parse_args(["run"] + argv)
        if args.interval <= 0 or args.keep < 1 or args.cmd_timeout <= 0 or args.journal_timeout <= 0:
            ap.error("--interval and the timeouts must be > 0, --keep >= 1")
        TIMEOUTS.update(cmd=args.cmd_timeout, journal=args.journal_timeout)
        return cmd_run(args)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
