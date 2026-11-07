#!/usr/bin/env bash
# O-P5G-1 (user-approved 2026-11-06): the crash-loop keys StartLimitIntervalSec=
# and StartLimitBurst= belong to [Unit]. Under [Service], systemd 252 (Debian
# bookworm, the bench and the fleet) logs
#   Unknown key 'StartLimitIntervalSec' in section [Service], ignoring.
# and keeps the default 10 s window. Run in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/tests/unit-sections.sh
# Checks every Rust unit (systemd/*.service) and every bench drop-in
# (systemd/bench/*.conf):
#   1. static: no StartLimit*= key outside [Unit] (a self-test proves the check
#      catches the old [Service] placement);
#   2. systemd 252 itself: `systemd-analyze verify` of each unit (with the bench
#      drop-in for the agent) prints no "Unknown key" line.
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }

# Print "<file>:<line>: <key> in [<section>]" for each StartLimit* key that is
# not in [Unit].
misplaced() {
  awk '
    /^[[:space:]]*\[/ { sec = $0; gsub(/[[:space:]]|\[|\]/, "", sec); next }
    /^[[:space:]]*(#|;)/ { next }
    /^[[:space:]]*StartLimit[A-Za-z]*[[:space:]]*=/ {
      key = $0; sub(/[[:space:]]*=.*/, "", key); gsub(/[[:space:]]/, "", key)
      if (sec != "Unit") printf "%s:%d: %s in [%s]\n", FILENAME, FNR, key, sec
    }' "$@"
}

echo "== self-test: the check flags the pre-O-P5G-1 placement"
T=$(mktemp -d)
printf '[Unit]\nDescription=x\n\n[Service]\nExecStart=/bin/true\nStartLimitIntervalSec=60\nStartLimitBurst=10\n' > "$T/old.service"
printf '[Unit]\nDescription=x\nStartLimitIntervalSec=60\nStartLimitBurst=10\n[Service]\nExecStart=/bin/true\n# StartLimitBurst=3 (a comment)\n' > "$T/new.service"
[ "$(misplaced "$T/old.service" | wc -l)" -eq 2 ] && ok "self-test: both keys under [Service] are flagged" || bad "self-test: old placement not flagged"
[ -z "$(misplaced "$T/new.service")" ] && ok "self-test: [Unit] keys and comments pass" || bad "self-test: [Unit] placement flagged"

echo "== static: StartLimit* only in [Unit]"
units=("$REPO"/systemd/*.service "$REPO"/systemd/bench/*.conf)
out=$(misplaced "${units[@]}")
[ -z "$out" ] && ok "no StartLimit* key outside [Unit] in ${#units[@]} unit files / drop-ins" || { bad "misplaced keys:"; echo "$out"; }
grep -q '^StartLimitIntervalSec=60$' "$REPO/systemd/xorgate-agent-rs.service" && grep -q '^StartLimitBurst=10$' "$REPO/systemd/xorgate-agent-rs.service"
[ $? = 0 ] && ok "xorgate-agent-rs.service keeps 10 starts / 60 s" || bad "xorgate-agent-rs.service lost its StartLimit values"

# O-P6-1 (Phase 6 window 1): Conflicts= without an ordering let a convert start
# the Rust agent in parallel with the JS agent's stop. The unit must carry
# After=xorgate-agent.service in [Unit] next to its Conflicts=.
# unit_has <file> <section> <key> <value>: the key=value line (one of possibly
# several space-separated values) is present, uncommented, in that section.
unit_has() {
  awk -v want_sec="$2" -v key="$3" -v val="$4" '
    /^[[:space:]]*\[/ { sec = $0; gsub(/[[:space:]]|\[|\]/, "", sec); next }
    /^[[:space:]]*(#|;)/ { next }
    {
      line = $0; sub(/^[[:space:]]*/, "", line)
      k = line; sub(/[[:space:]]*=.*/, "", k)
      if (k != key || sec != want_sec) next
      v = line; sub(/^[^=]*=[[:space:]]*/, "", v)
      n = split(v, vs, /[[:space:]]+/)
      for (i = 1; i <= n; i++) if (vs[i] == val) found = 1
    }
    END { exit found ? 0 : 1 }' "$1"
}
echo "== static: O-P6-1 ordering against the JS agent"
RS_UNIT="$REPO/systemd/xorgate-agent-rs.service"
printf '[Unit]\nConflicts=xorgate-agent.service\n[Service]\nExecStart=/bin/true\n' > "$T/noafter.service"
printf '[Unit]\nConflicts=xorgate-agent.service\n# After=xorgate-agent.service\n[Service]\nAfter=xorgate-agent.service\nExecStart=/bin/true\n' > "$T/wrongsec.service"
printf '[Unit]\nAfter=network-online.target xorgate-agent.service\n[Service]\nExecStart=/bin/true\n' > "$T/multi.service"
! unit_has "$T/noafter.service" Unit After xorgate-agent.service && ok "self-test: a unit without After=xorgate-agent.service is flagged" || bad "self-test: missing After= not flagged"
! unit_has "$T/wrongsec.service" Unit After xorgate-agent.service && ok "self-test: a commented or [Service] After= is flagged" || bad "self-test: commented/[Service] After= accepted"
unit_has "$T/multi.service" Unit After xorgate-agent.service && ok "self-test: a multi-value After= line counts" || bad "self-test: multi-value After= not recognised"
unit_has "$RS_UNIT" Unit After xorgate-agent.service && ok "xorgate-agent-rs.service: After=xorgate-agent.service in [Unit]" || bad "xorgate-agent-rs.service: no After=xorgate-agent.service in [Unit] (O-P6-1)"
unit_has "$RS_UNIT" Unit Conflicts xorgate-agent.service && ok "xorgate-agent-rs.service: Conflicts=xorgate-agent.service in [Unit]" || bad "xorgate-agent-rs.service: lost Conflicts=xorgate-agent.service"
# The bench's installed copy (scripts/bench/install.sh strips the Conflicts=
# line with this exact sed) keeps the ordering and loses only the conflict.
BENCH_SED='s|^Conflicts=xorgate-agent.service$|# BENCH (scripts/bench/install.sh): Conflicts=xorgate-agent.service stripped — both agents coexist on the bench|'
grep -qF "$BENCH_SED" "$REPO/scripts/bench/install.sh" && ok "bench install.sh still strips Conflicts= with the modelled sed" || bad "bench install.sh strip changed: update this model"
sed -e "$BENCH_SED" "$RS_UNIT" > "$T/bench-xorgate-agent-rs.service"
{ ! grep -q '^Conflicts=' "$T/bench-xorgate-agent-rs.service" && unit_has "$T/bench-xorgate-agent-rs.service" Unit After xorgate-agent.service; } \
  && ok "bench shape: Conflicts= stripped, After=xorgate-agent.service kept" || bad "bench shape wrong"

echo "== systemd-analyze verify (systemd 252)"
if ! command -v systemd-analyze >/dev/null; then
  apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq systemd >/dev/null 2>&1
fi
if command -v systemd-analyze >/dev/null; then
  echo "  $(systemctl --version | head -1)"
  # The ExecStart/EnvironmentFile targets verify checks for.
  mkdir -p /opt/xorgate-rs/bin /usr/local/bin /etc/xorgate
  for f in /opt/xorgate-rs/bin/xorgate-rs /usr/local/bin/xorgate-rs; do [ -e "$f" ] || { printf '#!/bin/sh\n' > "$f"; chmod 0755 "$f"; }; done
  touch /etc/xorgate/agent.env /etc/xorgate/agent-rs-dev.env
  unknown() { # <unit file> [drop-in] -> prints systemd's "Unknown key" lines
    local d u; d=$(mktemp -d); cp "$1" "$d/"; u=$(basename "$1")
    if [ -n "${2:-}" ]; then mkdir -p "$d/$u.d"; cp "$2" "$d/$u.d/"; fi
    SYSTEMD_UNIT_PATH="$d:" systemd-analyze verify "$d/$u" 2>&1 | grep 'Unknown key'
    rm -rf "$d"
  }
  neg=$(unknown "$T/old.service")
  printf '%s' "$neg" | grep -q "Unknown key 'StartLimitIntervalSec' in section \[Service\]" \
    && ok "negative: systemd 252 ignores the old [Service] StartLimitIntervalSec ($neg)" \
    || bad "negative: systemd did not report the [Service] StartLimitIntervalSec (got: ${neg:-nothing})"
  for u in "$REPO"/systemd/*.service; do
    o=$(unknown "$u"); [ -z "$o" ] && ok "$(basename "$u"): no 'Unknown key'" || bad "$(basename "$u"): $o"
  done
  for c in "$REPO"/systemd/bench/*.conf; do
    o=$(unknown "$REPO/systemd/xorgate-agent-rs.service" "$c")
    [ -z "$o" ] && ok "xorgate-agent-rs.service + bench/$(basename "$c"): no 'Unknown key'" || bad "bench/$(basename "$c"): $o"
  done
  # O-P6-1: the whole verify output must be EMPTY (not only free of "Unknown
  # key"), next to the JS unit the Conflicts=/After= name (its shape on a
  # device: device/xorgate-device-agent systemd/xorgate-agent.service @ 09da38d).
  for f in /usr/local/bin/xorgate; do [ -e "$f" ] || { printf '#!/bin/sh\n' > "$f"; chmod 0755 "$f"; }; done
  JS_STUB='[Unit]\nDescription=Xorgate device agent (IoT MQTT telemetry)\nAfter=network-online.target\nWants=network-online.target\n[Service]\nType=simple\nUser=root\nEnvironmentFile=/etc/xorgate/agent.env\nStateDirectory=xorgate\nExecStartPre=-/usr/local/bin/xorgate led boot\nExecStart=/usr/bin/env node /opt/xorgate/dist/worker.js\nExecStopPost=-/usr/local/bin/xorgate led exit-state\nRestart=always\n[Install]\nWantedBy=multi-user.target\n'
  verify_all() { # <unit file> [drop-in] -> every line systemd-analyze verify prints
    local d u; d=$(mktemp -d); cp "$1" "$d/"; u=$(basename "$1")
    printf "$JS_STUB" > "$d/xorgate-agent.service"
    if [ -n "${2:-}" ]; then mkdir -p "$d/$u.d"; cp "$2" "$d/$u.d/"; fi
    SYSTEMD_UNIT_PATH="$d:" systemd-analyze verify "$d/$u" "$d/xorgate-agent.service" 2>&1
    rm -rf "$d"
  }
  o=$(verify_all "$RS_UNIT"); [ -z "$o" ] && ok "xorgate-agent-rs.service (+ the JS unit): systemd-analyze verify prints nothing" || bad "xorgate-agent-rs.service verify: $o"
  o=$(verify_all "$T/bench-xorgate-agent-rs.service" "$REPO/systemd/bench/90-bench-dev.conf")
  [ -z "$o" ] && ok "bench shape + 90-bench-dev.conf (+ the JS unit): systemd-analyze verify prints nothing" || bad "bench shape verify: $o"
  o=$(verify_all "$REPO/systemd/xorgate-updater-rs.service"); [ -z "$o" ] && ok "xorgate-updater-rs.service: systemd-analyze verify prints nothing" || bad "xorgate-updater-rs.service verify: $o"
else
  bad "systemd-analyze unavailable (apt-get install systemd failed)"
fi
rm -rf "$T"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
