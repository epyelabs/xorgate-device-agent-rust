#!/usr/bin/env bash
# Enable + start xorgate-agent-rs on the bench (dev identity, next to the JS
# prod agent). Idempotent. Refuses to start without the bench env file so a
# half-installed bench can never bring the unit up on the shipped
# /etc/xorgate/agent.env (= the PROD identity the JS agent owns).
#
#   scripts/bench/start.sh
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

log "enable --now $UNIT on $BENCH"
# shellcheck disable=SC2119
bench_sudo_script <<'REMOTE'
set -euo pipefail
UNIT=xorgate-agent-rs
[ -f /etc/systemd/system/$UNIT.service ] || { echo "unit not installed — run scripts/bench/install.sh first"; exit 1; }
[ -f /etc/systemd/system/$UNIT.service.d/90-bench-dev.conf ] || { echo "bench drop-in missing — run scripts/bench/install.sh first"; exit 1; }
[ -f /etc/xorgate/agent-rs-dev.env ] || { echo "/etc/xorgate/agent-rs-dev.env missing — run scripts/bench/install.sh first"; exit 1; }
# Belt + braces: the effective Conflicts= must not name the JS prod agent,
# else starting us would stop it. (shutdown.target is always listed
# implicitly, so "empty" is not the test.)
CONFLICTS=$(systemctl show $UNIT -p Conflicts --value)
case " $CONFLICTS " in
  *" xorgate-agent.service "*) echo "effective Conflicts='$CONFLICTS' names xorgate-agent.service — install.sh's bench strip not applied; refusing (daemon-reload?)"; exit 1 ;;
esac
systemctl reset-failed $UNIT 2>/dev/null || true
systemctl enable --now $UNIT
sleep 2
printf '%-18s %s\n' xorgate-agent "$(systemctl is-active xorgate-agent || true)" \
                    $UNIT "$(systemctl is-active $UNIT || true)"
systemctl status --no-pager -n 15 $UNIT || true
REMOTE
