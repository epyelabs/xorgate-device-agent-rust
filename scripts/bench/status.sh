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
REMOTE
