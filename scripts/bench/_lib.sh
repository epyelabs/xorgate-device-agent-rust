#!/usr/bin/env bash
# Shared helpers for scripts/bench/*.sh — run from the Mac against the bench CM5.
#
#   BENCH=pi@192.168.8.203   (default; pi@10.0.0.111 on home Wi-Fi)
#
# All bench scripts are idempotent and only ever touch the Rust agent's own
# paths: /opt/xorgate-rs{,.new,.prev}, /usr/local/bin/xorgate-rs,
# /etc/systemd/system/xorgate-agent-rs.service(.d/), /etc/xorgate/agent-rs-dev.env,
# /var/lib/xorgate-rs. They NEVER touch xorgate-agent.service,
# /etc/xorgate/agent.env, /var/lib/xorgate or sim7600-lte.service (the JS prod
# agent and the modem daemon keep running throughout Phase 0-5).
# shellcheck disable=SC2034  # variables are consumed by the sourcing scripts
set -euo pipefail

BENCH="${BENCH:-pi@192.168.8.203}"
BENCH_SSH_OPTS=(-o ConnectTimeout=10 -o ServerAliveInterval=15)
UNIT=xorgate-agent-rs
JS_UNIT=xorgate-agent
RS_ROOT=/opt/xorgate-rs
RS_HOME=/var/lib/xorgate-rs
RS_ENV=/etc/xorgate/agent-rs-dev.env
DEV_BACKUP=/etc/xorgate/dev-backup-20260823
EVIDENCE_LOCAL="${EVIDENCE_LOCAL:-/Users/vynci/vynci/xorgate/plans/device-agent-js-to-rust/phase-0/evidence}"

# shellcheck disable=SC2029  # remote command words are expanded client-side on purpose
bench_ssh() { ssh "${BENCH_SSH_OPTS[@]}" "$BENCH" "$@"; }
# Interactive (tty) variant for `journalctl -f` and friends so Ctrl-C reaches the remote.
bench_ssh_tty() { ssh -t "${BENCH_SSH_OPTS[@]}" "$BENCH" "$@"; }
bench_scp_to() { scp "${BENCH_SSH_OPTS[@]}" -q "$1" "$BENCH:$2"; }
bench_scp_from() { scp "${BENCH_SSH_OPTS[@]}" -q "$BENCH:$1" "$2"; }
# Run a bash script (stdin) as root on the bench; arguments become $1.. there.
# shellcheck disable=SC2120  # callers may pass no args (script via stdin only)
bench_sudo_script() { ssh "${BENCH_SSH_OPTS[@]}" "$BENCH" sudo bash -s -- "$@"; }

log() { printf '\033[1;34m>>\033[0m %s\n' "$*" >&2; }

# ---------------------------------------------------------------------------
# Phase 2 (WP7) additions — the input profiles and the exclusive window
# (plan phase-2-cloud-plane.md §Bench recipe, decision O6). Appended only.
DROPIN_DIR=/etc/systemd/system/$UNIT.service.d
RUN_DROPIN_DIR=/run/systemd/system/$UNIT.service.d
INPUTS_DROPIN=$DROPIN_DIR/95-inputs.conf
EXCLUSIVE_DROPIN=$RUN_DROPIN_DIR/99-exclusive.conf
TAPE_DROPIN=$RUN_DROPIN_DIR/98-tape.conf
RS_ETC=$RS_HOME/etc
EVIDENCE_PHASE2_LOCAL="${EVIDENCE_PHASE2_LOCAL:-/Users/vynci/vynci/xorgate/plans/device-agent-js-to-rust/phase-2/evidence}"

# Shell functions shared by the Phase 2 remote scripts. Print them in front of
# a remote heredoc: `{ bench_remote_lib; cat <<'REMOTE' … REMOTE; } | bench_sudo_script`.
# Every MUTATING command goes through `act` so `--dry-run` (DRY=1) prints the
# command instead of running it; reads always run.
bench_remote_lib() {
  cat <<'LIB'
set -euo pipefail
UNIT=xorgate-agent-rs
JS_UNIT=xorgate-agent
MODEM_UNIT=sim7600-lte
RS_HOME=/var/lib/xorgate-rs
RS_ETC=$RS_HOME/etc
RS_ENV=/etc/xorgate/agent-rs-dev.env
DEV_BACKUP=/etc/xorgate/dev-backup-20260823
DROPIN_DIR=/etc/systemd/system/$UNIT.service.d
RUN_DROPIN_DIR=/run/systemd/system/$UNIT.service.d
INPUTS_DROPIN=$DROPIN_DIR/95-inputs.conf
EXCLUSIVE_DROPIN=$RUN_DROPIN_DIR/99-exclusive.conf
TAPE_DROPIN=$RUN_DROPIN_DIR/98-tape.conf
C15_BASELINE=$RS_HOME/evidence/c15-baseline.sha256
HW_SHARED=$RS_ETC/hardware.json
HW_EXCLUSIVE=$RS_ETC/hardware-exclusive.json
EXCLUSIVE_ENV=/run/xorgate-agent-rs-exclusive.env
DRY="${DRY:-0}"
CHANGED=()
act() { if [ "$DRY" = 1 ]; then echo "  [dry-run] would: $*"; else "$@"; fi; }
note() { CHANGED+=("$1"); echo "  - $1"; }
# write_if_changed <path> <mode> : content on stdin
write_if_changed() {
  local path=$1 mode=$2 tmp
  tmp=$(mktemp)
  cat > "$tmp"
  if [ -f "$path" ] && cmp -s "$tmp" "$path"; then
    echo "  = $path unchanged"; rm -f "$tmp"; return 1
  fi
  if [ "$DRY" = 1 ]; then
    echo "  [dry-run] would write $path:"; sed 's/^/      | /' "$tmp"; rm -f "$tmp"
  else
    install -d -m 0755 "$(dirname "$path")"
    install -m "$mode" "$tmp" "$path"; rm -f "$tmp"
  fi
  note "$path written"
  return 0
}
remove_if_present() {
  if [ -e "$1" ]; then act rm -f "$1"; note "$1 removed"; return 0; fi
  echo "  = $1 absent"; return 1
}
# Orchestrator decision (WP2 amb. 10): in the SHARED profile the Rust agent's
# IMU detection must not probe I2C bus 1 (`i2cdetect -y 1`) while the JS helper
# holds its SHTP session with the BNO085. The python-dir trick only blocks the
# helper spawn, so the Rust agent's DEV hardware copy says `"imu": null`
# ($HW_SHARED, the HARDWARE_MANIFEST of its env file); the exclusive window
# points it at $HW_EXCLUSIVE (the real block, a verbatim copy of the prod
# /etc/xorgate/hardware.json, which is only ever READ). Re-applied on every
# `profile.sh shared` because a cloud `hardware` block saved by the agent
# (worker.ts:781-900 order, WP3) would bring the IMU back.
hardware_copies() {
  local src=/etc/xorgate/hardware.json tmp
  if [ ! -f "$src" ]; then echo "  no $src — hardware copies skipped (agent: no hardware block)"; return 0; fi
  [ -d "$RS_ETC" ] || { act install -d -o pi -g pi -m 0700 "$RS_ETC"; note "$RS_ETC created"; }
  if [ -f "$HW_EXCLUSIVE" ] && cmp -s "$src" "$HW_EXCLUSIVE"; then echo "  = $HW_EXCLUSIVE unchanged"
  else act install -o pi -g pi -m 0600 "$src" "$HW_EXCLUSIVE"; note "$HW_EXCLUSIVE <- $src (real IMU block, exclusive window only)"; fi
  tmp=$(mktemp)
  python3 - "$src" > "$tmp" <<'PY'
import collections, json, sys
d = json.load(open(sys.argv[1]), object_pairs_hook=collections.OrderedDict)
d["imu"] = None
print(json.dumps(d, indent=2))
PY
  if [ -f "$HW_SHARED" ] && cmp -s "$tmp" "$HW_SHARED"; then echo "  = $HW_SHARED unchanged (imu: null)"
  else
    if [ -f "$HW_SHARED" ]; then echo "  $HW_SHARED differs:"; diff "$HW_SHARED" "$tmp" | sed 's/^/    /' || true; fi
    act install -o pi -g pi -m 0600 "$tmp" "$HW_SHARED"; note "$HW_SHARED <- $src with imu: null (shared profile)"
  fi
  rm -f "$tmp"
}
# The env file is an EnvironmentFile=, which beats every Environment= of the
# drop-ins: it must never carry the input knobs the profiles own.
env_file_guard() {
  local bad
  bad=$(grep -E '^(GPS_NMEA_PORT|XORGATE_PYTHON_DIR|PATH)=' "$RS_ENV" 2>/dev/null | sed 's/=.*//' || true)
  if [ -n "$bad" ]; then
    echo "REFUSING: $RS_ENV sets $(echo "$bad" | tr '\n' ' ')— an EnvironmentFile overrides the profile drop-ins; remove those keys by hand"
    exit 1
  fi
}
# 0 when the SHARED profile is fully in force (or an exclusive window is open
# with the JS agent stopped); prints every missing piece otherwise.
inputs_safe() {
  local ok=0 js
  if [ -f "$EXCLUSIVE_DROPIN" ]; then
    js=$(systemctl is-active $JS_UNIT || true)
    if [ "$js" = active ] || [ "$js" = activating ]; then echo "  UNSAFE: exclusive drop-in present while $JS_UNIT is $js"; return 1; fi
    return 0
  fi
  grep -qx 'Environment=PATH=/usr/local/bin:/usr/bin:/bin' "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: PATH without /usr/sbin in $INPUTS_DROPIN"; ok=1; }
  grep -qx 'Environment=GPS_NMEA_PORT=/run/xorgate-rs-no-gps' "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: GPS_NMEA_PORT=/run/xorgate-rs-no-gps in $INPUTS_DROPIN"; ok=1; }
  grep -qx 'Environment=XORGATE_PYTHON_DIR=/var/lib/xorgate-rs/no-python' "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: XORGATE_PYTHON_DIR=/var/lib/xorgate-rs/no-python in $INPUTS_DROPIN"; ok=1; }
  grep -qx "HARDWARE_MANIFEST=$HW_SHARED" "$RS_ENV" 2>/dev/null || { echo "  missing: HARDWARE_MANIFEST=$HW_SHARED in $RS_ENV (dev-manifests.sh)"; ok=1; }
  if grep -qE '^(GPS_NMEA_PORT|XORGATE_PYTHON_DIR|PATH)=' "$RS_ENV" 2>/dev/null; then echo "  UNSAFE: $RS_ENV carries GPS_NMEA_PORT/XORGATE_PYTHON_DIR/PATH"; ok=1; fi
  if [ -f "/etc/xorgate/hardware.json" ]; then
    [ "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("imu"))' "$HW_SHARED" 2>/dev/null)" = None ] || { echo "  missing: imu: null in $HW_SHARED (dev-manifests.sh / profile.sh shared)"; ok=1; }
  fi
  return $ok
}
profile_shared() {
  env_file_guard
  # O6 shared: the JS agent owns the GNSS tty and the BNO085, so the Rust
  # agent's GPS port does not exist, its Python helper cannot spawn, and its
  # hardware copy declares no IMU (no i2cdetect probe of bus 1).
  hardware_copies
  remove_if_present "$EXCLUSIVE_ENV" || true
  write_if_changed "$INPUTS_DROPIN" 0644 <<'CONF' || true
# Phase 2 SHARED input profile (scripts/bench/profile.sh, plan O6). Persistent.
# The JS prod agent owns /dev/ttyUSB1 (NMEA) and the BNO085 (I2C, SHTP): two
# readers would split NMEA bytes and two SHTP clients break the sensor, so the
# Rust agent gets an absent GPS port (gps group omitted), a python dir with no
# helper (spawn fails → IMU parked), no i2cdetect on PATH, and (env file) a
# hardware.json copy with imu: null, while both agents run.
[Service]
# PATH without /usr/sbin hides i2cdetect, so IMU detection never probes I2C bus
# 1 even if a cloud hardware block brings `imu` back into hardware.json (WP3
# saves it, message.rs:137). Every tool Phase 2 needs (vcgencmd, python3, sudo,
# systemctl, journalctl, timedatectl) is in /usr/bin.
Environment=PATH=/usr/local/bin:/usr/bin:/bin
Environment=GPS_NMEA_PORT=/run/xorgate-rs-no-gps
Environment=XORGATE_PYTHON_DIR=/var/lib/xorgate-rs/no-python
CONF
  remove_if_present "$EXCLUSIVE_DROPIN" || true
}
profile_exclusive() {
  env_file_guard
  # Refuse while the JS agent runs: it must be stopped (exclusive.sh begin).
  local js
  js=$(systemctl is-active $JS_UNIT || true)
  if [ "$js" = active ] || [ "$js" = activating ]; then
    if [ "$DRY" = 1 ]; then
      echo "  [dry-run] NOTE: $JS_UNIT is $js — a real run refuses here until it is stopped"
    else
      echo "refusing the exclusive profile: $JS_UNIT is $js (two NMEA/SHTP readers)"; exit 1
    fi
  fi
  write_if_changed "$EXCLUSIVE_DROPIN" 0644 <<'CONF' || true
# Phase 2 EXCLUSIVE input profile (scripts/bench/profile.sh exclusive, plan O6).
# RUNTIME drop-in under /run: it vanishes on reboot, so a reboot during the
# window falls back to the shared profile by construction. It sets the values
# a converted device has (/dev/ttyUSB1, the packaged python dir, full PATH,
# the real hardware block).
[Service]
# Explicit values (not UnsetEnvironment=): the full PATH (i2cdetect in
# /usr/sbin), the SIM7600 GNSS port (hardware/gps.ts:15 default), the packaged
# python dir (/opt/xorgate-rs/python, scripts/pack.sh).
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
Environment=GPS_NMEA_PORT=/dev/ttyUSB1
Environment=XORGATE_PYTHON_DIR=/opt/xorgate-rs/python
# EnvironmentFile= wins over Environment=, and a later file over an earlier
# one: this one (last drop-in) points HARDWARE_MANIFEST at the real IMU block.
EnvironmentFile=/run/xorgate-agent-rs-exclusive.env
CONF
  [ -f "$HW_EXCLUSIVE" ] || { if [ "$DRY" = 1 ]; then echo "  [dry-run] NOTE: $HW_EXCLUSIVE missing — run dev-manifests.sh"; else echo "refusing: $HW_EXCLUSIVE missing — run scripts/bench/dev-manifests.sh"; exit 1; fi; }
  write_if_changed "$EXCLUSIVE_ENV" 0644 <<ENV || true
# Phase 2 exclusive window (scripts/bench/profile.sh exclusive). RUNTIME file.
HARDWARE_MANIFEST=$HW_EXCLUSIVE
ENV
}
reload_restart_rs() {
  act systemctl daemon-reload
  act systemctl restart $UNIT
  if [ "$DRY" != 1 ]; then
    sleep 3
    echo "  $UNIT: $(systemctl is-active $UNIT || true)"
  fi
  echo "  unit Environment=: $(systemctl show $UNIT -p Environment --value | tr ' ' '\n' | grep -E '^(GPS_NMEA_PORT|XORGATE_PYTHON_DIR|XORGATE_RS_TAPE)=' | tr '\n' ' ' || true)"
  local mp
  mp=$(systemctl show $UNIT -p MainPID --value)
  if [ -n "$mp" ] && [ "$mp" != 0 ] && [ -r /proc/$mp/environ ]; then
    echo "  process env (pid $mp): $(tr '\0' '\n' < /proc/$mp/environ | grep -E '^(GPS_NMEA_PORT|XORGATE_PYTHON_DIR|XORGATE_RS_TAPE)=' | tr '\n' ' ' || true)(unset = the agent's defaults)"
  fi
  echo "  drop-ins: $(systemctl show $UNIT -p DropInPaths --value)"
}
units_report() {
  for u in $JS_UNIT $UNIT $MODEM_UNIT; do printf '  %-18s %s\n' "$u" "$(systemctl is-active $u || true)"; done
}
c15_check() {
  if [ ! -f "$C15_BASELINE" ]; then echo "  C15: no baseline at $C15_BASELINE (run dev-manifests.sh)"; return 0; fi
  if sha256sum --quiet -c "$C15_BASELINE" >/dev/null 2>&1; then
    echo "  C15: /etc/xorgate/{agent.env,kvs.json,hardware.json} sha256 UNCHANGED vs baseline"
  else
    echo "  C15: CHANGED vs baseline:"; sha256sum -c "$C15_BASELINE" 2>&1 | sed 's/^/      /'
  fi
}
LIB
}
