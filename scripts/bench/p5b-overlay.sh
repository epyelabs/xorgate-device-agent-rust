#!/usr/bin/env bash
# Phase 5b (WP4) — the H13-H15 bench overlay, SHARED profile, no exclusive
# window (plan phase-5b-js-catchup.md §12). GATE RUNNER ONLY.
#
#   scripts/bench/p5b-overlay.sh wheels              # (Mac) build the ELM327-emulator
#                                                    #   wheelhouse in Docker → tools/replay/.cache/p5b-wheels
#   scripts/bench/p5b-overlay.sh apply [--dry-run]   # see below
#   scripts/bench/p5b-overlay.sh revert [--dry-run]  # undo everything apply did
#   scripts/bench/p5b-overlay.sh status
#
# apply: the DEV manifest copy ($HW_SHARED) gains peripherals.obd (the original
# is kept in /run/xorgate-rs-p5b/); /run/xorgate-rs-obd-python/ = symlinks to
# the installed helper (/opt/xorgate-rs/python/obd_read.py, obd_scan.py, obd/;
# NO bno085_read.py); an ELM327-emulator pty (taos-2022, venv in /tmp from the
# copied wheelhouse — nothing system-wide) and WP3's FakeModem pty
# (tools/sms-fake-modem, the p4-sms inbox, --allow-clear), both run as `pi`;
# the runtime drop-in /run/systemd/system/xorgate-agent-rs.service.d/97-p5b.conf
# (XORGATE_PYTHON_DIR, XORGATE_OBD_SERIAL, XORGATE_SMS_PORT); daemon-reload;
# restart ONLY xorgate-agent-rs.
#
# It writes ONLY under /run, /tmp and $HW_SHARED, restarts ONLY xorgate-agent-rs
# (never the JS prod agent, never sim7600-lte, never a reboot), and REFUSES
# unless the JS unit is active (the shared profile), no exclusive window is
# open and the Rust unit is the bench flavour. `profile.sh shared` verify FAILS
# while the drop-in or the manifest key exists without the overlay's marker
# (CF76, _lib.sh p5b_inputs_safe). /run makes a reboot a full revert.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
CMD="${1:-}"; shift || true
DRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
case "$CMD" in wheels|apply|revert|status) ;; *) sed -n '2,26p' "$0"; exit 2 ;; esac
WHEELS="${P5B_WHEELS:-$REPO/tools/replay/.cache/p5b-wheels}"
PY="${P5B_PYTHON_SRC:-$REPO/python}"
FAKE_MODEM="${P5B_FAKE_MODEM:-$REPO/tools/sms-fake-modem/sms_fake_modem.py}"
INBOX="$REPO/fixtures/replay/p4-sms-modem.json"

if [ "$CMD" = wheels ]; then
  mkdir -p "$WHEELS"
  docker run --rm --label "xorgate.agent=${XORGATE_AGENT_LABEL:-p5b-rig}" --platform linux/arm64 \
    -v "$WHEELS:/w" debian:bookworm sh -c \
    'apt-get update -qq >/dev/null && apt-get install -y -qq python3-pip >/dev/null && pip3 download --break-system-packages -q -d /w "ELM327-emulator==4.0.0" && ls /w'
  exit 0
fi

# Files the bench needs in /tmp (apply only): the emulator launcher + the
# harness and scenario it imports, the FakeModem + its inbox, the wheelhouse.
if [ "$CMD" = apply ] && [ "${P5B_NO_COPY:-0}" != 1 ]; then
  for f in "$PY/tests/emulator_harness.py" "$PY/obd/emulator/taos_2022.py" "$FAKE_MODEM" "$INBOX" "$REPO/tools/replay/obd-emulator.py"; do
    [ -f "$f" ] || { echo "missing $f (WP2/WP3 merged? P5B_PYTHON_SRC / P5B_FAKE_MODEM)" >&2; exit 1; }
  done
  ls "$WHEELS"/*.whl >/dev/null 2>&1 || ls "$WHEELS"/*.tar.gz >/dev/null 2>&1 || { echo "no wheelhouse in $WHEELS (run: $0 wheels)" >&2; exit 1; }
  STAGE=$(mktemp -d)
  mkdir -p "$STAGE/pyemu/tests" "$STAGE/pyemu/obd/emulator" "$STAGE/wheels"
  cp "$PY/tests/emulator_harness.py" "$STAGE/pyemu/tests/"
  cp "$PY/obd/emulator/taos_2022.py" "$STAGE/pyemu/obd/emulator/"
  cp "$REPO/tools/replay/obd-emulator.py" "$FAKE_MODEM" "$INBOX" "$STAGE/"
  cp "$WHEELS"/* "$STAGE/wheels/"
  log "p5b-overlay: copying the emulator + FakeModem files to $BENCH:/tmp/xorgate-p5b"
  if [ "$DRY" = 1 ]; then echo "  [dry-run] would scp $STAGE/* → $BENCH:/tmp/xorgate-p5b/"
  else
    tar -C "$STAGE" -cf - . | bench_ssh "rm -rf /tmp/xorgate-p5b && mkdir -p /tmp/xorgate-p5b && tar -C /tmp/xorgate-p5b -xf -"
  fi
  rm -rf "$STAGE"
fi

log "p5b-overlay $CMD on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY CMD=$CMD"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
EMU_PID=$P5B_RUN/emulator.pid
MODEM_PID=$P5B_RUN/fake-modem.pid
OBD_PTY_FILE=$P5B_RUN/obd-pty
SMS_LINK=$P5B_RUN/sms-pty
MANIFEST_ORIG=$P5B_RUN/hardware.json.orig
as_pi() { runuser -u pi -- "$@"; }
stop_pid() { # <pidfile>
  local p; p=$(cat "$1" 2>/dev/null || true)
  [ -n "$p" ] || return 0
  if kill -0 "$p" 2>/dev/null; then
    act kill -TERM "$p" || true
    for _ in $(seq 1 50); do kill -0 "$p" 2>/dev/null || break; sleep 0.1; done
    kill -0 "$p" 2>/dev/null && act kill -KILL "$p" || true
  fi
}
js_state() { systemctl is-active $JS_UNIT 2>/dev/null || true; }
preconditions() {
  local js; js=$(js_state)
  [ "$js" = active ] || { echo "REFUSING: $JS_UNIT is ${js:-unknown} — the overlay runs in the SHARED profile only (JS prod agent active)"; return 1; }
  [ ! -f "$EXCLUSIVE_DROPIN" ] || { echo "REFUSING: an exclusive window is open ($EXCLUSIVE_DROPIN)"; return 1; }
  if rs_unit_converted; then echo "REFUSING: $RS_CONVERTED_REFUSAL"; return 1; fi
  [ -f "$HW_SHARED" ] || { echo "REFUSING: no $HW_SHARED (profile.sh shared first)"; return 1; }
  [ -f /opt/xorgate-rs/python/obd_read.py ] || { echo "REFUSING: /opt/xorgate-rs/python/obd_read.py missing (install a Phase 5b Rust tarball)"; return 1; }
  return 0
}
status() {
  echo "  marker:   $([ -e "$P5B_MARK" ] && echo applied || echo absent)"
  echo "  drop-in:  $([ -f "$P5B_DROPIN" ] && { echo "present:"; sed 's/^/            /' "$P5B_DROPIN"; } || echo absent)"
  echo "  manifest: peripherals.obd $(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print("PRESENT" if "obd" in (d.get("peripherals") or {}) else "absent")' "$HW_SHARED" 2>/dev/null || echo '?') in $HW_SHARED"
  echo "  obd py:   $([ -d "$P5B_OBD_PY" ] && ls "$P5B_OBD_PY" | tr '\n' ' ' || echo absent)"
  for pf in "$EMU_PID" "$MODEM_PID"; do
    local p; p=$(cat "$pf" 2>/dev/null || true)
    echo "  $(basename "$pf" .pid): $([ -n "$p" ] && kill -0 "$p" 2>/dev/null && echo "running pid $p" || echo stopped)"
  done
  echo "  $JS_UNIT: $(js_state)   $UNIT: $(systemctl is-active $UNIT 2>/dev/null || true)"
}
case "$CMD" in
status)
  echo "== p5b overlay"
  status
  ;;
apply)
  echo "== 0. preconditions"
  preconditions || exit 1
  if [ -e "$P5B_MARK" ]; then echo "  = already applied (revert first)"; status; exit 0; fi
  echo "  ok: $JS_UNIT active, no exclusive window, bench-flavour $UNIT, helper installed"
  echo "== 1. DEV manifest: + peripherals.obd ($HW_SHARED; original kept in $P5B_RUN)"
  act install -d -o pi -g pi -m 0755 "$P5B_RUN"
  [ -f "$MANIFEST_ORIG" ] || act cp -p "$HW_SHARED" "$MANIFEST_ORIG"
  if [ "$DRY" != 1 ]; then
    tmp=$(mktemp)
    python3 - "$HW_SHARED" > "$tmp" <<'PY'
import collections, json, sys
d = json.load(open(sys.argv[1]), object_pairs_hook=collections.OrderedDict)
per = d.get("peripherals") if isinstance(d.get("peripherals"), dict) else collections.OrderedDict()
per["obd"] = collections.OrderedDict([("adapter", "elm327"), ("transport", "bt-spp")])
d["peripherals"] = per
print(json.dumps(d, indent=2))
PY
    install -o pi -g pi -m 0600 "$tmp" "$HW_SHARED"; rm -f "$tmp"
  fi
  echo "== 2. obd-only python dir $P5B_OBD_PY (no bno085_read.py: the IMU stays the JS agent's)"
  act install -d -o pi -g pi -m 0755 "$P5B_OBD_PY"
  for n in obd_read.py obd_scan.py obd; do act ln -sfn "/opt/xorgate-rs/python/$n" "$P5B_OBD_PY/$n"; done
  echo "== 3. ELM327-emulator venv in $P5B_TMP (as pi, from the copied wheelhouse)"
  act chown -R pi:pi "$P5B_TMP"
  if [ "${XORGATE_TEST_P5B_SKIP_VENV:-0}" != 1 ]; then
    act as_pi python3 -m venv "$P5B_TMP/venv"
    act as_pi "$P5B_TMP/venv/bin/pip" install -q --no-index --find-links "$P5B_TMP/wheels" "ELM327-emulator==4.0.0"
  fi
  echo "== 4. emulator + FakeModem ptys (as pi)"
  if [ "$DRY" != 1 ]; then
    rm -f "$OBD_PTY_FILE"
    EMU_CMD="${XORGATE_TEST_P5B_EMULATOR_CMD:-env OBD_ELM_BIN=$P5B_TMP/venv/bin/elm TMPDIR=$P5B_TMP PYTHONDONTWRITEBYTECODE=1 python3 $P5B_TMP/obd-emulator.py $P5B_TMP/pyemu taos-2022 $OBD_PTY_FILE}"
    # The pid is written by the pi-owned shell itself (not runuser's $!).
    as_pi setsid sh -c "echo \$\$ > $EMU_PID; exec $EMU_CMD" > "$P5B_TMP/emulator.log" 2>&1 < /dev/null &
    MODEM_CMD="${XORGATE_TEST_P5B_MODEM_CMD:-python3 $P5B_TMP/sms_fake_modem.py --inbox $P5B_TMP/p4-sms-modem.json --link $SMS_LINK --log $P5B_TMP/sms-fake.log --allow-clear}"
    as_pi setsid sh -c "echo \$\$ > $MODEM_PID; exec $MODEM_CMD" > "$P5B_TMP/fake-modem.log" 2>&1 < /dev/null &
    for _ in $(seq 1 150); do [ -s "$OBD_PTY_FILE" ] && [ -L "$SMS_LINK" ] && break; sleep 0.2; done
    OBD_PTY=$(cat "$OBD_PTY_FILE" 2>/dev/null || true)
    case "$OBD_PTY" in /dev/*) echo "  emulator on $OBD_PTY" ;; *) echo "FAILED: the emulator printed no pty (see $P5B_TMP/emulator.log) — reverting"; APPLY_FAILED=1 ;; esac
    [ -L "$SMS_LINK" ] && echo "  FakeModem on $(readlink "$SMS_LINK")" || { echo "FAILED: no FakeModem link $SMS_LINK — reverting"; APPLY_FAILED=1; }
  else
    OBD_PTY=/dev/pts/N
  fi
  if [ "${APPLY_FAILED:-0}" = 0 ]; then
    echo "== 5. runtime drop-in $P5B_DROPIN"
    write_if_changed "$P5B_DROPIN" 0644 <<EOF || true
# xorgate-rs Phase 5b overlay (scripts/bench/p5b-overlay.sh apply; /run = a
# reboot reverts it). H13: OBD helper on an ELM327-emulator pty; H15: SMS on
# WP3's FakeModem pty (the real interface-05 port is never opened).
[Service]
Environment=XORGATE_PYTHON_DIR=$P5B_OBD_PY
Environment=XORGATE_OBD_SERIAL=$OBD_PTY
Environment=XORGATE_SMS_PORT=$SMS_LINK
EOF
    act systemctl daemon-reload
    echo "== 6. restart $UNIT only"
    act systemctl restart $UNIT
    [ "$DRY" = 1 ] || date -u +%Y-%m-%dT%H:%M:%SZ > "$P5B_MARK"
    status
    exit 0
  fi
  ;&
revert)
  echo "== revert: emulator + FakeModem, drop-in, manifest, /run + /tmp"
  stop_pid "$EMU_PID"; stop_pid "$MODEM_PID"
  remove_if_present "$P5B_DROPIN" || true
  if [ -f "$MANIFEST_ORIG" ]; then
    act install -o pi -g pi -m 0600 "$MANIFEST_ORIG" "$HW_SHARED"; note "$HW_SHARED restored"
  elif [ -f "$HW_SHARED" ] && [ "$DRY" != 1 ]; then
    tmp=$(mktemp)
    python3 - "$HW_SHARED" > "$tmp" <<'PY'
import collections, json, sys
d = json.load(open(sys.argv[1]), object_pairs_hook=collections.OrderedDict)
per = d.get("peripherals")
if isinstance(per, dict):
    per.pop("obd", None)
    if not per:
        del d["peripherals"]
print(json.dumps(d, indent=2))
PY
    if ! cmp -s "$tmp" "$HW_SHARED"; then install -o pi -g pi -m 0600 "$tmp" "$HW_SHARED"; note "$HW_SHARED: peripherals.obd stripped"; fi
    rm -f "$tmp"
  fi
  act rm -rf "$P5B_OBD_PY" "$P5B_RUN" "$P5B_TMP"
  act systemctl daemon-reload
  echo "== restart $UNIT only"
  act systemctl restart $UNIT
  for i in 1 2 3; do echo "  $JS_UNIT is-active ($i/3): $(js_state)"; [ "$DRY" = 1 ] || sleep 1; done
  echo "  next: scripts/bench/profile.sh shared   (verify must PASS)"
  status
  [ "${APPLY_FAILED:-0}" = 0 ] || exit 1
  ;;
esac
REMOTE
} | bench_sudo_script
