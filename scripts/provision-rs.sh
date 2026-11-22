#!/usr/bin/env bash
# provision-rs.sh: the NATIVE Rust-agent installer (plans/device-agent-js-to-rust
# phase-8-dual-agent-registration.md D1.2-D1.4; runbook docs/provision.md).
#
# It provisions a clean Raspberry Pi with the Rust agent the way the backend's
# JS install.sh provisions one with the JS agent: the same system steps (apt,
# IMU, claim, /etc/xorgate, sudoers, logind, modem, uplink, GStreamer, KVS
# manifest, pipelines, overlays), but no Node, no npm and no /opt/xorgate. It
# ships INSIDE the Rust tarball, so the install layout is versioned with the
# agent. The backend serves only a thin bootstrap (install-script-rs.ts) that
# downloads + verifies the tarball and runs this file from it (I-P8-1):
#
#   provision-rs.sh --reg-code <c> --api-base <url> --from <bucket> --tarball <f> [--cleanup <dir>] [--reprovision]
#   provision-rs.sh --uninstall
#   provision-rs.sh -h | --help
#
# Order (the D1.4 table; the JS script claims BEFORE it downloads, this one
# never claims until everything the binary needs is installed and the staged
# binary has run): P0 preconditions -> P1 hand-off + checksum -> P1b
# ModemManager off + stale usb0 default routes -> 1 apt -> 1b IMU -> 1c GStreamer ->
# P2 stage + smoke-exec -> 2 claim -> 3 /etc/xorgate -> 4 swap -> 5b wrapper
# -> 6 units, sudoers, helpers, updater, logind -> 7 modem -> 7b uplink ->
# 8 kvs.json, hardware.json, pipelines -> 8b overlays -> P9 enable + start +
# wait <= 90 s for active.
#
# Every system file is written cmp-before-write (identical bytes + mode +
# owner = not touched), with `install` (a new inode: a running script is never
# rewritten in place), and a daemon is reloaded/restarted only when one of its
# files changed. An active sim7600-lte daemon is never restarted.
#
# Exit codes (D1.2):
#   0 ok
#   1 pre-claim artifact or dependency failure; nothing claimed (the code is still usable)
#   2 usage
#   3 precondition refused; nothing changed
#   4 claimed, but the agent did not become active (recovery printed)
#   5 claim refused (e.g. 409 duplicate serial); only the dependencies installed
set -euo pipefail
umask 022

CONF_DIR=/etc/xorgate
JS_PREFIX=/opt/xorgate
RS_PREFIX=/opt/xorgate-rs
STAGING=/opt/xorgate-rs.new
BACKUP=/opt/xorgate-rs.old
SYSD=/etc/systemd/system
JS_UNIT=$SYSD/xorgate-agent.service
RS_SVC=xorgate-agent-rs
RS_UNIT=$SYSD/$RS_SVC.service
RS_UNIT_D=$RS_UNIT.d
UPD_UNIT=$SYSD/xorgate-updater-rs.service
UPD_UNIT_D=$UPD_UNIT.d
WRAPPER=/usr/local/bin/xorgate-rs
GENERATOR=/etc/systemd/system-generators/xorgate-agent-rs-native
SUDOERS_D=/etc/sudoers.d
SUDOERS_024=$SUDOERS_D/024_xorgate-update-rs
# The camera-line owner (Rust-only): the helper, its overlay allow-list, 025.
CAMCFG_BIN=/usr/local/bin/xorgate-camcfg
CAMCFG_LIST=/usr/local/bin/xorgate-camcfg.overlays
SUDOERS_025=$SUDOERS_D/025_xorgate-camcfg
STATE_DIR=/var/lib/xorgate
CONVERT_RECORD=/var/lib/xorgate-rs-install/record
LOCK=/run/xorgate-provision-rs.lock
# review-wp-r1 N2: the work after the pre-claim refusals runs as this
# transient unit, so a dropped ssh session cannot cut a run in half. The log
# is append-only (a header per run); each run's exit code is PROV_RC.<run id>.
PROV_UNIT=xorgate-provision-rs
PROV_LOG=/var/log/xorgate-provision-rs.log
PROV_RC=/run/xorgate-provision-rs.rc
# What a staged build must carry (install-rs.sh REQUIRED_MEMBERS + this file).
REQUIRED_MEMBERS=(bin/xorgate-rs build-info.json systemd/xorgate-agent-rs.service systemd/xorgate-updater-rs.service
  install-rs.sh provision-rs.sh)
# update_plan::SMOKE_PREFIX.
SMOKE_PREFIX="xorgate-rs "
# P9: how long the agent gets to report READY (Type=notify).
START_WAIT_SECS=90
# 1c: the packages providing the binary's NEEDED libraries (readelf -d on the
# release binary: libgstreamer-1.0.so.0, libglib-2.0.so.0, libgcc_s.so.1,
# libm.so.6, libc.so.6; the last three come from the Essential libc6 +
# libgcc-s1). A failure here is fatal (exit 1, nothing claimed): the binary
# cannot even start without them. Everything else in the JS GStreamer set only
# WARNs, as in JS (D1.4, review N3). DA8 re-derives this list with ldd.
APT_NEEDED=(libglib2.0-0 libgstreamer1.0-0)
APT_GST_PLUGINS=(libgstreamer-plugins-base1.0-0 gstreamer1.0-plugins-base gstreamer1.0-plugins-good
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-tools)

# ---------------------------------------------------------------------------
# Bodies. Pinned byte for byte by fixtures/install_rs/bodies.json, which
# crates/xorgate-core/tests/install_rs_bodies.rs ties to update_plan (so the
# first `xorgate-rs update` rewrites nothing) and scripts/tests/provision-rs-bodies.sh
# ties to these functions and to the JS install.sh golden (the shared steps
# stay JS's bytes).

# update_plan::WRAPPER_BODY (= install-rs.sh rs_wrapper_body). No `xorgate` alias (Q-8.7).
rs_wrapper_body() {
  # shellcheck disable=SC2016  # the $ belong to the wrapper, written literally
  printf '%s\n' '#!/usr/bin/env bash' \
    'exec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" /opt/xorgate-rs/bin/xorgate-rs "$@"'
}
# update_plan::updater_sudoers_body(<user>) (= install-rs.sh rs_update_sudoers_body).
rs_update_sudoers_body() {
  printf '%s\n' \
    "# Installed by xorgate-rs. Allows the agent (running as $1) to start the" \
    "# one-shot self-update unit on a cloud-issued 'update' command, and nothing" \
    "# else. See device-agent-rust crates/xorgate-hal/src/privileged.rs + systemd/xorgate-updater-rs.service." \
    "$1 ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater-rs.service"
}
# update_plan::clock_sudoers_body(<user>) (= the JS install.sh 021 body).
clock_sudoers_body() {
  printf '%s\n' \
    "# Installed by xorgate. Allows the agent (running as $1) to apply an" \
    "# offline GPS/NITZ clock step. See device-agent src/agent/clock-step.ts." \
    "$1 ALL=(root) NOPASSWD: /usr/local/bin/xorgate-setclock"
}
# update_plan::camrotate_sudoers_body(<user>) (= the JS install.sh 023 body).
camrotate_sudoers_body() {
  printf '%s\n' \
    "# Installed by xorgate. Allows the agent (running as $1) to set the" \
    "# camera overlay's rotation parameter in the boot config, and nothing else." \
    "# See device-agent src/hardware/boot-config.ts + system/xorgate-camrotate." \
    "$1 ALL=(root) NOPASSWD: /usr/local/bin/xorgate-camrotate"
}
# update_plan::camcfg_sudoers_body(<user>) (= install-rs.sh rs_camcfg_sudoers_body).
# Rust-only: the camera-line owner, system/xorgate-camcfg (sudoers 025).
rs_camcfg_sudoers_body() {
  printf '%s\n' \
    "# Installed by xorgate-rs. Allows the agent (running as $1) to change the" \
    "# camera overlay lines of the boot config through the camera-line owner, and" \
    "# nothing else. See device-agent-rust system/xorgate-camcfg + crates/xorgate-core/src/boot_config.rs." \
    "$1 ALL=(root) NOPASSWD: /usr/local/bin/xorgate-camcfg"
}
# The JS install.sh 020 body.
power_sudoers_body() {
  printf '%s\n' \
    "# Installed by xorgate. Allows the agent (running as $1) to act on" \
    "# cloud-issued reboot / power-off commands. See device-agent src/agent/commands.ts." \
    "$1 ALL=(root) NOPASSWD: /usr/bin/systemctl reboot, /usr/bin/systemctl poweroff"
}
# The JS install.sh 10-user.conf body: <user> <group>.
user_dropin_body() {
  printf '%s\n' '[Service]' "User=$1" "Group=$2"
}
# D1.3: a native device only. When the JS installer later provisions this
# device, the JS agent wins cleanly from the next boot.
native_dropin_body() {
  printf '%s\n' \
    '# Installed by provision-rs.sh (a native Rust install). If the JS installer' \
    '# later provisions this device, /opt/xorgate appears and this unit no longer' \
    '# starts from the next boot: the JS agent alone runs. Remove the then-inert' \
    '# Rust files with: sudo /opt/xorgate-rs/provision-rs.sh --uninstall' \
    '[Unit]' \
    'ConditionPathExists=!/opt/xorgate'
}
# D1.3 (WP-R1 finding): the boot-time enablement of a NATIVE install. A plain
# `systemctl enable` cannot let the JS installer win: with both units enabled
# the boot transaction holds both start jobs, the Rust unit's Conflicts= adds
# a JS stop job that matters to the anchor while JS's own Wants=-pulled start
# does not, so systemd drops the JS start; 20-native.conf then skips Rust and
# NO agent runs. This generator decides before any transaction: it wants the
# Rust unit only while /opt/xorgate is absent (systemctl is-enabled: generated).
native_generator_body() {
  # shellcheck disable=SC2016  # the $1 belongs to the generator, written literally
  printf '%s\n' \
    '#!/bin/sh' \
    '# Installed by provision-rs.sh (a native Rust install). Wants xorgate-agent-rs' \
    '# at boot only while the JS agent is not installed (/opt/xorgate absent), so' \
    '# a later JS install wins cleanly from the next boot. Remove with:' \
    '# sudo /opt/xorgate-rs/provision-rs.sh --uninstall' \
    '[ -n "$1" ] || exit 0' \
    '[ -e /opt/xorgate ] && exit 0' \
    'mkdir -p "$1/multi-user.target.wants" || exit 0' \
    'ln -sf /etc/systemd/system/xorgate-agent-rs.service "$1/multi-user.target.wants/xorgate-agent-rs.service"'
}
# The updater follows xorgate-agent-rs-latest (README §6; agent.env says the same).
native_dist_dropin_body() {
  printf '%s\n' \
    '# Installed by provision-rs.sh. The Rust updater always follows' \
    '# xorgate-agent-rs-latest (README §6: the cloud update stays latest-only).' \
    '[Service]' \
    'Environment=DEVICE_DIST_VERSION=latest'
}
# The JS install.sh logind drop-in body.
logind_body() {
  printf '%s\n' \
    '# Installed by xorgate. Bridged camera pipelines publish frames through POSIX' \
    '# shared memory owned by the agent user; logind must not remove it when the' \
    '# last ssh session closes.' \
    '[Login]' \
    'RemoveIPC=no'
}
# The JS install.sh NM uplink-priority body, with the Rust fork's LTE metric
# (v0.1.1, F-LTE-METRIC: the Rust sim7600-lte.sh defaults to 1000, above NM's
# wired 100; the JS copy keeps 100). The settings are byte-equal to JS's; only
# the comment's number differs (pinned by provision-rs-bodies.sh §3).
uplink_body() {
  printf '%s\n' \
    '# Prefer WiFi over the LTE modem (usb0) for the IPv4 default route.' \
    '# WiFi connections get route-metric 50; usb0 is unmanaged by NM and the' \
    '# sim7600-lte daemon installs its default route at metric 1000, so WiFi wins' \
    '# whenever it is associated. LTE stays the fallback, used only when WiFi is' \
    '# down or absent. Interface-bound health checks in the SIM7600 manager' \
    '# (ping -I usb0) are unaffected by this.' \
    '[connection-wifi-priority]' \
    'match-device=type:wifi' \
    'ipv4.route-metric=50'
}
# The JS install.sh apt lock-timeout body.
apt_lock_body() {
  printf '%s\n' 'DPkg::Lock::Timeout "600";'
}
# agent.env: the JS install.sh heredoc with the Rust media root, the Rust CLI
# name in the comments (review N6) and DEVICE_DIST_VERSION=latest LITERALLY:
# I-P8-1 passes no version, and deriving one from the tarball name would pin
# every native device to its install build (review-wp-b S2). The JS script
# writes the `latest` the backend renders, so the result is the same.
# <device id> <endpoint> <bucket url>
agent_env_body() {
  cat <<EOF
DEVICE_ID=$1
IOT_DATA_ENDPOINT=$2
CERT_PATH=$CONF_DIR/device.cert.pem
KEY_PATH=$CONF_DIR/device.key.pem
CA_PATH=$CONF_DIR/root-ca.pem
MODE=real
# KVS WebRTC media runtime (supervised by the agent's media collector; it reuses
# the IoT cert above for KVS creds — channels/endpoints live in kvs.json). On for
# every provisioned device; turn a camera off with
# 'xorgate-rs set recording.video.cameras.<streamKey> off' or from the web UI.
# (No backticks in this heredoc: its delimiter is unquoted, so a backtick
# would command-substitute at install time and mangle the written file.)
KVS_RUNTIME_BIN=$RS_PREFIX/media/xorgate-kvs-webrtc
KVS_RUNTIME_LIB_DIR=$RS_PREFIX/media/lib
KVS_MANIFEST=$CONF_DIR/kvs.json
# Device hardware block (cm4-support): written in step 8 when the claim
# resolves a v2 device model; the agent falls back to legacy CM5 behavior
# when the file is absent.
HARDWARE_MANIFEST=$CONF_DIR/hardware.json
# Per-stream GStreamer pipelines: drop <streamKey>.gst here (e.g. cam0.gst), or
# manage them with 'xorgate-rs pipeline set <streamKey> <file>'.
KVS_PIPELINE_DIR=$CONF_DIR/pipelines
# Where 'xorgate-rs update' pulls new builds from.
DEVICE_DIST_BUCKET_URL=$3
DEVICE_DIST_VERSION=latest
EOF
}

# JS step 8's legacy pipeline writer, VERBATIM (pinned against the JS golden by
# scripts/tests/provision-rs-bodies.sh). Reads CONF_DIR, SVC_USER, SVC_GROUP.
XORGATE_SHM_DIR=/var/lib/xorgate/shm
write_bridged_pipeline() {
  key="$1"; camera_name="$2"
  [ -f "$CONF_DIR/pipelines/$key.gst" ] && return 0
  install -m 0644 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/pipelines/$key.gst" <<GST
shmsrc socket-path=$XORGATE_SHM_DIR/$key.sock is-live=true do-timestamp=true ! video/x-raw,format=I420,width=1280,height=720,framerate=15/1 ! x264enc name=sampleVideoEncoder bframes=0 speed-preset=ultrafast bitrate=1000 byte-stream=TRUE tune=zerolatency threads=2 ! video/x-h264,stream-format=byte-stream,alignment=au,profile=baseline ! appsink sync=TRUE emit-signals=TRUE name=appsink-video
GST
  install -m 0644 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/pipelines/$key.camera" <<CAM
$camera_name
CAM
  install -m 0644 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/pipelines/$key.gst.direct" <<GST
libcamerasrc name=$key camera-name=$camera_name ! video/x-raw,format=I420,width=1280,height=720,framerate=15/1 ! x264enc bframes=0 speed-preset=ultrafast bitrate=1000 byte-stream=TRUE tune=zerolatency threads=2 ! video/x-h264,stream-format=byte-stream,alignment=au,profile=baseline ! appsink sync=TRUE emit-signals=TRUE name=appsink-video
GST
}

usage() {
  cat <<'EOF'
provision-rs.sh: provision this device with the native Rust xorgate agent.

  provision-rs.sh --reg-code <c> --api-base <url> --from <bucket> --tarball <f> [--cleanup <dir>] [--reprovision]
      Normally run by the bootstrap the console's "Register device" (agent: Rust)
      serves:  curl -s <install url> -o install.sh && sudo bash install.sh
      --reg-code     the registration code (claimed only after every dependency
                     is installed and the staged binary has run)
      --api-base     the API base, ending in /v1 (https only)
      --from         the dist bucket URL (written to agent.env for xorgate-rs update)
      --tarball      the verified xorgate-agent-rs-<v>.tar.gz; <f>.sha256 is REQUIRED
      --cleanup      the bootstrap's temp dir, removed when this run ends
      --reprovision  re-register a device that already runs the Rust agent: new
                     /etc/xorgate identity, the build swapped, the agent restarted,
                     /var/lib/xorgate/config-sync.json deleted
  sudo /opt/xorgate-rs/provision-rs.sh --uninstall
      After the JS installer took this device over: disable the Rust units and
      remove the Rust tree, units, drop-ins, wrapper, sudoers 024 and the
      camera-line owner (xorgate-camcfg, its allow-list, sudoers 025). Never
      touches /etc/xorgate, /var/lib/xorgate, the modem/system assets or 020-023.

Exit codes: 0 ok; 1 pre-claim failure, nothing claimed; 2 usage; 3 refused,
nothing changed; 4 claimed but the agent is not active (recovery printed);
5 claim refused (code unused or already used), only dependencies installed.
Runbook: docs/provision.md in the agent repo.
EOF
}

# ---------------------------------------------------------------------------
# Output, failure handling.
log()  { echo "[xorgate] $*"; }
warn() { echo "[xorgate] WARN: $*"; }
PHASE=pre          # pre (nothing claimed) | claiming (the POST in flight: state unknown) | claimed
HANDED_OFF=0       # 1 = the transient unit owns the run (N2)
FINISHED=0
PRIV=""            # private 0700 scratch (claim response, drafts)
CLEANUP=""
DEVICE_ID=""
STAGED=0
fail() { # <code> <message>
  echo "[xorgate] ERROR: $2" >&2
  FINISHED=1
  exit "$1"
}
refuse() { fail 3 "$1 (nothing changed)"; }
usage_error() { echo "[xorgate] $1" >&2; echo "[xorgate] see: provision-rs.sh --help" >&2; FINISHED=1; exit 2; }
recovery() {
  echo "[xorgate] RECOVERY: the registration code is USED: device ${DEVICE_ID:-?} exists in the cloud." >&2
  echo "[xorgate]   1. fix the cause above (journal: sudo journalctl -u $RS_SVC -n 100)" >&2
  echo "[xorgate]   2. delete that device in the console (its serial blocks a new claim in the workspace)" >&2
  echo "[xorgate]   3. register again (agent: Rust) and run: sudo bash install.sh --reprovision" >&2
}
on_exit() {
  local rc=$?
  set +e
  trap - EXIT
  if [ "$HANDED_OFF" = 1 ]; then   # the unit owns the staging, the cleanup dir and the outcome
    [ -z "$PRIV" ] || rm -rf "$PRIV"
    exit "$rc"
  fi
  if [ "$FINISHED" != 1 ] && [ "$rc" != 0 ]; then
    echo "[xorgate] ERROR: unexpected failure (exit $rc)" >&2
    case $PHASE in claimed) rc=4 ;; claiming) rc=5 ;; *) rc=1 ;; esac
  fi
  if [ "$PHASE" != claimed ] && [ "$STAGED" = 1 ]; then rm -rf "$STAGING"; fi
  if [ "$PHASE" = pre ] && [ "$rc" = 1 ]; then echo "[xorgate] nothing was claimed: the registration code is still usable" >&2; fi
  if [ "$PHASE" = claiming ]; then
    echo "[xorgate] claim state UNKNOWN: the claim request may have reached the API. If the console lists this device, delete it and register again; otherwise run the same command again" >&2
  fi
  if [ "$PHASE" = claimed ] && [ "$rc" = 4 ]; then recovery; fi
  [ -z "$PRIV" ] || rm -rf "$PRIV"
  [ -z "$CLEANUP" ] || rm -rf "$CLEANUP"
  if [ -n "${XORGATE_PROVISION_RCFILE:-}" ]; then
    echo "[xorgate] (provisioning unit) exit code $rc"
    echo "$rc" > "$XORGATE_PROVISION_RCFILE" 2>/dev/null
  fi
  exit "$rc"
}
on_signal() { trap '' HUP INT TERM; echo "[xorgate] interrupted" >&2; exit "$1"; }

# ---------------------------------------------------------------------------
# File helpers: cmp-before-write; `install` replaces by unlink (new inode).
# put_file <dest> <mode> <owner:group> <src>: returns 0 when
# it wrote, returns 1 when the file already had these bytes, mode and owner.
put_file() {
  local dest=$1 mode=$2 own=$3 src=$4 want
  want="${mode#0} $(id -u "${own%%:*}"):$(getent group "${own#*:}" | cut -d: -f3)"
  if [ -f "$dest" ] && [ ! -L "$dest" ] && cmp -s "$src" "$dest" && [ "$(stat -c '%a %u:%g' "$dest")" = "$want" ]; then
    log "  = $dest unchanged"
    return 1
  fi
  if [ -L "$dest" ]; then rm -f "$dest"; fi   # never write through a symlink
  if [ -e "$dest" ] && [ ! -f "$dest" ]; then fail "$([ "$PHASE" = claimed ] && echo 4 || echo 1)" "$dest is in the way (not a regular file)"; fi
  install -m "$mode" -o "${own%%:*}" -g "${own#*:}" "$src" "$dest" \
    || fail "$([ "$PHASE" = claimed ] && echo 4 || echo 1)" "cannot write $dest"
  log "  - $dest written"
  return 0
}
# put_body <dest> <mode> <owner:group> <body function> [args]
put_body() {
  local dest=$1 mode=$2 own=$3 tmp rc=0
  shift 3
  tmp=$(mktemp "$PRIV/body.XXXXXX") || fail "$([ "$PHASE" = claimed ] && echo 4 || echo 1)" "mktemp failed"
  "$@" > "$tmp" || fail "$([ "$PHASE" = claimed ] && echo 4 || echo 1)" "cannot render the body of $dest"
  put_file "$dest" "$mode" "$own" "$tmp" || rc=$?
  rm -f "$tmp"
  return "$rc"
}
# put_sudoers <dest> <warning> <body function> <user>: visudo -cf the draft first.
put_sudoers() {
  local dest=$1 warning=$2 fn=$3 user=$4 tmp
  tmp=$(mktemp "$PRIV/sudoers.XXXXXX")
  "$fn" "$user" > "$tmp"
  chmod 0440 "$tmp"
  if visudo -cf "$tmp" >/dev/null 2>&1; then
    put_file "$dest" 0440 root:root "$tmp" || true
  else
    warn "$warning"
  fi
  rm -f "$tmp"
}
ensure_dir() { # <dir> <mode> <owner:group>: created when absent; an existing dir is left as it is
  if [ -d "$1" ] && [ ! -L "$1" ]; then return 0; fi
  if [ -e "$1" ] || [ -L "$1" ]; then fail "$([ "$PHASE" = claimed ] && echo 4 || echo 1)" "$1 is in the way (not a directory)"; fi
  install -d -m "$2" -o "${3%%:*}" -g "${3#*:}" "$1" || fail "$([ "$PHASE" = claimed ] && echo 4 || echo 1)" "cannot create $1"
  log "  - $1/ created"
}
in_group() { id -nG "$1" 2>/dev/null | tr ' ' '\n' | grep -qx "$2"; }
add_group() { # <user> <group>
  [ "$1" != root ] || return 0
  getent group "$2" >/dev/null 2>&1 || return 0
  if in_group "$1" "$2"; then log "  = $1 already in group $2"; return 0; fi
  usermod -aG "$2" "$1" || warn "could not add $1 to the $2 group"
}
unit_active() { local o; o=$(systemctl is-active "$1" 2>/dev/null) || true; printf '%s' "${o%%$'\n'*}"; }
unit_enabled() { local o; o=$(systemctl is-enabled "$1" 2>/dev/null) || true; printf '%s' "${o%%$'\n'*}"; }

# ModemManager claims the SIM7600's AT port and fights the sim7600-lte daemon
# for it (JS step 7). Since v0.1.1 it is also turned off at P1b, before the
# first download, when a SIM7600 is present (review-v011-wp-a SHOULD-4: a box
# whose cellular uplink is some other MM-managed modem keeps it until step 7,
# as before), so it can no longer claim the modem in the middle of the run and
# strand usb0's routes (F-DA21-NM). Step 7 still turns it off unconditionally.
disable_modemmanager() {
  case $(unit_enabled ModemManager.service)/$(unit_active ModemManager.service) in
    enabled/* | */active | */activating) systemctl disable --now ModemManager.service 2>/dev/null || true; log "  - ModemManager disabled" ;;
  esac
}

# F-DA21-NM (v0.1.1). On a fresh Pi OS boot NetworkManager auto-connects the
# SIM7600's usb0 (RNDIS) and takes the IPv4 AND the IPv6 default at metric
# 100. When ModemManager then claims the modem, NM drops usb0 from its device
# list but LEAVES usb0's address and both default routes behind; with no data
# they are black holes. In DA21 apt and pip stalled on AAAA mirrors for about
# 4 minutes of the registration code's 15. P1b runs this after ModemManager is
# off and before the first apt call: it deletes usb0's IPv4 and IPv6 default
# routes ONLY when all of these hold (review-v011-wp-a MUST-1: on an
# LTE-only device the usb0 default is the LIVE uplink the bootstrap's
# download just used, and deleting it would cut the device off):
#   1. nothing live owns them:
#      - no running dhclient has usb0 among its arguments (the sim7600-lte
#        daemon's own, or an ifupdown one);
#      - sim7600-lte.service is not active, activating or reloading (e.g. a
#        --reprovision, or the daemon in its auto-restart);
#      - NetworkManager does not list usb0 as a device it manages (absent, or
#        `unmanaged`, e.g. after the bootstrap's 0a conf: NM leaves an
#        unmanaged device's routes alone, so they are nobody's);
#   2. ANOTHER interface holds an IPv4 default (with none, deleting usb0's can
#      never help: apt has no route either way);
#   3. usb0 itself does NOT reach the internet: a usb0-bound probe fails
#      TWICE, 2 s apart (a modem still settling is not judged dead). One
#      probe = `ping -I usb0` to 1.1.1.1 / 8.8.8.8 (3 s each), then
#      `curl --interface usb0 http://1.1.1.1/` (5 s; an IP literal, so no DNS
#      lookup over the other, possibly dead, uplink; any HTTP answer counts,
#      so a carrier that drops ICMP still passes). A working usb0 is kept
#      even beside another default: the daemon's metric orders them from
#      step 7 on.
# Then IPv6 is turned off on usb0 (`disable_ipv6`, runtime only, exactly what
# the daemon's ipv6_off_iface does at every connect; LTE is IPv4 by design) so
# a modem RA cannot re-add the v6 black hole while apt runs (SHOULD-1).
# Nothing else is touched: no IPv4 address, no other interface's route (every
# delete names `dev usb0`, which the kernel matches), no NM reload.
# The bootstrap's own download (and the console's line 1) run BEFORE this and
# can still meet the black hole: docs/provision.md "Fresh image with a SIM7600".
LTE_IFACE=usb0
LTE_PROBE_HOSTS="1.1.1.1 8.8.8.8"
LTE_PROBE_URL="http://1.1.1.1/"   # fix-check SHOULD-5: an IP literal (DNS-independent)
LTE_PROBE_RETRY_S=2
# A SIM7600 is fitted: its RNDIS netdev exists, or a SIMCom USB device
# (vendor 1e0e) is enumerated.
sim7600_present() {
  local v
  if command -v ip >/dev/null 2>&1 && ip link show "$LTE_IFACE" >/dev/null 2>&1; then return 0; fi
  for v in /sys/bus/usb/devices/*/idVendor; do
    [ -r "$v" ] || continue
    [ "$(cat "$v" 2>/dev/null)" = 1e0e ] && return 0
  done
  return 1
}
# usb0 reaches the internet (a bound probe, retried once; see the rule 3 above).
lte_iface_probe() {
  local h
  if command -v ping >/dev/null 2>&1; then
    for h in $LTE_PROBE_HOSTS; do
      ping -c 1 -W 3 -I "$LTE_IFACE" "$h" >/dev/null 2>&1 && return 0
    done
  fi
  if command -v curl >/dev/null 2>&1; then
    curl -q -s -o /dev/null -m 5 --interface "$LTE_IFACE" "$LTE_PROBE_URL" 2>/dev/null && return 0
  fi
  return 1
}
lte_iface_works() {
  lte_iface_probe && return 0
  sleep "$LTE_PROBE_RETRY_S"
  lte_iface_probe
}
lte_iface_owner() { # -> the live owner of usb0's routes, or nothing
  local d c a st
  for d in /proc/[0-9]*; do
    { read -r c < "$d/comm"; } 2>/dev/null || continue
    [ "$c" = dhclient ] || continue
    # (the process may exit at any point: a failed read is not an owner)
    { while IFS= read -r -d '' a; do
        if [ "$a" = "$LTE_IFACE" ]; then echo "dhclient (pid ${d#/proc/})"; return 0; fi
      done < "$d/cmdline"; } 2>/dev/null || true
  done
  case $(unit_active sim7600-lte.service) in
    active | activating | reloading) echo "sim7600-lte.service ($(unit_active sim7600-lte.service))"; return 0 ;;
  esac
  if command -v nmcli >/dev/null 2>&1; then
    st=$(nmcli -t -f DEVICE,STATE device 2>/dev/null | awk -F: -v d="$LTE_IFACE" '$1 == d { print $2; exit }') || st=""
    case $st in "" | unmanaged) ;; *) echo "NetworkManager ($st)"; return 0 ;; esac
  fi
  return 0
}
clear_stale_lte_defaults() {
  local fam rt owner other n m v6=0 node
  command -v ip >/dev/null 2>&1 || return 0
  ip link show "$LTE_IFACE" >/dev/null 2>&1 || return 0
  [ -n "$(ip -4 route show default dev "$LTE_IFACE" 2>/dev/null)$(ip -6 route show default dev "$LTE_IFACE" 2>/dev/null)" ] || return 0
  owner=$(lte_iface_owner)
  if [ -n "$owner" ]; then log "  = $LTE_IFACE default routes kept (live owner: $owner)"; return 0; fi
  other=$(ip -4 route show default 2>/dev/null | awk -v d="$LTE_IFACE" '{ for (i = 1; i < NF; i++) if ($i == "dev" && $(i + 1) != d) { print $(i + 1); exit } }') || other=""
  if [ -z "$other" ]; then log "  = $LTE_IFACE default routes kept (no other IPv4 uplink: $LTE_IFACE may be this device's only one)"; return 0; fi
  if lte_iface_works; then log "  = $LTE_IFACE default routes kept ($LTE_IFACE reaches the internet; $other is up too)"; return 0; fi
  for fam in 4 6; do
    n=0
    while [ "$n" -lt 16 ]; do
      rt=$(ip -"$fam" route show default dev "$LTE_IFACE" 2>/dev/null | head -n1) || rt=""
      [ -n "$rt" ] || break
      n=$((n + 1))
      m=$(awk '{ for (i = 1; i < NF; i++) if ($i == "metric") { print $(i + 1); exit } }' <<<"$rt") || m=""
      if ip -"$fam" route del default dev "$LTE_IFACE" 2>/dev/null; then
        [ "$fam" = 4 ] || v6=1
        log "  - removed a stale IPv$fam default route on $LTE_IFACE (no live owner, $LTE_IFACE unreachable, $other is up; metric ${m:-0})"
      else
        warn "could not remove the stale IPv$fam default route on $LTE_IFACE; downloads may stall. By hand: sudo ip -$fam route del default dev $LTE_IFACE"
        break
      fi
    done
  done
  node=/proc/sys/net/ipv6/conf/$LTE_IFACE/disable_ipv6
  if [ "$v6" = 1 ] && [ -e "$node" ]; then
    if { echo 1 > "$node"; } 2>/dev/null; then log "  - IPv6 turned off on $LTE_IFACE (runtime only, as the LTE daemon does), so a modem RA cannot bring the default back"
    else warn "could not turn IPv6 off on $LTE_IFACE; a modem RA may re-add its IPv6 default"; fi
  fi
}

# update_plan::parse_sha256_sidecar: the first `<64 hex>  [*]<basename>` line.
sidecar_digest() { # <file> <basename>
  local line hex rest
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line%$'\r'}
    hex=${line%%[[:space:]]*}
    rest=${line#"$hex"}; rest=${rest#"${rest%%[![:space:]]*}"}; rest=${rest#\*}
    [ "$rest" = "$2" ] || continue
    [[ $hex =~ ^[0-9a-fA-F]{64}$ ]] || continue
    printf '%s' "${hex,,}"
    return 0
  done < "$1"
  return 1
}

# ---------------------------------------------------------------------------
# Provision.
REG_CODE=""; API_BASE=""; FROM=""; TARBALL=""; REPROVISION=0
SVC_USER=root; SVC_GROUP=root


apt_hard() { # <step label> <pkgs…>: failure = exit 1 (nothing claimed)
  local what=$1; shift
  apt-get install -y -qq "$@" || fail 1 "apt-get install $* failed ($what); nothing was claimed. Re-run the same command once apt works (sudo apt-get update)"
}

# N2 (review-wp-r1): once the refusals have passed, the rest of the run
# (apt, claim, install, start) is a transient systemd unit, so a dropped ssh
# session (SIGHUP) can no longer stop it between the claim and P9 and leave a
# half-provisioned device. `systemd-run --no-block` only queues the unit
# (`--wait` would stop it on the client's SIGINT); this session then polls the
# unit, prints its part of the log and returns the exit code the unit wrote.
# If this session is interrupted, hung up or loses its output pipe, it
# detaches at once and the unit carries on; the log says how it ended.
# Where systemd is not running (a container), or when the caller is ALREADY a
# systemd service (fix-check MUST-2: e.g. the bench window's p8-install-*
# unit, whose stop must stop the provisioner too), the run stays in-process.
# The log is append-only with one header per run and the exit code goes to a
# per-run file, so a second run can never truncate or delete the first's
# (fix-check NIT-c).
ORIG_ARGS=()
RUN_ID=""
RUN_RC=""
on_detach() {
  trap '' HUP INT TERM PIPE
  { echo "[xorgate] this session stopped following; provisioning CONTINUES in $PROV_UNIT (run $RUN_ID)"
    echo "[xorgate]   follow it: sudo tail -f $PROV_LOG   (systemctl status $PROV_UNIT; exit code in $RUN_RC)"; } 2>/dev/null
  FINISHED=1
  exit "$1"
}
follow_log() { # <offset var name>: print what the log gained since the offset; fails if the output is gone
  # F-DA21-FOLLOW (v0.1.1): the locals must not share a name with the
  # caller's variable. v0.1.0 had `local n off=${!1}` with the caller passing
  # `off`, so `printf -v "$1"` assigned THIS function's local `off`: the
  # caller's offset never moved and every poll re-printed the whole run
  # (61 791 client lines in DA21). Pinned by provision-rs-bodies.sh §7.
  local __fl_size __fl_from=${!1}
  __fl_size=$(stat -c %s "$PROV_LOG" 2>/dev/null || echo 0)
  if [ "$__fl_size" -gt "$__fl_from" ]; then
    tail -c +"$((__fl_from + 1))" "$PROV_LOG" | head -c "$((__fl_size - __fl_from))" || return 1
    printf -v "$1" '%s' "$__fl_size"
  fi
  return 0
}
handoff_to_unit() {
  local self rc off start i=0 st mine
  if [ -n "${INVOCATION_ID:-}" ]; then
    log "NOTE: already running as a systemd service (INVOCATION_ID set): provisioning inside it, so stopping that unit stops this run"
    return 0
  fi
  if [ ! -d /run/systemd/system ] || ! command -v systemd-run >/dev/null 2>&1; then
    log "NOTE: systemd is not running here: provisioning in THIS session. Keep it alive (tmux): a session that drops after the claim leaves a half-provisioned device"
    return 0
  fi
  case $(unit_active "$PROV_UNIT") in
    active | activating | deactivating) refuse "a provisioning run is already in progress ($PROV_UNIT)" ;;
  esac
  systemctl reset-failed "$PROV_UNIT" >/dev/null 2>&1 || true
  self=$(cd "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)/$(basename -- "${BASH_SOURCE[0]}")
  RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
  RUN_RC="$PROV_RC.$RUN_ID"
  [ -e "$PROV_LOG" ] || install -m 0600 /dev/null "$PROV_LOG"
  start=$(stat -c %s "$PROV_LOG")
  # shellcheck disable=SC2034  # follow_log reads and advances it by name
  off=$start
  printf '[xorgate] ===== run %s (%s) =====\n' "$RUN_ID" "$(date -u +%FT%TZ)" >> "$PROV_LOG"
  exec 9>&-   # the unit takes the lock itself
  log "continuing as the transient unit $PROV_UNIT (a dropped ssh session no longer stops it)"
  log "  log: $PROV_LOG (run $RUN_ID)"
  HANDED_OFF=1
  trap 'on_detach 129' HUP; trap 'on_detach 130' INT; trap 'on_detach 143' TERM; trap 'on_detach 141' PIPE
  systemd-run --unit="$PROV_UNIT" --description="xorgate native Rust provisioning" --no-block --collect --quiet \
    -p StandardOutput="append:$PROV_LOG" -p StandardError="append:$PROV_LOG" \
    --setenv=XORGATE_PROVISION_IN_UNIT=1 --setenv=XORGATE_PROVISION_RCFILE="$RUN_RC" \
    --setenv=SUDO_USER="${SUDO_USER:-}" --setenv=PATH="$PATH" --setenv=HOME=/root \
    bash "$self" ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"} < /dev/null || { HANDED_OFF=0; fail 1 "systemd-run could not start $PROV_UNIT"; }
  # Until the unit has written its exit code; a unit that is gone (or never
  # left the queue) for 10 s without one has died.
  while [ ! -s "$RUN_RC" ]; do
    follow_log off || on_detach 141
    st=$(unit_active "$PROV_UNIT")
    case $st in active | activating | deactivating | reloading) i=0 ;; *) i=$((i + 1)) ;; esac
    [ "$i" -lt 10 ] || break
    sleep 1
  done
  sleep 0.2; follow_log off || on_detach 141
  if [ -s "$RUN_RC" ]; then
    rc=$(cat "$RUN_RC"); rm -f "$RUN_RC"
  else
    # Killed or timed out without writing its exit code (fix-check NIT-b): how
    # far THIS run got is in its part of the log.
    mine=$(tail -c +"$((start + 1))" "$PROV_LOG" 2>/dev/null)
    if grep -q '^\[xorgate\] claimed: device' <<<"$mine"; then
      echo "[xorgate] ERROR: $PROV_UNIT ended without an exit code ($st) AFTER the claim: claim done, install state UNKNOWN" >&2
      DEVICE_ID=$(sed -n 's/^\[xorgate\] claimed: device //p' <<<"$mine" | head -n1)
      recovery
      rc=4
    elif grep -q '^\[xorgate\] claiming registration code' <<<"$mine"; then
      echo "[xorgate] ERROR: $PROV_UNIT ended without an exit code ($st) during the claim: claim state UNKNOWN. If the console lists this device, delete it and register again; otherwise run the same command again" >&2
      rc=5
    else
      echo "[xorgate] ERROR: $PROV_UNIT ended without an exit code ($st) before the claim; nothing was claimed. See $PROV_LOG and journalctl -u $PROV_UNIT" >&2
      rc=1
    fi
  fi
  FINISHED=1
  exit "$rc"
}

provision_main() {
  local m out rc status msg

  # ---- P0: preconditions (refuse with exit 3, change nothing) ----
  [ "$(id -u)" = 0 ] || refuse "run with sudo"
  m=$(uname -m)
  [ "$m" = aarch64 ] || refuse "the Rust agent needs a 64-bit ARM (aarch64) host; this one is $m"
  if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
  else
    refuse "cannot read /etc/os-release (unsupported distro)"
  fi
  case "${ID:-}${ID_LIKE:-}" in
    *debian*|*ubuntu*) ;;
    *) refuse "unsupported distro (${ID:-unknown}); only Debian/Ubuntu supported" ;;
  esac
  # (A converter record alone is not consulted: with /opt/xorgate and the JS
  # unit gone there is no JS agent to protect.)
  if [ -e "$JS_PREFIX" ] || [ -e "$JS_UNIT" ]; then
    refuse "this device has the JS agent ($JS_PREFIX or $JS_UNIT present). To move it to Rust, convert it with install-rs.sh (docs/convert.md); to re-register it, use the JS installer"
  fi
  if [ "$REPROVISION" = 0 ] && { [ -e "$CONF_DIR/agent.env" ] || [ -e "$RS_PREFIX" ] || [ -e "$RS_UNIT" ] || [ -e "$GENERATOR" ]; }; then
    refuse "this device is already provisioned ($CONF_DIR/agent.env, $RS_PREFIX or $RS_UNIT present). To re-register it, run: sudo bash install.sh --reprovision"
  fi
  [ -f "$TARBALL" ] || fail 1 "no such tarball: $TARBALL"
  exec 9>"$LOCK"
  flock -n 9 || refuse "another provision-rs.sh is running ($LOCK)"
  # N2: everything after the refusals runs as a transient unit (returns only
  # where systemd is not running, e.g. a container).
  if [ -z "${XORGATE_PROVISION_IN_UNIT:-}" ]; then handoff_to_unit; fi
  trap 'on_signal 129' HUP; trap 'on_signal 130' INT; trap 'on_signal 143' TERM
  PRIV=$(mktemp -d)
  chmod 0700 "$PRIV"
  if [ -r /proc/device-tree/compatible ] && tr '\0' '\n' < /proc/device-tree/compatible | grep -qx 'brcm,bcm2711'; then
    warn "this is a BCM2711 (CM4/Pi 4) board: the Rust agent is not yet verified on CM4 hardware (plan D7)"
  fi
  if [ "$REPROVISION" = 1 ]; then log "re-provisioning (--reprovision)"; fi

  # 0. The user the agent will RUN as (JS step 0, verbatim semantics).
  if [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ] && id "${SUDO_USER}" >/dev/null 2>&1; then
    SVC_USER="${SUDO_USER}"
    SVC_GROUP="$(id -gn "${SUDO_USER}")"
  fi
  log "agent will run as '$SVC_USER'"

  # P1. The tarball's own checksum again (the bootstrap verified it; this is
  #     the same rule as install-rs.sh --tarball: no sidecar, no install).
  local base want have
  base=$(basename -- "$TARBALL")
  [ -f "$TARBALL.sha256" ] || fail 1 "no checksum next to $base ($TARBALL.sha256); refusing an unverified build"
  want=$(sidecar_digest "$TARBALL.sha256" "$base") || fail 1 "$base.sha256 has no entry for $base; refusing an unverified build"
  have=$(sha256sum < "$TARBALL" | cut -d' ' -f1)
  [ "$have" = "$want" ] || fail 1 "checksum mismatch for $base (published $want, file $have)"
  log "checksum ok ($base)"

  # P1b. The network before the first download (v0.1.1, F-DA21-NM): with a
  #      SIM7600 present, ModemManager off (moved up from step 7), then
  #      usb0's stale default routes, v4 and v6, removed when nothing live
  #      owns them, another IPv4 uplink exists and usb0 does not work.
  #      Both log only when they act.
  if sim7600_present; then disable_modemmanager; fi
  clear_stale_lte_defaults

  # 1. Dependencies (JS step 1; NOT NodeSource/nodejs). An operator's
  #    APT::Get::Upgrade "false" in apt.conf.d (the bench's --no-upgrade window,
  #    Q-8.20) is honoured: no call here overrides apt's configuration.
  log "installing apt dependencies"
  export DEBIAN_FRONTEND=noninteractive
  ensure_dir /etc/apt/apt.conf.d 0755 root:root
  put_body /etc/apt/apt.conf.d/99xorgate-lock-timeout 0644 root:root apt_lock_body || true
  apt-get update -qq || fail 1 "apt-get update failed; nothing was claimed. Check the network and re-run the same command"
  # The provisioner's own tools (jq, curl) and the pip IMU build (S3). JS
  # call 1 + build-essential, which JS also installs here.
  apt_hard "the installer's own tools" curl ca-certificates jq build-essential python3

  # 1b. IMU (BNO085) tooling (JS step 1b, identical; best-effort).
  log "installing IMU tooling (i2c-tools, adafruit-circuitpython-bno08x)"
  apt-get install -y -qq i2c-tools python3-pip python3-dev \
    || log "WARN: i2c-tools/python3 pip install failed; IMU disabled"
  python3 -m pip install --break-system-packages -q \
      adafruit-circuitpython-bno08x adafruit-blinka 2>/dev/null \
    || python3 -m pip install -q adafruit-circuitpython-bno08x adafruit-blinka \
    || log "WARN: adafruit python libs failed to install; IMU disabled until installed"
  if command -v raspi-config >/dev/null 2>&1; then
    raspi-config nonint do_i2c 0 || log "WARN: 'raspi-config do_i2c' failed; enable I2C manually"
  else
    modprobe i2c-dev 2>/dev/null || true
  fi
  add_group "$SVC_USER" i2c

  # 1c. GStreamer BEFORE the claim (the binary links it; JS installs it in its
  #     step 8 and only WARNs). The NEEDED providers are fatal; the plugin set
  #     and libcamera WARN, as in JS.
  log "installing the GStreamer runtime"
  apt_hard "the libraries the agent binary links" "${APT_NEEDED[@]}"
  apt-get install -y -qq "${APT_GST_PLUGINS[@]}" \
    || log "WARN: GStreamer runtime install failed; media streaming disabled. Re-run: sudo apt-get install -y gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly"
  apt-get install -y -qq gstreamer1.0-libcamera 2>/dev/null \
    || log "WARN: gstreamer1.0-libcamera not installed; Raspberry Pi cameras will not stream"

  # P2. Stage the build and smoke-run it, still before the claim.
  log "staging the build in $STAGING"
  if [ -e "$STAGING" ] || [ -L "$STAGING" ]; then rm -rf "$STAGING"; log "  - a stale $STAGING removed"; fi
  STAGED=1
  mkdir -p "$STAGING"
  tar --no-same-owner -xzf "$TARBALL" -C "$STAGING" 2> "$PRIV/tar.err" \
    || fail 1 "extract failed: $(tr -s '\n' ' ' < "$PRIV/tar.err")"
  chown root:root "$STAGING"
  chmod 0755 "$STAGING"
  for m in "${REQUIRED_MEMBERS[@]}"; do
    [ -e "$STAGING/$m" ] || fail 1 "the build is missing $m"
  done
  log "checking the staged build runs on this device"
  rc=0; out=$(timeout 5 "$STAGING/bin/xorgate-rs" -v 2> "$PRIV/smoke.err") || rc=$?
  if [ "$rc" != 0 ] || [ "${out#"$SMOKE_PREFIX"}" = "$out" ]; then
    msg=$(tr -s '\n' ' ' < "$PRIV/smoke.err"); [ -n "$msg" ] || msg="exit $rc, printed \"${out%%$'\n'*}\""
    if command -v ldd >/dev/null 2>&1 && ldd "$STAGING/bin/xorgate-rs" 2>/dev/null | grep -q 'not found'; then
      msg="$msg; missing libraries: $(ldd "$STAGING/bin/xorgate-rs" 2>/dev/null | grep 'not found' | awk '{print $1}' | tr '\n' ' ')"
    fi
    fail 1 "the staged build does not run on this device ($msg); nothing was claimed"
  fi
  log "  staged ${out%%$'\n'*}"

  # 2. Claim (JS step 2's body + agent:"rust"). https only, as the bootstrap.
  local HOSTNAME_VAL PLATFORM_VAL SERIAL_VAL DT_MODEL CAM_COUNT body
  HOSTNAME_VAL=$(hostname || echo unknown)
  PLATFORM_VAL=$(uname -m || echo unknown)
  SERIAL_VAL=$(cat /etc/machine-id 2>/dev/null \
    || cat /sys/class/dmi/id/product_uuid 2>/dev/null \
    || echo "$HOSTNAME_VAL")
  DT_MODEL=$(tr -d '\0' 2>/dev/null < /proc/device-tree/model || echo unknown)
  CAM_COUNT=$( { rpicam-hello --list-cameras 2>/dev/null || true; } | grep -cE '^[0-9]+ +:' || true)
  CAM_COUNT="${CAM_COUNT:-0}"
  body=$(jq -n --arg s "$SERIAL_VAL" --arg h "$HOSTNAME_VAL" --arg p "$PLATFORM_VAL" \
        --arg d "$DT_MODEL" --argjson c "${CAM_COUNT:-0}" \
        '{serial:$s, hostname:$h, platform:$p, dtModel:$d, cameras:$c, agent:"rust"}')
  log "claiming registration code via $API_BASE (model: $DT_MODEL, cameras: $CAM_COUNT)"
  local RESP=$PRIV/claim.json
  PHASE=claiming   # N1: from here until an HTTP answer, the claim state is unknown
  status=$(curl -sS --proto =https --retry 3 --retry-delay 2 \
    -X POST -H 'Content-Type: application/json' -d "$body" \
    -o "$RESP" -w '%{http_code}' "$API_BASE/device-registrations/$REG_CODE/claim" 2> "$PRIV/claim.err") || status=000
  if [ "$status" != 200 ] && [ "$status" != 000 ]; then PHASE=pre; fi   # a definite answer: not claimed
  if [ "$status" != 200 ]; then
    msg=$(jq -r '.message // .error.message // .error // empty' "$RESP" 2>/dev/null | head -c 300 || true)
    msg=${msg%.}
    case $status in
      409) fail 5 "claim refused (409): ${msg:-a device with this serial is already registered in the workspace}. Delete it in the console, then run this same command again (the code is unused)" ;;
      404) fail 5 "claim refused (404): ${msg:-registration code not found, already used, or expired}. Mint a new code in the console" ;;
      000) fail 5 "claim failed: $(tr -s '\n' ' ' < "$PRIV/claim.err")(no HTTP response)" ;;
      *) fail 5 "claim refused (HTTP $status)${msg:+: $msg}" ;;
    esac
  fi
  PHASE=claimed
  DEVICE_ID=$(jq -r '.deviceId // empty' "$RESP")
  local ENDPOINT CA_URL
  ENDPOINT=$(jq -r '.dataEndpoint // empty' "$RESP")
  CA_URL=$(jq -r '.caUrl // empty' "$RESP")
  [ -n "$DEVICE_ID" ] || fail 4 "claim response missing deviceId"
  [ -n "$ENDPOINT" ] || fail 4 "claim response missing dataEndpoint"
  log "claimed: device $DEVICE_ID"

  # 3. /etc/xorgate (JS step 3: the same files, modes and owner). On
  #    --reprovision the running agent stops first, so it never sees a half-
  #    written identity, and its config-sync.json (the old identity's rev
  #    guard) goes (D1.3).
  if [ "$REPROVISION" = 1 ]; then
    case $(unit_active "$RS_SVC") in
      active | activating | reloading | deactivating)
        systemctl stop "$RS_SVC" || fail 4 "could not stop $RS_SVC"
        log "  - $RS_SVC stopped (re-provisioning)" ;;
    esac
    if [ -e "$STATE_DIR/config-sync.json" ]; then rm -f "$STATE_DIR/config-sync.json"; log "  - $STATE_DIR/config-sync.json deleted (the old identity's)"; fi
  fi
  install -d -m 0700 -o "$SVC_USER" -g "$SVC_GROUP" "$CONF_DIR"
  jq -r '.certPem' "$RESP" | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/device.cert.pem"
  jq -r '.privateKey' "$RESP" | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/device.key.pem"
  curl -fsSL --proto =https --retry 3 --retry-delay 2 "${CA_URL:-https://www.amazontrust.com/repository/AmazonRootCA1.pem}" -o "$PRIV/root-ca.pem" \
    || fail 4 "could not download the root CA (${CA_URL:-AmazonRootCA1})"
  install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" "$PRIV/root-ca.pem" "$CONF_DIR/root-ca.pem"
  agent_env_body "$DEVICE_ID" "$ENDPOINT" "$FROM" | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/agent.env"
  log "  - $CONF_DIR written (agent.env, device cert + key, root CA)"

  # 4. The build: .new -> /opt/xorgate-rs; on --reprovision the current tree
  #    goes to .old first, removed once the agent is active (N7).
  if [ -e "$RS_PREFIX" ]; then
    rm -rf "$BACKUP"
    mv -T "$RS_PREFIX" "$BACKUP"
  fi
  mv -T "$STAGING" "$RS_PREFIX"
  STAGED=0
  log "  - $RS_PREFIX installed ($(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$RS_PREFIX/build-info.json" | head -n1))"

  # 5b. The CLI wrapper (update_plan::WRAPPER_BODY). No `xorgate` alias.
  log "installing $WRAPPER"
  for m in /usr/local/bin "$SYSD" /etc/cron.d /etc/udev/rules.d; do ensure_dir "$m" 0755 root:root; done
  put_body "$WRAPPER" 0755 root:root rs_wrapper_body || true

  # 6. The agent unit + drop-ins (10-user.conf as JS; 20-native.conf, D1.3).
  log "installing systemd units"
  put_file "$RS_UNIT" 0644 root:root "$RS_PREFIX/systemd/xorgate-agent-rs.service" || true
  ensure_dir "$RS_UNIT_D" 0755 root:root
  put_body "$RS_UNIT_D/10-user.conf" 0644 root:root user_dropin_body "$SVC_USER" "$SVC_GROUP" || true
  put_body "$RS_UNIT_D/20-native.conf" 0644 root:root native_dropin_body || true
  ensure_dir "${GENERATOR%/*}" 0755 root:root
  put_body "$GENERATOR" 0755 root:root native_generator_body || true
  for m in "$RS_UNIT_D"/*; do
    case $m in "$RS_UNIT_D/10-user.conf" | "$RS_UNIT_D/20-native.conf") ;; *) [ ! -e "$m" ] || log "  foreign drop-in kept: $m" ;; esac
  done
  # 6. sudoers 020 (the JS body).
  ensure_dir "$SUDOERS_D" 0750 root:root
  put_sudoers "$SUDOERS_D/020_xorgate-power" "sudoers drop-in failed validation; reboot/power-off commands will not work" power_sudoers_body "$SVC_USER"
  # 6a/6a1. Clock + camera-rotation helpers, their grants (the update_plan
  #     bodies, so a later `xorgate-rs update` rewrites nothing), the cron.
  if [ -f "$RS_PREFIX/system/xorgate-setclock" ]; then
    put_file /usr/local/bin/xorgate-setclock 0755 root:root "$RS_PREFIX/system/xorgate-setclock" || true
    put_sudoers "$SUDOERS_D/021_xorgate-clock" "clock sudoers drop-in failed validation; offline clock steps will not work" clock_sudoers_body "$SVC_USER"
  fi
  if [ -f "$RS_PREFIX/system/xorgate-fake-hwclock.cron" ]; then
    put_file /etc/cron.d/xorgate-fake-hwclock 0644 root:root "$RS_PREFIX/system/xorgate-fake-hwclock.cron" || true
  fi
  if [ -f "$RS_PREFIX/system/xorgate-camrotate" ]; then
    put_file /usr/local/bin/xorgate-camrotate 0755 root:root "$RS_PREFIX/system/xorgate-camrotate" || true
    put_sudoers "$SUDOERS_D/023_xorgate-camrotate" "camrotate sudoers drop-in failed validation; camera rotation will not apply" camrotate_sudoers_body "$SVC_USER"
  fi
  # 6a1b. The camera-line owner (Rust-only; no JS counterpart): its overlay
  #     allow-list FIRST (the helper never exists without it), the helper,
  #     then 025 (update_plan; none for a root agent, as update_plan::grant_user).
  #     The Rust agent calls this helper, not xorgate-camrotate, which stays
  #     installed above for a device that is later given the JS agent.
  if [ -f "$RS_PREFIX/system/xorgate-camcfg" ] && [ -f "$RS_PREFIX/system/xorgate-camcfg.overlays" ]; then
    put_file "$CAMCFG_LIST" 0644 root:root "$RS_PREFIX/system/xorgate-camcfg.overlays" || true
    put_file "$CAMCFG_BIN" 0755 root:root "$RS_PREFIX/system/xorgate-camcfg" || true
    if [ "$SVC_USER" != root ]; then
      put_sudoers "$SUDOERS_025" "camcfg sudoers drop-in failed validation; camera line changes will not apply" rs_camcfg_sudoers_body "$SVC_USER"
    fi
  fi
  # 6a2. The Rust update rail: xorgate-updater-rs.service (+ 10-dist.conf) and
  #     024 (update_plan; none for a root agent, as update_plan::grant_user).
  #     No JS updater, no 022.
  put_file "$UPD_UNIT" 0644 root:root "$RS_PREFIX/systemd/xorgate-updater-rs.service" || true
  ensure_dir "$UPD_UNIT_D" 0755 root:root
  put_body "$UPD_UNIT_D/10-dist.conf" 0644 root:root native_dist_dropin_body || true
  if [ "$SVC_USER" != root ]; then
    put_sudoers "$SUDOERS_024" "update sudoers drop-in failed validation; remote updates will not work" rs_update_sudoers_body "$SVC_USER"
  fi
  # 6b. logind RemoveIPC=no; logind is restarted only when the file changed.
  ensure_dir /etc/systemd/logind.conf.d 0755 root:root
  if put_body /etc/systemd/logind.conf.d/10-xorgate-removeipc.conf 0644 root:root logind_body; then
    systemctl try-restart systemd-logind 2>/dev/null \
      || log "WARN: could not restart systemd-logind; RemoveIPC=no applies on next boot"
  fi
  systemctl daemon-reload

  # 7. SIM7600 LTE connection manager (JS step 7; cmp-before-write; an active
  #    daemon is never restarted: `enable --now` only starts an inactive one).
  log "installing SIM7600 connection manager (LTE + GPS)"
  apt-get install -y -qq isc-dhcp-client usbutils \
    || log "WARN: isc-dhcp-client/usbutils install failed; LTE may not come up"
  disable_modemmanager   # already off since P1b; kept for the JS step-7 order
  add_group "$SVC_USER" dialout
  if [ -f "$RS_PREFIX/modem/sim7600-lte.sh" ]; then
    local unit_changed=0
    put_file /usr/local/bin/sim7600-lte.sh 0755 root:root "$RS_PREFIX/modem/sim7600-lte.sh" || true
    if put_file "$SYSD/sim7600-lte.service" 0644 root:root "$RS_PREFIX/modem/sim7600-lte.service"; then unit_changed=1; fi
    if put_file /etc/udev/rules.d/99-sim7600-lte.rules 0644 root:root "$RS_PREFIX/modem/99-sim7600-lte.rules"; then
      udevadm control --reload-rules 2>/dev/null || true
    fi
    if [ -f "$RS_PREFIX/modem/99-sim7600-unmanaged.conf" ]; then
      ensure_dir /etc/NetworkManager/conf.d 0755 root:root
      if put_file /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf 0644 root:root "$RS_PREFIX/modem/99-sim7600-unmanaged.conf"; then
        systemctl reload NetworkManager 2>/dev/null || true
      fi
    fi
    if [ "$unit_changed" = 1 ]; then systemctl daemon-reload; fi
    if [ "$(unit_active sim7600-lte.service)" = active ] && [ "$(unit_enabled sim7600-lte.service)" = enabled ]; then
      log "  = sim7600-lte.service enabled + active (not restarted)"
    else
      systemctl enable --now sim7600-lte.service 2>/dev/null \
        || systemctl enable sim7600-lte.service 2>/dev/null \
        || log "WARN: could not enable sim7600-lte.service; LTE/GPS disabled"
    fi
  else
    log "WARN: modem assets missing from build ($RS_PREFIX/modem); LTE/GPS disabled"
  fi

  # 7b. Uplink priority (JS step 7b, same text).
  if command -v nmcli >/dev/null 2>&1 && systemctl is-active --quiet NetworkManager 2>/dev/null; then
    log "setting WiFi to outrank the LTE modem for the IPv4 default route"
    ensure_dir /etc/NetworkManager/conf.d 0755 root:root
    if put_body /etc/NetworkManager/conf.d/10-uplink-priority.conf 0644 root:root uplink_body; then
      systemctl reload NetworkManager 2>/dev/null || true
    fi
  else
    log "NOTE: NetworkManager not active; skipping WiFi/LTE route-priority config"
  fi

  # 8. KVS manifest, hardware block, pipelines (JS step 8; the GStreamer
  #    packages already went in at 1c).
  log "writing the KVS manifest and the hardware block"
  jq '{deviceId, region, credentialEndpoint, roleAlias, channels: (.channels // []), uploadBucket}' "$RESP" \
    | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/kvs.json"
  if jq -e '.hardware' "$RESP" >/dev/null 2>&1; then
    jq '.hardware' "$RESP" | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/hardware.json"
    log "hardware block written ($CONF_DIR/hardware.json)"
  elif [ -e "$CONF_DIR/hardware.json" ]; then
    rm -f "$CONF_DIR/hardware.json"
    log "  - a stale $CONF_DIR/hardware.json removed (this registration's model has no hardware block)"
  fi
  install -d -m 0755 -o "$SVC_USER" -g "$SVC_GROUP" "$CONF_DIR/pipelines"
  if [ ! -f "$CONF_DIR/hardware.json" ]; then
    log "no hardware block (legacy model); writing the historical CM5 pipeline defaults"
    write_bridged_pipeline cam0 /base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a
    write_bridged_pipeline cam1 /base/axi/pcie@1000120000/rp1/i2c@70000/imx290@1a
  else
    log "hardware block present; the agent renders pipelines on first start after reboot"
  fi
  if [ -x "$RS_PREFIX/media/xorgate-kvs-webrtc" ]; then
    if ls "$RS_PREFIX"/media/lib/*.so* >/dev/null 2>&1; then
      log "media runtime staged ($RS_PREFIX/media)"
    else
      log "NOTE: KVS SDK .so libs missing in $RS_PREFIX/media/lib; supply them to enable streaming"
    fi
  else
    log "WARN: media runtime binary missing from build ($RS_PREFIX/media); streaming disabled"
  fi

  # 8b. Camera device-tree overlays (JS step 8b). The same lines; config.txt is
  #     rewritten only when a line is actually missing or wrong.
  local CONFIG_TXT=/boot/firmware/config.txt CAM_OVERLAY_ADDED=0 OVERLAY_LINES overlay_line
  [ -f "$CONFIG_TXT" ] || CONFIG_TXT=/boot/config.txt
  if [ -f "$CONFIG_TXT" ]; then
    cp -n "$CONFIG_TXT" "$CONFIG_TXT.bak-xorgate" 2>/dev/null || true
    if grep -qE '^camera_auto_detect=' "$CONFIG_TXT"; then
      if grep -E '^camera_auto_detect=' "$CONFIG_TXT" | grep -qvx 'camera_auto_detect=0'; then
        sed -i 's/^camera_auto_detect=.*/camera_auto_detect=0/' "$CONFIG_TXT"
      fi
    else
      echo 'camera_auto_detect=0' >> "$CONFIG_TXT"
    fi
    OVERLAY_LINES=$(jq -r '.hardware.video // [] | .[] | "dtoverlay=\(.sensor),\(.connector | sub("^csi"; "cam"))"' "$RESP" 2>/dev/null || true)
    if [ -z "$OVERLAY_LINES" ]; then
      OVERLAY_LINES=$(printf 'dtoverlay=imx290,cam0\ndtoverlay=imx290,cam1')
    fi
    while IFS= read -r overlay_line; do
      [ -n "$overlay_line" ] || continue
      grep -qF "$overlay_line" "$CONFIG_TXT" || { echo "$overlay_line" >> "$CONFIG_TXT"; CAM_OVERLAY_ADDED=1; }
    done <<EOF
$OVERLAY_LINES
EOF
    if [ "$CAM_OVERLAY_ADDED" = "1" ]; then
      log "camera overlays added to $CONFIG_TXT — REBOOT required before cameras stream"
    else
      log "camera overlays already present in $CONFIG_TXT"
    fi
  else
    log "WARN: no config.txt found; skipping camera overlays"
  fi

  # P9. Enable + start; wait for READY (Type=notify) <= START_WAIT_SECS.
  log "starting $RS_SVC"
  # Enablement = the generator (not `systemctl enable`, see native_generator_body);
  # a stale /etc wants link from an older install is removed.
  if [ -L "$SYSD/multi-user.target.wants/$RS_SVC.service" ]; then rm -f "$SYSD/multi-user.target.wants/$RS_SVC.service"; log "  - the /etc wants link replaced by the generator"; fi
  systemctl daemon-reload
  # The generator's wants link is the enablement (systemctl is-enabled says
  # "generated", or "enabled-runtime" on systemd 252).
  [ -L "/run/systemd/generator/multi-user.target.wants/$RS_SVC.service" ] \
    || fail 4 "the boot generator did not want $RS_SVC after daemon-reload (is-enabled: $(unit_enabled "$RS_SVC")); see $GENERATOR"
  log "  = wanted at boot by $GENERATOR (is-enabled: $(unit_enabled "$RS_SVC"))"
  if [ "$(unit_active "$RS_SVC")" = active ]; then
    systemctl restart --no-block "$RS_SVC" || fail 4 "systemctl restart $RS_SVC failed"
  else
    systemctl start --no-block "$RS_SVC" || fail 4 "systemctl start $RS_SVC failed"
  fi
  local deadline=$((SECONDS + START_WAIT_SECS)) state
  while :; do
    state=$(unit_active "$RS_SVC")
    if [ "$state" = active ]; then
      sleep 3
      state=$(unit_active "$RS_SVC")
      [ "$state" = active ] && break
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "[xorgate] journal tail (journalctl -u $RS_SVC):" >&2
      journalctl -u "$RS_SVC" -n 20 --no-pager 2>&1 | sed 's/^/[xorgate]     /' >&2 || true
      if [ -e "$BACKUP" ]; then echo "[xorgate] the previous build is kept at $BACKUP" >&2; fi
      fail 4 "$RS_SVC is '$state' after ${START_WAIT_SECS}s (want active)"
    fi
    sleep 2
  done
  rm -rf "$BACKUP"
  log "  $RS_SVC active"
  log "registered device $DEVICE_ID"
  log "tail logs: sudo journalctl -u $RS_SVC -f"
  FINISHED=1
  exit 0
}

# ---------------------------------------------------------------------------
# --uninstall (D1.3): after the JS installer took this device over.
uninstall_main() {
  local p d rest
  [ "$(id -u)" = 0 ] || refuse "run with sudo"
  if [ -e "$CONVERT_RECORD" ]; then
    refuse "this device was CONVERTED by install-rs.sh ($CONVERT_RECORD): use sudo /opt/xorgate-rs/install-rs.sh --revert [--purge]"
  fi
  if [ ! -e "$JS_PREFIX" ] || [ ! -e "$JS_UNIT" ]; then
    refuse "the JS agent is not installed ($JS_PREFIX + $JS_UNIT): removing the Rust agent would leave this device with no agent"
  fi
  # review-wp-r1 S1: only a NATIVE install (its generator or 20-native.conf)
  # that the JS installer has since taken over (JS enabled). A device converted
  # by hand, or whose convert record is gone, has neither marker.
  if [ ! -e "$GENERATOR" ] && [ ! -e "$RS_UNIT_D/20-native.conf" ]; then
    refuse "this is not a native Rust install ($GENERATOR and $RS_UNIT_D/20-native.conf are absent): a converted device is reverted with sudo /opt/xorgate-rs/install-rs.sh --revert"
  fi
  if [ "$(unit_enabled xorgate-agent)" != enabled ]; then
    refuse "xorgate-agent (JS) is not enabled ($(unit_enabled xorgate-agent)): removing the Rust agent would leave this device with no agent at boot"
  fi
  exec 9>"$LOCK"
  flock -n 9 || refuse "another provision-rs.sh is running ($LOCK)"
  # This file is inside the tree it removes: carry on from a private copy.
  if [ -z "${XORGATE_PROVISION_REEXEC:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    p=$(mktemp /tmp/provision-rs.XXXXXX)
    cp -- "${BASH_SOURCE[0]}" "$p"
    exec 9>&-
    XORGATE_PROVISION_REEXEC=$p exec bash "$p" --uninstall
  fi
  [ -z "${XORGATE_PROVISION_REEXEC:-}" ] || rm -f "$XORGATE_PROVISION_REEXEC"
  log "removing the native Rust agent (the JS agent stays)"
  if [ "$(unit_enabled "$RS_SVC")" = enabled ]; then systemctl disable "$RS_SVC" 2>/dev/null || warn "systemctl disable $RS_SVC failed"; log "  - $RS_SVC disabled"; fi
  case $(unit_active "$RS_SVC") in
    inactive | failed | unknown | "") ;;
    *) systemctl stop "$RS_SVC" 2>/dev/null || warn "systemctl stop $RS_SVC failed"; log "  - $RS_SVC stopped" ;;
  esac
  for p in "$GENERATOR" "$SYSD/multi-user.target.wants/$RS_SVC.service" "$RS_UNIT_D/10-user.conf" "$RS_UNIT_D/20-native.conf" "$RS_UNIT" "$UPD_UNIT_D/10-dist.conf" "$UPD_UNIT" \
           "$SUDOERS_024" "$SUDOERS_025" "$CAMCFG_BIN" "$CAMCFG_LIST" "$WRAPPER"; do
    if [ -e "$p" ] || [ -L "$p" ]; then rm -f "$p"; log "  - $p removed"; fi
  done
  for d in "$RS_UNIT_D" "$UPD_UNIT_D"; do
    [ -d "$d" ] || continue
    if rmdir "$d" 2>/dev/null; then log "  - $d/ removed"; else
      for rest in "$d"/*; do [ ! -e "$rest" ] || log "  foreign drop-in kept: $rest"; done
    fi
  done
  for d in "$RS_PREFIX" "$STAGING" "$BACKUP"; do
    if [ -e "$d" ] || [ -L "$d" ]; then rm -rf "$d"; log "  - $d removed"; fi
  done
  systemctl daemon-reload || true
  log "uninstalled; kept: $CONF_DIR, $STATE_DIR, the modem/system assets, sudoers 020-023"
  FINISHED=1
  exit 0
}

# ---------------------------------------------------------------------------
abs_path() { # <path> -> the same path, absolute (its directory resolved)
  local d
  d=$(cd "$(dirname -- "$1")" 2>/dev/null && pwd -P) || return 1
  printf '%s/%s' "$d" "$(basename -- "$1")"
}
# --cleanup: only a temp dir that holds the tarball (never a path that happens
# to be passed by mistake).
set_cleanup() {
  local cdir tdir
  if [ -n "${CLEANUP_ARG:-}" ]; then
    cdir=$(cd "$CLEANUP_ARG" 2>/dev/null && pwd -P) || cdir=""
    tdir=$(cd "$(dirname -- "$TARBALL")" 2>/dev/null && pwd -P) || tdir=""
    case $cdir in
      /tmp/?* | "${TMPDIR:-/tmp}"/?*)
        if [ -n "$tdir" ] && [ "$tdir" = "$cdir" ] || [ "${tdir#"$cdir"/}" != "$tdir" ]; then CLEANUP=$cdir
        else log "NOTE: --cleanup $CLEANUP_ARG does not hold the tarball: not removed"; fi ;;
      *) log "NOTE: --cleanup $CLEANUP_ARG is not a temp dir: not removed" ;;
    esac
  fi
}
main() {
  local mode=provision n=$# a prev=""
  # N6 (review-wp-r1): learn --cleanup/--tarball first, so a usage error
  # (exit 2) still removes the bootstrap's temp dir (and the tarball in it).
  for a in "$@"; do
    case $prev in --cleanup) CLEANUP_ARG=$a ;; --tarball) TARBALL=$a ;; esac
    prev=$a
  done
  set_cleanup
  while [ $# -gt 0 ]; do
    case $1 in
      --reg-code | --api-base | --from | --tarball | --cleanup)
        { [ $# -ge 2 ] && [ -n "$2" ]; } || usage_error "$1 needs a value"
        case $1 in
          --reg-code) REG_CODE=$2 ;; --api-base) API_BASE=$2 ;; --from) FROM=$2 ;;
          --tarball) TARBALL=$2 ;; --cleanup) CLEANUP_ARG=$2 ;;
        esac
        shift 2 ;;
      --reprovision) REPROVISION=1; shift ;;
      --uninstall) mode=uninstall; shift ;;
      -h | --help) usage; FINISHED=1; exit 0 ;;
      *) usage_error "unknown argument: $1" ;;
    esac
  done
  if [ "$mode" = uninstall ]; then
    [ "$n" = 1 ] || usage_error "--uninstall takes no other argument"
    uninstall_main
  fi
  if [ -z "$REG_CODE" ] || [ -z "$API_BASE" ] || [ -z "$FROM" ] || [ -z "$TARBALL" ]; then
    usage_error "--reg-code, --api-base, --from and --tarball are required"
  fi
  [[ $REG_CODE =~ ^[A-Za-z0-9_-]{1,64}$ ]] || usage_error "--reg-code: not a registration code"
  case $API_BASE in https://*) ;; *) usage_error "--api-base must be an https:// URL" ;; esac
  case $FROM in https://*) ;; *) usage_error "--from must be an https:// URL" ;; esac
  API_BASE=${API_BASE%/}; FROM=${FROM%/}
  # Fix-check NIT-a: the hand-off runs in `/`, so a relative --tarball or
  # --cleanup is made absolute here (before anything is claimed); one that
  # does not resolve is a usage error.
  TARBALL=$(abs_path "$TARBALL") || usage_error "--tarball: cannot resolve the path (its directory does not exist)"
  [ -z "$CLEANUP" ] || CLEANUP_ARG=$CLEANUP   # set_cleanup's canonical dir
  ORIG_ARGS=(--reg-code "$REG_CODE" --api-base "$API_BASE" --from "$FROM" --tarball "$TARBALL")
  [ -z "$CLEANUP" ] || ORIG_ARGS+=(--cleanup "$CLEANUP")
  [ "$REPROVISION" = 0 ] || ORIG_ARGS+=(--reprovision)
  provision_main
}

CLEANUP_ARG=""
if [ "${BASH_SOURCE[0]}" = "$0" ] || [ -n "${XORGATE_PROVISION_REEXEC:-}" ]; then
  trap on_exit EXIT
  main "$@"
fi
