#!/usr/bin/env bash
# p8-lib.sh: shared by the Phase 8 bench-window tools (sourced, never run).
# Plan: plans/device-agent-js-to-rust/phase-8-dual-agent-registration.md §6-§7,
# H1-H10, DA15; PROGRESS carry-overs F1/F3/F5 + nits.
#
# The tools run ON the bench as root from the window dir /root/p8-<ts>/bin/
# (staged by `p8-backup.sh stage` from the Mac), except the Mac-side
# subcommands (stage, pull, cloud*, cam0, da15), which keep to bash 3.2.
#
# PRIVACY: nothing here prints a file's CONTENTS. Manifests carry path, type,
# mode, owner, size and sha256 only, and stay in the 0700 window dir (and the
# Mac's ~/xorgate-p8-private/). NetworkManager connection names are hashed in
# anything printed.
#
# P8_ROOT (tests only) prefixes every system path, so the plain suite can run
# the real code against a sandbox tree. On the bench it is empty.
# shellcheck disable=SC2034  # consumed by the sourcing tools
P8_ROOT=${P8_ROOT:-}
P8_LOCK=${P8_LOCK:-$P8_ROOT/run/p8.lock}
P8_MARKER=$P8_ROOT/root/p8-window.active
P8_SD=$P8_ROOT/etc/systemd/system
P8_STAMP=$P8_ROOT/var/lib/systemd/timers/stamp-p8-deadman.timer
P8_H1_CONF=$P8_ROOT/etc/apt/apt.conf.d/00p8-no-upgrade
P8_SVC_USER=${P8_SVC_USER:-pi}
P8_TELEMETRY=${P8_TELEMETRY:-$P8_ROOT/run/sim7600-lte/telemetry.json}
P8_DPKG_DIR=${P8_DPKG_DIR:-$P8_ROOT/var/lib/dpkg}
P8_DPKG_WAIT_S=${P8_DPKG_WAIT_S:-120}
P8_START_WAIT_S=${P8_START_WAIT_S:-90}
# The certified PROD build on 58b51737 (DA15; handbook "CONVERTED BENCH FACTS").
P8_CERT_BIN_SHA=${P8_CERT_BIN_SHA:-e46b3359aac31543b2ea1ff8eb96e726bd3425c3247902dc1ca23812c0b19feb}
P8_CERT_VERSION=${P8_CERT_VERSION:-9bd55b9}
P8_CERT_TARBALL_SHA=a2119987b6f62759371d75b7ebccb6ee21a50472c20bd7ba5cfe79a6350923ae
P8_PROD_UNIT=xorgate-agent-rs
P8_JS_UNIT=xorgate-agent
P8_MODEM_UNIT=sim7600-lte
P8_SAMPLER_UNIT=p8-modem-sampler
# Every agent/updater unit a window can leave enabled or queued (F1).
P8_AGENT_UNITS="xorgate-agent.service xorgate-agent-rs.service xorgate-updater.service xorgate-updater-rs.service"
# Enablement recorded as manifest pseudo rows (S8.3: ModemManager).
P8_ENABLED_UNITS="ModemManager.service NetworkManager.service sim7600-lte.service xorgate-agent.service xorgate-agent-rs.service xorgate-drive-blackbox.service"

# §7.2.5: the agent surface moved aside (mv -T, one fs). Order matters:
# /var/lib/xorgate first, so the seed lands within the same second (H10).
P8_SURFACE="/var/lib/xorgate
/etc/xorgate
/opt/xorgate
/opt/xorgate-rs
/opt/xorgate-rs.prev
/opt/xorgate-rs.new
/opt/xorgate-rs.old
/etc/systemd/system/xorgate-agent.service
/etc/systemd/system/xorgate-agent.service.d
/etc/systemd/system/xorgate-agent-rs.service
/etc/systemd/system/xorgate-agent-rs.service.d
/etc/systemd/system/xorgate-updater.service
/etc/systemd/system/xorgate-updater.service.d
/etc/systemd/system/xorgate-updater-rs.service
/etc/systemd/system/xorgate-updater-rs.service.d
/etc/systemd/system/multi-user.target.wants/xorgate-agent.service
/etc/systemd/system/multi-user.target.wants/xorgate-agent-rs.service
/etc/sudoers.d/020_xorgate-power
/etc/sudoers.d/021_xorgate-clock
/etc/sudoers.d/022_xorgate-update
/etc/sudoers.d/023_xorgate-camrotate
/etc/sudoers.d/024_xorgate-update-rs
/usr/local/bin/xorgate
/usr/local/bin/xorgate-rs"

p8_ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }
p8_log() { printf '[p8 %s] %s\n' "$(p8_ts)" "$*" >&2; [ -z "${P8_LOGFILE:-}" ] || printf '[p8 %s] %s\n' "$(p8_ts)" "$*" >> "$P8_LOGFILE" 2>/dev/null || true; }
p8_die() { p8_log "FATAL: $*"; exit "${P8_DIE_RC:-1}"; }
p8_need_root() { [ "$(id -u)" -eq 0 ] || p8_die "run as root (sudo)"; }
p8_sha() { sha256sum "$1" | cut -c1-64; }
p8_sha_stdin() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -c1-64; }

# A window dir is /root/p8-<ts>, root-owned 0700 (H8: it holds PROD keys).
p8_check_window() {
  local w=$1 m o
  [ -n "$w" ] || p8_die "no window dir (pass --window /root/p8-<ts>)"
  case $w in "$P8_ROOT"/root/p8-*) ;; *) p8_die "window dir must be $P8_ROOT/root/p8-<ts>: $w" ;; esac
  [ -d "$w" ] && [ ! -L "$w" ] || p8_die "window dir $w missing"
  m=$(stat -c %a "$w"); o=$(stat -c %u "$w")
  [ "$m" = 700 ] || p8_die "window dir $w is mode $m, not 0700"
  [ "$o" = 0 ] || [ -n "$P8_ROOT" ] || p8_die "window dir $w is not root-owned"
}
# The marker names the armed window: window=<dir> label=<wN> deadline=<epoch>.
p8_marker_get() { [ -f "$P8_MARKER" ] && sed -n "s/^$1=//p" "$P8_MARKER" | head -n1; }

# A unit's systemctl property, empty when systemd cannot say.
p8_prop() { systemctl show -p "$2" --value "$1" 2>/dev/null || true; }
p8_enabled() { systemctl is-enabled "$1" 2>/dev/null || true; }
p8_active() { systemctl is-active "$1" 2>/dev/null || true; }

# mv -T with the M3 guards: the destination must be absent and on the same
# filesystem as the source (a rename, never a copy).
p8_mvT() {
  local src=$1 dst=$2 sd dd
  [ -e "$src" ] || [ -L "$src" ] || p8_die "mv: source $src absent"
  if [ -e "$dst" ] || [ -L "$dst" ]; then P8_DIE_RC=3 p8_die "mv -T guard: destination $dst already exists (refusing)"; fi
  mkdir -p "$(dirname "$dst")"
  sd=$(stat -c %d "$(dirname "$src")"); dd=$(stat -c %d "$(dirname "$dst")")
  [ "$sd" = "$dd" ] || P8_DIE_RC=3 p8_die "mv -T guard: $src and $dst are on different filesystems"
  mv -T "$src" "$dst" || p8_die "mv -T $src $dst failed"
}

# Redact NetworkManager connection names (they can carry an SSID) for output.
p8_redact() { sed -E 's#(/etc/NetworkManager/system-connections/)([^\t ]+)#\1<redacted>#'; }

# ---------------------------------------------------------------- manifest
# p8_manifest <out>: the §7.2.3 manifest, minus the volatile list, sorted.
# Rows: path TAB type(f|d|l|o|p) TAB mode TAB uid:gid TAB size TAB sha256|->target|value
p8_manifest() {
  local out=$1 tmp u g
  tmp=$(mktemp) || return 1
  python3 -I - "$P8_ROOT" > "$tmp" <<'PY' || { rm -f "$tmp"; return 1; }
import glob, hashlib, os, stat, sys
root = sys.argv[1]
TREES = ["/etc/xorgate", "/var/lib/xorgate", "/var/lib/xorgate-rs-install", "/var/lib/xorgate-rs",
         "/opt/xorgate", "/opt/xorgate-rs", "/opt/xorgate-rs.prev", "/opt/xorgate-rs.new", "/opt/xorgate-rs.old"]
GLOBS = ["/etc/systemd/system/xorgate-*", "/etc/systemd/system/sim7600-lte*", "/etc/systemd/system/*.wants/*",
         "/etc/sudoers.d/*", "/usr/local/bin/xorgate", "/usr/local/bin/xorgate-rs", "/usr/local/bin/xorgate-setclock",
         "/usr/local/bin/xorgate-camrotate", "/usr/local/bin/sim7600-lte.sh", "/etc/udev/rules.d/99-sim7600-lte.rules",
         "/etc/NetworkManager/conf.d/*", "/etc/NetworkManager/system-connections/*", "/etc/systemd/logind.conf.d/*",
         "/etc/apt/apt.conf.d/*", "/etc/cron.d/xorgate-fake-hwclock", "/boot/firmware/config.txt", "/etc/modules",
         "/etc/modprobe.d/raspi-blacklist.conf"]
# M4 volatile list: excluded from equality, checked separately by p8-verify.sh.
VOL_TREES = ["/var/lib/xorgate-rs/drive-blackbox"]
def volatile(p):
    if p == "/etc/apt/apt.conf.d/00p8-no-upgrade":
        return True
    if p.startswith("/etc/systemd/system/"):
        if any(part.startswith("p8-") for part in p[len("/etc/systemd/system/"):].split("/")):
            return True
    return any(p == v or p.startswith(v + "/") for v in VOL_TREES)
def digest(a):
    h = hashlib.sha256()
    with open(a, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()
seen, rows = set(), []
def add(a):
    p = a[len(root):] if root else a
    if p in seen or volatile(p):
        return
    seen.add(p)
    try:
        st = os.lstat(a)
    except FileNotFoundError:
        return
    if "\t" in p or "\n" in p:
        sys.exit("p8: unsupported path (tab/newline) under " + os.path.dirname(p))
    m = st.st_mode
    if stat.S_ISREG(m):
        t, size, d = "f", str(st.st_size), digest(a)
    elif stat.S_ISDIR(m):
        t, size, d = "d", "-", "-"
    elif stat.S_ISLNK(m):
        t, size, d = "l", "-", "->" + os.readlink(a)
    else:
        t, size, d = "o", "-", "-"
    rows.append((p, t, "%04o" % stat.S_IMODE(m), "%d:%d" % (st.st_uid, st.st_gid), size, d))
    if t == "d":
        for n in sorted(os.listdir(a)):
            add(os.path.join(a, n))
for t in TREES:
    add(root + t)
for g in GLOBS:
    for a in sorted(glob.glob(root + g)):
        add(a)
for r in rows:
    print("\t".join(r))
PY
  {
    u=$P8_SVC_USER
    g=$(id -nG "$u" 2>/dev/null | tr ' ' '\n' | LC_ALL=C sort | paste -sd, -)
    printf '@groups:%s\tp\t-\t-\t-\t%s\n' "$u" "${g:-none}"
    for g in $P8_ENABLED_UNITS; do printf '@enabled:%s\tp\t-\t-\t-\t%s\n' "$g" "$(p8_enabled "$g" | head -n1)"; done
  } >> "$tmp"
  LC_ALL=C sort -t "$(printf '\t')" -k1,1 "$tmp" > "$out" && rm -f "$tmp"
}
# p8_mdiff <a> <b>: "changed|missing|new TAB path" lines (missing = in a, not b).
p8_mdiff() {
  python3 -I - "$1" "$2" <<'PY'
import sys
def load(f):
    d = {}
    with open(f) as fh:
        for line in fh:
            line = line.rstrip("\n")
            if line:
                k, _, v = line.partition("\t")
                d[k] = v
    return d
a, b = load(sys.argv[1]), load(sys.argv[2])
for k in sorted(set(a) | set(b)):
    if k not in b:
        print("missing\t" + k)
    elif k not in a:
        print("new\t" + k)
    elif a[k] != b[k]:
        print("changed\t" + k)
PY
}
# p8_tarlist <manifest> <out0>: the tar members (NUL list, relative paths):
# every real row except /var/lib/xorgate-rs/** (M4: never moved, location data).
p8_tarlist() {
  awk -F '\t' '$2 != "p" && $2 != "o" && $1 != "/var/lib/xorgate-rs" && index($1, "/var/lib/xorgate-rs/") != 1 { printf "%s%c", substr($1, 2), 0 }' "$1" > "$2"
}

# ---------------------------------------------------------------- dpkg (F3)
# True while any dpkg/apt lock is held. Reads /proc/locks (no lock is taken):
# a POSIX lock on the inode of lock-frontend, lock, or the apt lists/archives
# lock. P8_LOCKS_FILE overrides /proc/locks for tests.
p8_dpkg_locked() {
  local f ino held=1
  for f in "$P8_DPKG_DIR/lock-frontend" "$P8_DPKG_DIR/lock" "$P8_ROOT/var/lib/apt/lists/lock" "$P8_ROOT/var/cache/apt/archives/lock"; do
    [ -e "$f" ] || continue
    ino=$(stat -c %i "$f")
    if awk -v i="$ino" '{ n = split($6, a, ":"); if (a[n] == i) found = 1 } END { exit !found }' "${P8_LOCKS_FILE:-/proc/locks}" 2>/dev/null; then held=0; fi
  done
  return $held
}
# F3: WAIT for the dpkg locks to be free BEFORE stopping the installer unit
# (stopping its cgroup mid-unpack leaves a half-installed package), bounded by
# P8_DPKG_WAIT_S; then stop it; then repair if dpkg reports anything.
p8_stop_installers() {
  local units u waited=0
  units=$(systemctl list-units --all --plain --no-legend 'p8-install-*' 2>/dev/null | awk '{print $1}')
  if [ -n "$units" ]; then
    while p8_dpkg_locked && [ "$waited" -lt "$P8_DPKG_WAIT_S" ]; do
      [ "$waited" = 0 ] && p8_log "F3: a dpkg/apt lock is held: waiting (<= ${P8_DPKG_WAIT_S}s) before stopping the installer"
      sleep 1; waited=$((waited + 1))
    done
    if p8_dpkg_locked; then p8_log "F3: dpkg lock still held after ${waited}s: stopping the installer anyway"
    else p8_log "F3: dpkg locks free (waited ${waited}s)"; fi
    for u in $units; do p8_log "stopping installer unit $u"; systemctl stop "$u" 2>/dev/null || true; systemctl reset-failed "$u" 2>/dev/null || true; done
  fi
  p8_dpkg_repair
}
p8_dpkg_repair() {
  local a
  a=$(dpkg --audit 2>/dev/null || true)
  [ -n "$a" ] || return 0
  p8_log "F3: dpkg --audit reports unfinished packages: dpkg --configure -a"
  DEBIAN_FRONTEND=noninteractive dpkg --configure -a >/dev/null 2>&1 || p8_log "F3: dpkg --configure -a failed"
  a=$(dpkg --audit 2>/dev/null || true)
  [ -n "$a" ] || { p8_log "F3: dpkg repaired by --configure -a"; return 0; }
  p8_log "F3: still unfinished: apt-get -f install --no-upgrade"
  DEBIAN_FRONTEND=noninteractive apt-get -f install -y --no-upgrade >/dev/null 2>&1 || p8_log "F3: apt-get -f install --no-upgrade failed (network?)"
  a=$(dpkg --audit 2>/dev/null || true)
  if [ -z "$a" ]; then p8_log "F3: dpkg repaired by apt-get -f install --no-upgrade"; else p8_log "F3: dpkg STILL reports unfinished packages: fix by hand"; fi
}

# F1: cancel every queued job of the agent/updater units, so a boot (or the
# timer racing a boot) never starts a test unit, or a restored unit before the
# verify. Prints the cancelled job lines.
p8_cancel_agent_jobs() {
  local id unit u
  while read -r id unit _; do
    for u in $P8_AGENT_UNITS; do
      if [ "$unit" = "$u" ]; then p8_log "F1: cancelling queued job $id ($unit)"; systemctl cancel "$id" 2>/dev/null || true; fi
    done
  done < <(systemctl list-jobs --no-legend --plain 2>/dev/null || true)
}
