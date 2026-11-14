#!/usr/bin/env bash
# /dev/shm audit on the bench (plan phase-3-recording-media.md §Bench recipe
# step 4; evidence for D13/D15/D17 and P7). READ-ONLY by construction.
#
#   scripts/bench/shm-audit.sh [--json] [--fail-on-dead]
#
# For every /dev/shm/shmpipe.<pid>.* (shmsink's segments) prints name, size,
# pid and owner: JS (the pid is in xorgate-agent.service's cgroup or descends
# from its MainPID), RUST (same for xorgate-agent-rs), OTHER-LIVE, or DEAD (no
# such pid / a zombie: an orphaned segment). Also lists the shm sockets
# /var/lib/xorgate-rs/shm/*.sock and /var/lib/xorgate/shm/*.sock. Ends with a
# SUMMARY line of counts (or a JSON document with --json).
# Exit 0 always (evidence, not a gate) unless --fail-on-dead and a DEAD row exists.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
JSON=0; FAIL_DEAD=0
for a in "$@"; do
  case "$a" in --json) JSON=1 ;; --fail-on-dead) FAIL_DEAD=1 ;; -h|--help) sed -n '2,15p' "$0"; exit 0 ;; *) echo "unknown arg $a" >&2; exit 2 ;; esac
done
[ "$JSON" = 1 ] || log "shm audit on $BENCH"
{ echo "JSON=$JSON FAIL_DEAD=$FAIL_DEAD"; bench_remote_lib; cat <<'REMOTE'
ROWS=$(shm_audit_rows)
if [ "$JSON" = 1 ]; then shm_audit --json; else echo "== shm audit ($(hostname), $(date -u +%Y-%m-%dT%H:%M:%SZ))"; shm_audit; fi
DEAD=$(awk -F'\t' '$1=="shm" && $5=="DEAD"{n++} END{print n+0}' <<<"$ROWS")
if [ "$FAIL_DEAD" = 1 ] && [ "$DEAD" -gt 0 ]; then
  [ "$JSON" = 1 ] || echo "FAIL: $DEAD orphaned shmpipe segment(s) (owner DEAD)"
  exit 1
fi
exit 0
REMOTE
} | bench_sudo_script
