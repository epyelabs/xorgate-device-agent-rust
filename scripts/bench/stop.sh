#!/usr/bin/env bash
# Disable + stop xorgate-agent-rs on the bench. Idempotent; the JS prod agent
# is untouched (prints both units' state to prove it).
#
#   scripts/bench/stop.sh
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

log "disable --now $UNIT on $BENCH"
# shellcheck disable=SC2119
bench_sudo_script <<'REMOTE'
set -euo pipefail
UNIT=xorgate-agent-rs
if ! systemctl cat $UNIT >/dev/null 2>&1; then
  echo "$UNIT is not installed — nothing to stop"
else
  # Time the graceful stop: the agent must publish online:false and exit 0
  # within 3 s (acceptance A5).
  t0=$(date +%s.%N)
  systemctl disable --now $UNIT
  t1=$(date +%s.%N)
  echo "stopped in $(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}') s; last exit: $(systemctl show $UNIT -p ExecMainStatus -p Result --value | paste -sd' ' -)"
fi
printf '%-18s %s\n' xorgate-agent "$(systemctl is-active xorgate-agent || true)" \
                    $UNIT "$(systemctl is-active $UNIT || true)"
REMOTE
