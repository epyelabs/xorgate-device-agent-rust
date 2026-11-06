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
else
  bad "systemd-analyze unavailable (apt-get install systemd failed)"
fi
rm -rf "$T"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
