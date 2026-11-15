#!/usr/bin/env bash
# p8-deadman.sh: the reboot-persistent dead-man for a Phase 8 bench window
# (plan §7.2.1, review S8.6, PROGRESS F1/F5 + the stamp nit). Root, on the bench.
#
#   p8-deadman.sh arm --window /root/p8-<ts> --cap-min <60|75> [--label w1]
#       deadline = now + (cap - 10) min (F5: the <= 10 min restore fits inside
#       the cap). Installs in /etc/systemd/system:
#         p8-deadman.timer        OnCalendar=<absolute UTC deadline>, Persistent=true
#                                 (fires at once after a reboot past the deadline)
#         p8-deadman.service      oneshot: p8-restore.sh --auto (After= the boot unit)
#         p8-deadman-boot.service enabled oneshot, Before= both agents,
#                                 ConditionPathExists=/root/p8-window.active:
#                                 p8-restore.sh --auto --boot (F1: cancels the
#                                 agents' queued jobs, verifies inside the
#                                 oneshot, starts PROD with --no-block)
#       plus the marker /root/p8-window.active. Refuses when already armed.
#       Refuses a window that already ran (aside.list, aside.restored or
#       restore.done present: review T1). The timer carries BOTH the wall-clock
#       OnCalendar= and a monotonic OnActiveSec= (review T4: this bench has
#       wall-clock steps), whichever comes first.
#   p8-deadman.sh arm … --deadline-in-s <s>   TEST ONLY: a short deadline
#   p8-deadman.sh failsafe --window W --label wN [--in-min 10]
#       p8-restore.sh only (review T2): re-arm a short --auto dead-man on a
#       manual restore that will not start PROD, so a lost ssh session can
#       never leave 58b51737 down. Skips the "window already ran" guard.
#   p8-deadman.sh rearm --window W [--in-min 5]
#       the armed window only: move the timer's deadline to now + N min (keep
#       the marker and the boot unit); the restore's survivor retry uses it
#   p8-deadman.sh disarm      remove both units, the marker and the timer stamp
#                             (p8-restore.sh calls it; never stops the unit it runs in)
#   p8-deadman.sh status      armed?, deadline, unit states (no contents)
# Every unit runs with HOME=/root.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/bench/p8-lib.sh
. "$HERE/p8-lib.sh"

TIMER=$P8_SD/p8-deadman.timer
SVC=$P8_SD/p8-deadman.service
BOOT=$P8_SD/p8-deadman-boot.service

write_units() { # <window> <deadline epoch>
  local w=$1 dl=$2 cal r act
  cal=$(date -u -d "@$dl" '+%Y-%m-%d %H:%M:%S UTC')
  act=$(( dl - $(date -u +%s) )); [ "$act" -ge 1 ] || act=1
  r=${w#"$P8_ROOT"}/bin/p8-restore.sh
  cat > "$TIMER.tmp" <<EOF
# Phase 8 bench window dead-man (p8-deadman.sh; removed by p8-restore.sh).
[Unit]
Description=Phase 8 bench window dead-man (restore 58b51737 at the deadline)

[Timer]
OnCalendar=$cal
OnActiveSec=${act}s
Persistent=true
AccuracySec=1s
Unit=p8-deadman.service

[Install]
WantedBy=timers.target
EOF
  cat > "$SVC.tmp" <<EOF
# Phase 8 bench window dead-man restore (p8-deadman.sh; removed by p8-restore.sh).
[Unit]
Description=Phase 8 bench window dead-man restore
# A Persistent= firing at boot waits for the boot restore (F1): never two at once.
After=p8-deadman-boot.service

[Service]
Type=oneshot
Environment=HOME=/root
Environment=P8_SELF_UNIT=p8-deadman.service
ExecStart=/bin/bash $r --auto --window ${w#"$P8_ROOT"}
TimeoutStartSec=20min
EOF
  cat > "$BOOT.tmp" <<EOF
# Phase 8 bench window boot restore (p8-deadman.sh; removed by p8-restore.sh).
# Any boot during a window restores BEFORE either agent starts (§7.2.1, F1).
[Unit]
Description=Phase 8 bench window boot restore
ConditionPathExists=${P8_MARKER#"$P8_ROOT"}
After=local-fs.target
Before=xorgate-agent.service xorgate-agent-rs.service xorgate-updater.service xorgate-updater-rs.service

[Service]
Type=oneshot
Environment=HOME=/root
Environment=P8_SELF_UNIT=p8-deadman-boot.service
ExecStart=/bin/bash $r --auto --boot --window ${w#"$P8_ROOT"}
TimeoutStartSec=20min

[Install]
WantedBy=multi-user.target
EOF
  mv -f "$TIMER.tmp" "$TIMER"; mv -f "$SVC.tmp" "$SVC"; mv -f "$BOOT.tmp" "$BOOT"
  chmod 0644 "$TIMER" "$SVC" "$BOOT"
}

arm() {
  local w="" cap="" label=w in_s="" now dl f
  while [ $# -gt 0 ]; do
    case $1 in
      --window) w=$2; shift 2 ;;
      --cap-min) cap=$2; shift 2 ;;
      --label) label=$2; shift 2 ;;
      --deadline-in-s) in_s=$2; shift 2 ;;
      --in-min) in_s=$(( $2 * 60 )); shift 2 ;;
      *) P8_DIE_RC=2 p8_die "arm: unknown argument $1" ;;
    esac
  done
  p8_need_root
  p8_check_window "$w"
  [[ $label =~ ^[a-z0-9-]+$ ]] || P8_DIE_RC=2 p8_die "bad --label $label"
  { [ -x "$w/bin/p8-restore.sh" ] && [ -f "$w/bin/p8-lib.sh" ]; } || p8_die "$w/bin has no p8-restore.sh/p8-lib.sh (run p8-backup.sh stage)"
  [ ! -e "$P8_MARKER" ] || P8_DIE_RC=3 p8_die "already armed ($(p8_marker_get window)): disarm or restore first"
  if [ "${FAILSAFE:-0}" != 1 ]; then
    for f in aside.list aside.restored restore.done; do
      [ ! -e "$w/$f" ] || P8_DIE_RC=3 p8_die "window ${w#"$P8_ROOT"} already ran ($f exists): use a NEW window dir (p8-backup.sh stage)"
    done
  fi
  now=$(date -u +%s)
  if [ -n "$in_s" ]; then
    { [[ $in_s =~ ^[0-9]+$ ]] && [ "$in_s" -ge 5 ]; } || P8_DIE_RC=2 p8_die "--deadline-in-s must be >= 5"
    dl=$((now + in_s))
  else
    { [[ $cap =~ ^[0-9]+$ ]] && [ "$cap" -ge 15 ] && [ "$cap" -le 120 ]; } || P8_DIE_RC=2 p8_die "--cap-min must be 15..120 (the window's hard cap)"
    dl=$((now + (cap - 10) * 60))    # F5: cap - 10 min
  fi
  write_units "$w" "$dl"
  printf 'window=%s\nlabel=%s\ndeadline=%s\narmed=%s\n' "${w#"$P8_ROOT"}" "$label" "$dl" "$now" > "$P8_MARKER.tmp"
  chmod 0600 "$P8_MARKER.tmp"; mv -f "$P8_MARKER.tmp" "$P8_MARKER"
  systemctl daemon-reload
  systemctl enable p8-deadman-boot.service >/dev/null 2>&1 || p8_die "enable p8-deadman-boot.service failed"
  systemctl enable --now p8-deadman.timer >/dev/null 2>&1 || p8_die "enable --now p8-deadman.timer failed"
  [ "$(p8_enabled p8-deadman-boot.service)" = enabled ] || p8_die "p8-deadman-boot.service is not enabled"
  [ "$(p8_active p8-deadman.timer)" = active ] || p8_die "p8-deadman.timer is not active"
  echo "armed: window=${w#"$P8_ROOT"} label=$label deadline=$(date -u -d "@$dl" '+%Y-%m-%dT%H:%M:%SZ') (in $(( (dl - now) / 60 )) min $(( (dl - now) % 60 )) s)"
}

disarm() {
  p8_need_root
  # Never stop p8-deadman*.service: the restore may be running INSIDE one.
  systemctl stop p8-deadman.timer >/dev/null 2>&1 || true
  systemctl disable p8-deadman.timer p8-deadman-boot.service >/dev/null 2>&1 || true
  rm -f "$TIMER" "$SVC" "$BOOT" "$P8_SD/timers.target.wants/p8-deadman.timer" \
        "$P8_SD/multi-user.target.wants/p8-deadman-boot.service" "$P8_STAMP" "$P8_MARKER"
  systemctl daemon-reload >/dev/null 2>&1 || true
  # a dead-man restore that ended FAILED stays loaded as a failed unit: clear it
  # (a no-op on the one that may be running this very disarm)
  systemctl reset-failed p8-deadman.timer p8-deadman.service p8-deadman-boot.service >/dev/null 2>&1 || true
  echo "disarmed: units, marker and timer stamp removed"
}

status() {
  if [ -f "$P8_MARKER" ]; then
    echo "armed: window=$(p8_marker_get window) label=$(p8_marker_get label) deadline=$(date -u -d "@$(p8_marker_get deadline)" '+%Y-%m-%dT%H:%M:%SZ')"
  else echo "not armed (no marker)"; fi
  echo "p8-deadman.timer: active=$(p8_active p8-deadman.timer) enabled=$(p8_enabled p8-deadman.timer)"
  echo "p8-deadman-boot.service: enabled=$(p8_enabled p8-deadman-boot.service)"
  for f in "$TIMER" "$SVC" "$BOOT" "$P8_STAMP"; do [ -e "$f" ] && echo "present: ${f#"$P8_ROOT"}"; done
  return 0
}

rearm() {
  local w="" m=5 dl
  while [ $# -gt 0 ]; do case $1 in --window) w=$2; shift 2 ;; --in-min) m=$2; shift 2 ;; *) P8_DIE_RC=2 p8_die "rearm: unknown argument $1" ;; esac; done
  p8_need_root; p8_check_window "$w"
  [ "$(p8_marker_get window)" = "${w#"$P8_ROOT"}" ] || P8_DIE_RC=3 p8_die "rearm: $w is not the armed window"
  { [[ $m =~ ^[0-9]+$ ]] && [ "$m" -ge 1 ]; } || P8_DIE_RC=2 p8_die "rearm: --in-min must be >= 1"
  dl=$(( $(date -u +%s) + m * 60 ))
  write_units "$w" "$dl"
  sed -i "s/^deadline=.*/deadline=$dl/" "$P8_MARKER"
  systemctl daemon-reload
  systemctl restart p8-deadman.timer >/dev/null 2>&1 || p8_die "rearm: restart p8-deadman.timer failed"
  echo "rearmed: window=${w#"$P8_ROOT"} deadline=$(date -u -d "@$dl" '+%Y-%m-%dT%H:%M:%SZ')"
}

case "${1:-}" in
  arm) shift; arm "$@" ;;
  rearm) shift; rearm "$@" ;;
  failsafe) shift; FAILSAFE=1 arm "$@" ;;
  disarm) disarm ;;
  status) status ;;
  *) sed -n '2,37p' "$0"; exit 2 ;;
esac
