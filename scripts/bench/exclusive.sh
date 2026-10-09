#!/usr/bin/env bash
# The Phase 2 EXCLUSIVE WINDOW on the bench (plan phase-2-cloud-plane.md
# §Bench recipe step 3/4). The gate runner alone runs this for real.
#
#   scripts/bench/exclusive.sh begin [--dry-run]
#   scripts/bench/exclusive.sh end   [--dry-run]      # idempotent; run after the reboot or on ANY abort
#   scripts/bench/exclusive.sh status
#
# begin:
#   1. preflight: Rust unit installed, C15 baseline present, all units reported;
#   2. captures the JS agent's state.json `latest` + `systemctl is-active` ×3 into
#      /var/lib/xorgate-rs/evidence/window-<ts>/ (persistent: survives the reboot);
#   3. `systemctl stop xorgate-agent` (NEVER disable) and waits for inactive;
#   4. exclusive input profile (runtime 99-exclusive.conf) + runtime tape drop-in
#      98-tape.conf (XORGATE_RS_TAPE=/var/lib/xorgate-rs/tapes/<ts>.ndjson.gz);
#   5. daemon-reload + restart xorgate-agent-rs.
#   Order inside the window (phase file): C4 → C5 → C6 → C7 → C12 bench half →
#   tape ≥ 10 min → `exclusive.sh tape-off` (closes the tape) → C14 → C8 → C7
#   reboot LAST. The reboot ends the window: JS starts itself, /run drop-ins vanish.
#
#   scripts/bench/exclusive.sh tape-off [--dry-run]  # remove 98-tape.conf + restart Rust (closes the tape)
#
# end: removes both runtime drop-ins, re-asserts the shared profile, restarts
#   Rust only if anything changed or it is not active, starts the JS agent if
#   inactive, waits for all three units active, re-checks the C15 sha256s and
#   lists files under /var/lib/xorgate modified while the JS agent was stopped
#   (= touched by someone else). Leaves Rust running on dev.
# Never touches sim7600-lte, /etc/xorgate/agent.env or /var/lib/xorgate contents.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
VERB="${1:-}"; shift || true
DRY=0
for a in "$@"; do case "$a" in --dry-run) DRY=1 ;; *) echo "unknown arg $a" >&2; exit 2 ;; esac; done
case "$VERB" in begin|end|tape-off|status) ;; *) sed -n '2,28p' "$0"; exit 2 ;; esac
TS=$(date -u +%Y%m%dT%H%M%SZ)
log "exclusive $VERB on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY VERB=$VERB TS=$TS"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
WIN_ROOT=$RS_HOME/evidence
latest_window() { ls -1d "$WIN_ROOT"/window-* 2>/dev/null | tail -n1; }

case "$VERB" in
status)
  units_report
  ls -l "$DROPIN_DIR" "$RUN_DROPIN_DIR" 2>/dev/null | sed 's/^/  /' || true
  echo "  latest window: $(latest_window || echo none)"
  for hw in "$HW_SHARED" "$HW_EXCLUSIVE"; do
    if [ -f "$hw" ]; then echo "  $(basename "$hw"): imu=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])).get("imu")))' "$hw" 2>/dev/null || echo '?')"; fi
  done
  if [ -f "$EXCLUSIVE_ENV" ]; then echo "  $EXCLUSIVE_ENV present (exclusive hardware)"; fi
  c15_check
  ;;
begin)
  echo "== 1. preflight"
  need() { if [ "$DRY" = 1 ]; then echo "  [dry-run] PREFLIGHT would refuse: $1"; else echo "refusing: $1"; exit 1; fi; }
  [ -f /etc/systemd/system/$UNIT.service ] || need "$UNIT not installed"
  [ -f "$C15_BASELINE" ] || need "no C15 baseline — run scripts/bench/dev-manifests.sh first"
  grep -q "^KVS_MANIFEST=$RS_ETC/" "$RS_ENV" || need "$RS_ENV does not use the dev manifests — run dev-manifests.sh"
  units_report
  WIN=$WIN_ROOT/window-$TS
  echo "== 2. capture JS pre-window state -> $WIN"
  if [ "$DRY" = 1 ]; then
    echo "  [dry-run] would mkdir $WIN and save js-state-latest.json, units.txt, begin"
    python3 - <<'PY' || true
import json
d = json.load(open("/var/lib/xorgate/state.json"))
print("  JS state.json latest groups:", sorted((d.get("latest") or {}).get("metrics", {}).keys()))
PY
  else
    install -d -o pi -g pi -m 0755 "$WIN"
    python3 - "$WIN/js-state-latest.json" <<'PY'
import json, sys
d = json.load(open("/var/lib/xorgate/state.json"))
json.dump(d.get("latest"), open(sys.argv[1], "w"), indent=2)
print("  JS latest groups:", sorted((d.get("latest") or {}).get("metrics", {}).keys()))
PY
    for i in 1 2 3; do units_report; sleep 1; done > "$WIN/units-before.txt"
    date -u +%Y-%m-%dT%H:%M:%SZ > "$WIN/begin"
    chown -R pi:pi "$WIN"
  fi
  echo "== 3. stop $JS_UNIT (never disable)"
  act systemctl stop $JS_UNIT
  if [ "$DRY" != 1 ]; then
    for _ in $(seq 1 30); do [ "$(systemctl is-active $JS_UNIT || true)" = active ] || break; sleep 1; done
    [ "$(systemctl is-active $JS_UNIT || true)" != active ] || { echo "JS agent still active — aborting (run exclusive.sh end)"; exit 1; }
    date -u +%Y-%m-%dT%H:%M:%SZ > "$WIN/js-stopped"
  fi
  echo "== 4. exclusive profile + tape drop-in"
  profile_exclusive
  [ -d $RS_HOME/tapes ] || { act install -d -o pi -g pi -m 0700 $RS_HOME/tapes; note "$RS_HOME/tapes created"; }
  write_if_changed "$TAPE_DROPIN" 0644 <<CONF || true
# Phase 2 window tape (scripts/bench/exclusive.sh begin $TS). RUNTIME drop-in.
[Service]
Environment=XORGATE_RS_TAPE=$RS_HOME/tapes/$TS.ndjson.gz
CONF
  echo "== 5. reload + restart $UNIT"
  reload_restart_rs
  units_report
  echo "== window OPEN at $TS — tape $RS_HOME/tapes/$TS.ndjson.gz. On ANY abort: scripts/bench/exclusive.sh end"
  echo "   CLI steps (C6) must carry the tape path so their \`cli\` events reach the side file"
  echo "   $RS_HOME/tapes/$TS.ndjson.gz.cli.ndjson (WP6 A6), e.g.:"
  echo "   sudo -u pi env XORGATE_HOME=$RS_HOME XORGATE_RS_TAPE=$RS_HOME/tapes/$TS.ndjson.gz xorgate-rs set timeSync.stepThresholdSecs 15"
  ;;
tape-off)
  remove_if_present "$TAPE_DROPIN" || true
  reload_restart_rs
  ls -l $RS_HOME/tapes/ 2>/dev/null | tail -n 3 | sed 's/^/  /' || true
  ;;
end)
  echo "== 1. runtime drop-ins"
  remove_if_present "$EXCLUSIVE_DROPIN" || true
  remove_if_present "$TAPE_DROPIN" || true
  echo "== 2. shared profile"
  profile_shared
  if [ ${#CHANGED[@]} -gt 0 ] || [ "$(systemctl is-active $UNIT || true)" != active ]; then
    echo "== 3. reload + restart $UNIT"; reload_restart_rs
  else
    echo "== 3. $UNIT unchanged and active — no restart"
  fi
  echo "== 4. JS agent"
  if [ "$(systemctl is-active $JS_UNIT || true)" != active ]; then act systemctl start $JS_UNIT; note "$JS_UNIT started"; else echo "  = $JS_UNIT active"; fi
  if [ "$DRY" != 1 ]; then
    for _ in $(seq 1 30); do
      ok=1; for u in $JS_UNIT $UNIT $MODEM_UNIT; do [ "$(systemctl is-active $u || true)" = active ] || ok=0; done
      [ $ok = 1 ] && break; sleep 1
    done
  fi
  units_report
  echo "== 5. C15"
  c15_check
  WIN=$(latest_window || true)
  if [ -n "$WIN" ] && [ -f "$WIN/js-stopped" ]; then
    FROM=$(cat "$WIN/js-stopped")
    TO=$(systemctl show $JS_UNIT -p ActiveEnterTimestamp --value)
    echo "  /var/lib/xorgate files modified between JS stop ($FROM) and JS start ($TO):"
    if [ -n "$TO" ]; then
      M=$(find /var/lib/xorgate -newermt "$FROM" ! -newermt "$TO" -type f 2>/dev/null | head -50)
      [ -z "$M" ] && echo "    none (C15 OK)" || echo "$M" | sed 's/^/    MODIFIED: /'
    fi
    [ "$DRY" = 1 ] || { date -u +%Y-%m-%dT%H:%M:%SZ > "$WIN/end"; units_report > "$WIN/units-after.txt"; chown -R pi:pi "$WIN"; }
  else
    echo "  (no window record with js-stopped — nothing to attribute)"
  fi
  echo "== done: ${#CHANGED[@]} change(s)$([ "$DRY" = 1 ] && echo ' (dry run: nothing written)')"
  ;;
esac
REMOTE
} | bench_sudo_script
