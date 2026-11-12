#!/usr/bin/env bash
# Switch the Rust agent's INPUT profile (plan decision O6), then daemon-reload
# and restart xorgate-agent-rs. Idempotent; prints what it changed.
#
#   scripts/bench/profile.sh shared     [--dry-run] [--no-restart]
#   scripts/bench/profile.sh exclusive  [--dry-run] [--no-restart]
#
#   shared    (default, persistent, MEDIA-OFF) /etc/systemd/system/xorgate-agent-rs.service.d/95-inputs.conf:
#             PATH=/var/lib/xorgate-rs/shared-shims:/usr/local/bin:/usr/bin:/bin (shims
#             first, no i2cdetect), GPS_NMEA_PORT=/run/xorgate-rs-no-gps,
#             XORGATE_PYTHON_DIR=/var/lib/xorgate-rs/no-python,
#             KVS_RUNTIME_BIN=/var/lib/xorgate-rs/no-kvs (must NOT exist: masters never start);
#             (re)creates the shims /var/lib/xorgate-rs/shared-shims/{rpicam-hello,gst-launch-1.0}
#             (root 0755; print "disabled in the shared profile", exit 1: no camera probe,
#             no capture — plan P3); re-writes /var/lib/xorgate-rs/etc/hardware.json with
#             "imu": null (no i2cdetect probe of bus 1); removes the runtime exclusive
#             drop-in + env file. REFUSES unless agent-rs-dev.env has
#             KVS_MANIFEST=/var/lib/xorgate-rs/etc/kvs.json (the channels-[] copy).
#             Phase 7a/7c pins: XORGATE_CAPTURE_BACKEND=gst-launch + XORGATE_CAPTURE_HOST_BIN
#             and XORGATE_IMU_BACKEND=python + XORGATE_IMU_READER_BIN, each program a
#             blocking shim (shared-shims/xorgate-capture, shared-shims/xorgate-imu).
#   exclusive (RUNTIME, media-on) /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf:
#             explicit full PATH (no shims), GPS_NMEA_PORT=/dev/ttyUSB1, XORGATE_PYTHON_DIR=/opt/xorgate-rs/python,
#             KVS_RUNTIME_BIN=/opt/xorgate-rs/media/xorgate-kvs-webrtc;
#             EnvironmentFile=/run/xorgate-agent-rs-exclusive.env → HARDWARE_MANIFEST=…/hardware-exclusive.json,
#             KVS_MANIFEST=…/kvs-exclusive.json (refuses if that is missing: dev-manifests.sh).
#             Vanishes on reboot. REFUSES while xorgate-agent (JS) is active — use
#             exclusive.sh begin, which stops it first.
#   Both refuse if /etc/xorgate/agent-rs-dev.env (an EnvironmentFile, which beats the
#   drop-ins) sets GPS_NMEA_PORT, XORGATE_PYTHON_DIR, PATH or KVS_RUNTIME_BIN (and the
#   Phase 4/7a/7c knobs: XORGATE_LED_*/SMS/OBD, XORGATE_CAPTURE_*, XORGATE_IMU_*).
# Never touches xorgate-agent.service, sim7600-lte or anything of the JS agent.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
MODE="${1:-}"; shift || true
DRY=0; RESTART=1
for a in "$@"; do
  case "$a" in --dry-run) DRY=1 ;; --no-restart) RESTART=0 ;; *) echo "unknown arg $a" >&2; exit 2 ;; esac
done
case "$MODE" in shared|exclusive) ;; *) sed -n '2,28p' "$0"; exit 2 ;; esac
log "profile $MODE on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY MODE=$MODE RESTART=$RESTART"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
[ -f /etc/systemd/system/$UNIT.service ] || { echo "$UNIT not installed — run scripts/bench/install.sh first"; exit 1; }
echo "== profile $MODE"
if [ "$MODE" = shared ]; then profile_shared; else profile_exclusive; fi
if [ "$RESTART" = 1 ]; then echo "== reload + restart $UNIT"; reload_restart_rs; else act systemctl daemon-reload; fi
# Phase 3 media knobs as the unit / the running process see them.
echo "  media env (unit): $(systemctl show $UNIT -p Environment --value | tr ' ' '\n' | grep -E '^(PATH|KVS_RUNTIME_BIN)=' | tr '\n' ' ' || true)"
MP=$(systemctl show $UNIT -p MainPID --value)
if [ -n "$MP" ] && [ "$MP" != 0 ] && [ -r "/proc/$MP/environ" ]; then
  echo "  media env (pid $MP): $(tr '\0' '\n' < "/proc/$MP/environ" | grep -E '^(PATH|KVS_RUNTIME_BIN|KVS_MANIFEST)=' | tr '\n' ' ' || true)"
fi
units_report
echo "== done: ${#CHANGED[@]} change(s)$([ "$DRY" = 1 ] && echo ' (dry run: nothing written)')"
REMOTE
} | bench_sudo_script
