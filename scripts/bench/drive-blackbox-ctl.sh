#!/usr/bin/env bash
# The drive blackbox on the bench (USER 2026-11-07: it replaces the retired
# xorgate-blackbox). Run from the Mac like every scripts/bench/ script:
#
#   scripts/bench/drive-blackbox-ctl.sh install [--expect prod|dev|<device-id prefix>] [--dry-run]
#   scripts/bench/drive-blackbox-ctl.sh status
#   scripts/bench/drive-blackbox-ctl.sh uninstall [--dry-run]
#   scripts/bench/drive-blackbox-ctl.sh summary <file|dir> ...   # a pulled copy, on the Mac
#   scripts/bench/drive-blackbox-ctl.sh summary latest|all|<bench path>   # on the bench
#
# install: REFUSES (exit 1, nothing changed) unless xorgate-agent-rs is active
#   and its DEVICE_ID (read from /proc/<MainPID>/environ, else its state.json
#   deviceId) starts with the --expect identity: prod = 58b51737 (default: the
#   drive runs on the converted PROD bench), dev = a73f7a36 (the parked DEV
#   identity), or any id prefix. It prints the identity it found. Then it copies scripts/bench/drive-blackbox.py to /usr/local/bin/xorgate-drive-blackbox
#   and scripts/bench/systemd/xorgate-drive-blackbox.service to /etc/systemd/system/,
#   creates /var/lib/xorgate-rs/drive-blackbox (root 0755), stops + disables the
#   OLD xorgate-blackbox.service (its unit file and /usr/local/bin/xorgate-blackbox.sh
#   are KEPT), daemon-reload, enable --now (restart when the script or unit
#   changed), then waits up to BB_WAIT_S (12) s for a fresh line. Idempotent.
# status: both units, the newest file (lines, size), the data dir size, the
#   last line and its AGE; exits 1 when there is no line or the last line is
#   older than 30 s (the recorder writes every 5 s).
# uninstall: disable --now + remove the new unit and script. The data stays,
#   and the old xorgate-blackbox.service is NOT re-enabled.
# summary: drive-blackbox.py's privacy-safe digest (min/max rail, throttled
#   events, restarts, gaps > 15 s, mqtt / gps-fix windows, peak RSS/CPU).
#   A path that exists on the Mac is summarised locally; `latest` (newest
#   file), `all` (the whole data dir) or a bench path runs on the bench.
#
# Never touches the agents, their settings, /etc/xorgate, sim7600-lte or the
# modem: the recorder itself is read-only toward them (see drive-blackbox.py).
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PY_SRC=$HERE/drive-blackbox.py
UNIT_SRC=$HERE/systemd/xorgate-drive-blackbox.service
usage() { sed -n '2,24p' "$0"; exit "${1:-0}"; }
VERB=${1:-}
[ -n "$VERB" ] || usage 2
shift
DRY=0; EXPECT=prod
case "$VERB" in
  -h|--help) usage 0 ;;
  install|uninstall|status)
    while [ $# -gt 0 ]; do
      case "$1" in
        --dry-run) DRY=1; shift ;;
        --expect) [ "$VERB" = install ] && [ -n "${2:-}" ] || { echo "--expect <prod|dev|id prefix> is an install option" >&2; exit 2; }
                  EXPECT=$2; shift 2 ;;
        *) echo "unknown arg $1" >&2; exit 2 ;;
      esac
    done ;;
  summary)
    [ $# -ge 1 ] || { echo "summary needs a file, a dir, 'latest' or 'all'" >&2; exit 2; }
    case "$1" in
      latest|all|/var/lib/xorgate-rs/*) ;;
      *) if [ -e "$1" ]; then exec python3 -I "$PY_SRC" summary "$@"; fi ;;
    esac
    [ $# -eq 1 ] || { echo "a bench summary takes one path" >&2; exit 2; } ;;
  *) echo "unknown verb $VERB" >&2; usage 2 ;;
esac
b64() { base64 < "$1" | tr -d '\n'; }

log "drive-blackbox $VERB on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{
  printf 'DRY=%q\nVERB=%q\nBB_WAIT_S=%q\n' "$DRY" "$VERB" "${BB_WAIT_S:-12}"
  # every verb's body gets every variable (empty when unused: one shape to lint)
  if [ "$VERB" = install ]; then printf 'BB_PY_B64=%q\nBB_UNIT_B64=%q\n' "$(b64 "$PY_SRC")" "$(b64 "$UNIT_SRC")"
  else printf 'BB_PY_B64=\nBB_UNIT_B64=\n'; fi
  printf 'BB_ARG=%q\n' "${1:-}"
  case "$EXPECT" in prod) EXPECT=58b51737 ;; dev) EXPECT=a73f7a36 ;; esac
  printf 'BB_EXPECT=%q\n' "$EXPECT"
  bench_remote_lib
  cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
BB_BIN=/usr/local/bin/xorgate-drive-blackbox
BB_UNIT=xorgate-drive-blackbox.service
BB_UNIT_FILE=/etc/systemd/system/$BB_UNIT
BB_DATA=/var/lib/xorgate-rs/drive-blackbox
OLD_UNIT=xorgate-blackbox.service
STATUS_RC=0
newest() { ls -1t "$BB_DATA"/*.ndjson 2>/dev/null | head -n1 || true; }
unit_state() { printf '%s / %s' "$(systemctl is-active "$1" 2>/dev/null || true)" "$(systemctl is-enabled "$1" 2>/dev/null || true)"; }
case "$VERB" in
install)
  command -v python3 >/dev/null || { echo "python3 missing on the bench"; exit 1; }
  echo "== the agent this blackbox will watch (expect DEVICE_ID $BB_EXPECT*)"
  rs_state=$(systemctl is-active xorgate-agent-rs 2>/dev/null || true)
  rs_pid=$(systemctl show xorgate-agent-rs -p MainPID --value 2>/dev/null || true)
  id_env=""; home=""
  if [ -n "$rs_pid" ] && [ "$rs_pid" != 0 ] && [ -r "/proc/$rs_pid/environ" ]; then
    id_env=$(tr '\0' '\n' < "/proc/$rs_pid/environ" | sed -n 's/^DEVICE_ID=//p' | head -n1)
    home=$(tr '\0' '\n' < "/proc/$rs_pid/environ" | sed -n 's/^XORGATE_HOME=//p' | head -n1)
  fi
  : "${home:=/var/lib/xorgate}"
  id_state=$(python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1])).get("deviceId") or "")
except Exception: print("")' "$home/state.json" 2>/dev/null || true)
  id=${id_env:-$id_state}
  echo "  xorgate-agent-rs: ${rs_state:-unknown}, MainPID ${rs_pid:-?}, XORGATE_HOME $home"
  echo "  identity: DEVICE_ID=${id_env:-<not in environ>}  state.json deviceId=${id_state:-<none>}"
  if [ "$rs_state" != active ]; then echo "REFUSING: xorgate-agent-rs is ${rs_state:-unknown}, not active (nothing changed)"; exit 1; fi
  case "$id" in
    "$BB_EXPECT"*) echo "  ok: identity ${id} matches the expected $BB_EXPECT*" ;;
    *) echo "REFUSING: the Rust agent runs as '${id:-unknown}', expected $BB_EXPECT* (--expect prod|dev|<prefix>; nothing changed)"; exit 1 ;;
  esac
  CHANGED_FILES=0
  echo "== recorder + unit"
  if printf '%s' "$BB_PY_B64" | base64 -d | write_if_changed "$BB_BIN" 0755; then CHANGED_FILES=1; fi
  if printf '%s' "$BB_UNIT_B64" | base64 -d | write_if_changed "$BB_UNIT_FILE" 0644; then CHANGED_FILES=1; fi
  if [ -d "$BB_DATA" ]; then echo "  = $BB_DATA exists"; else act install -d -o root -g root -m 0755 "$BB_DATA"; note "$BB_DATA created"; fi
  echo "== retire the old $OLD_UNIT (files kept)"
  if systemctl cat "$OLD_UNIT" >/dev/null 2>&1; then
    a=$(systemctl is-active "$OLD_UNIT" 2>/dev/null || true); e=$(systemctl is-enabled "$OLD_UNIT" 2>/dev/null || true)
    echo "  was: $a / $e"
    if [ "$a" = active ] || [ "$a" = activating ] || [ "$e" = enabled ]; then
      act systemctl disable --now "$OLD_UNIT"
      note "$OLD_UNIT stopped + disabled (its unit file and script are kept)"
    else echo "  = already stopped + disabled"; fi
  else echo "  = $OLD_UNIT not installed"; fi
  echo "== enable"
  act systemctl daemon-reload
  T0=$(date +%s)
  st=$(systemctl is-active "$BB_UNIT" 2>/dev/null || true)
  if [ "$(systemctl is-enabled "$BB_UNIT" 2>/dev/null || true)" != enabled ] || [ "$st" != active ]; then
    act systemctl enable --now "$BB_UNIT"; note "$BB_UNIT enabled + started"
  elif [ "$CHANGED_FILES" = 1 ]; then
    act systemctl restart "$BB_UNIT"; note "$BB_UNIT restarted (script/unit changed)"
  else echo "  = $BB_UNIT already enabled + active"; fi
  if [ "$DRY" != 1 ]; then
    f=""; i=0
    while [ "$i" -lt "$BB_WAIT_S" ]; do
      f=$(newest); if [ -n "$f" ] && [ "$(stat -c %Y "$f")" -ge "$T0" ]; then break; fi
      f=""; sleep 1; i=$((i + 1))
    done
    if [ -n "$f" ]; then echo "  ok: recording to $f"; tail -n1 "$f" | cut -c1-400 | sed 's/^/    /'
    else echo "  WARN: no fresh line within ${BB_WAIT_S} s (check: drive-blackbox-ctl.sh status; journalctl -u $BB_UNIT)"; fi
  fi
  ;;
status)
  printf '  %-34s %s\n' "$BB_UNIT" "$(unit_state "$BB_UNIT")"
  if systemctl cat "$OLD_UNIT" >/dev/null 2>&1; then printf '  %-34s %s  (retired; should be inactive / disabled)\n' "$OLD_UNIT" "$(unit_state "$OLD_UNIT")"; fi
  echo "  $(systemctl show "$BB_UNIT" -p MainPID -p NRestarts -p ActiveEnterTimestamp 2>/dev/null | tr '\n' ' ')"
  f=$(newest)
  if [ -n "$f" ]; then
    echo "  newest: $f ($(wc -l < "$f") lines, $(stat -c %s "$f") bytes, modified $(date -u -d "@$(stat -c %Y "$f")" +%Y-%m-%dT%H:%M:%SZ))"
    echo "  data dir: $(du -sh "$BB_DATA" 2>/dev/null | cut -f1) in $(ls -1 "$BB_DATA"/*.ndjson 2>/dev/null | wc -l) file(s)"
    tail -n1 "$f" | cut -c1-400 | sed 's/^/    /'
    age=$(tail -n1 "$f" | python3 -c 'import json,sys,time
try: print(int(time.time() - json.load(sys.stdin)["ts"] / 1000.0))
except Exception: print("")' 2>/dev/null || true)
    if [ -z "$age" ]; then age=$(( $(date +%s) - $(stat -c %Y "$f") )); echo "  last line unparseable (torn?): file modified ${age} s ago"; fi
    if [ "$age" -le 30 ]; then echo "  ok: last line ${age} s old"
    else echo "  STALE: last line ${age} s old (> 30 s; the recorder writes every 5 s)"; STATUS_RC=1; fi
  else echo "  STALE: no files under $BB_DATA"; STATUS_RC=1; fi
  ;;
uninstall)
  if [ -e "$BB_UNIT_FILE" ]; then
    act systemctl disable --now "$BB_UNIT"; note "$BB_UNIT stopped + disabled"
  else echo "  = $BB_UNIT_FILE absent"; fi
  remove_if_present "$BB_UNIT_FILE" || true
  remove_if_present "$BB_BIN" || true
  act systemctl daemon-reload
  echo "  data kept: $BB_DATA; the old $OLD_UNIT is NOT re-enabled"
  ;;
summary)
  [ -x "$BB_BIN" ] || { echo "$BB_BIN missing (drive-blackbox-ctl.sh install first)"; exit 1; }
  case "$BB_ARG" in
    latest) p=$(newest); [ -n "$p" ] || { echo "no files under $BB_DATA"; exit 1; } ;;
    all) p=$BB_DATA ;;
    *) p=$BB_ARG ;;
  esac
  python3 -I "$BB_BIN" summary "$p"
  ;;
esac
[ "$VERB" = summary ] || [ "$VERB" = status ] || echo "== done: ${#CHANGED[@]} change(s)$([ "$DRY" = 1 ] && echo ' (dry run: nothing written)')"
exit "$STATUS_RC"
REMOTE
} | bench_sudo_script
