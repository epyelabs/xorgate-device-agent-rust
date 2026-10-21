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
# drop-ins: it must never carry the input knobs the profiles own. Phase 3
# (WP6): KVS_RUNTIME_BIN too (any value) — the shared profile points it at an
# absent file (masters never start), the exclusive one at the packaged runtime.
env_file_guard() {
  local bad
  bad=$(grep -E "^(GPS_NMEA_PORT|XORGATE_PYTHON_DIR|PATH|KVS_RUNTIME_BIN|$P4_KNOBS_RE)=" "$RS_ENV" 2>/dev/null | sed 's/=.*//' || true)
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
  grep -qx "Environment=PATH=$SHARED_PATH" "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: PATH=$SHARED_PATH (shared-shims first, no /usr/sbin) in $INPUTS_DROPIN"; ok=1; }
  grep -qx "Environment=KVS_RUNTIME_BIN=$NO_KVS" "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: KVS_RUNTIME_BIN=$NO_KVS in $INPUTS_DROPIN"; ok=1; }
  if [ -e "$NO_KVS" ]; then echo "  UNSAFE: $NO_KVS exists (must stay absent: KVS runtime unavailable in the shared profile)"; ok=1; fi
  local s rc
  for s in $SHIM_NAMES; do
    if [ ! -x "$SHIMS_DIR/$s" ]; then echo "  missing: shim $SHIMS_DIR/$s (profile.sh shared)"; ok=1; continue; fi
    if "$SHIMS_DIR/$s" >/dev/null 2>&1; then rc=0; else rc=$?; fi
    [ "$rc" = 1 ] || { echo "  UNSAFE: shim $SHIMS_DIR/$s exits $rc (want 1)"; ok=1; }
  done
  [ "$(kvs_channels_count "$KVS_SHARED")" = 0 ] || { echo "  missing: channels: [] in $KVS_SHARED (dev-manifests.sh)"; ok=1; }
  grep -qx "KVS_MANIFEST=$KVS_SHARED" "$RS_ENV" 2>/dev/null || { echo "  missing: KVS_MANIFEST=$KVS_SHARED in $RS_ENV (dev-manifests.sh)"; ok=1; }
  grep -qx 'Environment=GPS_NMEA_PORT=/run/xorgate-rs-no-gps' "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: GPS_NMEA_PORT=/run/xorgate-rs-no-gps in $INPUTS_DROPIN"; ok=1; }
  grep -qx 'Environment=XORGATE_PYTHON_DIR=/var/lib/xorgate-rs/no-python' "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: XORGATE_PYTHON_DIR=/var/lib/xorgate-rs/no-python in $INPUTS_DROPIN"; ok=1; }
  grep -qx "HARDWARE_MANIFEST=$HW_SHARED" "$RS_ENV" 2>/dev/null || { echo "  missing: HARDWARE_MANIFEST=$HW_SHARED in $RS_ENV (dev-manifests.sh)"; ok=1; }
  if grep -qE "^(GPS_NMEA_PORT|XORGATE_PYTHON_DIR|PATH|KVS_RUNTIME_BIN|$P4_KNOBS_RE)=" "$RS_ENV" 2>/dev/null; then echo "  UNSAFE: $RS_ENV carries GPS_NMEA_PORT/XORGATE_PYTHON_DIR/PATH/KVS_RUNTIME_BIN/XORGATE_LED_*/XORGATE_SMS_PORT"; ok=1; fi
  if [ -f "/etc/xorgate/hardware.json" ]; then
    [ "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("imu"))' "$HW_SHARED" 2>/dev/null)" = None ] || { echo "  missing: imu: null in $HW_SHARED (dev-manifests.sh / profile.sh shared)"; ok=1; }
  fi
  p4_inputs_safe || ok=1
  return $ok
}
profile_shared() {
  env_file_guard
  # Phase 3 (WP6, plan P3): the shared profile is MEDIA-OFF — refuse an env
  # file that would turn media back on (KVS_MANIFEST other than the
  # channels-[] copy beats the drop-ins), and the no-kvs path must not exist.
  env_file_guard_shared
  if [ -e "$NO_KVS" ]; then echo "REFUSING: $NO_KVS exists — it must stay ABSENT (KVS_RUNTIME_BIN of the shared profile); remove it by hand"; exit 1; fi
  # O6 shared: the JS agent owns the GNSS tty and the BNO085, so the Rust
  # agent's GPS port does not exist, its Python helper cannot spawn, and its
  # hardware copy declares no IMU (no i2cdetect probe of bus 1).
  hardware_copies
  # P3: rpicam-hello / gst-launch-1.0 shims first on PATH (no camera probe,
  # no capture while the JS agent owns the sensors). Created HERE (not in
  # dev-manifests.sh) so the profile and its verifier own them together.
  shared_shims
  p4_sudo_shim
  local nch
  nch=$(kvs_channels_count "$KVS_SHARED")
  [ "$nch" = 0 ] || echo "  WARNING: $KVS_SHARED has channels=$nch (want 0) — run scripts/bench/dev-manifests.sh (inputs_safe fails until then)"
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
# Phase 3 (plan P3, media-off): shared-shims/{rpicam-hello,gst-launch-1.0}
# come FIRST (they print "disabled in the shared profile" and exit 1: no
# camera probe, no capture), and KVS_RUNTIME_BIN names a file that does not
# exist (runtime unavailable -> one [media] warn, masters never start). The
# Rust kvs.json copy has channels: [] (dev-manifests.sh); telemetry
# recording + upload still run.
Environment=PATH=/var/lib/xorgate-rs/shared-shims:/usr/local/bin:/usr/bin:/bin
Environment=GPS_NMEA_PORT=/run/xorgate-rs-no-gps
Environment=XORGATE_PYTHON_DIR=/var/lib/xorgate-rs/no-python
Environment=KVS_RUNTIME_BIN=/var/lib/xorgate-rs/no-kvs
# Phase 4 (WP6, plan Q6 / phase-4-operations.md §9): the JS prod agent drives
# the status LED, the SMS port, the clock stepper and the camera mount. Two LED
# writers would fight, so the agent's LED is inert ("gpio chip not found") and
# shared-shims/pinctrl refuses (the `led boot|exit-state` unit hooks are
# '-'-prefixed: harmless); shared-shims/sudo refuses ONLY xorgate-setclock and
# xorgate-camrotate (two steppers double-step; DEV cameraMount would fight
# PROD's over the same config.txt) and execs /usr/bin/sudo otherwise; the SMS
# port does not exist (read_sms answers no_modem; the two agents' lock files
# live in different homes on the bench and would not interlock).
Environment=XORGATE_LED_GPIOCHIP=/run/xorgate-rs-no-gpio
Environment=XORGATE_SMS_PORT=/run/xorgate-rs-no-sms
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
# Phase 3 (media-on): the full PATH has NO shared-shims (the real
# rpicam-hello / gst-launch-1.0), and KVS_RUNTIME_BIN is the packaged runtime.
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
Environment=GPS_NMEA_PORT=/dev/ttyUSB1
Environment=XORGATE_PYTHON_DIR=/opt/xorgate-rs/python
Environment=KVS_RUNTIME_BIN=/opt/xorgate-rs/media/xorgate-kvs-webrtc
# Phase 4 (WP6, §9): the real LED chip (by label pinctrl-rp1), the SMS port by
# USB identity (interface 05, never /dev/ttyUSB2), the real pinctrl + sudo
# (full PATH above has no shared-shims): drop the shared profile's knobs.
UnsetEnvironment=XORGATE_LED_GPIOCHIP XORGATE_SMS_PORT XORGATE_LED_WRITER
# EnvironmentFile= wins over Environment=, and a later file over an earlier
# one: this one (last drop-in) points HARDWARE_MANIFEST at the real IMU block
# and KVS_MANIFEST at the channels-on DEV manifest (beats agent-rs-dev.env).
EnvironmentFile=/run/xorgate-agent-rs-exclusive.env
CONF
  [ -f "$HW_EXCLUSIVE" ] || { if [ "$DRY" = 1 ]; then echo "  [dry-run] NOTE: $HW_EXCLUSIVE missing — run dev-manifests.sh"; else echo "refusing: $HW_EXCLUSIVE missing — run scripts/bench/dev-manifests.sh"; exit 1; fi; }
  [ -f "$KVS_EXCLUSIVE" ] || { if [ "$DRY" = 1 ]; then echo "  [dry-run] NOTE: $KVS_EXCLUSIVE missing — run dev-manifests.sh"; else echo "refusing: $KVS_EXCLUSIVE missing — run scripts/bench/dev-manifests.sh"; exit 1; fi; }
  [ -x "$KVS_RUNTIME_RS" ] || echo "  NOTE: $KVS_RUNTIME_RS not installed (a pre-Phase-3 build?) — masters will report the runtime unavailable"
  write_if_changed "$EXCLUSIVE_ENV" 0644 <<ENV || true
# Phase 2/3 exclusive window (scripts/bench/profile.sh exclusive). RUNTIME file.
HARDWARE_MANIFEST=$HW_EXCLUSIVE
KVS_MANIFEST=$KVS_EXCLUSIVE
ENV
}
reload_restart_rs() {
  # Phase 5 (I6, CF65): a SHIPPED (converted) unit — install-rs.sh installs it
  # byte-equal, Conflicts=xorgate-agent.service included — must never be
  # (re)started on the bench: starting it stops the PROD JS agent.
  if rs_unit_converted; then
    echo "  $RS_CONVERTED_REFUSAL"
    echo "  REFUSING to restart $UNIT: bench flavour lost (above) — run scripts/bench/install.sh <tarball>, then retry"
    return 1
  fi
  act systemctl daemon-reload
  # Phase 4 (WP6): an `xorgate-rs update` (E11) reinstalls the SHIPPED unit,
  # whose Conflicts=xorgate-agent.service would stop the JS prod agent on this
  # restart — refuse until install.sh has restored the bench flavour.
  if ! bench_flavour_ok; then
    echo "  REFUSING to restart $UNIT: bench flavour lost (above) — run scripts/bench/install.sh <tarball>, then retry"
    return 1
  fi
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
# ---------------------------------------------------------------------------
# Phase 3 (WP6) additions — media-off shared profile, window helpers
# (plan phase-3-recording-media.md §Bench recipe, P3/P7/P13). Appended only.
SHIMS_DIR=$RS_HOME/shared-shims
SHIM_NAMES="rpicam-hello gst-launch-1.0 pinctrl"
SHARED_PATH=$SHIMS_DIR:/usr/local/bin:/usr/bin:/bin
NO_KVS=$RS_HOME/no-kvs
KVS_SHARED=$RS_ETC/kvs.json
KVS_EXCLUSIVE=$RS_ETC/kvs-exclusive.json
KVS_RUNTIME_RS=/opt/xorgate-rs/media/xorgate-kvs-webrtc
JS_HOME=/var/lib/xorgate
GATE_DROPIN=$RUN_DROPIN_DIR/97-gate-seam.conf
GATE_UNIT=xorgate-rs-gate-seam
GATE_DIR=/run/xorgate-rs-lte
GATE_FILE=$GATE_DIR/telemetry.json
# The modem daemon's file (JS src/hardware/modem.ts:31 default) — READ ONLY.
GATE_SRC=/run/sim7600-lte/telemetry.json
GATE_SCRIPT=/run/xorgate-rs-gate-seam.sh
GATE_MODE=/run/xorgate-rs-gate-seam.status
SOAK_UNIT=xorgate-rs-soak
SOAK_RUN=/run/xorgate-rs-soak
FILLER_ROOT=$RS_HOME/recordings/video
FILLER_MARK=.xorgate-filler
# Test-only seams (scripts/bench/tests/*.sh; NEVER set on the bench — ssh +
# sudo drop the caller's environment there anyway):
#   XORGATE_TEST_JS_MEDIA_PIDS_CMD / XORGATE_TEST_RS_MEDIA_PIDS_CMD  a bash
#     command whose stdout replaces the JS / Rust media pid scan;
#   XORGATE_TEST_JS_RELEASE_SECS  replaces the 60 s camera-release wait.
# Number of KVS channels in a manifest ("?" when unreadable).
kvs_channels_count() {
  python3 -c 'import json,sys; c=json.load(open(sys.argv[1])).get("channels"); print(len(c) if isinstance(c, list) else "?")' "$1" 2>/dev/null || echo "?"
}
# Shared-profile-only env-file guard (plan P3): KVS_MANIFEST must name the
# channels-[] copy; unset = the agent default /etc/xorgate/kvs.json (PROD,
# channels on), any other value (kvs-exclusive.json, a prod path) = media on.
env_file_guard_shared() {
  [ -f "$RS_ENV" ] || { echo "  NOTE: $RS_ENV absent (the unit cannot start without it)"; return 0; }
  local km
  km=$(sed -n 's/^KVS_MANIFEST=//p' "$RS_ENV" | tail -n1)
  if [ "$km" != "$KVS_SHARED" ]; then
    echo "REFUSING: $RS_ENV has KVS_MANIFEST='${km:-<unset: agent default /etc/xorgate/kvs.json>}' — the shared (media-off) profile needs KVS_MANIFEST=$KVS_SHARED (run scripts/bench/dev-manifests.sh; an EnvironmentFile overrides the drop-ins)"
    exit 1
  fi
}
shared_shims() {
  local s
  if [ ! -d "$SHIMS_DIR" ]; then act install -d -o root -g root -m 0755 "$SHIMS_DIR"; note "$SHIMS_DIR created (root 0755)"; fi
  for s in $SHIM_NAMES; do
    write_if_changed "$SHIMS_DIR/$s" 0755 <<SHIM || true
#!/bin/sh
# xorgate-rs SHARED-profile shim (scripts/bench/profile.sh shared, plan P3).
# First on the Rust agent's PATH while the JS prod agent owns the cameras:
# no camera probe, no capture. The exclusive profile's PATH omits this dir.
echo "$s: disabled in the shared profile (xorgate-rs bench, plan P3)" >&2
exit 1
SHIM
  done
}
# --- process classification ------------------------------------------------
# alive = /proc entry present and not a zombie.
pid_alive() {
  local s
  s=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
  s=${s##*) }
  [ "${s%% *}" != Z ]
}
# argv[0] basename (comm for an empty cmdline). Fails for a dead pid.
proc_name() {
  local a
  a=$(tr '\0' '\n' < "/proc/$1/cmdline" 2>/dev/null | head -n1) || true
  [ -n "$a" ] || a=$(cat "/proc/$1/comm" 2>/dev/null) || return 1
  [ -n "$a" ] || return 1
  basename -- "$a"
}
is_media_name() { case "$1" in gst-launch-1.0|xorgate-kvs-webrtc) return 0 ;; *) return 1 ;; esac; }
# Pids whose cgroup is <unit>.service (or below it).
pids_in_unit() {
  local f p
  for f in /proc/[0-9]*/cgroup; do
    grep -qE "/$1\.service(/|\$)" "$f" 2>/dev/null || continue
    p=${f#/proc/}; echo "${p%/cgroup}"
  done
}
_main_pids_refresh() {
  _JS_MAIN=$(systemctl show "$JS_UNIT" -p MainPID --value 2>/dev/null || true)
  _RS_MAIN=$(systemctl show "$UNIT" -p MainPID --value 2>/dev/null || true)
  : "${_JS_MAIN:=0}" "${_RS_MAIN:=0}"
}
# JS | RUST | OTHER-LIVE | DEAD: the unit cgroup first, then the parent chain
# against both units' MainPIDs.
pid_owner() {
  local p=$1 cg pp s i
  pid_alive "$p" || { echo DEAD; return 0; }
  cg=$(cat "/proc/$p/cgroup" 2>/dev/null || true)
  if grep -qE "/$JS_UNIT\.service(/|\$)" <<<"$cg"; then echo JS; return 0; fi
  if grep -qE "/$UNIT\.service(/|\$)" <<<"$cg"; then echo RUST; return 0; fi
  [ -n "${_JS_MAIN:-}" ] || _main_pids_refresh
  if [ "$p" = "$_JS_MAIN" ] && [ "$_JS_MAIN" != 0 ]; then echo JS; return 0; fi
  if [ "$p" = "$_RS_MAIN" ] && [ "$_RS_MAIN" != 0 ]; then echo RUST; return 0; fi
  pp=$p
  for i in $(seq 1 64); do
    s=$(cat "/proc/$pp/stat" 2>/dev/null) || break
    s=${s##*) }; read -r _ pp _ <<<"$s"
    case "$pp" in ''|0|1) break ;; esac
    [ "$pp" = "$_JS_MAIN" ] && [ "$_JS_MAIN" != 0 ] && { echo JS; return 0; }
    [ "$pp" = "$_RS_MAIN" ] && [ "$_RS_MAIN" != 0 ] && { echo RUST; return 0; }
  done
  : "$i"
  echo OTHER-LIVE
}
# JS capture/master pids (cgroup scan of xorgate-agent.service).
js_media_pids() {
  if [ -n "${XORGATE_TEST_JS_MEDIA_PIDS_CMD:-}" ]; then bash -c "$XORGATE_TEST_JS_MEDIA_PIDS_CMD"; return 0; fi
  local p n
  for p in $(pids_in_unit "$JS_UNIT"); do
    n=$(proc_name "$p" 2>/dev/null) || continue
    if is_media_name "$n"; then echo "$p"; fi
  done
}
# Rust capture/master pids: every live gst-launch-1.0 / xorgate-kvs-webrtc that
# is in (or descends from) the Rust unit, plus escapees outside both units whose
# argv/exe names the Rust tree (/var/lib/xorgate-rs/, /opt/xorgate-rs/).
# NEVER a pid of the JS unit.
rs_media_pids() {
  if [ -n "${XORGATE_TEST_RS_MEDIA_PIDS_CMD:-}" ]; then bash -c "$XORGATE_TEST_RS_MEDIA_PIDS_CMD"; return 0; fi
  local d p n o
  _main_pids_refresh
  for d in /proc/[0-9]*; do
    p=${d#/proc/}
    n=$(proc_name "$p" 2>/dev/null) || continue
    is_media_name "$n" || continue
    o=$(pid_owner "$p")
    case "$o" in
      RUST) echo "$p" ;;
      OTHER-LIVE)
        if grep -qa -e /var/lib/xorgate-rs/ -e /opt/xorgate-rs/ "/proc/$p/cmdline" 2>/dev/null \
           || readlink "/proc/$p/exe" 2>/dev/null | grep -q '^/opt/xorgate-rs/'; then echo "$p"; fi ;;
    esac
  done
}
# SIGINT every Rust media pid, wait 5 s, SIGKILL the rest. Re-checks before
# EVERY signal that the pid is not in (or below) the JS unit. Never fails
# (callers run it right before the JS restart).
kill_rs_media() {
  local p targets="" left=""
  for p in $(rs_media_pids | sort -u); do
    pid_alive "$p" || continue
    if [ "$(pid_owner "$p")" = JS ]; then echo "  ! pid $p belongs to $JS_UNIT — NOT signalled"; continue; fi
    targets="$targets $p"
  done
  if [ -z "$targets" ]; then echo "  = no Rust gst-launch-1.0/xorgate-kvs-webrtc alive"; return 0; fi
  for p in $targets; do
    note "SIGINT -> Rust $(proc_name "$p" 2>/dev/null || echo '?') pid $p"
    act kill -INT "$p" || true
  done
  [ "$DRY" = 1 ] && return 0
  for _ in $(seq 1 10); do
    left=""; for p in $targets; do if pid_alive "$p"; then left="$left $p"; fi; done
    if [ -z "$left" ]; then echo "  = Rust media leftovers gone after SIGINT"; return 0; fi
    sleep 0.5
  done
  for p in $left; do
    pid_alive "$p" || continue
    if [ "$(pid_owner "$p")" = JS ]; then echo "  ! pid $p belongs to $JS_UNIT — NOT signalled"; continue; fi
    kill -KILL "$p" 2>/dev/null || true
    note "SIGKILL -> Rust pid $p (ignored SIGINT for 5 s)"
  done
  return 0
}
# --- /dev/shm audit (shm-audit.sh, exclusive.sh begin/end, soak.sh) ---------
# TSV rows: shm<TAB>name<TAB>size<TAB>pid<TAB>owner | sock<TAB>path<TAB>size<TAB>-<TAB>home
# shmsink names its segments "shmpipe.%5d.%5d" (pid, counter; space padded).
shm_audit_rows() {
  local f name pid size owner
  _main_pids_refresh
  for f in /dev/shm/shmpipe.*; do
    [ -e "$f" ] || continue
    name=${f#/dev/shm/}
    pid=${name#shmpipe.}; pid=${pid%%.*}; pid=${pid// /}
    size=$(stat -c %s "$f" 2>/dev/null || echo 0)
    case "$pid" in ''|*[!0-9]*) owner=UNPARSED ;; *) owner=$(pid_owner "$pid") ;; esac
    printf 'shm\t%s\t%s\t%s\t%s\n' "$name" "$size" "$pid" "$owner"
  done
  for f in "$RS_HOME"/shm/*.sock "$JS_HOME"/shm/*.sock; do
    [ -e "$f" ] || continue
    case "$f" in "$RS_HOME"/*) owner=rust_home ;; *) owner=js_home ;; esac
    printf 'sock\t%s\t%s\t-\t%s\n' "$f" "$(stat -c %s "$f" 2>/dev/null || echo 0)" "$owner"
  done
}
# "js=N rust=N other=N dead=N" for the shmpipe rows.
shm_counts() {
  shm_audit_rows | awk -F'\t' '$1=="shm"{c[$5]++} END{printf "js=%d rust=%d other=%d dead=%d\n", c["JS"], c["RUST"], c["OTHER-LIVE"]+c["UNPARSED"], c["DEAD"]}'
}
# Human (default) or --json report; always returns 0 (evidence, not a gate).
# shellcheck disable=SC2120  # --json is passed by shm-audit.sh only
shm_audit() {
  local rows
  rows=$(shm_audit_rows)
  if [ "${1:-}" = --json ]; then
    python3 -c '
import json, sys
shm, socks, c = [], [], {"JS": 0, "RUST": 0, "OTHER-LIVE": 0, "DEAD": 0, "UNPARSED": 0}
for line in sys.stdin.read().splitlines():
    if not line.strip():
        continue
    k, name, size, pid, owner = line.split("\t")
    if k == "shm":
        shm.append({"name": name, "size": int(size), "pid": int(pid) if pid.isdigit() else None, "owner": owner})
        c[owner] = c.get(owner, 0) + 1
    else:
        socks.append({"path": name, "size": int(size), "home": owner})
print(json.dumps({"shm": shm, "sockets": socks, "summary": {"shm": len(shm), "js": c["JS"], "rust": c["RUST"], "otherLive": c["OTHER-LIVE"] + c["UNPARSED"], "dead": c["DEAD"], "sockets": len(socks)}}, indent=2))
' <<<"$rows"
    return 0
  fi
  echo "  /dev/shm/shmpipe.* (owner: JS = xorgate-agent child, RUST = xorgate-agent-rs child, DEAD = no such pid):"
  if grep -q '^shm' <<<"$rows"; then
    awk -F'\t' '$1=="shm"{printf "    %-10s pid=%-8s size=%-10s %s\n", $5, $4, $3, "\x27" $2 "\x27"}' <<<"$rows"
  else
    echo "    (none)"
  fi
  echo "  shm sockets:"
  if grep -q '^sock' <<<"$rows"; then
    awk -F'\t' '$1=="sock"{printf "    %-9s %s\n", $5, $2}' <<<"$rows"
  else
    echo "    (none)"
  fi
  awk -F'\t' '$1=="shm"{n++; c[$5]++} $1=="sock"{s++} END{printf "  SUMMARY shm=%d js=%d rust=%d other-live=%d dead=%d sockets=%d\n", n, c["JS"], c["RUST"], c["OTHER-LIVE"]+c["UNPARSED"], c["DEAD"], s}' <<<"$rows"
  return 0
}
# --- window helpers shared by gate-seam.sh / soak.sh / filler.sh / exclusive.sh end
# D12 seam teardown: stop the copier, drop the runtime drop-in + files.
gate_seam_off() {
  local st
  st=$(systemctl is-active "$GATE_UNIT" 2>/dev/null || true)
  if [ "$st" = active ] || [ "$st" = activating ]; then act systemctl stop "$GATE_UNIT"; note "$GATE_UNIT stopped"; else echo "  = $GATE_UNIT not running ($st)"; fi
  act systemctl reset-failed "$GATE_UNIT" 2>/dev/null || true
  remove_if_present "$GATE_DROPIN" || true
  remove_if_present "$GATE_SCRIPT" || true
  remove_if_present "$GATE_MODE" || true
  if [ -d "$GATE_DIR" ]; then
    case "$GATE_DIR" in /run/xorgate-rs-lte) act rm -rf -- "$GATE_DIR"; note "$GATE_DIR removed" ;; *) echo "  refusing to remove $GATE_DIR" ;; esac
  else echo "  = $GATE_DIR absent"; fi
}
# --- exclusive-window dead-man (review-wp6 B1). `exclusive.sh begin` arms a
# transient timer; if `exclusive.sh end` never runs (gate runner died, usage
# cut, laptop asleep) it fires after DEADMAN_SECS and gives the PROD JS agent
# back: stops the Rust agent + window helpers, drops the runtime drop-ins and
# the exclusive env, daemon-reload, reverts an install-rs.sh conversion still
# in place (Phase 5), enable (if disabled) + start + verify xorgate-agent. Idempotent
# (every step tolerates "already done"). `end` cancels it; a reboot clears it.
DEADMAN_UNIT=xorgate-rs-window-deadman
DEADMAN_SCRIPT=/run/xorgate-rs-window-deadman.sh
deadman_state() { systemctl is-active "$DEADMAN_UNIT.timer" 2>/dev/null || true; }
# deadman_arm <seconds> <window dir>: (re)arms; an armed timer is replaced.
deadman_arm() {
  local secs=$1 win=$2
  deadman_cancel quiet
  write_if_changed "$DEADMAN_SCRIPT" 0700 <<SCRIPT || true
#!/bin/bash
# xorgate-rs exclusive-window dead-man, armed by scripts/bench/exclusive.sh begin
# (window $win). Fires only when \`exclusive.sh end\` never ran. Idempotent.
set -u
say() { echo "[$DEADMAN_UNIT] \$*"; }
say "exclusive window never ended — restoring the PROD JS agent ($JS_UNIT)"
for u in $UNIT $GATE_UNIT $SOAK_UNIT; do systemctl stop "\$u" 2>/dev/null || true; done
rm -f $EXCLUSIVE_DROPIN $TAPE_DROPIN $GATE_DROPIN $EXCLUSIVE_ENV
systemctl daemon-reload || true
act() { "\$@"; }
note() { say "\$1"; }
DRY=0
JS_UNIT=$JS_UNIT
# Phase 5 (I6): a conversion still in place (install-rs.sh record state
# converted|converting) is reverted FIRST — keep mode: Rust disabled + stopped,
# the JS enablement restored from the record, JS started. Bounded; a failed or
# missing revert never blocks the enable + start below.
REC=$RS_INSTALL_RECORD
st=\$(sed -n 's/^state=//p' "\$REC" 2>/dev/null | head -n1)
case "\$st" in
  converted|converting)
    if [ -f $RS_INSTALLER ]; then
      say "conversion record state=\$st — running $RS_INSTALLER --revert"
      if timeout 300 bash $RS_INSTALLER --revert </dev/null; then say "revert done"
      else rc=\$?; say "$RS_INSTALLER --revert exited \$rc — continuing with enable + start of $JS_UNIT"; fi
    else
      say "conversion record state=\$st but $RS_INSTALLER is missing — cannot revert; continuing with enable + start of $JS_UNIT"
    fi ;;
esac
# Phase 4 (review-wp6 M4): NTP back on + undo an ntp-unsync.sh break.
NTP_STATE=$NTP_STATE
$(declare -f ntp_restore)
ntp_restore
# Phase 5 (CF64): enable (if is-enabled != enabled) + start + verify.
$(declare -f start_js_verified enable_start_js_verified)
if enable_start_js_verified; then
  say "$JS_UNIT active + \$(systemctl is-enabled $JS_UNIT 2>/dev/null || true)"
  date -u +%Y-%m-%dT%H:%M:%SZ > "$win/deadman-fired" 2>/dev/null || true
  exit 0
fi
say "$JS_UNIT FAILED TO START — the PROD agent is DOWN: sudo systemctl enable --now $JS_UNIT"
exit 1
SCRIPT
  act systemd-run --unit="$DEADMAN_UNIT" --on-active="${secs}s" --timer-property=AccuracySec=1s \
    --description="xorgate-rs exclusive-window dead-man (scripts/bench/exclusive.sh)" /bin/bash "$DEADMAN_SCRIPT"
  note "$DEADMAN_UNIT armed: restores $JS_UNIT in ${secs}s unless exclusive.sh end runs"
}
# deadman_cancel [quiet]: stops the timer (never a restore already firing),
# removes the script. No-op when not armed.
deadman_cancel() {
  local st
  st=$(deadman_state)
  if [ "$st" = active ] || [ "$st" = activating ]; then act systemctl stop "$DEADMAN_UNIT.timer"; note "$DEADMAN_UNIT cancelled"
  elif [ "${1:-}" != quiet ]; then echo "  = $DEADMAN_UNIT not armed ($st)"; fi
  act systemctl reset-failed "$DEADMAN_UNIT.timer" "$DEADMAN_UNIT.service" 2>/dev/null || true
  if [ -e "$DEADMAN_SCRIPT" ]; then act rm -f "$DEADMAN_SCRIPT"; [ "${1:-}" = quiet ] || note "$DEADMAN_SCRIPT removed"; fi
  return 0
}
soak_stop() {
  local st
  st=$(systemctl is-active "$SOAK_UNIT" 2>/dev/null || true)
  if [ "$st" = active ] || [ "$st" = activating ]; then act systemctl stop "$SOAK_UNIT"; note "$SOAK_UNIT stopped"; else echo "  = $SOAK_UNIT not running ($st)"; fi
  act systemctl reset-failed "$SOAK_UNIT" 2>/dev/null || true
  if [ -d "$SOAK_RUN" ]; then act rm -rf -- "$SOAK_RUN"; note "$SOAK_RUN removed"; fi
  return 0
}
# Absolute, symlink-resolved path must be under /var/lib/xorgate-rs/ (never
# /var/lib/xorgate itself).
under_rs_home() {
  local r
  r=$(realpath -m -- "$1")
  case "$r" in /var/lib/xorgate-rs/*) return 0 ;; *) return 1 ;; esac
}
filler_sessions() {
  local m
  for m in "$FILLER_ROOT"/*/*/"$FILLER_MARK"; do [ -e "$m" ] && dirname -- "$m"; done
  return 0
}
filler_clear() {
  local d n=0
  under_rs_home "$FILLER_ROOT" || { echo "  REFUSING: $FILLER_ROOT resolves to $(realpath -m -- "$FILLER_ROOT") (outside /var/lib/xorgate-rs)"; return 1; }
  while IFS= read -r d; do
    [ -n "$d" ] || continue
    if ! under_rs_home "$d"; then echo "  REFUSING: $d resolves outside /var/lib/xorgate-rs"; continue; fi
    echo "  filler session $d ($(du -sh -- "$d" 2>/dev/null | cut -f1), $(find "$d" -maxdepth 1 -name '*.mp4' | wc -l) fragments)"
    act rm -rf -- "$d"; note "$d removed (filler)"; n=$((n + 1))
  done < <(filler_sessions)
  [ "$n" -gt 0 ] || echo "  = no filler sessions under $FILLER_ROOT"
  return 0
}
# D17: the JS agent's capture/masters back, read from its state.json (READ
# ONLY). $1 = timeout seconds (0 = report once); $2 = optional baseline
# "cam0=running cam1=stopped" (exclusive.sh begin's js-media-before.txt).
# Returns 0 when, in a state.json written by the CURRENT JS process, every
# stream is running OR the states equal the baseline.
js_media_wait_running() {
  local t=${1:-0} base=${2:-} i out
  for ((i = 0; ; i++)); do
    _main_pids_refresh
    out=$(python3 - "$JS_HOME/state.json" "$_JS_MAIN" "$(pids_in_unit "$JS_UNIT" | tr '\n' ' ')" "$base" <<'PY' || true
import json, sys
path, main, unit_pids, base = sys.argv[1], sys.argv[2], sys.argv[3].split(), sys.argv[4]
try:
    d = json.load(open(path))
except Exception as e:
    print(f"WAIT state.json unreadable ({e.__class__.__name__})"); sys.exit(0)
pid = str(d.get("pid"))
fresh = "fresh" if (main in ("", "0") or pid == main or pid in unit_pids) else "STALE"
media = (d.get("latest") or {}).get("media") or {}
streams = media.get("streams") or []
states = " ".join(f"{s.get('streamKey')}={s.get('state')}" for s in streams) or "(no streams)"
running = bool(streams) and all(s.get("state") == "running" for s in streams)
same = bool(base) and states == base
ok = fresh == "fresh" and (running or same)
why = "all running" if running else ("= pre-window baseline" if same else (f"baseline: {base}" if base else "not all running"))
print(("OK" if ok else "WAIT") + f" pid={pid} ({fresh}; MainPID {main or '?'}) kvs={(media.get('kvs') or {}).get('value')} [{why}] streams: {states}")
PY
)
    if [ "${out%% *}" = OK ] || [ "$i" -ge "$t" ]; then break; fi
    sleep 1
  done
  echo "  JS media after ${i}s: ${out#* }"
  [ "${out%% *}" = OK ]
}
# ---------------------------------------------------------------------------
# Phase 4 (WP6) additions — LED / SMS / clock / camera-mount knobs of the
# shared profile, the updater bootstrap (plan phase-4-operations.md §9, Q6).
# The knobs live ONLY in the shared drop-in + shims, never in $RS_ENV.
P4_KNOBS_RE='XORGATE_LED_GPIOCHIP|XORGATE_LED_WRITER|XORGATE_SMS_PORT'
NO_GPIO=/run/xorgate-rs-no-gpio
NO_SMS=/run/xorgate-rs-no-sms
SUDO_SHIM=$SHIMS_DIR/sudo
UPDATER_UNIT=xorgate-updater-rs.service
UPDATE_SUDOERS=/etc/sudoers.d/024_xorgate-update-rs
# The shared-profile sudo shim. Refuses the two helpers the JS agent owns on
# the bench, by ANY argument naming them (path or basename), before anything
# runs; every other call is the real sudo (Phase 2 command checks keep working).
sudo_shim_body() {
  cat <<'SHIM'
#!/bin/sh
# xorgate-rs SHARED-profile shim (scripts/bench/profile.sh shared, Phase 4 §9).
# The JS prod agent owns the clock stepper and the camera mount on the bench:
# refuse xorgate-setclock / xorgate-camrotate, exec the real sudo otherwise.
# The exclusive profile's PATH omits this dir.
for a in "$@"; do
  case "$a" in
    /usr/local/bin/xorgate-setclock|xorgate-setclock|/usr/local/bin/xorgate-camrotate|xorgate-camrotate)
      echo "sudo $a: disabled in the shared profile (xorgate-rs bench, Phase 4)" >&2
      exit 1 ;;
  esac
done
exec /usr/bin/sudo "$@"
SHIM
}
p4_sudo_shim() {
  write_if_changed "$SUDO_SHIM" 0755 < <(sudo_shim_body) || true
}
# 0 when the Phase 4 half of the shared profile is in force. The sudo shim is
# checked by CONTENT, never by running it: a broken shim would exec the real
# `sudo -n xorgate-setclock …`.
p4_inputs_safe() {
  local ok=0 want
  grep -qx "Environment=XORGATE_LED_GPIOCHIP=$NO_GPIO" "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: XORGATE_LED_GPIOCHIP=$NO_GPIO in $INPUTS_DROPIN (Phase 4)"; ok=1; }
  grep -qx "Environment=XORGATE_SMS_PORT=$NO_SMS" "$INPUTS_DROPIN" 2>/dev/null || { echo "  missing: XORGATE_SMS_PORT=$NO_SMS in $INPUTS_DROPIN (Phase 4)"; ok=1; }
  if [ -e "$NO_GPIO" ]; then echo "  UNSAFE: $NO_GPIO exists (must stay absent: the agent LED is inert in the shared profile)"; ok=1; fi
  if [ -e "$NO_SMS" ]; then echo "  UNSAFE: $NO_SMS exists (must stay absent: no SMS port in the shared profile)"; ok=1; fi
  want=$(sudo_shim_body)
  if [ ! -x "$SUDO_SHIM" ]; then echo "  missing: shim $SUDO_SHIM (profile.sh shared, Phase 4)"; ok=1
  elif [ "$(cat "$SUDO_SHIM")" != "$want" ]; then echo "  UNSAFE: $SUDO_SHIM differs from the shared-profile body (re-run profile.sh shared)"; ok=1; fi
  return $ok
}
# Bootstrap of the Rust remote-update rail (CF47 bench half, §5 "Bootstrap"):
# the first xorgate-updater-rs.service + 024 grant; every later
# `xorgate-rs update` reinstalls them from its tarball. $1 = install root,
# $2 = service user. visudo BEFORE it lands; a refusal warns, never fatal.
update_sudoers_body() {
  printf '%s\n' \
    "# Installed by xorgate-rs. Allows the agent (running as $1) to start the" \
    "# one-shot self-update unit on a cloud-issued 'update' command, and nothing" \
    "# else. See device-agent-rust crates/xorgate-hal/src/privileged.rs + systemd/xorgate-updater-rs.service." \
    "$1 ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater-rs.service"
}
p4_updater_bootstrap() {
  local root=$1 user=$2 tmpd
  if [ -f "$root/systemd/$UPDATER_UNIT" ]; then
    if [ -f "/etc/systemd/system/$UPDATER_UNIT" ] && cmp -s "$root/systemd/$UPDATER_UNIT" "/etc/systemd/system/$UPDATER_UNIT"; then
      echo "  = $UPDATER_UNIT unchanged"
    else
      act install -m 0644 "$root/systemd/$UPDATER_UNIT" "/etc/systemd/system/$UPDATER_UNIT"; note "/etc/systemd/system/$UPDATER_UNIT installed (no [Install]: never enabled)"
    fi
  else
    echo "  NOTE: tarball has no systemd/$UPDATER_UNIT (a pre-Phase-4 build) — remote update rail not bootstrapped"
    return 0
  fi
  if [ -z "$user" ] || [ "$user" = root ]; then echo "  = no $UPDATE_SUDOERS (service user '${user:-?}' needs no grant)"; return 0; fi
  tmpd=$(mktemp -d)
  update_sudoers_body "$user" > "$tmpd/024_xorgate-update-rs"
  if [ -f "$UPDATE_SUDOERS" ] && cmp -s "$tmpd/024_xorgate-update-rs" "$UPDATE_SUDOERS"; then
    echo "  = $UPDATE_SUDOERS unchanged"
  elif visudo -cf "$tmpd/024_xorgate-update-rs" >/dev/null 2>&1; then
    act install -m 0440 "$tmpd/024_xorgate-update-rs" "$UPDATE_SUDOERS"; note "$UPDATE_SUDOERS installed (visudo -cf ok; $user → systemctl start --no-block $UPDATER_UNIT)"
  else
    echo "  WARNING: 024 sudoers body failed visudo -cf — not installed; remote updates will not work"
  fi
  rm -rf "$tmpd"
  visudo -c >/dev/null 2>&1 && echo "  visudo -c: clean" || echo "  WARNING: visudo -c reports a problem in /etc/sudoers*"
}
# Phase 4 E8 clock break (ntp-unsync.sh) — review-wp6 M4. `break` records what
# it changed in NTP_STATE (persistent: `set-ntp false` survives a reboot too);
# `ntp_restore` (ntp-unsync.sh restore, exclusive.sh end, the dead-man) turns
# NTP back on ALWAYS and undoes the −secs step only when the clock was not
# stepped since (wall − record = uptime − record, ±5 s); a stepped clock (the
# stepper's E8 correction) is left to NTP. Idempotent: no record = set-ntp only.
NTP_STATE=$RS_HOME/ntp-unsync.state
ntp_restore() {
  local st=$NTP_STATE secs wall up now nowup drift
  act timedatectl set-ntp true || echo "  ! timedatectl set-ntp true failed"
  if [ ! -f "$st" ]; then echo "  ntp: set-ntp true (no ntp-unsync record)"; return 0; fi
  secs=$(sed -n 's/^secs=//p' "$st"); wall=$(sed -n 's/^wall=//p' "$st"); up=$(sed -n 's/^uptime=//p' "$st")
  now=$(date +%s); nowup=$(cut -d' ' -f1 /proc/uptime | cut -d. -f1)
  case "$secs$wall$up" in ''|*[!0-9]*) echo "  ntp: unreadable record $st — left to NTP" ;;
  *)
    if [ "$nowup" -lt "$up" ]; then echo "  ntp: rebooted since the break — clock left to NTP"
    else
      drift=$((now - (wall + nowup - up)))
      if [ "${drift#-}" -le 5 ]; then act date -u -s "@$((now + secs))" >/dev/null; echo "  ntp: clock untouched since the break (drift ${drift}s) → stepped back +${secs}s"
      else echo "  ntp: clock moved ${drift}s since the break (a step corrected it) → left to NTP"; fi
    fi ;;
  esac
  act rm -f "$st"; note "$st removed (NTP on)"
  return 0
}
# The bench flavour of the agent unit + wrapper: `xorgate-rs update` (E11)
# reinstalls the SHIPPED unit (with Conflicts=xorgate-agent.service) and the
# shipped wrapper (XORGATE_HOME=/var/lib/xorgate, the JS home). Starting the
# JS agent with that unit in place would stop the Rust one, and the CLI would
# edit the JS agent's settings.json — install.sh restores both.
bench_flavour_ok() {
  local ok=0
  if grep -q '^Conflicts=xorgate-agent.service' "/etc/systemd/system/$UNIT.service" 2>/dev/null; then
    echo "  UNSAFE: /etc/systemd/system/$UNIT.service carries Conflicts=xorgate-agent.service (an 'xorgate-rs update' reinstalled the shipped unit) — re-run scripts/bench/install.sh <tarball>"; ok=1
  fi
  if [ -f /usr/local/bin/xorgate-rs ] && ! grep -q 'XORGATE_HOME:-/var/lib/xorgate-rs}' /usr/local/bin/xorgate-rs; then
    echo "  UNSAFE: /usr/local/bin/xorgate-rs is not the bench wrapper (XORGATE_HOME default /var/lib/xorgate-rs) — re-run scripts/bench/install.sh <tarball>"; ok=1
  fi
  return $ok
}
# ---------------------------------------------------------------------------
# Phase 5 (WP3) additions — a CONVERSION window (install-rs.sh, plan
# phase-5-distribution.md §12, I6, CF64, CF65). Appended only.
RS_INSTALL_RECORD=/var/lib/xorgate-rs-install/record
RS_INSTALLER=/opt/xorgate-rs/install-rs.sh
RS_CONVERTED_REFUSAL="shipped (converted) unit installed — run scripts/bench/install.sh first; NOT starting $UNIT (it would stop the PROD JS agent)"
# True when the installed Rust unit — or any of its drop-ins (persistent or
# runtime) — carries a Conflicts= naming the JS unit: the SHIPPED unit that
# install-rs.sh (or `xorgate-rs update`) installs, not the bench flavour
# (install.sh strips the line). A superset of I6's `^Conflicts=xorgate-agent.service$`:
# any Conflicts= list naming xorgate-agent.service stops the PROD agent.
rs_unit_converted() {
  local f
  for f in "/etc/systemd/system/$UNIT.service" "$DROPIN_DIR"/*.conf "$RUN_DROPIN_DIR"/*.conf; do
    [ -f "$f" ] || continue
    if grep -Eq "^[[:space:]]*Conflicts[[:space:]]*=[[:space:]]*(.*[[:space:]])?$JS_UNIT\.service([[:space:]]|\$)" "$f"; then return 0; fi
  done
  return 1
}
# The conversion record's state (install-rs.sh I2): converted|converting|…;
# "none" without a record (never converted, or reverted: record.reverted-<ts>).
conversion_state() {
  local st=""
  [ -f "$RS_INSTALL_RECORD" ] && st=$(sed -n 's/^state=//p' "$RS_INSTALL_RECORD" 2>/dev/null | head -n1)
  echo "${st:-none}"
}
# Start the PROD JS agent and verify it is active. Never aborts the script
# (set -e safe): returns 1 after ~30 s of retries so the caller can shout.
# (Moved here from exclusive.sh in Phase 5 so the dead-man embeds the same body.)
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
# CF64 (I6): after a conversion the JS unit is DISABLED; giving PROD back means
# enable (only if `is-enabled` != enabled) + start + verify. An enable failure
# (masked, …) is reported and the start is still attempted. set -e safe.
enable_start_js_verified() {
  local en
  en=$(systemctl is-enabled "$JS_UNIT" 2>/dev/null || true)
  if [ "$en" != enabled ]; then
    if [ "$DRY" = 1 ]; then
      echo "  [dry-run] would: systemctl enable $JS_UNIT (is-enabled=${en:-unknown})"
    elif systemctl enable "$JS_UNIT"; then
      note "$JS_UNIT enabled (was ${en:-unknown})"
    else
      echo "!!!! systemctl enable $JS_UNIT FAILED (is-enabled=${en:-unknown}) — starting it anyway; fix by hand: sudo systemctl enable $JS_UNIT" >&2
    fi
  fi
  start_js_verified
}
LIB
}
