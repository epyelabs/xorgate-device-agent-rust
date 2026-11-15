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
# binary has run): P0 preconditions -> 1 apt -> 1b IMU -> 1c GStreamer ->
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
STATE_DIR=/var/lib/xorgate
CONVERT_RECORD=/var/lib/xorgate-rs-install/record
LOCK=/run/xorgate-provision-rs.lock
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
# The JS install.sh NM uplink-priority body.
uplink_body() {
  printf '%s\n' \
    '# Prefer WiFi over the LTE modem (usb0) for the IPv4 default route.' \
    '# WiFi connections get route-metric 50; usb0 is unmanaged by NM and the' \
    '# sim7600-lte daemon installs its default route at metric 100, so WiFi wins' \
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
      remove the Rust tree, units, drop-ins, wrapper and sudoers 024. Never
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
PHASE=pre          # pre (nothing claimed) | claimed
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
  if [ "$FINISHED" != 1 ] && [ "$rc" != 0 ]; then
    echo "[xorgate] ERROR: unexpected failure (exit $rc)" >&2
    if [ "$PHASE" = claimed ]; then rc=4; else rc=1; fi
  fi
  if [ "$PHASE" = pre ] && [ "$STAGED" = 1 ]; then rm -rf "$STAGING"; fi
  if [ "$PHASE" = pre ] && [ "$rc" = 1 ]; then echo "[xorgate] nothing was claimed: the registration code is still usable" >&2; fi
  if [ "$PHASE" = claimed ] && [ "$rc" = 4 ]; then recovery; fi
  [ -z "$PRIV" ] || rm -rf "$PRIV"
  [ -z "$CLEANUP" ] || rm -rf "$CLEANUP"
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
  DT_MODEL=$(tr -d '\0' < /proc/device-tree/model 2>/dev/null || echo unknown)
  CAM_COUNT=$( { rpicam-hello --list-cameras 2>/dev/null || true; } | grep -cE '^[0-9]+ +:' || true)
  CAM_COUNT="${CAM_COUNT:-0}"
  body=$(jq -n --arg s "$SERIAL_VAL" --arg h "$HOSTNAME_VAL" --arg p "$PLATFORM_VAL" \
        --arg d "$DT_MODEL" --argjson c "${CAM_COUNT:-0}" \
        '{serial:$s, hostname:$h, platform:$p, dtModel:$d, cameras:$c, agent:"rust"}')
  log "claiming registration code via $API_BASE (model: $DT_MODEL, cameras: $CAM_COUNT)"
  local RESP=$PRIV/claim.json
  status=$(curl -sS --proto =https --retry 3 --retry-delay 2 \
    -X POST -H 'Content-Type: application/json' -d "$body" \
    -o "$RESP" -w '%{http_code}' "$API_BASE/device-registrations/$REG_CODE/claim" 2> "$PRIV/claim.err") || status=000
  if [ "$status" != 200 ]; then
    msg=$(jq -r '.message // .error.message // .error // empty' "$RESP" 2>/dev/null | head -c 300 || true)
    msg=${msg%.}
    case $status in
      409) fail 5 "claim refused (409): ${msg:-a device with this serial is already registered in the workspace}. Delete it in the console, then run this same command again (the code is unused)" ;;
      404) fail 5 "claim refused (404): ${msg:-registration code not found, already used, or expired}. Mint a new code in the console" ;;
      000) fail 5 "claim failed: $(tr -s '\n' ' ' < "$PRIV/claim.err")(no HTTP response). If the console shows the device, delete it; then register again" ;;
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
  case $(unit_enabled ModemManager.service)/$(unit_active ModemManager.service) in
    enabled/* | */active | */activating) systemctl disable --now ModemManager.service 2>/dev/null || true; log "  - ModemManager disabled" ;;
  esac
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
  [ "$(unit_enabled "$RS_SVC")" = generated ] || fail 4 "$RS_SVC is '$(unit_enabled "$RS_SVC")' after daemon-reload, want 'generated' ($GENERATOR)"
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
           "$SUDOERS_024" "$WRAPPER"; do
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
main() {
  local mode=provision n=$#
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
  [ -n "$REG_CODE" ] && [ -n "$API_BASE" ] && [ -n "$FROM" ] && [ -n "$TARBALL" ] \
    || usage_error "--reg-code, --api-base, --from and --tarball are required"
  [[ $REG_CODE =~ ^[A-Za-z0-9_-]{1,64}$ ]] || usage_error "--reg-code: not a registration code"
  case $API_BASE in https://*) ;; *) usage_error "--api-base must be an https:// URL" ;; esac
  case $FROM in https://*) ;; *) usage_error "--from must be an https:// URL" ;; esac
  API_BASE=${API_BASE%/}; FROM=${FROM%/}
  # --cleanup: only a temp dir that holds the tarball (never a path that
  # happens to be passed by mistake).
  if [ -n "${CLEANUP_ARG:-}" ]; then
    local cdir tdir
    cdir=$(cd "$CLEANUP_ARG" 2>/dev/null && pwd -P) || cdir=""
    tdir=$(cd "$(dirname -- "$TARBALL")" 2>/dev/null && pwd -P) || tdir=""
    case $cdir in
      /tmp/?* | "${TMPDIR:-/tmp}"/?*)
        if [ -n "$tdir" ] && [ "$tdir" = "$cdir" ] || [ "${tdir#"$cdir"/}" != "$tdir" ]; then CLEANUP=$cdir
        else log "NOTE: --cleanup $CLEANUP_ARG does not hold the tarball: not removed"; fi ;;
      *) log "NOTE: --cleanup $CLEANUP_ARG is not a temp dir: not removed" ;;
    esac
  fi
  provision_main
}

CLEANUP_ARG=""
if [ "${BASH_SOURCE[0]}" = "$0" ] || [ -n "${XORGATE_PROVISION_REEXEC:-}" ]; then
  trap on_exit EXIT
  main "$@"
fi
