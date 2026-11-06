#!/usr/bin/env bash
# Phase 4 E8 — the NTP-unsynced boot for the clock stepper (plan
# phase-4-operations.md §9 "Clock (E8)"). EXCLUSIVE WINDOW ONLY (gate runner):
# the stepper disarms for the whole boot on its FIRST tick if NTP is synced,
# so the clock is broken BEFORE the Rust agent starts.
#
#   scripts/bench/ntp-unsync.sh status                 # NTPSynchronized, date -u vs the Mac, hwclock, fake-hwclock mtime
#   scripts/bench/ntp-unsync.sh break [--secs 600]     # stop rs agent → set-ntp false → clock −secs → start rs agent
#   scripts/bench/ntp-unsync.sh restore                # set-ntp true, wait ≤120 s for NTPSynchronized=yes;
#                                                      #   still unsynced → date -s <the Mac's epoch> + set-ntp true
#   [--dry-run]                                        # print what would run
#
# `break` REFUSES unless an exclusive window is open (the runtime 99-exclusive
# drop-in present AND xorgate-agent — the JS prod agent — inactive): the JS
# agent's own stepper would step the clock back, and a skewed clock under the
# PROD agent breaks its mTLS. --secs is capped at 900 (mTLS tolerates minutes,
# not days). Restore BEFORE `exclusive.sh end` — though `end` and the window
# dead-man now restore too (review-wp6 M4): `break` records what it changed in
# /var/lib/xorgate-rs/ntp-unsync.state ONCE (a re-run never moves the clock
# again) and `ntp_restore` (_lib.sh) turns NTP back on and undoes the step
# unless the clock was stepped since. Never touches sim7600-lte.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
CMD="${1:-}"; shift || true
SECS=600; DRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --secs) SECS="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
case "$CMD" in status|break|restore) ;; *) sed -n '2,19p' "$0"; exit 2 ;; esac
case "$SECS" in ''|*[!0-9]*) echo "--secs must be a positive integer" >&2; exit 2 ;; esac
if [ "$SECS" -lt 1 ] || [ "$SECS" -gt 900 ]; then echo "--secs must be 1..900 (mTLS tolerates minutes, not days)" >&2; exit 2; fi
# The reference clock is the Mac's (NTP-synced): its epoch at send time.
MAC_EPOCH=$(date +%s)
log "ntp-unsync $CMD on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY CMD=$CMD SECS=$SECS MAC_EPOCH=$MAC_EPOCH"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
synced() { timedatectl show -p NTPSynchronized --value 2>/dev/null || echo "?"; }
clock_report() {
  local now; now=$(date +%s)
  echo "  NTPSynchronized=$(synced)  NTP=$(timedatectl show -p NTP --value 2>/dev/null || echo '?')"
  echo "  date -u: $(date -u +%Y-%m-%dT%H:%M:%SZ)  skew vs the Mac at send: $((now - MAC_EPOCH)) s (+ ssh latency)"
  echo "  hwclock -r: $(hwclock -r 2>/dev/null || echo '(no RTC readable)')"
  echo "  /etc/fake-hwclock.data: $(stat -c '%y' /etc/fake-hwclock.data 2>/dev/null || echo absent)"
}
window_open() {
  [ -f "$EXCLUSIVE_DROPIN" ] || { echo "  no exclusive window ($EXCLUSIVE_DROPIN absent) — run scripts/bench/exclusive.sh begin first"; return 1; }
  local js; js=$(systemctl is-active $JS_UNIT || true)
  if [ "$js" = active ] || [ "$js" = activating ]; then echo "  $JS_UNIT is $js — the window must have it stopped"; return 1; fi
  return 0
}
WIN=$(ls -1d "$RS_HOME"/evidence/window-* 2>/dev/null | tail -n1 || true)
record() { [ -n "$WIN" ] && [ "$DRY" != 1 ] && { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*" >> "$WIN/ntp-unsync.log"; chown pi:pi "$WIN/ntp-unsync.log"; } || true; }
case "$CMD" in
status)
  echo "== clock"
  clock_report
  ;;
break)
  echo "== 0. preconditions"
  window_open || { echo "REFUSING: break needs an open exclusive window"; exit 1; }
  # Phase 5 (CF65, review-wp3 S1): break ends with `systemctl start` of the Rust
  # unit — never for a SHIPPED (converted) unit; refuse before touching the clock.
  if rs_unit_converted; then echo "REFUSING: $RS_CONVERTED_REFUSAL"; exit 1; fi
  # Phase 6 Q6 (WP5): break ends with a start; a parked bench drop-in = PROD identity.
  rs_bench_dropin_ok || exit 1
  echo "  ok: exclusive window open, $JS_UNIT stopped"
  # Idempotent (review-wp6 S1/M4): the original offset is recorded ONCE; a
  # re-run never moves the clock again.
  if [ -f "$NTP_STATE" ]; then
    echo "  = already broken: $(tr '\n' ' ' < "$NTP_STATE") — the clock is NOT moved again (restore first)"
    clock_report
    exit 0
  fi
  if [ "$(timedatectl show -p NTP --value 2>/dev/null)" = no ]; then
    echo "REFUSING: NTP is already off and there is no ntp-unsync record ($NTP_STATE) — not ours to break; run 'timedatectl set-ntp true' first"
    exit 1
  fi
  clock_report
  echo "== 1. stop $UNIT (its stepper must first tick on the broken clock)"
  act systemctl stop $UNIT
  echo "== 2. NTP off + clock -$SECS s"
  act timedatectl set-ntp false
  T=$(( $(date +%s) - SECS ))
  act date -u -s "@$T"
  if [ "$DRY" != 1 ]; then
    printf 'secs=%s\nwall=%s\nuptime=%s\n' "$SECS" "$(date +%s)" "$(cut -d' ' -f1 /proc/uptime | cut -d. -f1)" > "$NTP_STATE"
    chmod 0644 "$NTP_STATE"
  fi
  record "break: set-ntp false; date -s @$T (-$SECS s); record $NTP_STATE"
  if [ "$DRY" != 1 ]; then
    sleep 1
    [ "$(synced)" = no ] || echo "  WARNING: NTPSynchronized=$(synced) after date -s (want no) — the stepper would disarm on its first tick"
  fi
  clock_report
  echo "== 3. start $UNIT (expect within one tick after a GPS fix:"
  echo "     [agent] [clock] stepped system clock +$SECS.0s from gps (NTP unsynced; step 1/4 this boot))"
  act systemctl start $UNIT
  echo "  restore with: scripts/bench/ntp-unsync.sh restore   (BEFORE exclusive.sh end)"
  ;;
restore)
  echo "== 1. NTP on + undo the recorded break (ntp_restore)"
  ntp_restore
  record "restore: ntp_restore"
  if [ "$DRY" != 1 ]; then
    for _ in $(seq 1 120); do [ "$(synced)" = yes ] && break; sleep 1; done
    if [ "$(synced)" != yes ]; then
      # Fall back to the Mac's clock (+ the seconds since send), then NTP again.
      T=$(( MAC_EPOCH + SECONDS ))
      echo "  NTPSynchronized still $(synced) after 120 s — date -u -s @$T (the Mac's epoch) + set-ntp true"
      date -u -s "@$T" >/dev/null
      timedatectl set-ntp true
      record "restore fallback: date -s @$T (Mac epoch); set-ntp true"
    fi
  fi
  clock_report
  echo "  next: the stepper's tick logs the stood-down event; report hint clockStepperArmed:false"
  ;;
esac
REMOTE
} | bench_sudo_script
