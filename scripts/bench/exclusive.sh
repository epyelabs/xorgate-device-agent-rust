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
#   From step 3 until the window is open an EXIT/INT/TERM trap restores the JS agent.
#   Order inside the window (phase file): C4 → C5 → C6 → C7 → C12 bench half →
#   tape ≥ 10 min → `exclusive.sh tape-off` (closes the tape) → C14 → C8 → C7
#   reboot LAST. The reboot ends the window: JS starts itself, /run drop-ins vanish.
#
#   scripts/bench/exclusive.sh tape-off [--dry-run]  # remove 98-tape.conf + restart Rust (closes the tape)
#
# Phase 3 (WP6) additions to begin, BEFORE stopping JS (step 2): each JS
#   gst-launch-1.0 /proc/<pid>/cmdline -> js-gst-<pid>.cmdline (NUL -> newline),
#   each JS xorgate-kvs-webrtc cmdline -> js-kvs-<pid>.cmdline + its environ KEY
#   list (never values) -> js-kvs-<pid>.envkeys, `sha256sum /etc/xorgate/pipelines/*`
#   -> pipelines.sha256, the shm audit -> shm-before.txt. After the JS stop
#   (step 3) it waits <= 60 s (1 s polls) until no gst-launch-1.0 /
#   xorgate-kvs-webrtc of the JS unit's cgroup (or one seen before the stop)
#   is alive — the cameras are released — else aborts through the trap (JS
#   restored). Exclusive profile = media-on (profile.sh exclusive).
#   Window helpers: gate-seam.sh (D12), soak.sh (D15), filler.sh (D9), shm-audit.sh.
#
# end: stops Rust, SIGINT/5 s/SIGKILL any Rust gst-launch-1.0/xorgate-kvs-webrtc
#   left (never a JS pid; cannot fail), starts + verifies the JS agent (so no
#   later failure can leave PROD down), removes the runtime drop-ins, the D12
#   gate seam, the soak sampler and the D9 filler, re-asserts the shared
#   (media-off) profile, restarts Rust, waits for all three units active, waits
#   <= 90 s for the JS capture/masters (state.json latest.media.streams[].state
#   "running", READ ONLY), reruns the shm audit -> shm-after.txt, re-checks the
#   C15 sha256s + the window's pipelines.sha256 (D17) and lists files under
#   /var/lib/xorgate modified while the JS agent was stopped (= touched by
#   someone else). Leaves Rust running on dev. Idempotent: run it first on ANY abort.
# Never touches sim7600-lte, /etc/xorgate/agent.env or /var/lib/xorgate contents.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
VERB="${1:-}"; shift || true
DRY=0
for a in "$@"; do case "$a" in --dry-run) DRY=1 ;; *) echo "unknown arg $a" >&2; exit 2 ;; esac; done
case "$VERB" in begin|end|tape-off|status) ;; *) sed -n '2,46p' "$0"; exit 2 ;; esac
TS=$(date -u +%Y%m%dT%H%M%SZ)
log "exclusive $VERB on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY VERB=$VERB TS=$TS"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
WIN_ROOT=$RS_HOME/evidence
latest_window() { ls -1d "$WIN_ROOT"/window-* 2>/dev/null | tail -n1; }
# Start the PROD JS agent and verify it is active. Never aborts the script
# (set -e safe): returns 1 after ~30 s of retries so the caller can shout.
start_js_verified() {
  if [ "$DRY" = 1 ]; then echo "  [dry-run] would: systemctl start $JS_UNIT (+ verify active)"; return 0; fi
  local i
  for i in 1 2 3; do
    systemctl start "$JS_UNIT" || true
    for _ in $(seq 1 10); do
      [ "$(systemctl is-active "$JS_UNIT" || true)" = active ] && { echo "  = $JS_UNIT active"; return 0; }
      sleep 1
    done
  done
  echo "!!!! $JS_UNIT FAILED TO START — the PROD agent is DOWN; fix by hand: sudo systemctl start $JS_UNIT" >&2
  return 1
}
# Phase 3: camera-release wait (XORGATE_TEST_JS_RELEASE_SECS is test-only).
JS_RELEASE_SECS=${XORGATE_TEST_JS_RELEASE_SECS:-60}
JS_MEDIA_BEFORE=""
# JS media pids still alive: the cgroup scan now + the pids seen before the stop.
js_media_lingering() {
  local p out
  out=$( { js_media_pids; for p in $JS_MEDIA_BEFORE; do
             if pid_alive "$p" && is_media_name "$(proc_name "$p" 2>/dev/null || true)"; then echo "$p"; fi
           done; } | sort -u | tr '\n' ' ')
  echo "${out% }"
}
wait_js_media_released() {
  local t=$1 i left
  for ((i = 0; i <= t; i++)); do
    left=$(js_media_lingering)
    if [ -z "$left" ]; then echo "  = JS capture/masters gone after ${i}s (camera released)"; return 0; fi
    sleep 1
  done
  echo "  still alive after ${t}s: $left"
  return 1
}
# Save each JS media process' argv (and, for masters, the environ KEYS only)
# into $1; $1 = "" prints what would be saved (dry run). Read-only on /proc.
capture_js_media() {
  local dir=$1 p n kind
  if [ -z "$JS_MEDIA_BEFORE" ]; then echo "  JS media processes: none found in $JS_UNIT's cgroup"; return 0; fi
  for p in $JS_MEDIA_BEFORE; do
    n=$(proc_name "$p" 2>/dev/null) || { echo "  pid $p vanished before capture"; continue; }
    case "$n" in gst-launch-1.0) kind=gst ;; xorgate-kvs-webrtc) kind=kvs ;; *) continue ;; esac
    if [ -z "$dir" ]; then
      echo "  [dry-run] would save js-$kind-$p.cmdline ($(tr '\0' '\n' < "/proc/$p/cmdline" 2>/dev/null | wc -l) args)$([ $kind = kvs ] && echo " + js-$kind-$p.envkeys ($(tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -c = || true) keys)")"
      continue
    fi
    tr '\0' '\n' < "/proc/$p/cmdline" > "$dir/js-$kind-$p.cmdline" 2>/dev/null || { echo "  pid $p: cmdline unreadable"; continue; }
    if [ "$kind" = kvs ]; then
      tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | sed -n 's/=.*//p' > "$dir/js-$kind-$p.envkeys" || true
    fi
    echo "  saved js-$kind-$p.cmdline$([ $kind = kvs ] && echo " + js-$kind-$p.envkeys (keys only)")"
  done
}

case "$VERB" in
status)
  units_report
  ls -l "$DROPIN_DIR" "$RUN_DROPIN_DIR" 2>/dev/null | sed 's/^/  /' || true
  echo "  latest window: $(latest_window || echo none)"
  for hw in "$HW_SHARED" "$HW_EXCLUSIVE"; do
    if [ -f "$hw" ]; then echo "  $(basename "$hw"): imu=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])).get("imu")))' "$hw" 2>/dev/null || echo '?')"; fi
  done
  if [ -f "$EXCLUSIVE_ENV" ]; then echo "  $EXCLUSIVE_ENV present (exclusive hardware)"; fi
  if inputs_safe; then echo "  inputs: SAFE (shared profile in force, or a window with the JS agent stopped)"; else echo "  inputs: NOT SAFE for a WP2+ build (see above)"; fi
  c15_check
  printf '  %-22s %s\n' "$GATE_UNIT" "$(systemctl is-active "$GATE_UNIT" 2>/dev/null || true)" "$SOAK_UNIT" "$(systemctl is-active "$SOAK_UNIT" 2>/dev/null || true)"
  [ -f "$GATE_DROPIN" ] && echo "  gate seam drop-in present ($(cat "$GATE_MODE" 2>/dev/null || echo '?'))"
  echo "  filler sessions: $(filler_sessions | wc -l)   Rust media pids: $(rs_media_pids | tr '\n' ' ')"
  echo "  shm: $(shm_counts)"
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
  JS_MEDIA_BEFORE=$(js_media_pids | sort -u | tr '\n' ' ')
  JS_MEDIA_BEFORE=${JS_MEDIA_BEFORE% }
  echo "  JS media pids (cgroup scan): ${JS_MEDIA_BEFORE:-none}"
  js_media_wait_running 0 || true   # the D17 baseline: JS streams before the window
  if [ "$DRY" = 1 ]; then
    echo "  [dry-run] would mkdir $WIN and save js-state-latest.json, units.txt, begin"
    python3 - <<'PY' || true
import json
d = json.load(open("/var/lib/xorgate/state.json"))
print("  JS state.json latest groups:", sorted((d.get("latest") or {}).get("metrics", {}).keys()))
PY
    capture_js_media ""
    echo "  [dry-run] would save pipelines.sha256: $(ls /etc/xorgate/pipelines 2>/dev/null | wc -l) file(s) under /etc/xorgate/pipelines"
    echo "  [dry-run] would save shm-before.txt:"; shm_audit | sed 's/^/  /'
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
    capture_js_media "$WIN"
    echo "$JS_MEDIA_BEFORE" > "$WIN/js-media-pids.txt"
    if ls /etc/xorgate/pipelines/* >/dev/null 2>&1; then
      sha256sum /etc/xorgate/pipelines/* > "$WIN/pipelines.sha256"
      echo "  pipelines.sha256: $(wc -l < "$WIN/pipelines.sha256") file(s)"
    else
      : > "$WIN/pipelines.sha256"; echo "  pipelines.sha256: /etc/xorgate/pipelines empty or absent"
    fi
    js_media_wait_running 0 > "$WIN/js-media-before.txt" || true
    shm_audit > "$WIN/shm-before.txt"
    echo "  shm-before: $(tail -n1 "$WIN/shm-before.txt" | sed 's/^ *//')"
    chown -R pi:pi "$WIN"
  fi
  echo "== 3. stop $JS_UNIT (never disable)"
  # From here until the window is fully open, ANY exit (error, signal) must give
  # the PROD JS agent back: stop Rust (it may already hold the exclusive inputs),
  # drop the runtime drop-ins, start + verify JS.
  begin_abort() {
    local rc=$?
    trap - EXIT INT TERM
    echo "!! exclusive begin aborted (rc=$rc) — restoring the PROD JS agent" >&2
    systemctl stop "$UNIT" 2>/dev/null || true
    rm -f "$EXCLUSIVE_DROPIN" "$TAPE_DROPIN" "$EXCLUSIVE_ENV" 2>/dev/null || true
    systemctl daemon-reload 2>/dev/null || true
    start_js_verified || true
    echo "!! run scripts/bench/exclusive.sh end to re-apply the shared profile and restart Rust" >&2
    exit "$rc"
  }
  [ "$DRY" = 1 ] || trap begin_abort EXIT INT TERM
  act systemctl stop $JS_UNIT
  if [ "$DRY" != 1 ]; then
    for _ in $(seq 1 30); do [ "$(systemctl is-active $JS_UNIT || true)" = active ] || break; sleep 1; done
    [ "$(systemctl is-active $JS_UNIT || true)" != active ] || { echo "JS agent still active — aborting (run exclusive.sh end)"; exit 1; }
    date -u +%Y-%m-%dT%H:%M:%SZ > "$WIN/js-stopped"
  fi
  echo "== 3b. wait for the JS capture/masters to exit (camera release, <= ${JS_RELEASE_SECS}s)"
  if [ "$DRY" = 1 ]; then
    echo "  [dry-run] would wait until none of these is alive: ${JS_MEDIA_BEFORE:-(none now)} (+ any gst-launch-1.0/xorgate-kvs-webrtc left in $JS_UNIT's cgroup)"
  else
    if ! wait_js_media_released "$JS_RELEASE_SECS"; then
      echo "camera not released: JS media processes still alive after ${JS_RELEASE_SECS}s — aborting (the trap restores JS)"; exit 1
    fi
    date -u +%Y-%m-%dT%H:%M:%SZ > "$WIN/js-media-released"
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
  [ "$DRY" = 1 ] || trap - EXIT INT TERM
  echo "== window OPEN at $TS — tape $RS_HOME/tapes/$TS.ndjson.gz. On ANY abort: scripts/bench/exclusive.sh end"
  echo "   CLI steps (C6) must carry the tape path so their \`cli\` events reach the side file"
  echo "   $RS_HOME/tapes/$TS.ndjson.gz.cli.ndjson (WP6 A6), e.g.:"
  echo "   sudo -u pi env XORGATE_HOME=$RS_HOME XORGATE_RS_TAPE=$RS_HOME/tapes/$TS.ndjson.gz xorgate-rs set timeSync.stepThresholdSecs 15"
  echo "   Phase 3 helpers (Mac side): scripts/bench/shm-audit.sh; scripts/bench/filler.sh seed|clear (D9);"
  echo "   scripts/bench/soak.sh --minutes 60 | status | stop (D15); scripts/bench/gate-seam.sh on|release|off (D12,"
  echo "   then: sudo systemctl restart $UNIT). Recording settings change ONLY through the dev cloud."
  ;;
tape-off)
  remove_if_present "$TAPE_DROPIN" || true
  reload_restart_rs
  ls -l $RS_HOME/tapes/ 2>/dev/null | tail -n 3 | sed 's/^/  /' || true
  ;;
end)
  # ORDER MATTERS (review-wp7 fix 6): the PROD JS agent must be running again
  # before ANY step that can fail under set -e. Rust is stopped first because in
  # the exclusive profile it may hold the NMEA tty / I2C bus the JS agent needs.
  echo "== 1. stop $UNIT (frees the exclusive inputs)"
  act systemctl stop $UNIT || true
  # Leftover Rust capture/masters would hold the cameras the JS agent is about
  # to reopen. kill_rs_media cannot fail (always returns 0, bounded 5 s, and
  # runs in an errexit-exempt `||` context), so PROD is still restored next.
  echo "== 1b. Rust gst-launch-1.0/xorgate-kvs-webrtc leftovers (SIGINT, 5 s, SIGKILL; never a JS pid)"
  kill_rs_media || echo "  ! leftover sweep failed — continuing (JS first)"
  echo "== 2. JS agent (start + verify BEFORE anything that can fail)"
  JS_OK=1
  if [ "$(systemctl is-active $JS_UNIT || true)" != active ]; then
    start_js_verified || JS_OK=0
    [ "$JS_OK" = 1 ] && note "$JS_UNIT started"
  else
    echo "  = $JS_UNIT active"
  fi
  echo "== 3. runtime drop-ins + window helpers (gate seam, soak, filler)"
  remove_if_present "$EXCLUSIVE_DROPIN" || true
  remove_if_present "$TAPE_DROPIN" || true
  gate_seam_off || echo "  ! gate seam teardown incomplete (see above)"
  soak_stop || echo "  ! soak stop incomplete (see above)"
  filler_clear || echo "  ! filler clear refused (see above)"
  echo "== 4. shared profile + restart $UNIT"
  profile_shared
  reload_restart_rs
  if [ "$DRY" != 1 ]; then
    for _ in $(seq 1 30); do
      ok=1; for u in $JS_UNIT $UNIT $MODEM_UNIT; do [ "$(systemctl is-active $u || true)" = active ] || ok=0; done
      [ $ok = 1 ] && break; sleep 1
    done
  fi
  [ "$JS_OK" = 1 ] || echo "!!!! $JS_UNIT is NOT active — see above" >&2
  units_report
  WIN=$(latest_window || true)
  echo "== 5. JS capture/masters back (state.json, read only; <= 90 s)"
  BASE=""
  if [ -n "$WIN" ] && [ -f "$WIN/js-media-before.txt" ]; then BASE=$(sed -n 's/.*streams: //p' "$WIN/js-media-before.txt" | head -n1); echo "  pre-window baseline: ${BASE:-?}"; fi
  if [ "$DRY" = 1 ]; then js_media_wait_running 0 "$BASE" || true; else js_media_wait_running 90 "$BASE" || echo "  ! JS streams neither all running nor = the pre-window baseline (states above) — check before calling D17"; fi
  echo "  Rust media pids now (want none): $(rs_media_pids | tr '\n' ' ')"
  echo "== 6. shm audit"
  if [ -n "$WIN" ] && [ "$DRY" != 1 ]; then shm_audit > "$WIN/shm-after.txt"; chown pi:pi "$WIN/shm-after.txt"; echo "  -> $WIN/shm-after.txt"; fi
  shm_audit
  echo "== 7. C15 + pipelines (D17)"
  c15_check
  if [ -n "$WIN" ] && [ -f "$WIN/pipelines.sha256" ]; then
    if [ ! -s "$WIN/pipelines.sha256" ]; then echo "  pipelines: none at begin"
    elif sha256sum --quiet -c "$WIN/pipelines.sha256" >/dev/null 2>&1; then echo "  pipelines: /etc/xorgate/pipelines/* sha256 UNCHANGED vs begin ($(wc -l < "$WIN/pipelines.sha256") files)"
    else echo "  pipelines: CHANGED vs begin:"; sha256sum -c "$WIN/pipelines.sha256" 2>&1 | sed 's/^/      /'; fi
  else
    echo "  pipelines: no pipelines.sha256 in the latest window"
  fi
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
