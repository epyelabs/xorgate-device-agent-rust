#!/usr/bin/env bash
# Switch the Rust agent's INPUT profile (plan decision O6), then daemon-reload
# and restart xorgate-agent-rs. Idempotent; prints what it changed.
#
#   scripts/bench/profile.sh shared     [--dry-run] [--no-restart]
#   scripts/bench/profile.sh exclusive  [--dry-run] [--no-restart]
#
#   shared    (default, persistent) /etc/systemd/system/xorgate-agent-rs.service.d/95-inputs.conf:
#             GPS_NMEA_PORT=/run/xorgate-rs-no-gps, XORGATE_PYTHON_DIR=/var/lib/xorgate-rs/no-python;
#             removes the runtime exclusive drop-in.
#   exclusive (RUNTIME) /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf:
#             UnsetEnvironment of both → the agent's own defaults (/dev/ttyUSB1, packaged python).
#             Vanishes on reboot. REFUSES while xorgate-agent (JS) is active — use
#             exclusive.sh begin, which stops it first.
# Never touches xorgate-agent.service, sim7600-lte or anything of the JS agent.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
MODE="${1:-}"; shift || true
DRY=0; RESTART=1
for a in "$@"; do
  case "$a" in --dry-run) DRY=1 ;; --no-restart) RESTART=0 ;; *) echo "unknown arg $a" >&2; exit 2 ;; esac
done
case "$MODE" in shared|exclusive) ;; *) sed -n '2,18p' "$0"; exit 2 ;; esac
log "profile $MODE on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY MODE=$MODE RESTART=$RESTART"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
[ -f /etc/systemd/system/$UNIT.service ] || { echo "$UNIT not installed — run scripts/bench/install.sh first"; exit 1; }
echo "== profile $MODE"
if [ "$MODE" = shared ]; then profile_shared; else profile_exclusive; fi
if [ "$RESTART" = 1 ]; then echo "== reload + restart $UNIT"; reload_restart_rs; else act systemctl daemon-reload; fi
units_report
echo "== done: ${#CHANGED[@]} change(s)$([ "$DRY" = 1 ] && echo ' (dry run: nothing written)')"
REMOTE
} | bench_sudo_script
