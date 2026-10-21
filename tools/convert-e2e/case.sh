#!/bin/bash
# One G2 case (phase-5-distribution.md §11) INSIDE a fresh convert-e2e
# container: run.sh starts one `--rm --init` container per case, so every case
# begins from an empty box, provisions a JS device (provision-js.sh), takes
# the I3 manifest M0 (manifest.sh, WP2's own), runs install-rs.sh the way an
# operator does (`sudo bash install-rs.sh` from pi's home) and checks the exit
# code, the I1 final line, the I2 record and an I3 manifest diff.
#
#   case.sh <name>
#
# Mounts (run.sh): /bucket (prep-bucket.sh), /e2e (this dir), /in (install-rs.sh,
# js/ = the JS repo @ JS_REF, convert.md if any), /fixtures (repo fixtures/).
# Prints one line per check and `CASE <name>: PASS <n> FAIL <m>`; exit 1 on
# any FAIL.
set -uo pipefail
CASE=${1:?case name}
PASS=0
FAIL=0
E=/run/e2e
mkdir -p "$E"
ok() { echo "  ok   $*"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }
check() { local d=$1; shift; if "$@"; then ok "$d"; else bad "$d"; fi; }
note() { echo "  note $*"; }

P='[install-rs] '
IRS=/in/install-rs.sh
BUCKET=http://127.0.0.1:8000
SRV=/srv/bucket
REC_DIR=/var/lib/xorgate-rs-install
REC=$REC_DIR/record
SYS=/etc/systemd/system
WANTS=$SYS/multi-user.target.wants
RS_UNIT=$SYS/xorgate-agent-rs.service
RS_UPD=$SYS/xorgate-updater-rs.service
DIST_DROPIN=$SYS/xorgate-updater-rs.service.d/10-dist.conf
S024=/etc/sudoers.d/024_xorgate-update-rs
WRAP=/usr/local/bin/xorgate-rs
JS_WORKER='node /opt/xorgate/dist/worker.js'

# ---------------------------------------------------------------- bucket
start_bucket() {
  [ -d "$SRV" ] || cp -a /bucket "$SRV"
  (cd "$SRV" && exec python3 -m http.server 8000 --bind 127.0.0.1 >>"$E/http.log" 2>&1) &
  BUCKET_PID=$!
  for _ in $(seq 50); do curl -fs -o /dev/null "$BUCKET/" && return 0; sleep 0.1; done
  echo "bucket did not come up" >&2
}
stop_bucket() { kill "$BUCKET_PID" 2>/dev/null; wait "$BUCKET_PID" 2>/dev/null; ! curl -fs -o /dev/null "$BUCKET/"; }
flip_latest() { # <v>
  cp "$SRV/xorgate-agent-rs-$1.tar.gz" "$SRV/xorgate-agent-rs-latest.tar.gz"
  (cd "$SRV" && sha256sum xorgate-agent-rs-latest.tar.gz >xorgate-agent-rs-latest.tar.gz.sha256)
}

# ---------------------------------------------------------------- device
provision() {
  bash /e2e/provision-js.sh "$@" >"$E/provision.log" 2>&1 || { cat "$E/provision.log"; bad "provision-js.sh $*"; finish; }
  tail -n1 "$E/provision.log" | sed 's/^/  /'
  mv "$E/systemctl.log" "$E/systemctl.provision.log"
  : >"$E/systemctl.log"
  : >"$E/visudo.log"
  SIM_TS0=$(systemctl show -p ActiveEnterTimestamp --value sim7600-lte)
}
M() { bash /e2e/manifest.sh >"$1"; }
# path of an I3 line (everything after the first " /")
paths() { awk '{ n = index($0, " /"); print substr($0, n + 1) }' "$1"; }
RUST_RE='^(/opt/xorgate-rs(\.new|\.old)?|/usr/local/bin/xorgate-rs|/etc/systemd/system/xorgate-(agent|updater)-rs\.service(\.d(/.*)?)?|/etc/systemd/system/[^/]+\.(wants|requires)/xorgate-(agent|updater)-rs\.service|/etc/sudoers\.d/024_xorgate-update-rs|/var/lib/xorgate-rs-install(/.*)?)$'
NEVER_RE='^(/etc/xorgate(/.*)?|/opt/xorgate|/usr/local/bin/(xorgate|xorgate-setclock|xorgate-camrotate|sim7600-lte\.sh)|/etc/systemd/system/(xorgate-agent\.service(\.d(/.*)?)?|xorgate-updater\.service|sim7600-lte\.service)|/etc/sudoers\.d/0(10|2[0-3])_.*|/etc/cron\.d(/.*)?|/etc/udev/rules\.d(/.*)?|/etc/NetworkManager/conf\.d(/.*)?|/etc/systemd/logind\.conf\.d(/.*)?)$'
sel() { # <regex> <invert 0|1> <manifest>: the lines whose path matches (or not)
  awk -v re="$1" -v inv="$2" '{ n = index($0, " /"); p = substr($0, n + 1); m = (p ~ re); if (inv) m = !m; if (m) print }' "$3"
}
js_rows() { sel "$RUST_RE" 1 "$1"; }
no_rec_rows() { sel '^/var/lib/xorgate-rs-install(/.*)?$' 1 "$1"; }
never_rows() { sel "$NEVER_RE" 0 "$1"; }
same_file() { cmp -s "$1" "$2"; }
body_is() { [ "$(cat "$1"; echo x)" = "$(printf '%s\n' "$2"; echo x)" ]; } # <file> <text>: file == text + LF
member_is() { cmp -s "$1" <(tar -xzf "$2" -O "$3"); }                   # <file> <tarball> <member>
mode_is() { [ "$(stat -c %a "$1" 2>/dev/null)" = "$2" ]; }
same_rows() { # <label> <fn> <a> <b>
  if diff <("$2" "$3") <("$2" "$4") >"$E/diff.out"; then ok "$1"; else bad "$1"; sed 's/^/        /' "$E/diff.out" | head -40; fi
}

# ---------------------------------------------------------------- install-rs.sh
# irs [VAR=val…] [--as root|pi-nosudo] [--] <args…>: run it like an operator,
# `sudo bash install-rs.sh` from /home/pi (SUDO_USER=pi), env passed through
# sudo's command line (permitted: pi's grant is ALL).
irs() {
  local envs=() how=sudo
  while [ $# -gt 0 ]; do
    case "$1" in
      --as) how=$2; shift 2 ;;
      --) shift; break ;;
      *=*) envs+=("$1"); shift ;;
      *) break ;;
    esac
  done
  local t0=$SECONDS
  case "$how" in
    sudo) (cd /home/pi && runuser -u pi -- sudo ${envs[@]+"${envs[@]}"} bash "$IRS" "$@") >"$E/out" 2>"$E/err" ;;
    root) (cd /root && env ${envs[@]+"${envs[@]}"} bash "$IRS" "$@") >"$E/out" 2>"$E/err" ;;
    pi-nosudo) (cd /home/pi && runuser -u pi -- env ${envs[@]+"${envs[@]}"} bash "$IRS" "$@") >"$E/out" 2>"$E/err" ;;
    cmd) (cd /home/pi && runuser -u pi -- sudo ${envs[@]+"${envs[@]}"} "$@") >"$E/out" 2>"$E/err" ;;
  esac
  echo $? >"$E/rc"
  echo "  \$ install-rs.sh $* → rc $(cat "$E/rc") ($((SECONDS - t0)) s)"
  sed 's/^/  | /' "$E/out"
  [ -s "$E/err" ] && sed 's/^/  ! /' "$E/err"
  return 0
}
rc_is() { [ "$(cat "$E/rc")" = "$1" ]; }
last_line_is() { [ "$(tail -n1 "$E/out")" = "$1" ]; }
has_line() { grep -qxF -- "$1" "$E/out" "$E/err"; }
out_has() { grep -qF -- "$1" "$E/out" "$E/err"; }
all_prefixed() { ! cat "$E/out" "$E/err" | grep -v '^\[install-rs\] ' | grep -q .; }
changed_n() { sed -n 's/^\[install-rs\] .*changed (\([0-9]*\)).*/\1/p' "$E/out" | tail -n1; }
rec() { sed -n "s/^$1=//p" "$REC" 2>/dev/null | head -n1; }
rec_keys_ok() { [ "$(cut -d= -f1 "$REC" | tr '\n' ' ')" = "state startedAt convertedAt version channel sha256 source jsEnabled jsActive rsPreexisting serviceUser installed " ]; }
status_first() { (bash "$IRS" --status 2>/dev/null; echo "rc=$?") >"$E/status.out"; [ "$(head -n1 "$E/status.out")" = "$1" ] && [ "$(tail -n1 "$E/status.out")" = rc=0 ]; }

# ---------------------------------------------------------------- unit state
ise() { systemctl is-enabled "$1" 2>/dev/null; }
isa() { systemctl is-active "$1" 2>/dev/null; }
units_are() { [ "$(ise "$1")/$(isa "$1")" = "$2" ]; }
js_running() { pgrep -f "$JS_WORKER" >/dev/null; }
rs_pid() { systemctl show -p MainPID --value xorgate-agent-rs; }
rs_uid() { awk '/^Uid:/ {print $2}' "/proc/$(rs_pid)/status" 2>/dev/null; }
rs_exe() { readlink "/proc/$(rs_pid)/exe" 2>/dev/null; }
rs_env() { tr '\0' '\n' <"/proc/$(rs_pid)/environ" 2>/dev/null | sed -n "s/^$1=//p"; }

# ---------------------------------------------------------------- expected bytes
member() { tar -xzf "$1" -O "$2"; }
WRAPPER_BODY=$(printf '%s\n%s' '#!/usr/bin/env bash' 'exec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" /opt/xorgate-rs/bin/xorgate-rs "$@"')
DIST_BODY='# Installed by install-rs.sh. agent.env'"'"'s DEVICE_DIST_VERSION is the JS agent'"'"'s pin;
# the Rust updater always follows xorgate-agent-rs-latest (README §6: the cloud update stays latest-only).
[Service]
Environment=DEVICE_DIST_VERSION=latest'
# body_024 <user>: update_plan::updater_sudoers_body, derived independently of
# WP1's golden: the REAL TS 022 body (fixtures/update_plan/update.json, TS
# dump) with the three O3/Q11 substitutions update-e2e pins.
body_024() {
  python3 -I - "$1" <<'PY'
import json, sys
user = sys.argv[1]
f = json.load(open("/fixtures/update_plan/update.json"))
for c in f["cases"]:
    i = c["input"]
    if c["op"] == "sudoers" and i["grant"] == "update" and i["visudoExit"] == 0:
        b = c["expected"]["body"].replace(i["user"], user)
        b = b.replace("# Installed by xorgate. ", "# Installed by xorgate-rs. ", 1)
        b = b.replace("# else. See device-agent src/agent/commands.ts + systemd/xorgate-updater.service.\n",
                      "# else. See device-agent-rust crates/xorgate-hal/src/privileged.rs + systemd/xorgate-updater-rs.service.\n", 1)
        b = b.replace(" --no-block xorgate-updater.service\n", " --no-block xorgate-updater-rs.service\n", 1)
        sys.stdout.write(b)
        break
else:
    sys.exit("no TS update sudoers body in the fixture")
PY
}
# in_golden <text>: if WP1's golden (fixtures/install_rs/bodies.json) exists,
# the text must be one of its string values; absent golden → skipped (noted).
in_golden() {
  [ -f /fixtures/install_rs/bodies.json ] || { note "fixtures/install_rs/bodies.json absent (pre-WP1): golden cross-check skipped"; return 0; }
  python3 -I - "$1" <<'PY'
import json, sys
want = sys.argv[1]
def leaves(x):
    if isinstance(x, str): yield x
    elif isinstance(x, dict):
        for v in x.values(): yield from leaves(v)
    elif isinstance(x, list):
        for v in x: yield from leaves(v)
vals = list(leaves(json.load(open("/fixtures/install_rs/bodies.json"))))
sys.exit(0 if want in vals or want + "\n" in vals or want.rstrip("\n") in vals else 1)
PY
}
sha_of() { sha256sum | cut -d' ' -f1; }
tree_is() { [ "$(bash /e2e/manifest.sh /opt/xorgate-rs)" = "$(tree_digest "$1")" ]; } # <tarball>
tree_digest() { # <tarball>: the I3 `t` line /opt/xorgate-rs would get from it
  local x
  x=$(mktemp -d /var/tmp/tree.XXXXXX)
  mkdir -p "$x/opt/xorgate-rs"
  tar --no-same-owner -xzf "$1" -C "$x/opt/xorgate-rs"
  bash /e2e/manifest.sh --root "$x" /opt/xorgate-rs
  rm -rf "$x"
}

# expect_conv <M0> <out> <tarball> <svc> <dropin 0|1> [foreign…]: the I3
# manifest a conversion must produce from M0 — M0, minus the JS wants link,
# plus exactly the §4 NEW rows (record files: path/type/owner only).
expect_conv() {
  local m0=$1 out=$2 tgz=$3 svc=$4 dropin=$5 jsd
  jsd=$SYS/xorgate-agent.service.d/10-user.conf
  {
    grep -vF " $WANTS/xorgate-agent.service" "$m0" | grep -vE " /etc/systemd/system/xorgate-agent-rs\.service\.d\$"
    printf 'f 0644 0:0 %s %s\n' "$(member "$tgz" systemd/xorgate-agent-rs.service | sha_of)" "$RS_UNIT"
    if [ "$dropin" = 1 ] || grep -qE " $SYS/xorgate-agent-rs\.service\.d\$" "$m0"; then
      grep -E " $SYS/xorgate-agent-rs\.service\.d\$" "$m0" || printf 'd 0755 0:0 - %s.d\n' "$RS_UNIT"
    fi
    [ "$dropin" = 1 ] && printf 'f 0644 0:0 %s %s.d/10-user.conf\n' "$(sha_of <"$jsd")" "$RS_UNIT"
    printf 'l 0777 0:0 ->%s %s/xorgate-agent-rs.service\n' "$RS_UNIT" "$WANTS"
    printf 'f 0644 0:0 %s %s\n' "$(member "$tgz" systemd/xorgate-updater-rs.service | sha_of)" "$RS_UPD"
    printf 'd 0755 0:0 - %s.d\n' "$RS_UPD"
    printf 'f 0644 0:0 %s %s\n' "$(printf '%s\n' "$DIST_BODY" | sha_of)" "$DIST_DROPIN"
    [ "$svc" != root ] && printf 'f 0440 0:0 %s %s\n' "$(body_024 "$svc" | sha_of)" "$S024"
    printf 'f 0755 0:0 %s %s\n' "$(printf '%s\n' "$WRAPPER_BODY" | sha_of)" "$WRAP"
    tree_digest "$tgz"
    printf 'd 0700 0:0 - %s\n' "$REC_DIR"
    printf 'f * 0:0 * %s/pre.manifest\n' "$REC_DIR"
    printf 'f * 0:0 * %s/record\n' "$REC_DIR"
  } | awk '{ n = index($0, " /"); print substr($0, n + 1) "\t" $0 }' | LC_ALL=C sort -t$'\t' -k1,1 | cut -f2- >"$out"
}
norm_rec() { sed -E "s#^f [0-7]{4} ([0-9]+:[0-9]+) [0-9a-f]{64} ($REC_DIR/(record|pre\.manifest))\$#f * \\1 * \\2#" "$1"; }
manifest_is() { # <label> <actual> <expected>
  if diff <(norm_rec "$3") <(norm_rec "$2") >"$E/diff.out"; then ok "$1"; else bad "$1 (- expected, + actual)"; sed 's/^/        /' "$E/diff.out" | head -40; fi
}

# ---------------------------------------------------------------- invariants
# Every case ends here: the never-touched surface equals M0 (rule 1), the shim
# saw no reboot/poweroff, no sim7600-lte action, no unmodelled call, no start
# on stale unit config, and sim7600-lte's ActiveEnterTimestamp is unchanged.
invariants() { # <M0>
  M "$E/M.end"
  same_rows "G8: never-touched rows (/etc/xorgate, /opt/xorgate, JS wrapper/unit/.d/updater, sudoers 010+020-023, modem/system/cron/udev/NM/logind) == M0" never_rows "$1" "$E/M.end"
  check "G8: no reboot/poweroff/halt (shim log)" bash -c "! grep -qE '^(FORBIDDEN|(reboot|poweroff|halt|kexec)( |\$))' $E/systemctl.log"
  check "G8: sim7600-lte never started/stopped/restarted/disabled" bash -c "! grep -E '^(start|stop|restart|try-restart|reload|enable|disable|CONFLICT-STOP)( |.* )sim7600-lte' $E/systemctl.log | grep -q ."
  check "G8: sim7600-lte ActiveEnterTimestamp unchanged" [ "$(systemctl show -p ActiveEnterTimestamp --value sim7600-lte)" = "$SIM_TS0" ]
  check "G8: JS updater unit never started" bash -c "! grep -qE '^(start|restart).*xorgate-updater\\.service' $E/systemctl.log"
  check "shim: no UNMODELLED systemctl call" bash -c "! grep -q '^UNMODELLED' $E/systemctl.log"
  check "shim: no start on stale unit config (daemon-reload before start)" bash -c "! grep -q '^STALE' $E/systemctl.log"
  check "no reboot/poweroff/sim7600 restart in install-rs.sh source (G8 grep)" bash -c "! grep -nE '(^|[^-[:alnum:]_])(reboot|poweroff)([^-[:alnum:]_]|\$)|(restart|stop|try-restart)[^#]*sim7600' $IRS | grep -vE '^[0-9]+:[[:space:]]*#' | grep -q ."
}
finish() {
  echo "  --- systemctl calls (after provisioning)"
  sed 's/^/  > /' "$E/systemctl.log"
  echo "CASE $CASE: PASS $PASS FAIL $FAIL"
  [ "$FAIL" = 0 ]
  exit $?
}

# ---------------------------------------------------------------- composite checks
# assert_converted <M0> <tarball> <svc> <dropin> <ver> <jsEnabledBefore> <jsActiveBefore>
assert_converted() {
  local m0=$1 tgz=$2 svc=$3 dropin=$4 ver=$5 jse=$6 jsa=$7 hex
  M "$E/M.conv"
  expect_conv "$m0" "$E/M.expect" "$tgz" "$svc" "$dropin"
  manifest_is "I3: M_conv - M0 == exactly the §4 NEW/CHANGED rows (G8 footprint; CF67: no systemd/bench/* installed)" "$E/M.conv" "$E/M.expect"
  same_rows "/etc/xorgate rows + /opt/xorgate tree line identical" never_rows "$m0" "$E/M.conv"
  check "xorgate-agent-rs enabled + active" units_are xorgate-agent-rs.service enabled/active
  check "xorgate-agent disabled + inactive" units_are xorgate-agent.service disabled/inactive
  check "no JS worker process (node-stub)" bash -c "! pgrep -f '$JS_WORKER' >/dev/null"
  check "rs process = /opt/xorgate-rs/bin/xorgate-rs" [ "$(rs_exe)" = /opt/xorgate-rs/bin/xorgate-rs ]
  check "rs process runs as $svc (uid $(id -u "$svc"))" [ "$(rs_uid)" = "$(id -u "$svc")" ]
  check "unit == tarball systemd/xorgate-agent-rs.service" member_is "$RS_UNIT" "$tgz" systemd/xorgate-agent-rs.service
  check "unit carries Conflicts=xorgate-agent.service (shipped, not bench-stripped)" grep -qx 'Conflicts=xorgate-agent.service' "$RS_UNIT"
  check "updater unit == tarball systemd/xorgate-updater-rs.service" member_is "$RS_UPD" "$tgz" systemd/xorgate-updater-rs.service
  check "wrapper == update_plan::WRAPPER_BODY, 0755" bash -c 'body_is "$1" "$2" && mode_is "$1" 755' _ "$WRAP" "$WRAPPER_BODY"
  check "wrapper body is in WP1's golden (if present)" in_golden "$WRAPPER_BODY"
  check "10-dist.conf exact (§8), 0644" bash -c 'body_is "$1" "$2" && mode_is "$1" 644' _ "$DIST_DROPIN" "$DIST_BODY"
  if [ "$svc" != root ]; then
    check "024 == updater_sudoers_body($svc) (TS 022 body + O3/Q11 substitutions), 0440, visudo-clean" bash -c 'cmp -s "$1" "$2" && mode_is "$1" 440 && visudo.real -cf "$1" >/dev/null' _ "$S024" "$E/b024"
    check "024 body is in WP1's golden (if present)" in_golden "$(cat "$E/b024")"
    hex=$(sha_of <"$E/b024")
    check "visudo -cf ran on the 024 body BEFORE 024 existed (rule 3)" grep -qE "^-c.*\| sha256=$hex \| 024=absent\$" "$E/visudo.log"
  else
    check "root svc: no 024" [ ! -e "$S024" ]
    check "root svc: no rs 10-user.conf" [ ! -e "$RS_UNIT.d/10-user.conf" ]
  fi
  [ "$dropin" = 1 ] && check "rs 10-user.conf == JS 10-user.conf (byte copy)" same_file "$RS_UNIT.d/10-user.conf" "$SYS/xorgate-agent.service.d/10-user.conf"
  check "I2 record keys in order" rec_keys_ok
  check "record state=converted" [ "$(rec state)" = converted ]
  check "record version=$ver" [ "$(rec version)" = "$ver" ]
  check "record channel=dev" [ "$(rec channel)" = dev ]
  check "record sha256 == the installed tarball's" [ "$(rec sha256)" = "$(sha_of <"$tgz")" ]
  check "record jsEnabled=$jse jsActive=$jsa (raw is-enabled/is-active before)" [ "$(rec jsEnabled)/$(rec jsActive)" = "$jse/$jsa" ]
  check "record serviceUser=$svc" [ "$(rec serviceUser)" = "$svc" ]
  check "record startedAt + convertedAt set" bash -c "[ -n \"$(rec startedAt)\" ] && [ -n \"$(rec convertedAt)\" ]"
  local p bad_inst=""
  for p in $(rec installed); do
    case "$p" in /*) ;; *) bad_inst+=" $p(not-absolute)"; continue ;; esac
    grep -qE " $p\$" "$m0" && bad_inst+=" $p(pre-existed)"
  done
  check "record installed: absolute paths, none pre-existing${bad_inst:+:$bad_inst}" [ -z "$bad_inst" ]
  local missing=""
  for p in "$RS_UNIT" "$RS_UPD" "$DIST_DROPIN" "$WRAP"; do
    grep -qwF "$p" <<<" $(rec installed) " || missing+=" $p"
  done
  [ "$svc" != root ] && { grep -qwF "$S024" <<<" $(rec installed) " || missing+=" $S024"; }
  check "record installed lists every NEW file${missing:+ (missing:$missing)}" [ -z "$missing" ]
  if diff <(no_rec_rows "$m0") <(no_rec_rows "$REC_DIR/pre.manifest") >"$E/pre.diff" 2>&1; then
    ok "I3 cross-check: WP1's pre.manifest == WP2's independent M0 (outside the record dir)"
  else
    bad "I3 cross-check: WP1's pre.manifest == WP2's independent M0 (- WP2, + WP1)"; sed 's/^/        /' "$E/pre.diff" | head -20
  fi
  check "--status: first line state=converted, exit 0" status_first state=converted
}
export -f body_is mode_is

# assert_reverted <M0> <keep|purge> <jsEnabled> <jsActive>
assert_reverted() {
  local m0=$1 mode=$2 want_e=$3 want_a=$4
  M "$E/M.rev"
  if [ "$mode" = purge ]; then
    same_rows "I3: whole manifest == M0 (purge)" cat "$m0" "$E/M.rev"
  else
    same_rows "I3: JS-owned rows == M0 (keep)" js_rows "$m0" "$E/M.rev"
    check "keep: Rust files stay (unit, wrapper, /opt/xorgate-rs)" bash -c "[ -f $RS_UNIT ] && [ -f $WRAP ] && [ -x /opt/xorgate-rs/bin/xorgate-rs ]"
    check "keep: record renamed record.reverted-<ts>, no live record" bash -c "[ ! -e $REC ] && ls $REC_DIR/record.reverted-* >/dev/null 2>&1"
  fi
  check "xorgate-agent-rs disabled + not active" bash -c "[ \"\$(systemctl is-active xorgate-agent-rs)\" != active ] && ! [ -L $WANTS/xorgate-agent-rs.service ]"
  check "xorgate-agent $want_e + $want_a (from the record)" units_are xorgate-agent.service "$want_e/$want_a"
  if [ "$want_a" = active ]; then
    check "JS worker process running" js_running
  else
    check "no JS worker process" bash -c "! pgrep -f '$JS_WORKER' >/dev/null"
  fi
  local files=kept; [ "$mode" = purge ] && files=purged
  check "I1 final line (revert)" last_line_is "${P}reverted: xorgate-agent $want_e + $want_a; xorgate-agent-rs disabled (files $files). JS install matches the pre-conversion record: yes"
}

CONV_LINE() { echo "${P}converted: xorgate-agent-rs $1 (dev) active; xorgate-agent stopped + disabled. revert with: sudo /opt/xorgate-rs/install-rs.sh --revert"; }
LATEST=$SRV/xorgate-agent-rs-latest.tar.gz

# convert_ok <M0> <svc> <dropin> <jsE> <jsA> [irs args…]: a full conversion
convert_ok() {
  local m0=$1 svc=$2 dropin=$3 jse=$4 jsa=$5; shift 5
  [ "$svc" != root ] && body_024 "$svc" >"$E/b024"
  irs "$@"
  check "exit 0" rc_is 0
  check "I1 final line (convert)" last_line_is "$(CONV_LINE v1)"
  check "every output line carries '[install-rs] ' (rule 5)" all_prefixed
  local n; n=$(changed_n)
  check "changed (N) block: N == number of '  - ' lines (N=${n:-none})" bash -c "[ -n '$n' ] && [ \"\$(grep -cE '^\[install-rs\]   - ' $E/out)\" = '$n' ]"
  cp "$LATEST" "$E/conv.tgz"
  assert_converted "$m0" "$E/conv.tgz" "$svc" "$dropin" v1 "$jse" "$jsa"
}

echo "CASE $CASE"
start_bucket
case "$CASE" in
# -------------------------------------------------------------------------
convert-happy)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  check "G8: only the two agent units enabled/disabled; JS stopped only via Conflicts= or stop" bash -c "! grep -E '^(enable|disable)' $E/systemctl.log | grep -vE '^(enable|disable)( --now)? (xorgate-agent|xorgate-agent-rs)(\\.service)?\$' | grep -q ."
  check "Conflicts= stopped the JS agent when the rs unit started (or an explicit stop)" grep -qE '^(CONFLICT-STOP xorgate-agent\.service|stop xorgate-agent(\.service)?$|disable --now xorgate-agent)' "$E/systemctl.log"
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
convert-twice)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  cp "$REC" "$E/rec1"; cp "$E/M.conv" "$E/M1"; pid1=$(rs_pid)
  : >"$E/systemctl.log"
  irs
  check "2nd run exit 0" rc_is 0
  check "2nd run I1 final line (already converted … changed (0))" last_line_is "${P}already converted ($(rec convertedAt), v1); install verified, changed (0). change the build with: sudo systemctl start xorgate-updater-rs"
  M "$E/M2"
  check "I3: manifest byte-identical after the 2nd run" same_file "$E/M1" "$E/M2"
  check "I2: record byte-identical (written once)" same_file "$E/rec1" "$REC"
  check "rs not restarted (same MainPID)" [ "$(rs_pid)" = "$pid1" ]
  check "2nd run: no start/stop/restart/enable/disable" bash -c "! grep -qE '^(start|stop|restart|enable|disable|CONFLICT-STOP)' $E/systemctl.log"
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
convert-repair)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  cp "$E/M.conv" "$E/M1"
  rm -f "$WRAP" "$WANTS/xorgate-agent-rs.service"
  echo '# tampered' >>"$DIST_DROPIN"
  chmod 0644 "$S024"
  irs
  check "repair run exit 0" rc_is 0
  check "repair run reports changed (4) (wrapper, 10-dist.conf, 024 mode, enablement)" [ "$(changed_n)" = 4 ]
  M "$E/M2"
  manifest_is "I3: the surface is repaired to M_conv" "$E/M2" "$E/M1"
  check "rs enabled + active after repair" units_are xorgate-agent-rs.service enabled/active
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
revert-keep)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  : >"$E/systemctl.log"
  h0=$(wc -l <"$E/http.log")
  irs --revert
  check "revert exit 0" rc_is 0
  assert_reverted "$E/M0" keep enabled active
  check "§5.8: revert made no bucket request" [ "$(wc -l <"$E/http.log")" = "$h0" ]
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
revert-purge)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  irs --revert --purge
  check "purge exit 0" rc_is 0
  assert_reverted "$E/M0" purge enabled active
  check "purge ran daemon-reload after deleting the units" bash -c "tac $E/systemctl.log | grep -m1 -E '^(daemon-reload|start xorgate-agent)' | grep -q daemon-reload || grep -q daemon-reload $E/systemctl.log"
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
revert-twice)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  irs --revert
  check "1st revert exit 0" rc_is 0
  M "$E/Mr1"; jspid=$(pgrep -f "$JS_WORKER")
  : >"$E/systemctl.log"
  irs --revert
  check "2nd revert = no-op, exit 0" rc_is 0
  M "$E/Mr2"
  check "I3: manifest unchanged by the 2nd revert" same_file "$E/Mr1" "$E/Mr2"
  check "JS still enabled + active, not restarted" bash -c "[ \"\$(systemctl is-enabled xorgate-agent)/\$(systemctl is-active xorgate-agent)\" = enabled/active ] && [ \"\$(pgrep -f '$JS_WORKER')\" = '$jspid' ]"
  check "2nd revert: no start/stop/enable/disable" bash -c "! grep -qE '^(start|stop|restart|enable|disable|CONFLICT-STOP)' $E/systemctl.log"
  check "--status: state=reverted" status_first state=reverted
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
convert-revert-convert)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  st1=$(rec startedAt)
  irs --revert
  check "revert exit 0" rc_is 0
  sleep 1.1
  irs
  check "re-convert exit 0" rc_is 0
  check "re-convert I1 final line" last_line_is "$(CONV_LINE v1)"
  check "fresh record: state=converted, startedAt differs" bash -c "[ \"$(rec state)\" = converted ] && [ \"$(rec startedAt)\" != '$st1' ]"
  check "fresh record: jsEnabled=enabled jsActive=active (the restored JS)" [ "$(rec jsEnabled)/$(rec jsActive)" = enabled/active ]
  check "fresh record: rsPreexisting=yes (keep mode left /opt/xorgate-rs + unit; I2)" [ "$(rec rsPreexisting)" = yes ]
  check "the old record is kept as record.reverted-*" bash -c "ls $REC_DIR/record.reverted-* >/dev/null 2>&1"
  check "rs enabled + active, JS disabled + inactive" bash -c "[ \"\$(systemctl is-enabled xorgate-agent-rs)/\$(systemctl is-active xorgate-agent-rs)\" = enabled/active ] && [ \"\$(systemctl is-enabled xorgate-agent)/\$(systemctl is-active xorgate-agent)\" = disabled/inactive ]"
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
update-after-convert)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  cp "$E/M.conv" "$E/M1"
  # rule 2: the first update after a conversion rewrites nothing.
  : >"$E/systemctl.log"
  systemctl start xorgate-updater-rs.service; urc=$?
  check "same-build update (latest = v1) through the updater unit: exit 0" [ $urc = 0 ]
  M "$E/M1b"
  check "rule 2: same-build update leaves the WHOLE manifest byte-identical (unit/wrapper/updater/024 == what xorgate-rs update writes)" same_file "$E/M1" "$E/M1b"
  for f in "$RS_UNIT" "$RS_UPD" "$WRAP" "$S024" "$DIST_DROPIN"; do cp "$f" "$E/pre.$(basename "$f")"; done
  pid1=$(rs_pid)
  flip_latest v2
  : >"$E/systemctl.log"
  systemctl start xorgate-updater-rs.service; urc=$?
  sed 's/^/  j /' /run/e2e/journal/xorgate-updater-rs.service.log | tail -n 12
  check "update to v2 through the updater unit: exit 0" [ $urc = 0 ]
  check "the updater got DEVICE_DIST_VERSION=latest from 10-dist.conf (journal: downloading …-latest)" grep -q 'downloading http://127.0.0.1:8000/xorgate-agent-rs-latest.tar.gz' /run/e2e/journal/xorgate-updater-rs.service.log
  check "/opt/xorgate-rs is v2 (tree == tarball)" tree_is "$SRV/xorgate-agent-rs-v2.tar.gz"
  check "build-info version v2" grep -q '"version": *"v2"' /opt/xorgate-rs/build-info.json
  for f in "$RS_UNIT" "$RS_UPD" "$WRAP" "$S024" "$DIST_DROPIN"; do
    check "unchanged by the update (cmp): $f" same_file "$f" "$E/pre.$(basename "$f")"
  done
  check "10-dist.conf survives the update (the updater rewrites the unit, never its .d/)" [ -f "$DIST_DROPIN" ]
  check "rs restarted because it was active" grep -qE '^restart xorgate-agent-rs' "$E/systemctl.log"
  check "rs active again with a new MainPID" bash -c "[ \"\$(systemctl is-active xorgate-agent-rs)\" = active ] && [ \"\$(systemctl show -p MainPID --value xorgate-agent-rs)\" != '$pid1' ]"
  check "JS stays disabled + inactive" units_are xorgate-agent.service disabled/inactive
  M "$E/M2"
  if diff <(grep -v ' /opt/xorgate-rs$' "$E/M1") <(grep -v ' /opt/xorgate-rs$' "$E/M2") >/dev/null; then ok "I3: only the /opt/xorgate-rs tree line changed"; else bad "I3: only the /opt/xorgate-rs tree line changed"; fi
  irs
  check "install-rs.sh re-run after the update: already converted (…, v2), changed (0)" bash -c "[ \"\$(tail -n1 $E/out)\" = \"${P}already converted (\$(sed -n 's/^convertedAt=//p' $REC), v2); install verified, changed (0). change the build with: sudo systemctl start xorgate-updater-rs\" ]"
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
js-pin)
  provision --dist-version v0.1.2
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  check "Q11 JS-pin WARNING line (I1)" has_line "${P}WARNING: /etc/xorgate/agent.env pins DEVICE_DIST_VERSION=v0.1.2 (the JS agent's build); the Rust updater follows 'latest' through xorgate-updater-rs.service.d/10-dist.conf — a hand-run 'sudo xorgate-rs update' would request xorgate-agent-rs-v0.1.2 (use --tag latest)"
  check "§5.7: the convert ignored the pin (installed latest = v1)" grep -q '"version": *"v1"' /opt/xorgate-rs/build-info.json
  flip_latest v2
  mv "$DIST_DROPIN" /root/10-dist.conf.away
  systemctl daemon-reload
  M "$E/Ma"
  : >/run/e2e/journal/xorgate-updater-rs.service.log
  systemctl start xorgate-updater-rs.service; urc=$?
  sed 's/^/  j /' /run/e2e/journal/xorgate-updater-rs.service.log | tail -n 4
  check "CF63 without 10-dist.conf: the updater follows the JS pin and FAILS closed" [ $urc != 0 ]
  check "… on the 404 of xorgate-agent-rs-v0.1.2.tar.gz" grep -q 'GET /xorgate-agent-rs-v0.1.2.tar.gz HTTP/1.[01]" 404' "$E/http.log"
  M "$E/Mb"
  check "… device unchanged (manifest identical)" same_file "$E/Ma" "$E/Mb"
  mv /root/10-dist.conf.away "$DIST_DROPIN"
  systemctl daemon-reload
  systemctl start xorgate-updater-rs.service; urc=$?
  check "CF63 with 10-dist.conf: the update succeeds" [ $urc = 0 ]
  check "… /opt/xorgate-rs is now v2" grep -q '"version": *"v2"' /opt/xorgate-rs/build-info.json
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
bad-sha|missing-sidecar|wrong-arch|missing-member)
  provision
  M "$E/M0"
  case "$CASE" in
    bad-sha) tags=(badsha) ;;
    missing-sidecar) tags=(nosum) ;;
    wrong-arch) tags=(wrongarch) ;;
    missing-member) tags=(nomember-buildinfo nomember-installrs nomember-updater) ;;
  esac
  for t in "${tags[@]}"; do
    irs --tag "$t"
    check "[$t] exit 1" rc_is 1
    M "$E/M1"
    same_rows "[$t] §5.4: device unchanged (manifest == M0 outside the record dir)" no_rec_rows "$E/M0" "$E/M1"
    check "[$t] no record left as converted" bash -c "[ ! -e $REC ] || ! grep -qx 'state=converted' $REC"
    check "[$t] JS still enabled + active" units_are xorgate-agent.service enabled/active
    check "[$t] no rs process, no .new/.old" bash -c "[ \"\$(systemctl is-active xorgate-agent-rs)\" != active ] && [ ! -e /opt/xorgate-rs.new ] && [ ! -e /opt/xorgate-rs.old ]"
    check "[$t] no systemctl start/stop/enable/disable" bash -c "! grep -qE '^(start|stop|restart|enable|disable|daemon-reload)' $E/systemctl.log"
  done
  check "--status: not state=converted" bash -c "! bash $IRS --status | head -n1 | grep -qx state=converted"
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
failed-start)
  provision
  M "$E/M0"
  irs --tag runfails
  check "exit 4 (started but unhealthy → auto-reverted)" rc_is 4
  M "$E/M1"
  same_rows "I3: JS-owned rows == M0" js_rows "$E/M0" "$E/M1"
  check "JS restored: enabled + active" units_are xorgate-agent.service enabled/active
  check "JS worker process running" js_running
  check "rs disabled + not active" bash -c "[ \"\$(systemctl is-active xorgate-agent-rs)\" != active ] && ! [ -L $WANTS/xorgate-agent-rs.service ]"
  check "record moved to record.reverted-* (keep-mode revert)" bash -c "[ ! -e $REC ] && ls $REC_DIR/record.reverted-* >/dev/null 2>&1"
  check "output carries the journal tail (runfails stub's stderr)" out_has 'runfails stub: exiting 1 on run'
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
interrupt-[1-8]|interrupt-[1-8]-revert)
  n=${CASE#interrupt-}; n=${n%-revert}
  provision
  M "$E/M0"
  irs XORGATE_RS_CONVERT_FAIL_AT="$n"
  check "FAIL_AT=$n: exit 1" rc_is 1
  M "$E/Mi"
  check "FAIL_AT=$n: record state=converting + pre.manifest written (§5.1)" bash -c "grep -qx state=converting $REC && [ -s $REC_DIR/pre.manifest ]"
  if [ "$n" -le 7 ]; then
    same_rows "FAIL_AT=$n: exit 1 = device at its pre-run state (manifest == M0 outside the record dir)" no_rec_rows "$E/M0" "$E/Mi"
  else
    same_rows "FAIL_AT=$n: JS-owned rows == M0" js_rows "$E/M0" "$E/Mi"
  fi
  check "FAIL_AT=$n: exit 1 = JS back at its pre-run state (enabled + active)" units_are xorgate-agent.service enabled/active
  check "FAIL_AT=$n: --status: state=converting" status_first state=converting
  cp "$REC" "$E/rec.i"; cp "$REC_DIR/pre.manifest" "$E/pre.i"
  if [ "${CASE%-revert}" = "$CASE" ]; then
    irs
    check "re-run after FAIL_AT=$n: exit 0" rc_is 0
    check "re-run I1 final line (convert)" last_line_is "$(CONV_LINE v1)"
    check "§5.1: pre.manifest NOT rewritten by the re-run" same_file "$E/pre.i" "$REC_DIR/pre.manifest"
    check "§5.1: jsEnabled/jsActive/startedAt kept from the first run" bash -c "for k in startedAt jsEnabled jsActive; do [ \"\$(sed -n \"s/^\$k=//p\" $E/rec.i)\" = \"\$(sed -n \"s/^\$k=//p\" $REC)\" ] || exit 1; done"
    cp "$LATEST" "$E/conv.tgz"
    body_024 pi >"$E/b024"
    assert_converted "$E/M0" "$E/conv.tgz" pi 1 v1 enabled active
  else
    irs --revert
    check "revert from FAIL_AT=$n: exit 0" rc_is 0
    assert_reverted "$E/M0" keep enabled active
  fi
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
root-svc)
  provision --svc root --no-user-dropin
  M "$E/M0"
  convert_ok "$E/M0" root 0 enabled active
  check "visudo never asked about a 024 body" bash -c "! grep -q xorgate-update-rs $E/visudo.log"
  invariants "$E/M0" ;;
root-svc-dropin)
  # be:268-273 writes 10-user.conf with User=root on a root-provisioned device.
  provision --svc root
  M "$E/M0"
  convert_ok "$E/M0" root 1 enabled active
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
js-disabled-before)
  provision --js disabled-inactive
  M "$E/M0"
  convert_ok "$E/M0" pi 1 disabled inactive
  irs --revert
  check "revert exit 0" rc_is 0
  assert_reverted "$E/M0" keep disabled inactive
  invariants "$E/M0" ;;
js-stopped-before)
  provision --js enabled-inactive
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled inactive
  irs --revert
  check "revert exit 0" rc_is 0
  assert_reverted "$E/M0" keep enabled active
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
not-root)
  provision
  M "$E/M0"
  irs --as pi-nosudo
  check "exit 3 (not root)" rc_is 3
  M "$E/M1"
  check "nothing changed (manifest == M0, no record dir)" same_file "$E/M0" "$E/M1"
  check "no systemctl call that changes state" bash -c "! grep -qE '^(start|stop|restart|enable|disable|daemon-reload)' $E/systemctl.log"
  invariants "$E/M0" ;;
not-provisioned)
  provision
  M "$E/M0"
  for miss in /etc/xorgate/agent.env /opt/xorgate "$SYS/xorgate-agent.service"; do
    mv "$miss" "$miss.e2e-away"
    M "$E/Mp"
    irs
    check "[$miss missing] exit 3" rc_is 3
    M "$E/M1"
    check "[$miss missing] nothing changed (no record dir)" same_file "$E/Mp" "$E/M1"
    check "[$miss missing] no state-changing systemctl call" bash -c "! grep -qE '^(start|stop|restart|enable|disable|daemon-reload)' $E/systemctl.log"
    mv "$miss.e2e-away" "$miss"
  done
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
foreign-dropin)
  # The bench shape (§12 step 3): the bench drop-in pre-seeded, JS enabled but
  # stopped by the exclusive window; the dev identity env file it names.
  provision --js enabled-inactive
  install -d -m 0755 "$RS_UNIT.d"
  tar -xzf "$LATEST" -O systemd/bench/90-bench-dev.conf >"$RS_UNIT.d/90-bench-dev.conf"
  chmod 0644 "$RS_UNIT.d/90-bench-dev.conf"
  install -m 0600 -o pi -g pi /etc/xorgate/agent.env /etc/xorgate/agent-rs-dev.env
  systemctl daemon-reload
  M "$E/M0"
  SIM_TS0=$(systemctl show -p ActiveEnterTimestamp --value sim7600-lte)
  convert_ok "$E/M0" pi 1 enabled inactive
  check "90-bench-dev.conf kept byte-identical" bash -c "grep -F ' $RS_UNIT.d/90-bench-dev.conf' $E/M0 | cmp -s - <(grep -F ' $RS_UNIT.d/90-bench-dev.conf' $E/M.conv)"
  check "the foreign drop-in is named in the output" out_has 90-bench-dev.conf
  check "record rsPreexisting=no (only the .d/ existed, not /opt/xorgate-rs or the unit; I2)" [ "$(rec rsPreexisting)" = no ]
  check "rs runs with the bench drop-in (XORGATE_HOME=/var/lib/xorgate-rs)" [ "$(rs_env XORGATE_HOME)" = /var/lib/xorgate-rs ]
  irs --revert --purge
  check "purge exit 0" rc_is 0
  assert_reverted "$E/M0" purge enabled active
  check "purge names the foreign drop-in it leaves" out_has 90-bench-dev.conf
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
bootstrap-oneliner)
  provision
  M "$E/M0"
  Q3='B=$(sed -n '"'"'s/^DEVICE_DIST_BUCKET_URL=//p'"'"' /etc/xorgate/agent.env); curl -fsSLO "$B/install-rs.sh" -O "$B/install-rs.sh.sha256" && sha256sum -c install-rs.sh.sha256 && sudo bash install-rs.sh'
  ONE=$Q3
  if [ -f /in/convert.md ]; then
    doc=$(grep -F 'install-rs.sh.sha256' /in/convert.md | grep -F 'sha256sum -c' | grep -F 'sudo bash install-rs.sh' | head -n1 | sed 's/^[[:space:]]*//; s/^\$ //')
    check "docs/convert.md carries the bootstrap one-liner (sha256-checked)" [ -n "$doc" ]
    [ -n "$doc" ] && ONE=$doc
    [ "$ONE" = "$Q3" ] || note "docs one-liner differs from the plan's Q3 text; running the docs' verbatim"
  else
    note "no docs/convert.md (pre-WP1): running the plan's Q3 one-liner verbatim"
  fi
  echo "  one-liner: $ONE"
  echo "$ONE" >/home/pi/oneliner.sh
  (cd /home/pi && runuser -u pi -- bash -c "$ONE") >"$E/out" 2>"$E/err"; echo $? >"$E/rc"
  sed 's/^/  | /' "$E/out"; sed 's/^/  ! /' "$E/err"
  check "one-liner exit 0" rc_is 0
  check "sha256sum -c OK on the bucket copy" grep -qx 'install-rs.sh: OK' "$E/out"
  check "I1 final line (convert)" last_line_is "$(CONV_LINE v1)"
  cp "$LATEST" "$E/conv.tgz"; body_024 pi >"$E/b024"
  assert_converted "$E/M0" "$E/conv.tgz" pi 1 v1 enabled active
  check "the bucket copy == /opt/xorgate-rs/install-rs.sh (the tarball copy)" same_file /home/pi/install-rs.sh /opt/xorgate-rs/install-rs.sh
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
offline-revert)
  provision
  M "$E/M0"
  convert_ok "$E/M0" pi 1 enabled active
  check "bucket stopped" stop_bucket
  printf 'not an executable\n' >/opt/xorgate-rs/bin/xorgate-rs
  irs --as cmd /opt/xorgate-rs/install-rs.sh --revert
  check "§5.8: revert with no network and a broken Rust binary: exit 0" rc_is 0
  assert_reverted "$E/M0" keep enabled active
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
tarball-mode)
  provision
  M "$E/M0"
  check "bucket stopped (--tarball needs no network)" stop_bucket
  install -d -o pi -g pi /home/pi/dl
  install -m 0644 -o pi -g pi "$SRV/xorgate-agent-rs-v1.tar.gz" /home/pi/dl/
  irs --tarball /home/pi/dl/xorgate-agent-rs-v1.tar.gz
  check "no sidecar: exit 1 (sidecar REQUIRED)" rc_is 1
  M "$E/M1"
  same_rows "no sidecar: device unchanged outside the record dir" no_rec_rows "$E/M0" "$E/M1"
  install -m 0644 -o pi -g pi "$SRV/xorgate-agent-rs-v1.tar.gz.sha256" /home/pi/dl/
  body_024 pi >"$E/b024"
  irs --tarball /home/pi/dl/xorgate-agent-rs-v1.tar.gz
  check "with sidecar: exit 0" rc_is 0
  check "I1 final line (convert)" last_line_is "$(CONV_LINE v1)"
  assert_converted "$E/M0" /home/pi/dl/xorgate-agent-rs-v1.tar.gz pi 1 v1 enabled active
  check "record source names the tarball" bash -c "grep -q '^source=.*xorgate-agent-rs-v1.tar.gz' $REC"
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
visudo-refuses)
  provision
  M "$E/M0"
  touch "$E/visudo-refuse"
  irs
  check "visudo refusal is not fatal: exit 0" rc_is 0
  check "I1 final line (convert)" last_line_is "$(CONV_LINE v1)"
  check "a WARNING about the refused 024" grep -qE '^\[install-rs\] WARNING.*(024|sudoers)' "$E/out" "$E/err"
  check "no 024 installed" [ ! -e "$S024" ]
  check "rs enabled + active anyway" units_are xorgate-agent-rs.service enabled/active
  invariants "$E/M0" ;;
# -------------------------------------------------------------------------
usage-status)
  provision
  M "$E/M0"
  check "--status on a JS device: state=not-converted, exit 0" status_first state=not-converted
  irs --bogus-flag
  check "unknown flag: exit 2" rc_is 2
  irs -h
  check "-h: exit 0" rc_is 0
  check "-h documents XORGATE_RS_CONVERT_FAIL_AT as test-only" bash -c "grep -q XORGATE_RS_CONVERT_FAIL_AT $E/out $E/err && grep -i XORGATE_RS_CONVERT_FAIL_AT $E/out $E/err | grep -qi test"
  M "$E/M1"
  check "nothing changed" same_file "$E/M0" "$E/M1"
  invariants "$E/M0" ;;
*)
  bad "unknown case $CASE" ;;
esac
finish
