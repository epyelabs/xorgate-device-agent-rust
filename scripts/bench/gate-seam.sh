#!/usr/bin/env bash
# D12 boot-defer / GPS-LTE gate seam for the Rust agent (plan
# phase-3-recording-media.md §Bench recipe step 4, check D12). RUNTIME only.
#
#   scripts/bench/gate-seam.sh on      [--dry-run] [--restart]
#   scripts/bench/gate-seam.sh release [--dry-run]
#   scripts/bench/gate-seam.sh off     [--dry-run] [--restart]
#   scripts/bench/gate-seam.sh status
#
#   on      runtime drop-in /run/systemd/system/xorgate-agent-rs.service.d/97-gate-seam.conf
#           (Environment=LTE_TELEMETRY_FILE=/run/xorgate-rs-lte/telemetry.json) + a
#           transient unit `xorgate-rs-gate-seam` (systemd-run) that every 8 s copies
#           the modem daemon's /run/sim7600-lte/telemetry.json (READ ONLY) to
#           /run/xorgate-rs-lte/telemetry.json with ONLY `status` rewritten to
#           "connecting" (atomic tmp + rename, 0644: readable by pi; the daemon's own
#           `timestamp` is kept, so the policy's staleness arm still sees a live daemon).
#           The Rust agent's LTE collector reads the same file, so its `lte.status`
#           reads "connecting" while the seam is on (every other field = the daemon's).
#   release status "connected" from now on (one immediate rewrite) — with the GPS
#           fix that releases the gate as `gps-fix`; without a fix the gate releases
#           as `no-progress` after gpsGateNoProgressSecs regardless.
#   off     stops the copier, removes the drop-in, /run/xorgate-rs-lte and the
#           seam's runtime files.
#   on/off daemon-reload but do NOT restart xorgate-agent-rs unless --restart is
#   given (the gate runner restarts as part of D12); the follow-up is printed.
# Never touches sim7600-lte or its file (read only), the JS agent, /etc/xorgate
# or /var/lib/xorgate. Idempotent; prints what it changed.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
VERB="${1:-}"; shift || true
DRY=0; RESTART=0
for a in "$@"; do
  case "$a" in --dry-run) DRY=1 ;; --restart) RESTART=1 ;; *) echo "unknown arg $a" >&2; exit 2 ;; esac
done
case "$VERB" in on|release|off|status) ;; *) sed -n '2,26p' "$0"; exit 2 ;; esac
log "gate-seam $VERB on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY VERB=$VERB RESTART=$RESTART"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
seam_rewrite_script() {
  cat <<SCRIPT
#!/bin/bash
# xorgate-rs D12 gate seam copier (scripts/bench/gate-seam.sh). RUNTIME file.
# Reads $GATE_SRC (the modem daemon's, READ ONLY), writes $GATE_FILE with
# status = the word in $GATE_MODE. --once: one rewrite, then exit.
SRC=$GATE_SRC
DST=$GATE_FILE
MODEF=$GATE_MODE
SCRIPT
  cat <<'SCRIPT'
once() {
  local status
  status=$(cat "$MODEF" 2>/dev/null || echo connecting)
  python3 - "$SRC" "$DST" "$status" <<'PY'
import collections, json, os, sys, tempfile
src, dst, status = sys.argv[1:4]
try:
    with open(src) as f:
        d = json.load(f, object_pairs_hook=collections.OrderedDict)
except Exception as e:
    print(f"gate-seam: cannot read {src}: {e} (left {dst} as is)", file=sys.stderr)
    sys.exit(0)
if not isinstance(d, dict):
    print(f"gate-seam: {src} is not an object (left {dst} as is)", file=sys.stderr)
    sys.exit(0)
d["status"] = status
fd, tmp = tempfile.mkstemp(prefix=".telemetry.", dir=os.path.dirname(dst))
with os.fdopen(fd, "w") as f:
    json.dump(d, f)
    f.write("\n")
os.chmod(tmp, 0o644)
os.replace(tmp, dst)
PY
}
if [ "${1:-}" = --once ]; then once; exit 0; fi
while :; do once; sleep 8; done
SCRIPT
}
seam_show() {
  printf '  %-22s %s\n' "$GATE_UNIT" "$(systemctl is-active "$GATE_UNIT" 2>/dev/null || true)"
  [ -f "$GATE_DROPIN" ] && echo "  drop-in: $(grep '^Environment=' "$GATE_DROPIN")" || echo "  drop-in: absent"
  echo "  mode: $(cat "$GATE_MODE" 2>/dev/null || echo '(none)')"
  for f in "$GATE_SRC" "$GATE_FILE"; do
    if [ -f "$f" ]; then
      echo "  $f: $(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print("status=%s timestamp=%s reg=%s" % (d.get("status"), d.get("timestamp"), d.get("reg")))' "$f" 2>/dev/null || echo unreadable) (mtime $(stat -c %y "$f" | cut -d. -f1))"
    else echo "  $f: absent"; fi
  done
  echo "  agent process LTE_TELEMETRY_FILE: $(mp=$(systemctl show "$UNIT" -p MainPID --value); [ -n "$mp" ] && [ "$mp" != 0 ] && tr '\0' '\n' < "/proc/$mp/environ" 2>/dev/null | sed -n 's/^LTE_TELEMETRY_FILE=//p' | grep . || echo '(unset = the daemon file)')"
}
followup() {
  if [ "$RESTART" = 1 ]; then
    # Phase 5 (CF65, review-wp3 S1): never restart a SHIPPED (converted) unit.
    if rs_unit_converted; then echo "  REFUSING the restart: $RS_CONVERTED_REFUSAL"; return 1; fi
    act systemctl restart "$UNIT"; note "$UNIT restarted"
  else
    echo "  follow-up (not done here): sudo systemctl restart $UNIT   # the agent reads LTE_TELEMETRY_FILE at start"
  fi
}
case "$VERB" in
status) seam_show ;;
on)
  [ -f /etc/systemd/system/$UNIT.service ] || { echo "$UNIT not installed — run scripts/bench/install.sh first"; exit 1; }
  [ -f "$GATE_SRC" ] || echo "  NOTE: $GATE_SRC absent (modem daemon down?) — the copier writes nothing until it appears; the gate then releases as 'stale' after 60 s"
  echo "== 1. seam files"
  if [ ! -d "$GATE_DIR" ]; then act install -d -o root -g root -m 0755 "$GATE_DIR"; note "$GATE_DIR created (0755)"; fi
  if [ -f "$GATE_MODE" ] && [ "$(cat "$GATE_MODE")" = connected ]; then
    echo "  NOTE: the seam was already released (mode connected) — keeping it; run off then on to re-arm"
  else
    write_if_changed "$GATE_MODE" 0644 <<<"connecting" || true
  fi
  write_if_changed "$GATE_SCRIPT" 0755 < <(seam_rewrite_script) || true
  if [ "$DRY" = 1 ]; then echo "  [dry-run] would: bash $GATE_SCRIPT --once"
  else bash "$GATE_SCRIPT" --once; [ -f "$GATE_FILE" ] && echo "  $GATE_FILE refreshed (status $(cat "$GATE_MODE"))"; fi
  echo "== 2. copier unit $GATE_UNIT"
  st=$(systemctl is-active "$GATE_UNIT" 2>/dev/null || true)
  if [ "$st" = active ] || [ "$st" = activating ]; then echo "  = $GATE_UNIT already $st"
  else
    act systemctl reset-failed "$GATE_UNIT" 2>/dev/null || true
    act systemd-run --unit="$GATE_UNIT" --description="xorgate-rs D12 gate seam copier (scripts/bench/gate-seam.sh)" /bin/bash "$GATE_SCRIPT"
    note "$GATE_UNIT started (every 8 s: $GATE_SRC -> $GATE_FILE)"
  fi
  echo "== 3. drop-in"
  write_if_changed "$GATE_DROPIN" 0644 <<CONF || true
# Phase 3 D12 gate seam (scripts/bench/gate-seam.sh on). RUNTIME drop-in.
[Service]
Environment=LTE_TELEMETRY_FILE=$GATE_FILE
CONF
  act systemctl daemon-reload
  followup
  [ "$DRY" = 1 ] || seam_show
  ;;
release)
  [ -f "$GATE_DROPIN" ] || { if [ "$DRY" = 1 ]; then echo "  [dry-run] NOTE: the seam is not on ($GATE_DROPIN absent) — a real run refuses"; else echo "refusing: the seam is not on — run gate-seam.sh on first"; exit 1; fi; }
  write_if_changed "$GATE_MODE" 0644 <<<"connected" || true
  if [ "$DRY" = 1 ]; then echo "  [dry-run] would: bash $GATE_SCRIPT --once"
  elif [ -x "$GATE_SCRIPT" ]; then bash "$GATE_SCRIPT" --once; echo "  $GATE_FILE refreshed (status connected)"; fi
  [ "$DRY" = 1 ] || seam_show
  ;;
off)
  gate_seam_off
  act systemctl daemon-reload
  followup
  ;;
esac
echo "== done: ${#CHANGED[@]} change(s)$([ "$DRY" = 1 ] && echo ' (dry run: nothing written)')"
REMOTE
} | bench_sudo_script
