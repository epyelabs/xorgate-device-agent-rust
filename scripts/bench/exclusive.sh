#!/usr/bin/env bash
# The Phase 2 EXCLUSIVE WINDOW on the bench (plan phase-2-cloud-plane.md
# §Bench recipe step 3/4). The gate runner alone runs this for real.
#
#   scripts/bench/exclusive.sh begin [--dry-run] [--deadman-min N]   # N = 180
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
#   DEAD-MAN (review-wp6 B1): before step 3 begin arms a transient systemd timer
#   `xorgate-rs-window-deadman` (systemd-run --on-active = N min, default 180 =
#   the longest window, D15 soak included, + margin). If `end` never runs it
#   fires and restores the PROD JS agent on its own (stop Rust + window helpers,
#   drop the runtime drop-ins + exclusive env, daemon-reload, start + verify
#   xorgate-agent; idempotent). `end` (and the begin trap) cancel it; re-running
#   begin re-arms it. A soak longer than N needs a larger --deadman-min.
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
# Phase 5 (WP3; a CONVERSION window, phase-5-distribution.md §12, I6):
#   - end, the begin trap and the dead-man ENABLE the JS unit when
#     `is-enabled` != enabled (install-rs.sh disables it) before start + verify
#     (CF64: enable_start_js_verified in _lib.sh);
#   - begin, tape-off, end and profile.sh never (re)start a SHIPPED Rust unit
#     (Conflicts=xorgate-agent.service, as install-rs.sh installs it): they print
#     "shipped (converted) unit installed — run scripts/bench/install.sh first;
#     NOT starting xorgate-agent-rs (it would stop the PROD JS agent)" (CF65);
#   - the dead-man runs `/opt/xorgate-rs/install-rs.sh --revert` first when
#     /var/lib/xorgate-rs-install/record says state=converted|converting;
#     on such a record (or an unreadable one, state=unknown, or the shipped
#     unit) end still restores JS (steps 1-2), then disables xorgate-agent-rs,
#     leaves the dead-man ARMED, skips the Rust restart and exits 1; begin
#     refuses; status warns (the revert is the gate runner's G11 step).
#     Checklist: scripts/bench/convert-window.md.
# Never touches sim7600-lte, /etc/xorgate/agent.env or /var/lib/xorgate contents.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
VERB="${1:-}"; shift || true
DRY=0; DEADMAN_MIN=180
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --deadman-min) DEADMAN_MIN="${2:-}"; shift 2 || { echo "--deadman-min needs N" >&2; exit 2; } ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
case "$DEADMAN_MIN" in ''|*[!0-9]*) echo "--deadman-min: minutes, got '$DEADMAN_MIN'" >&2; exit 2 ;; esac
[ "$DEADMAN_MIN" -ge 10 ] || { echo "--deadman-min must be >= 10" >&2; exit 2; }
case "$VERB" in begin|end|tape-off|status) ;; *) sed -n '2,67p' "$0"; exit 2 ;; esac
TS=$(date -u +%Y%m%dT%H%M%SZ)
log "exclusive $VERB on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY VERB=$VERB TS=$TS DEADMAN_SECS=$((DEADMAN_MIN * 60))"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
WIN_ROOT=$RS_HOME/evidence
latest_window() { ls -1d "$WIN_ROOT"/window-* 2>/dev/null | tail -n1; }
# start_js_verified / enable_start_js_verified live in _lib.sh (Phase 5: the
# dead-man embeds the same bodies).
# Phase 5: a conversion (install-rs.sh) left unreverted — shout, never act
# (the revert is the gate runner's step; the dead-man runs it unattended).
conversion_warn() {
  local st
  st=$(conversion_state)
  case "$st" in
    converted|converting|unknown)
      echo "!!!! conversion record $RS_INSTALL_RECORD says state=$st — run: sudo $RS_INSTALLER --revert; then scripts/bench/install.sh <tarball>; then exclusive.sh end again" >&2 ;;
  esac
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
  echo "  dead-man $DEADMAN_UNIT.timer: $(deadman_state)   next: $(systemctl list-timers --all --no-pager "$DEADMAN_UNIT.timer" 2>/dev/null | sed -n 2p | tr -s ' ' || true)"
  echo "  conversion record: state=$(conversion_state)   $JS_UNIT is-enabled=$(systemctl is-enabled $JS_UNIT 2>/dev/null || true)"
  if rs_unit_converted; then echo "  UNSAFE: $RS_CONVERTED_REFUSAL"; fi
  conversion_warn
  for hw in "$HW_SHARED" "$HW_EXCLUSIVE"; do
    if [ -f "$hw" ]; then echo "  $(basename "$hw"): imu=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])).get("imu")))' "$hw" 2>/dev/null || echo '?')"; fi
  done
  if [ -f "$EXCLUSIVE_ENV" ]; then echo "  $EXCLUSIVE_ENV present (exclusive hardware)"; fi
  if inputs_safe; then echo "  inputs: SAFE (shared profile in force, or a window with the JS agent stopped)"; else echo "  inputs: NOT SAFE for a WP2+ build (see above)"; fi
  c15_check
  printf '  %-22s %s\n' "$GATE_UNIT" "$(systemctl is-active "$GATE_UNIT" 2>/dev/null || true)" "$SOAK_UNIT" "$(systemctl is-active "$SOAK_UNIT" 2>/dev/null || true)"
  [ -f "$GATE_DROPIN" ] && echo "  gate seam drop-in present ($(cat "$GATE_MODE" 2>/dev/null || echo '?'))"
  echo "  filler sessions: $(filler_sessions | wc -l)   Rust media pids: $(rs_media_pids | tr '\n' ' ')"
  imu_readers_report
  echo "  shm: $(shm_counts)"
  ;;
begin)
  echo "== 1. preflight"
  need() { if [ "$DRY" = 1 ]; then echo "  [dry-run] PREFLIGHT would refuse: $1"; else echo "refusing: $1"; exit 1; fi; }
  [ -f /etc/systemd/system/$UNIT.service ] || need "$UNIT not installed"
  # Phase 5 (CF65): the window opens on the bench flavour only; a SHIPPED
  # (converted) unit would stop the PROD JS agent on the step-5 restart.
  if rs_unit_converted; then need "$RS_CONVERTED_REFUSAL"; fi
  # Phase 6 Q6 (WP5): nor with the bench drop-in parked (the step-5 restart
  # would bring the unit up on the PROD identity) — refuse BEFORE the JS stop.
  [ -f "$BENCH_DEV_DROPIN" ] || need "$RS_DROPIN_REFUSAL"
  # review-wp3 N1: nor on an unreverted (or unreadable) conversion record.
  case "$(conversion_state)" in
    converted|converting|unknown) need "conversion record $RS_INSTALL_RECORD says state=$(conversion_state) — run: sudo $RS_INSTALLER --revert first" ;;
  esac
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
  echo "== 2b. dead-man timer (restores $JS_UNIT in $((DEADMAN_SECS / 60)) min unless end runs)"
  deadman_arm "$DEADMAN_SECS" "$WIN"
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
    enable_start_js_verified && deadman_cancel || true
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
  echo "== 1c. NTP (review-wp6 M4): set-ntp true + undo an ntp-unsync.sh break"
  ntp_restore || echo "  ! ntp restore failed — run scripts/bench/ntp-unsync.sh restore"
  echo "== 2. JS agent (enable + start + verify BEFORE anything that can fail)"
  # Phase 5 (CF64, I6): a conversion DISABLES the JS unit; end gives it back
  # enabled as well as active (enable only when is-enabled != enabled).
  JS_OK=1
  JS_WAS=$(systemctl is-active $JS_UNIT || true)
  JS_EN=$(systemctl is-enabled $JS_UNIT 2>/dev/null || true)
  if [ "$JS_WAS" != active ] || [ "$JS_EN" != enabled ]; then
    enable_start_js_verified || JS_OK=0
    [ "$JS_OK" = 1 ] && [ "$JS_WAS" != active ] && note "$JS_UNIT started"
  else
    echo "  = $JS_UNIT active + enabled"
  fi
  # review-wp3 M1: a conversion still in place (record converted|converting|
  # unknown, or the shipped unit) — JS is back (above), now close the reboot
  # race (both units enabled, one Conflicts= the other): disable the Rust unit,
  # keep the dead-man ARMED (its --revert is the unattended backstop), do not
  # restart Rust, and exit non-zero at the end.
  CONV=0
  if conversion_in_place; then
    CONV=1
    conversion_warn
    if [ "$(systemctl is-enabled $UNIT 2>/dev/null || true)" = enabled ]; then
      if act systemctl disable $UNIT; then
        note "$UNIT disabled (conversion in place: no boot race with $JS_UNIT)"
      else
        echo "!!!! systemctl disable $UNIT FAILED (still $(systemctl is-enabled $UNIT 2>/dev/null || true)) — both units enabled: a reboot races them" >&2
      fi
    else
      echo "  = $UNIT not enabled"
    fi
  fi
  echo "== 2b. dead-man timer"
  if [ "$CONV" = 1 ]; then echo "  ! conversion in place: dead-man left ARMED ($(deadman_state)) — it runs $RS_INSTALLER --revert when it fires"
  elif [ "$JS_OK" = 1 ]; then deadman_cancel || true
  else echo "  ! $JS_UNIT not active: dead-man left ARMED ($(deadman_state)) — it retries the restore when it fires"; fi
  echo "== 3. runtime drop-ins + window helpers (gate seam, soak, filler)"
  remove_if_present "$EXCLUSIVE_DROPIN" || true
  remove_if_present "$TAPE_DROPIN" || true
  gate_seam_off || echo "  ! gate seam teardown incomplete (see above)"
  soak_stop || echo "  ! soak stop incomplete (see above)"
  filler_clear || echo "  ! filler clear refused (see above)"
  echo "== 4. shared profile + restart $UNIT"
  profile_shared
  # JS is already back (step 2); a refused Rust restart (bench flavour lost
  # after an `xorgate-rs update`, Phase 4) must not skip the audits below.
  WAIT_UNITS="$JS_UNIT $UNIT $MODEM_UNIT"
  if [ "$CONV" = 1 ]; then
    if rs_unit_converted; then echo "  $RS_CONVERTED_REFUSAL"; fi
    echo "  ! conversion in place: $UNIT not restarted (see above) — revert, install.sh, then end again"
    WAIT_UNITS="$JS_UNIT $MODEM_UNIT"
  else
    reload_restart_rs || echo "  ! $UNIT not restarted (see above) — $JS_UNIT stays up; fix with install.sh then profile.sh shared"
  fi
  if [ "$DRY" != 1 ]; then
    for _ in $(seq 1 30); do
      ok=1; for u in $WAIT_UNITS; do [ "$(systemctl is-active $u || true)" = active ] || ok=0; done
      [ $ok = 1 ] && break; sleep 1
    done
  fi
  [ "$JS_OK" = 1 ] || echo "!!!! $JS_UNIT is NOT active — see above" >&2
  conversion_warn
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
  if [ "$CONV" = 1 ]; then
    conversion_warn
    echo "!!!! end INCOMPLETE: a conversion is in place — $JS_UNIT restored, $UNIT disabled, dead-man still ARMED; revert, install.sh, then run end again" >&2
    exit 1
  fi
  ;;
esac
REMOTE
} | bench_sudo_script
