#!/usr/bin/env bash
# p8-verify.sh: DA15, "restore verified after EACH window" (plan §9 DA15,
# §7.1 step 2/3, H10; WP0 C3/C6). Prints PASS/FAIL lines; booleans, counts and
# hash prefixes only.
#
# Bench (root, from W/bin):
#   baseline --window W        record the W0 values into W/baseline.env (0600) while PROD runs:
#                              boot id (C6: not uptime), sim7600-lte ActiveEnterTimestamp/NRestarts,
#                              ip route sha, throttled, ModemManager/JS enablement, unattended-
#                              upgrades presence, dpkg list sha, telemetry bias + APN-cache sha,
#                              the conversion record state, the binary sha, the blackbox size
#   preflight                  STOP (exit 1) when APT::Periodic keys or unattended-upgrades exist
#                              (baseline runs it and refuses with exit 3)
#   pre --window W             BEFORE the start (p8-restore.sh runs it): manifest == pre.manifest
#                              minus the volatile list; no window artifact; W/aside empty; the
#                              certified binary e46b3359… + build-info 9bd55b9
#   post --window W [--dpkg-ok FILE]   AFTER the start: xorgate-agent-rs active on the certified
#                              exe; JS disabled+inactive; ModemManager as before; no reboot (boot
#                              id); sim7600-lte timestamps + NRestarts unchanged; gnss_ant_bias 1
#                              and the APN-cache sha == baseline; ip route unchanged; record
#                              state=converted; throttled 0x0; blackbox active + growing; dpkg
#                              list equal (or == the OK'd list); no DEV key outside W/test-*;
#                              no leftover (incl. the sampler and the timer stamp)
#   dev-settings --window W    DURING a window: the DEV agent's settings.json has NO cellular
#                              (exit 1 = STOP, H10) and gnssAntBias == the seed
#   unlink-check               W0 §7.1.3 the WP0 way (hard link held on the old inode; the naive
#                              inode compare is invalid: ext4 reuses freed inodes)
# Mac:
#   cloud-baseline --device ID --out F [--api URL]   the PROD API view (status, configRev,
#                              reportedConfig.rev, agentVersion, config sha = sha256 of
#                              `jq -S -c` of .config, as WP0 recorded ee1d181c…) into F (0600)
#   cloud --device ID --baseline F [--api URL] [--wait 180] [--expect-rev N] [--expect-config-sha P]
#                              online within --wait s; configRev unchanged; reportedConfig.rev ==
#                              configRev; config sha == baseline; agentVersion 9bd55b9
#   cam0                       runs the executable "$P8_CAM0_HOOK" (the orchestrator's read-only
#                              Playwright PROD cam0 live check): rc 0 PASS; unset = PENDING
#   da15 --host H --window W --device ID --baseline F   post (ssh) + the restore's pre verdict +
#                              cloud + cam0, one table; rc 0 only when every row is PASS
# Cloud auth: XG_TOKEN (Bearer), XG_ORG, XG_WS in the environment; never printed.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/bench/p8-lib.sh
. "$HERE/p8-lib.sh"
P8_SSH=${P8_SSH:-ssh}

cmd=${1:-}; shift || true
W=""; DPKG_OK=""; DEVICE=""; OUT=""; BASEF=""; API=${XG_API:-https://api.xorgate.io}; WAIT=180; HOST=""; XREV=""; XSHA=""
while [ $# -gt 0 ]; do
  case $1 in
    --window) W=$2; shift 2 ;;
    --dpkg-ok) DPKG_OK=$2; shift 2 ;;
    --device) DEVICE=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --baseline) BASEF=$2; shift 2 ;;
    --api) API=$2; shift 2 ;;
    --wait) WAIT=$2; shift 2 ;;
    --host) HOST=$2; shift 2 ;;
    --expect-rev) XREV=$2; shift 2 ;;
    --expect-config-sha) XSHA=$2; shift 2 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done

FAILS=0
pass() { echo "PASS $1"; }
fail() { echo "FAIL $1"; FAILS=$((FAILS + 1)); }
check() { # <name> <got> <want>
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (got '$2', want '$3')"; fi
}
verdict() { echo "DA15 $1: $([ "$FAILS" = 0 ] && echo PASS || echo "FAIL ($FAILS)")"; [ "$FAILS" = 0 ]; }
bget() { sed -n "s/^$1=//p" "$W/baseline.env" | head -n1; }

# ------------------------------------------------------------ measurements
tele() { # -> bias=… apn_sha=… cell=…
  python3 -I - "$P8_TELEMETRY" <<'PY'
import hashlib, json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    d = {}
b = d.get("gnss_ant_bias")
c = d.get("cellular")
apn = c.get("apn_applied") if isinstance(c, dict) else None
print("tele_bias=%s" % ("1" if b == 1 else "0" if b == 0 else "na"))
print("apn_sha=" + hashlib.sha256((apn if isinstance(apn, str) and apn else "<absent>").encode()).hexdigest())
print("cell_present=%d" % isinstance(c, dict))
PY
}
# Review T12: "growing" = the newest tape's mtime moved on (a prune of old tapes
# during the window must not read as "not growing").
bb_newest() { find "$P8_ROOT/var/lib/xorgate-rs/drive-blackbox" -type f -name '*.ndjson' -printf '%T@\n' 2>/dev/null | sort -n | tail -n1 | cut -d. -f1; }
pip_sha() { { python3 -m pip freeze --all 2>/dev/null || true; } | LC_ALL=C sort | p8_sha_stdin; }
# (c) Review asked item 6: apt.systemd.daily acts only with APT::Periodic keys
# or unattended-upgrades; either one is a STOP (do NOT mask the timer).
apt_periodic_n() { { apt-config dump 2>/dev/null || true; } | grep -c '^APT::Periodic' || true; }
dpkg_list() { dpkg-query -W -f '${Package} ${Version} ${db:Status-Abbrev}\n' 2>/dev/null | LC_ALL=C sort; }
build_version() { python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1])).get("version",""))' "$P8_ROOT/opt/xorgate-rs/build-info.json" 2>/dev/null || echo none; }
bin_sha() { [ -f "$P8_ROOT/opt/xorgate-rs/bin/xorgate-rs" ] && p8_sha "$P8_ROOT/opt/xorgate-rs/bin/xorgate-rs" || echo none; }
record_state() { sed -n 's/^state=//p' "$P8_ROOT/var/lib/xorgate-rs-install/record" 2>/dev/null | head -n1; }
throttled() { if command -v vcgencmd >/dev/null 2>&1; then vcgencmd get_throttled 2>/dev/null | sed -n 's/^throttled=//p'; else echo na; fi; }
boot_id() { cat "${P8_BOOT_ID_FILE:-/proc/sys/kernel/random/boot_id}" 2>/dev/null || echo na; }
measure() {
  echo "boot_id=$(boot_id)"
  echo "sim_enter=$(p8_prop "$P8_MODEM_UNIT" ActiveEnterTimestamp)"
  echo "sim_nrestarts=$(p8_prop "$P8_MODEM_UNIT" NRestarts)"
  echo "iproute_sha=$(ip route 2>/dev/null | p8_sha_stdin)"
  echo "route_dev=$(ip route get 1.1.1.1 2>/dev/null | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -n1)"
  echo "throttled=$(throttled)"
  echo "mm_enabled=$(p8_enabled ModemManager.service | head -n1)"
  echo "js_enabled=$(p8_enabled "$P8_JS_UNIT".service | head -n1)"
  echo "js_active=$(p8_active "$P8_JS_UNIT".service | head -n1)"
  echo "uu_installed=$(dpkg-query -W -f '${db:Status-Abbrev}' unattended-upgrades 2>/dev/null | grep -q '^ii' && echo yes || echo no)"
  echo "uu_timer_enabled=$(p8_enabled apt-daily-upgrade.timer | head -n1)"
  echo "dpkg_sha=$(dpkg_list | p8_sha_stdin)"
  tele
  echo "record_state=$(record_state)"
  echo "bin_sha=$(bin_sha)"
  echo "build_version=$(build_version)"
  echo "bb_newest=$(bb_newest)"
  echo "pip_sha=$(pip_sha)"
  echo "apt_periodic=$(apt_periodic_n)"
  echo "bb_active=$(p8_active xorgate-drive-blackbox.service | head -n1)"
}

# ------------------------------------------------------------ leftovers
leftovers() { # <strict 0|1>: prints one line per window artifact still present
  local f u
  for f in "$P8_SD"/p8-* "$P8_SD"/*.wants/p8-* "$P8_STAMP" "$P8_MARKER" "$P8_H1_CONF" \
           "$P8_ROOT/opt/xorgate-rs.new" "$P8_ROOT/opt/xorgate-rs.old" "$P8_ROOT"/run/systemd/system/xorgate-* \
           "$P8_ROOT"/run/systemd/transient/p8-install-* "$P8_ROOT$P8_GENERATOR" \
           "$P8_ROOT/run/systemd/generator/$P8_GENERATOR_LINK" "$P8_ROOT/run/systemd/generator.early/$P8_GENERATOR_LINK" \
           "$P8_ROOT/run/systemd/generator.late/$P8_GENERATOR_LINK"; do
    if [ -e "$f" ] || [ -L "$f" ]; then echo "${f#"$P8_ROOT"}"; fi
  done
  while read -r u _; do
    [ -n "$u" ] || continue
    [ "$u" = "${P8_SELF_UNIT:-}" ] && continue
    [ "$1" = 0 ] && [ "$u" = "$P8_SAMPLER_UNIT.service" ] && continue
    echo "unit $u"
  done < <(systemctl list-units --all --plain --no-legend 'p8-*' 2>/dev/null | awk '$3 != "inactive" || $2 != "not-found" {print $1}')
}

cmd_preflight() {
  check "no APT::Periodic keys (apt.systemd.daily does nothing)" "$(apt_periodic_n)" 0
  check "unattended-upgrades not installed" "$(dpkg-query -W -f '${db:Status-Abbrev}' unattended-upgrades 2>/dev/null | grep -q '^ii' && echo installed || echo absent)" absent
  if [ "$FAILS" = 0 ]; then echo "PREFLIGHT PASS"; else echo "PREFLIGHT STOP"; fi
  [ "$FAILS" = 0 ]
}

cmd_baseline() {
  p8_need_root; p8_check_window "$W"
  cmd_preflight || { echo "baseline refused: PREFLIGHT STOP" >&2; exit 3; }
  [ "$(p8_active "$P8_PROD_UNIT")" = active ] || echo "NOTE: $P8_PROD_UNIT is not active (the baseline belongs before T0)"
  umask 077
  measure > "$W/baseline.env.tmp"; mv -f "$W/baseline.env.tmp" "$W/baseline.env"
  ip route > "$W/iproute.pre" 2>/dev/null || true
  dpkg_list > "$W/dpkg.pre"
  echo "baseline: $W/baseline.env"
  grep -E '^(route_dev|throttled|mm_enabled|js_enabled|js_active|uu_installed|uu_timer_enabled|tele_bias|cell_present|record_state|build_version|bb_active)=' "$W/baseline.env" | tr '\n' ' '; echo
  echo "apn_sha recorded; dpkg_sha + pip_sha recorded; bin_sha=$(bget bin_sha | cut -c1-16)…"
}

cmd_pre() {
  local d n
  p8_check_window "$W"
  [ -f "$W/pre.manifest" ] || { fail "pre.manifest exists"; verdict pre; return; }
  p8_manifest "$W/verify.manifest"
  d=$(p8_mdiff "$W/pre.manifest" "$W/verify.manifest" || true)
  if [ -z "$d" ]; then pass "manifest == pre.manifest ($(wc -l < "$W/pre.manifest" | tr -d ' ') rows, minus the volatile list)"
  else
    n=$(printf '%s\n' "$d" | wc -l | tr -d ' ')
    fail "manifest differs from pre.manifest in $n rows:"
    printf '%s\n' "$d" | head -n 25 | p8_redact | sed 's/^/       /'
  fi
  n=$(find "$W/aside" -mindepth 1 ! -type d 2>/dev/null | wc -l | tr -d ' ')
  check "W/aside holds nothing (every PROD path moved back)" "$n" 0
  d=$(leftovers 0 | tr '\n' ' ')
  check "no window artifact (dead-man units, marker, timer stamp, H1 conf, .new/.old, installer units)" "$d" ""
  check "certified binary sha256 (e46b3359…)" "$(bin_sha)" "$P8_CERT_BIN_SHA"
  check "build-info version" "$(build_version)" "$P8_CERT_VERSION"
  verdict pre
}

dev_keys_outside() { # prints the number of DEV key/cert copies outside W/test-*
  python3 -I - "$W" "$P8_ROOT" <<'PY'
import glob, hashlib, os, sys
w, root = sys.argv[1], sys.argv[2]
def h(p):
    with open(p, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()
keys = set()
for p in glob.glob(w + "/test-*/**", recursive=True):
    if os.path.isfile(p) and not os.path.islink(p) and (p.endswith(".pem") or p.endswith(".key")) and "root-ca" not in os.path.basename(p):
        keys.add(h(p))
hits = 0
for top in ["/etc", "/opt", "/var/lib", "/usr/local", "/home", "/root", "/tmp", "/var/tmp"]:
    for dp, dn, fn in os.walk(root + top):
        # every window dir (/root/p8-*) is a kept artifact by design (§7.4)
        if dp.startswith(w) or dp.startswith(root + "/root/p8-"):
            dn[:] = []
            continue
        for n in fn:
            p = os.path.join(dp, n)
            try:
                if os.path.isfile(p) and not os.path.islink(p) and os.path.getsize(p) < 65536 and h(p) in keys:
                    hits += 1
            except OSError:
                pass
print(len(keys), hits)
PY
}

cmd_post() {
  local pid exe k n
  p8_need_root; p8_check_window "$W"
  [ -f "$W/baseline.env" ] || { fail "baseline.env exists"; verdict post; return; }
  measure > "$W/post.env"
  ip route > "$W/iproute.post" 2>/dev/null || true
  pk() { sed -n "s/^$1=//p" "$W/post.env" | head -n1; }
  check "$P8_PROD_UNIT active" "$(p8_active "$P8_PROD_UNIT")" active
  pid=$(p8_prop "$P8_PROD_UNIT" MainPID); exe=$(readlink "$P8_ROOT/proc/${pid:-0}/exe" 2>/dev/null || echo none)
  check "$P8_PROD_UNIT runs /opt/xorgate-rs/bin/xorgate-rs" "$exe" /opt/xorgate-rs/bin/xorgate-rs
  check "certified binary sha256" "$(pk bin_sha)" "$P8_CERT_BIN_SHA"
  check "build-info version" "$(pk build_version)" "$P8_CERT_VERSION"
  check "JS agent inactive" "$(pk js_active)" inactive
  check "JS agent enablement as W0 ($(bget js_enabled))" "$(pk js_enabled)" "$(bget js_enabled)"
  check "ModemManager enablement as W0 ($(bget mm_enabled))" "$(pk mm_enabled)" "$(bget mm_enabled)"
  check "no reboot during the window (boot id, C6)" "$(pk boot_id)" "$(bget boot_id)"
  check "sim7600-lte ActiveEnterTimestamp unchanged" "$(pk sim_enter)" "$(bget sim_enter)"
  check "sim7600-lte NRestarts unchanged" "$(pk sim_nrestarts)" "$(bget sim_nrestarts)"
  check "gnss_ant_bias = 1" "$(pk tele_bias)" 1
  check "APN-cache sha == baseline (the daemon's APN_APPLIED; not a fresh modem read)" "$(pk apn_sha)" "$(bget apn_sha)"
  check "ip route unchanged" "$(pk iproute_sha)" "$(bget iproute_sha)"
  check "ip route get 1.1.1.1 dev unchanged" "$(pk route_dev)" "$(bget route_dev)"
  check "conversion record state=converted" "$(pk record_state)" converted
  check "throttled=0x0" "$(pk throttled)" 0x0
  check "xorgate-drive-blackbox active" "$(pk bb_active)" active
  if [ "$(pk bb_newest)" -gt "$(bget bb_newest)" ] 2>/dev/null; then pass "drive-blackbox writing (newest tape mtime moved on)"; else fail "drive-blackbox not writing (newest tape mtime did not move)"; fi
  check "pip freeze unchanged" "$(pk pip_sha)" "$(bget pip_sha)"
  if [ -n "$DPKG_OK" ]; then check "dpkg list == the OK'd list" "$(pk dpkg_sha)" "$(p8_sha_stdin < "$DPKG_OK")"
  else check "dpkg list unchanged (H1)" "$(pk dpkg_sha)" "$(bget dpkg_sha)"; fi
  read -r k n < <(dev_keys_outside)
  check "no DEV key/cert outside W/test-* ($k DEV key files known)" "$n" 0
  check "no window artifact left (strict, incl. the sampler)" "$(leftovers 1 | tr '\n' ' ')" ""
  verdict post
}

cmd_dev_settings() {
  p8_check_window "$W"
  python3 -I - "$P8_ROOT/var/lib/xorgate/settings.json" "$W/seed-settings.json" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception as e:
    print("FAIL DEV settings.json unreadable (%s)" % type(e).__name__); sys.exit(1)
s = json.load(open(sys.argv[2]))
rc = 0
if "cellular" in d:
    print("FAIL STOP (H10): the DEV settings carry a cellular key"); rc = 1
else:
    print("PASS no cellular key in the DEV settings")
ok = d.get("gnssAntBias") == s.get("gnssAntBias")
print("%s gnssAntBias == the seed (%s)" % ("PASS" if ok else "FAIL", json.dumps(d.get("gnssAntBias"))))
rc |= 0 if ok else 2
lr = d.get("lteRecovery")
ok = lr is None or lr == s.get("lteRecovery")
print("%s lteRecovery %s (C1: absent = the daemon defaults = PROD in effect)" % ("PASS" if ok else "FAIL", "absent" if lr is None else "== the seed" if ok else "DIFFERS"))
rc |= 0 if ok else 2
sys.exit(rc)
PY
}

cmd_unlink_check() {
  local d a b
  d=$(mktemp -d /tmp/p8w0-unlink.XXXXXX)
  printf 'OLD\n' > "$d/dst"; ln "$d/dst" "$d/held"; printf 'NEW\n' > "$d/src"
  install -m 0755 "$d/src" "$d/dst"
  a=$(stat -c %i "$d/held"); b=$(stat -c %i "$d/dst")
  if [ "$(cat "$d/held")" = OLD ] && [ "$a" != "$b" ] && [ "$(cat "$d/dst")" = NEW ]; then pass "install replaces by unlink (the held old inode keeps OLD)"
  else fail "install rewrote IN PLACE (STOP: the installer would rewrite the running modem script)"; fi
  printf 'OLD\n' > "$d/c"; ln "$d/c" "$d/ch"; cp "$d/src" "$d/c"
  echo "INFO contrast: cp (the update rails' fs::copy/copyFileSync) is in place: $([ "$(cat "$d/ch")" = NEW ] && echo yes || echo no)"
  rm -rf "$d"
  [ "$FAILS" = 0 ]
}

# ------------------------------------------------------------------ Mac side
api_get() { # <device> -> raw body on stdout
  python3 -I - "$API" "$1" <<'PY'
import os, sys, urllib.request
api, dev = sys.argv[1].rstrip("/"), sys.argv[2]
h = {"Authorization": "Bearer " + os.environ["XG_TOKEN"], "X-Organization-Id": os.environ.get("XG_ORG", "")}
if os.environ.get("XG_WS"):
    h["X-Workspace-Id"] = os.environ["XG_WS"]
req = urllib.request.Request("%s/v1/devices/%s" % (api, dev), headers=h)
with urllib.request.urlopen(req, timeout=20) as r:
    sys.stdout.buffer.write(r.read())
PY
}
view() { # <raw body file> -> key=value lines (no values beyond these)
  local cfgsha
  if command -v jq >/dev/null 2>&1; then
    cfgsha=$(jq -S -c '(.device // .) | .config' < "$1" | p8_sha_stdin)
  else
    cfgsha=$(python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); d=d.get("device",d); print(json.dumps(d.get("config"),sort_keys=True,separators=(",",":"),ensure_ascii=False))' "$1" | p8_sha_stdin)
  fi
  python3 -I - "$1" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
d = d.get("device", d)
rc = d.get("reportedConfig") or {}
print("status=%s" % d.get("status"))
print("configRev=%s" % d.get("configRev"))
print("reportedRev=%s" % rc.get("rev"))
print("agentVersion=%s" % d.get("agentVersion"))
print("config_keys=%s" % ",".join(sorted((d.get("config") or {}).keys())))
PY
  echo "config_sha=$cfgsha"
}
need_auth() { [ -n "${XG_TOKEN:-}" ] || { echo "set XG_TOKEN (and XG_ORG/XG_WS) for the PROD API (test user)" >&2; exit 2; }; [ -n "$DEVICE" ] || { echo "--device <full id>" >&2; exit 2; }; }

cmd_cloud_baseline() {
  local t
  need_auth; [ -n "$OUT" ] || { echo "--out FILE" >&2; exit 2; }
  t=$(mktemp); umask 077
  api_get "$DEVICE" > "$t"; view "$t" > "$OUT"; rm -f "$t"; chmod 0600 "$OUT"
  sed 's/^config_sha=\(.\{12\}\).*/config_sha=\1…/' "$OUT"
}

cmd_cloud() {
  local t i=0 st=""
  need_auth; [ -f "$BASEF" ] || { echo "--baseline FILE (from cloud-baseline)" >&2; exit 2; }
  t=$(mktemp)
  while :; do
    if api_get "$DEVICE" > "$t" 2>/dev/null; then view "$t" > "$t.v"; st=$(sed -n 's/^status=//p' "$t.v"); fi
    [ "$st" = online ] && break
    [ "$i" -ge "$WAIT" ] && break
    sleep 10; i=$((i + 10))
  done
  bv() { sed -n "s/^$1=//p" "$BASEF" | head -n1; }
  cv() { sed -n "s/^$1=//p" "$t.v" 2>/dev/null | head -n1; }
  check "58b51737 online in the PROD API (<= ${WAIT}s; took ~${i}s)" "$st" online
  check "configRev unchanged" "$(cv configRev)" "$(bv configRev)"
  [ -z "$XREV" ] || check "configRev == the WP0 value" "$(cv configRev)" "$XREV"
  check "reportedConfig.rev == configRev" "$(cv reportedRev)" "$(cv configRev)"
  check "settings deep-equal to W0 (config sha)" "$(cv config_sha)" "$(bv config_sha)"
  if [ -n "$XSHA" ]; then case $(cv config_sha) in "$XSHA"*) pass "config sha starts with the WP0 value $XSHA" ;; *) fail "config sha is not the WP0 value $XSHA" ;; esac; fi
  check "reported agentVersion" "$(cv agentVersion)" "$P8_CERT_VERSION"
  rm -f "$t" "$t.v"
  verdict cloud
}

cmd_cam0() {
  if [ -z "${P8_CAM0_HOOK:-}" ]; then echo "PENDING PROD cam0 live (set P8_CAM0_HOOK to the orchestrator's read-only Playwright check)"; return 3; fi
  local hrc=0
  [ -x "$P8_CAM0_HOOK" ] || { fail "cam0 hook $P8_CAM0_HOOK is not executable"; return 1; }
  # Review T11: a hard timeout (perl alarm: the Mac has no timeout(1)); fail-closed.
  perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "${P8_CAM0_TIMEOUT:-240}" "$P8_CAM0_HOOK" > "${P8_CAM0_OUT:-/dev/null}" 2>&1 || hrc=$?
  case $hrc in
    0) pass "PROD cam0 live plays (hook)" ;;
    142) fail "PROD cam0 live: the hook TIMED OUT after ${P8_CAM0_TIMEOUT:-240}s" ;;
    *) fail "PROD cam0 live (hook rc $hrc)" ;;
  esac
  [ "$FAILS" = 0 ]
}

cmd_da15() {
  local out rc=0
  { [ -n "$HOST" ] && [ -n "$W" ]; } || { echo "da15 --host H --window W --device ID --baseline F" >&2; exit 2; }
  echo "== DA15 pre-start (from the restore's own run)"
  out=$("$P8_SSH" -o BatchMode=yes -o ConnectTimeout=10 "$HOST" "sudo -n cat $W/verify-pre.out" 2>/dev/null || echo "FAIL no verify-pre.out")
  printf '%s\n' "$out" | p8_redact
  printf '%s\n' "$out" | grep -q '^DA15 pre: PASS' || rc=1
  echo "== DA15 post-start (bench)"
  "$P8_SSH" -o BatchMode=yes -o ConnectTimeout=10 "$HOST" "sudo -n bash $W/bin/p8-verify.sh post --window $W" || rc=1
  echo "== DA15 cloud (PROD API)"
  cmd_cloud || rc=1
  FAILS=0
  echo "== DA15 cam0"
  cmd_cam0 || rc=1
  echo "DA15 overall: $([ "$rc" = 0 ] && echo PASS || echo 'NOT GREEN')"
  return "$rc"
}

case $cmd in
  baseline) cmd_baseline ;;
  preflight) cmd_preflight ;;
  pre) cmd_pre ;;
  post) cmd_post ;;
  dev-settings) cmd_dev_settings ;;
  unlink-check) cmd_unlink_check ;;
  cloud-baseline) cmd_cloud_baseline ;;
  cloud) cmd_cloud ;;
  cam0) cmd_cam0 ;;
  da15) cmd_da15 ;;
  *) sed -n '2,44p' "$0"; exit 2 ;;
esac
