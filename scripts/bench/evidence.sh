#!/usr/bin/env bash
# Dump a soak/gate evidence snapshot on the bench and copy it to the plan's
# evidence folder on the Mac.
#
#   scripts/bench/evidence.sh <label>        # e.g. phase0-gate, phase0-10min
#
# Writes /var/lib/xorgate-rs/evidence/<label>-<UTC ts>.txt on the bench with:
# both units' status, the last 200 journal lines of xorgate-agent-rs, the
# process' ps numbers, `top -b -n 2 | head -20` (plus the second iteration,
# whose CPU% is the real 2 s sample — the first is the since-boot average),
# the binary size, build-info.json and state.json; then scp's it to
# $EVIDENCE_LOCAL (default plans/device-agent-js-to-rust/phase-0/evidence/).
# Read-only on the bench apart from the evidence file itself.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

LABEL="${1:-}"
[ -n "$LABEL" ] || { echo "usage: $0 <label>" >&2; exit 2; }
case "$LABEL" in *[!A-Za-z0-9._-]*) echo "label must be [A-Za-z0-9._-]" >&2; exit 2 ;; esac

TS=$(date -u +%Y%m%dT%H%M%SZ)
# EVIDENCE_REMOTE_DIR lets the script be exercised before install.sh created
# /var/lib/xorgate-rs/evidence (e.g. EVIDENCE_REMOTE_DIR=/home/pi/tmp-evidence).
REMOTE_FILE="${EVIDENCE_REMOTE_DIR:-$RS_HOME/evidence}/${LABEL}-${TS}.txt"
log "collecting $REMOTE_FILE on $BENCH"
bench_ssh bash -s -- "$REMOTE_FILE" <<'REMOTE'
set -uo pipefail
OUT="$1"
UNIT=xorgate-agent-rs
RS_HOME=/var/lib/xorgate-rs
mkdir -p "$(dirname "$OUT")"
section() { printf '\n==== %s ====\n' "$1"; }
{
  echo "xorgate-agent-rs bench evidence — $(hostname) — $(date -u +%Y-%m-%dT%H:%M:%SZ) (uptime:$(uptime))"
  section "units"
  printf '%-18s %s\n' xorgate-agent "$(systemctl is-active xorgate-agent)" \
                      $UNIT "$(systemctl is-active $UNIT)" \
                      sim7600-lte "$(systemctl is-active sim7600-lte)"
  section "systemctl status xorgate-agent (JS, prod)"
  systemctl status --no-pager -n 5 xorgate-agent 2>&1
  section "systemctl status $UNIT (Rust, dev)"
  systemctl status --no-pager -n 5 $UNIT 2>&1
  section "systemctl show $UNIT (effective)"
  systemctl show $UNIT -p Conflicts -p User -p EnvironmentFiles -p StateDirectory -p WorkingDirectory -p Type -p WatchdogUSec -p NRestarts -p ExecMainStartTimestamp -p MemoryCurrent 2>&1
  section "ps (xorgate-rs)"
  ps -o pid,rss,vsz,pcpu,etime,nlwp,cmd -C xorgate-rs 2>&1 || echo "(no xorgate-rs process)"
  section "ps (node worker, JS agent, for comparison)"
  ps -o pid,rss,vsz,pcpu,etime,nlwp,cmd -C node 2>&1 || echo "(no node process)"
  section "top -b -n 2 | head -20"
  top -b -n 2 -d 2 | head -20
  section "top second iteration (real 2 s sample)"
  top -b -n 2 -d 2 | awk '/^top -/{n++} n==2' | head -25
  section "binary"
  ls -l /opt/xorgate-rs/bin/xorgate-rs 2>&1
  stat -c 'size=%s bytes' /opt/xorgate-rs/bin/xorgate-rs 2>&1
  (ldd /opt/xorgate-rs/bin/xorgate-rs 2>&1 | grep -E 'gstreamer|libc\.so|libssl|not found') || true
  section "build-info.json"
  cat /opt/xorgate-rs/build-info.json 2>&1
  section "$RS_HOME"
  ls -la "$RS_HOME" 2>&1
  section "state.json"
  cat "$RS_HOME/state.json" 2>&1 || echo "(none)"
  section "journal $UNIT (last 200)"
  journalctl -u $UNIT -n 200 --no-pager -o short-iso 2>&1
  section "thermal / throttling"
  cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null | awk '{printf "cpu_temp=%.1fC\n",$1/1000}'
  vcgencmd get_throttled 2>&1 || true
} > "$OUT" 2>&1
echo "wrote $OUT ($(wc -l < "$OUT") lines)"
REMOTE

mkdir -p "$EVIDENCE_LOCAL"
LOCAL_FILE="$EVIDENCE_LOCAL/$(basename "$REMOTE_FILE")"
bench_scp_from "$REMOTE_FILE" "$LOCAL_FILE"
log "copied to $LOCAL_FILE"
sed -n '1,12p' "$LOCAL_FILE"
