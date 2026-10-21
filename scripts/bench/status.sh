#!/usr/bin/env bash
# Both agents' state on the bench + our unit's last 20 journal lines + the
# Rust process' numbers. Read-only; works even before install.sh ran.
#
#   scripts/bench/status.sh
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

bench_ssh bash -s <<'REMOTE'
set -uo pipefail
UNIT=xorgate-agent-rs
echo "== units ($(hostname), $(date -u +%Y-%m-%dT%H:%M:%SZ))"
printf '  %-18s %s\n' xorgate-agent "$(systemctl is-active xorgate-agent)" \
                      $UNIT "$(systemctl is-active $UNIT)" \
                      sim7600-lte "$(systemctl is-active sim7600-lte)"
echo "== $UNIT"
systemctl status --no-pager -n 20 $UNIT 2>&1 || true
echo "== process"
ps -o pid,rss,pcpu,etime,cmd -C xorgate-rs || echo "  (no xorgate-rs process)"
echo "== build"
cat /opt/xorgate-rs/build-info.json 2>/dev/null || echo "  (not installed)"
# Phase 4 (WP6): the shared-profile verification of phase-4-operations.md §9 /
# the WP6 brief — read-only (the one sudo is `visudo -c`, a syntax check).
echo "== phase 4 (shared profile: LED/SMS knobs, helpers refused, updater rail)"
echo "  unit env: $(systemctl show $UNIT -p Environment --value | tr ' ' '\n' | grep -E '^(XORGATE_LED_GPIOCHIP|XORGATE_LED_WRITER|XORGATE_SMS_PORT)=' | tr '\n' ' ')"
for p in /run/xorgate-rs-no-gpio /run/xorgate-rs-no-sms; do [ -e "$p" ] && echo "  UNSAFE: $p exists" || echo "  ok: $p absent"; done
SINCE=$(systemctl show $UNIT -p ActiveEnterTimestamp --value)
if [ -n "$SINCE" ]; then
  N=$(journalctl _COMM=sudo --since "$SINCE" --no-pager -q 2>/dev/null | grep -cE 'COMMAND=/usr/local/bin/xorgate-(setclock|camrotate)' || true)
  echo "  sudo journal since $UNIT start ($SINCE): $N xorgate-setclock/xorgate-camrotate run(s) — BOTH agents run as pi, so this includes the JS prod agent's own legitimate steps/rotations; the Rust agent's are refused by its sudo shim (shared profile), so compare with: journalctl -u xorgate-agent-rs | grep -c 'disabled in the shared profile'"
fi
if command -v gpioinfo >/dev/null 2>&1; then
  echo "  gpioinfo lines 12/16/21: $(gpioinfo gpiochip0 2>/dev/null | grep -E 'line +(12|16|21):' | tr -s ' ' | tr '\n' ';')"
  gpioinfo 2>/dev/null | grep -q '"xorgate-rs"' && echo "  UNSAFE: a gpio line is held by consumer xorgate-rs" || echo "  ok: no gpio line held by consumer xorgate-rs"
fi
systemctl cat xorgate-updater-rs.service >/dev/null 2>&1 && echo "  ok: xorgate-updater-rs.service installed ($(systemctl is-active xorgate-updater-rs.service || true))" || echo "  missing: xorgate-updater-rs.service (install.sh 3b)"
[ -e /etc/sudoers.d/024_xorgate-update-rs ] && echo "  ok: /etc/sudoers.d/024_xorgate-update-rs ($(stat -c '%a %U' /etc/sudoers.d/024_xorgate-update-rs))" || echo "  missing: /etc/sudoers.d/024_xorgate-update-rs (install.sh 3b)"
echo "  visudo -c: $(sudo -n visudo -c 2>&1 | tail -n1 || true)"
grep -q '^Conflicts=xorgate-agent.service' /etc/systemd/system/$UNIT.service 2>/dev/null && echo "  UNSAFE: shipped Conflicts= in the installed unit (re-run install.sh)" || echo "  ok: bench unit (no Conflicts=xorgate-agent.service)"
REMOTE
