#!/usr/bin/env bash
# Shared fixtures for scripts/bench/tests/*.sh (sourced; run as ROOT in a
# throwaway debian:bookworm container — these write to /etc, /var/lib, /run).
# Stubs ssh/sudo/systemctl/systemd-run/vcgencmd so the real remote bodies run
# locally. Unit state lives in $SYSD_STATE/<unit> ("active"/"inactive");
# $SYSD_STATE/<unit>.mainpid feeds `systemctl show -p MainPID`;
# `systemctl stop xorgate-agent` also kills the pids in $SYSD_STATE/js-children
# (the JS unit's capture/masters going down with it).
# Enablement (Phase 5) lives in $SYSD_STATE/<unit>.enabled ("enabled" when absent).
# shellcheck disable=SC2034  # variables are consumed by the sourcing tests
[ "$(id -u)" -eq 0 ] || { echo "run as root (in a container)"; exit 2; }
command -v python3 >/dev/null || { apt-get update -qq >/dev/null && apt-get install -y -qq python3 >/dev/null; }
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
B=$REPO/scripts/bench
STUB=$(mktemp -d); STATE=$(mktemp -d); LOGS=$(mktemp -d)
export SYSD_STATE=$STATE
cat > "$STUB/ssh" <<'S'
#!/usr/bin/env bash
# skip ssh options and the host, run the remote command locally
while [ $# -gt 0 ]; do case "$1" in -o|-i|-p|-F) shift 2;; -*) shift;; *) shift; break;; esac; done
if [ -n "${XORGATE_TEST_DUMP:-}" ]; then cat > "$XORGATE_TEST_DUMP"; exit 0; fi
exec "$@"
S
cat > "$STUB/sudo" <<'S'
#!/usr/bin/env bash
exec "$@"
S
cat > "$STUB/systemctl" <<'S'
#!/usr/bin/env bash
st="$SYSD_STATE"; cmd="$1"; shift || true
echo "systemctl $cmd $*" >> "$st/systemctl.log"
case "$cmd" in
  is-active) u="$1"; s=$(cat "$st/$u" 2>/dev/null || echo inactive); echo "$s"; [ "$s" = active ];;
  start|restart) u="$1"; [ -f "$st/FAIL_START_$u" ] && { echo "stub: start $u failed" >&2; exit 1; }; echo active > "$st/$u";;
  stop) u="$1"; echo inactive > "$st/$u"
        if [ "$u" = xorgate-agent ] && [ -f "$st/js-children" ]; then
          for p in $(cat "$st/js-children"); do kill -KILL "$p" 2>/dev/null || true; done; : > "$st/js-children"
        fi;;
  # Phase 5: enablement in $st/<unit>.enabled (absent = "enabled", the bench
  # default); `--now` also starts/stops; FAIL_ENABLE_/FAIL_DISABLE_<unit> make them fail.
  is-enabled) u="$1"; s=$(cat "$st/$u.enabled" 2>/dev/null || echo enabled); echo "$s"; [ "$s" = enabled ];;
  enable|disable) now=0; [ "${1:-}" = --now ] && { now=1; shift; }; u="$1"
        [ "$cmd" = enable ] && [ -f "$st/FAIL_ENABLE_$u" ] && { echo "stub: enable $u failed" >&2; exit 1; }
        [ "$cmd" = disable ] && [ -f "$st/FAIL_DISABLE_$u" ] && { echo "stub: disable $u failed" >&2; exit 1; }
        echo "${cmd}d" > "$st/$u.enabled"
        if [ "$now" = 1 ]; then if [ "$cmd" = enable ]; then echo active > "$st/$u"; else echo inactive > "$st/$u"; fi; fi;;
  daemon-reload) [ -f "$st/FAIL_RELOAD" ] && { echo "stub: daemon-reload failed" >&2; exit 1; }; true;;
  show) u="$1"; shift
        if [ "${1:-}" = -p ] && [ "${2:-}" = MainPID ]; then cat "$st/$u.mainpid" 2>/dev/null || echo 0
        # Phase 7c (review-wp3 F7): the effective Environment= (a fixture).
        elif [ "${1:-}" = -p ] && [ "${2:-}" = Environment ]; then cat "$st/$u.environment" 2>/dev/null || echo ""
        else echo ""; fi;;
  *) true;;
esac
S
cat > "$STUB/systemd-run" <<'S'
#!/usr/bin/env bash
# record the transient unit; do not run it (tests drive the scripts directly).
# With --on-active the unit is a TIMER (`<unit>.timer` active, like systemd);
# the command (first non-option argument on) goes to $SYSD_STATE/<unit>.cmd so
# a test can "fire" it.
echo "systemd-run $*" >> "$SYSD_STATE/systemd-run.log"
unit=""; timer=0; cmd=()
while [ $# -gt 0 ]; do
  case "$1" in
    --unit=*) unit=${1#--unit=}; shift ;;
    --on-active=*) timer=1; shift ;;
    --*) shift ;;
    *) cmd=("$@"); break ;;
  esac
done
if [ -n "$unit" ]; then
  if [ "$timer" = 1 ]; then echo active > "$SYSD_STATE/$unit.timer"; else echo active > "$SYSD_STATE/$unit"; fi
  printf '%q ' "${cmd[@]}" > "$SYSD_STATE/$unit.cmd"
fi
S
cat > "$STUB/vcgencmd" <<'S'
#!/usr/bin/env bash
echo "throttled=0x0"
S
chmod +x "$STUB"/*
export PATH="$STUB:$PATH" BENCH=stub@localhost
id pi >/dev/null 2>&1 || useradd -M pi
fail=0
pass() { echo "PASS: $*"; }
failc() { echo "FAIL: $*"; fail=1; }
expect() { # <0|1 condition-result> <msg>
  if [ "$1" = 0 ]; then pass "$2"; else failc "$2"; fi
}
js_state() { cat "$STATE/xorgate-agent" 2>/dev/null || echo inactive; }
reset_units() {
  rm -f "$STATE"/*
  echo active > "$STATE/xorgate-agent"; echo active > "$STATE/xorgate-agent-rs"; echo active > "$STATE/sim7600-lte"
}
DEV_ID=a73f7a36-0000-4000-8000-000000000001
DEV_BACKUP=/etc/xorgate/dev-backup-20260823
RS_ETC=/var/lib/xorgate-rs/etc
RS_ENV=/etc/xorgate/agent-rs-dev.env
# A bench as install.sh leaves it (the Rust unit + env file; prod files present).
fixture_bench() {
  rm -rf /etc/systemd/system/xorgate-agent-rs.service.d /run/systemd/system/xorgate-agent-rs.service.d \
         /var/lib/xorgate-rs /run/xorgate-rs-lte /run/xorgate-rs-gate-seam.* /run/xorgate-rs-soak /run/xorgate-agent-rs-exclusive.env
  mkdir -p /etc/systemd/system /etc/xorgate/pipelines "$DEV_BACKUP" /var/lib/xorgate/shm /run/systemd/system /run/sim7600-lte
  install -d -o pi -g pi -m 0700 /var/lib/xorgate-rs
  install -d -o pi -g pi -m 0755 /var/lib/xorgate-rs/evidence
  touch /etc/systemd/system/xorgate-agent-rs.service
  # install.sh's bench drop-in (DEV identity + own state dir); Phase 6 Q6:
  # every Rust (re)start refuses without it.
  mkdir -p /etc/systemd/system/xorgate-agent-rs.service.d
  printf '[Unit]\nConflicts=\n[Service]\nEnvironmentFile=\nEnvironmentFile=/etc/xorgate/agent-rs-dev.env\n' > /etc/systemd/system/xorgate-agent-rs.service.d/90-bench-dev.conf
  echo "DEVICE_ID=58b51737-0000-4000-8000-000000000002" > /etc/xorgate/agent.env
  echo '{"deviceId":"58b51737","region":"us-east-1","credentialEndpoint":"c.example","roleAlias":"r","channels":[{"streamKey":"cam0","channelName":"p0"}]}' > /etc/xorgate/kvs.json
  echo '{"v":2,"imu":"bno085","cameras":{}}' > /etc/xorgate/hardware.json
  printf 'libcamerasrc ! shmsink socket-path=/var/lib/xorgate/shm/cam0.sock\n' > /etc/xorgate/pipelines/cam0.gst
  printf 'libcamerasrc ! shmsink socket-path=/var/lib/xorgate/shm/cam0.sock direct\n' > /etc/xorgate/pipelines/cam0.gst.direct
  printf 'imx290\n' > /etc/xorgate/pipelines/cam0.camera
  cat > "$DEV_BACKUP/kvs.json" <<J
{"deviceId": "$DEV_ID", "region": "us-east-1", "credentialEndpoint": "creds.example", "roleAlias": "kvs-role", "channels": [{"streamKey": "cam0", "channelName": "dev-cam0"}, {"streamKey": "cam1", "channelName": "dev-cam1"}], "uploadBucket": "dev-bucket"}
J
  cat > "$RS_ENV" <<E
# fixture env file (install.sh)
DEVICE_ID=$DEV_ID
CERT_PATH=$DEV_BACKUP/device.cert.pem
KEY_PATH=$DEV_BACKUP/device.key.pem
CA_PATH=/etc/xorgate/root-ca.pem
XORGATE_HOME=/var/lib/xorgate-rs
KVS_RUNTIME_BIN=/opt/xorgate/media/xorgate-kvs-webrtc
KVS_RUNTIME_LIB_DIR=/opt/xorgate/media/lib
KVS_CACERT_PATH=/opt/xorgate/media/cert.pem
E
  chown pi:pi "$RS_ENV"; chmod 0600 "$RS_ENV"
  echo '{"pid": 0, "latest": {"metrics": {"system": {}}, "media": {"kvs": {"value": true}, "streams": [{"streamKey": "cam0", "channelName": "p0", "state": "running"}]}}}' > /var/lib/xorgate/state.json
  echo '{"timestamp": "2026-10-10T00:00:00Z", "status": "connected", "reg": 1, "rssi": -70}' > /run/sim7600-lte/telemetry.json
}
# Phase 2 leftovers in the Rust etc dir: copies of the JS renders + one Rust render.
fixture_phase2_copies() {
  install -d -o pi -g pi -m 0700 "$RS_ETC" "$RS_ETC/pipelines"
  cp /etc/xorgate/pipelines/cam0.gst /etc/xorgate/pipelines/cam0.camera "$RS_ETC/pipelines/"
  printf 'libcamerasrc ! shmsink socket-path=/var/lib/xorgate/shm/cam1.sock\n' > "$RS_ETC/pipelines/cam1.gst"   # an old JS copy
  printf 'libcamerasrc ! shmsink socket-path=/var/lib/xorgate-rs/shm/cam0.sock\n' > "$RS_ETC/pipelines/cam0.gst.direct"  # a Rust render
}
# Snapshot of everything the scripts could touch (path, type, size, mtime, mode, owner + file sha256).
snapshot() {
  find /etc /var/lib/xorgate /var/lib/xorgate-rs /run /dev/shm -xdev \( -type f -o -type d -o -type l \) \
    ! -path "$STATE*" ! -path "$STUB*" ! -path "$LOGS*" -printf '%p %y %s %T@ %m %u\n' 2>/dev/null | sort
  find /etc /var/lib/xorgate /var/lib/xorgate-rs /run /dev/shm -xdev -type f ! -path "$STATE*" ! -path "$STUB*" ! -path "$LOGS*" \
    -exec sha256sum {} + 2>/dev/null | sort -k2
}
