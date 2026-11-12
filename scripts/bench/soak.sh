#!/usr/bin/env bash
# D15 soak sampler on the bench (plan phase-3-recording-media.md §Bench recipe
# step 4, checks D15/D16). Read-only apart from its own runtime script and TSV.
#
#   scripts/bench/soak.sh [start] [--minutes N] [--interval S] [--dry-run]   # N=60, S=60
#   scripts/bench/soak.sh status      # unit state + the newest TSV's last lines
#   scripts/bench/soak.sh stop [--dry-run]
#
# start: a transient unit `xorgate-rs-soak` (systemd-run, RuntimeMaxSec = N min
#   + 2 min) runs /run/xorgate-rs-soak/sampler.sh, which every S seconds appends
#   one row to /var/lib/xorgate-rs/evidence/soak-<UTC ts>.tsv (pi-owned):
#     ts, uptime_s; the Rust MainPID with VmRSS kB, utime+stime ticks from
#     /proc/<pid>/stat and cpu_pct = the tick DELTA since the previous row /
#     (CLK_TCK x elapsed uptime) (never ps %CPU); every other pid of the Rust
#     unit's cgroup (gst-launch-1.0 or the Phase 7a capture host
#     xorgate-capture, xorgate-kvs-webrtc, python3, the Phase 7c IMU reader
#     xorgate-imu, …) as
#     name:pid:rss_kb:ticks:cpu_pct (comma-separated); the shm audit counts
#     (js/rust/other/dead, see shm-audit.sh); recordings backlog bytes
#     (du -sb /var/lib/xorgate-rs/recordings); zero_closed = *.mp4 of size 0
#     that are not the newest of their session dir; NRestarts and
#     WatchdogTimestamp of xorgate-agent-rs; `vcgencmd get_throttled`.
#   Refuses while a soak is already running.
# stop: stops the unit (exclusive.sh end does this too). Touches nothing else:
# never the agents, their settings, /etc/xorgate or /var/lib/xorgate.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
VERB=start
case "${1:-}" in start|status|stop) VERB=$1; shift ;; esac
DRY=0; MINUTES=60; INTERVAL=60
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --minutes) MINUTES="$2"; shift 2 ;;
    --interval) INTERVAL="$2"; shift 2 ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
case "$MINUTES$INTERVAL" in *[!0-9]*) echo "--minutes/--interval take whole numbers" >&2; exit 2 ;; esac
if [ "$MINUTES" -lt 1 ] || [ "$INTERVAL" -lt 1 ]; then echo "--minutes/--interval must be >= 1" >&2; exit 2; fi
TS=$(date -u +%Y%m%dT%H%M%SZ)
log "soak $VERB on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY VERB=$VERB MINUTES=$MINUTES INTERVAL=$INTERVAL TS=$TS"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
SAMPLER=$SOAK_RUN/sampler.sh
# The sampler: a standalone script carrying the shm-audit helpers of _lib.sh.
sampler_script() {
  echo '#!/bin/bash'
  echo '# xorgate-rs D15 soak sampler (scripts/bench/soak.sh). RUNTIME file.'
  echo '# usage: sampler.sh <tsv> <duration_s> <interval_s>'
  echo 'set -uo pipefail'
  declare -p UNIT JS_UNIT RS_HOME JS_HOME
  declare -f pid_alive proc_name is_media_name pids_in_unit _main_pids_refresh pid_owner shm_audit_rows shm_counts
  cat <<'S'
TSV=$1; DURATION=$2; INTERVAL=$3
HZ=$(getconf CLK_TCK)
declare -A PREV_T=() PREV_U=()
uptime_s() { cut -d' ' -f1 /proc/uptime; }
ticks() { local s; s=$(cat "/proc/$1/stat" 2>/dev/null) || { echo 0; return; }; s=${s##*) }; set -- $s; echo $(( ${12} + ${13} )); }
rss_kb() { awk '/^VmRSS:/{print $2; f=1} END{if(!f) print 0}' "/proc/$1/status" 2>/dev/null || echo 0; }
# Sets CPU (not echoed: a $(…) subshell would lose the PREV_* bookkeeping).
cpu_pct() { # pid ticks uptime
  local p=$1 t=$2 u=$3
  CPU=NA
  if [ -n "${PREV_T[$p]:-}" ]; then CPU=$(awk -v t="$t" -v pt="${PREV_T[$p]}" -v u="$u" -v pu="${PREV_U[$p]}" -v hz="$HZ" 'BEGIN{d=u-pu; if (d>0) printf "%.2f", (t-pt)/hz/d*100; else print "NA"}'); fi
  PREV_T[$p]=$t; PREV_U[$p]=$u
}
zero_closed() {
  local d n=0 f last
  for d in "$RS_HOME"/recordings/video/*/*/; do
    [ -d "$d" ] || continue
    last=$(ls -1 "$d" 2>/dev/null | grep -E '^[0-9]{5,}\.mp4$' | sort | tail -n1 || true)
    for f in "$d"*.mp4; do
      [ -e "$f" ] || continue
      [ "$(basename "$f")" = "$last" ] && continue
      [ -s "$f" ] || n=$((n + 1))
    done
  done
  echo "$n"
}
sample() {
  local u mp rs_rss rs_t rs_c kids="" p n t shm backlog nr wd thr
  u=$(uptime_s)
  mp=$(systemctl show "$UNIT" -p MainPID --value 2>/dev/null || echo 0)
  if [ -n "$mp" ] && [ "$mp" != 0 ] && [ -d "/proc/$mp" ]; then
    rs_rss=$(rss_kb "$mp"); rs_t=$(ticks "$mp"); cpu_pct "$mp" "$rs_t" "$u"; rs_c=$CPU
  else mp=0; rs_rss=0; rs_t=0; rs_c=NA; fi
  for p in $(pids_in_unit "$UNIT" | sort -n); do
    [ "$p" = "$mp" ] && continue
    n=$(proc_name "$p" 2>/dev/null) || continue
    t=$(ticks "$p"); cpu_pct "$p" "$t" "$u"
    kids="${kids:+$kids,}$n:$p:$(rss_kb "$p"):$t:$CPU"
  done
  shm=$(shm_counts | tr ' ' '\t' | sed 's/[a-z]*=//g')
  backlog=$(du -sb "$RS_HOME/recordings" 2>/dev/null | cut -f1 || true)
  nr=$(systemctl show "$UNIT" -p NRestarts --value 2>/dev/null || true)
  wd=$(systemctl show "$UNIT" -p WatchdogTimestamp --value 2>/dev/null || true)
  thr=$(vcgencmd get_throttled 2>/dev/null | sed 's/^throttled=//' || true)
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$u" "$mp" "$rs_rss" "$rs_t" "$rs_c" "${kids:--}" "$shm" \
    "${backlog:-0}" "$(zero_closed)" "${nr:-NA}" "${wd:-NA}" "${thr:-NA}" >> "$TSV"
}
if [ ! -s "$TSV" ]; then
  printf 'ts\tuptime_s\trs_pid\trs_rss_kb\trs_ticks\trs_cpu_pct\tchildren(name:pid:rss_kb:ticks:cpu_pct)\tshm_js\tshm_rust\tshm_other\tshm_dead\tbacklog_bytes\tzero_closed\tnrestarts\twatchdog_ts\tthrottled\n' > "$TSV"
  chown pi:pi "$TSV" 2>/dev/null || true
fi
END=$(( $(cut -d. -f1 /proc/uptime) + DURATION ))
while :; do
  sample
  [ "$(cut -d. -f1 /proc/uptime)" -lt "$END" ] || break
  sleep "$INTERVAL"
done
S
}
latest_tsv() { ls -1t "$RS_HOME"/evidence/soak-*.tsv 2>/dev/null | head -n1 || true; }
case "$VERB" in
status)
  printf '  %-18s %s\n' "$SOAK_UNIT" "$(systemctl is-active "$SOAK_UNIT" 2>/dev/null || true)"
  T=$(latest_tsv)
  if [ -n "$T" ]; then echo "  newest: $T ($(($(wc -l < "$T") - 1)) samples)"; head -n1 "$T" | sed 's/^/    /'; tail -n 5 "$T" | sed 's/^/    /'; else echo "  no soak TSV under $RS_HOME/evidence"; fi
  ;;
stop) soak_stop ;;
start)
  st=$(systemctl is-active "$SOAK_UNIT" 2>/dev/null || true)
  if [ "$st" = active ] || [ "$st" = activating ]; then echo "refusing: $SOAK_UNIT already $st (soak.sh stop first)"; [ "$DRY" = 1 ] || exit 1; fi
  TSV=$RS_HOME/evidence/soak-$TS.tsv
  echo "== sampler $SAMPLER -> $TSV (${MINUTES} min, every ${INTERVAL} s)"
  [ -d "$RS_HOME/evidence" ] || { act install -d -o pi -g pi -m 0755 "$RS_HOME/evidence"; note "$RS_HOME/evidence created"; }
  if [ ! -d "$SOAK_RUN" ]; then act install -d -o root -g root -m 0755 "$SOAK_RUN"; note "$SOAK_RUN created"; fi
  if [ "$DRY" = 1 ]; then
    echo "  [dry-run] would write $SAMPLER ($(sampler_script | wc -l) lines)"
  else
    write_if_changed "$SAMPLER" 0755 < <(sampler_script) || true
  fi
  act systemctl reset-failed "$SOAK_UNIT" 2>/dev/null || true
  act systemd-run --unit="$SOAK_UNIT" --description="xorgate-rs D15 soak sampler (scripts/bench/soak.sh)" \
    -p RuntimeMaxSec=$((MINUTES * 60 + 120)) /bin/bash "$SAMPLER" "$TSV" $((MINUTES * 60)) "$INTERVAL"
  note "$SOAK_UNIT started -> $TSV"
  echo "  follow: scripts/bench/soak.sh status   stop: scripts/bench/soak.sh stop"
  ;;
esac
echo "== done: ${#CHANGED[@]} change(s)$([ "$DRY" = 1 ] && echo ' (dry run: nothing written)')"
REMOTE
} | bench_sudo_script
