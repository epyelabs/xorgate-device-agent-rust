#!/usr/bin/env bash
# xorgate device install script.
# Generated per-request by GET /get/:code. Do not edit by hand on the host.
set -euo pipefail

REG_CODE='abcdefghijkmnpq2'
API_BASE='https://fvcro5guw7.execute-api.us-east-1.amazonaws.com/v1'
DIST_BUCKET_URL='https://xorgate-device-dist-865609249890-us-east-1.s3.us-east-1.amazonaws.com'
DIST_VERSION='latest'

CONF_DIR=/etc/xorgate
INSTALL_PREFIX=/opt/xorgate
UNIT_DEST=/etc/systemd/system/xorgate-agent.service

log()  { echo "[xorgate] $*"; }
fail() { echo "[xorgate] ERROR: $*" >&2; exit 1; }

if [ "$(id -u)" -ne 0 ]; then
  fail "run with sudo"
fi

# 0a. Keep NetworkManager off the SIM7600's usb0 BEFORE anything touches the
#     network (cm4-support plan, inventory #10). On a fresh device NM
#     auto-creates a profile for usb0 and grabs the modem's internal DHCP
#     lease; its metric-100 default route then black-holes the apt update,
#     the claim, and the tarball download below (verified live on the CM4
#     bring-up — the install died exactly here). The tarball ships the fully
#     commented version of this conf and step 7 re-installs it; this early
#     minimal copy exists only because the tarball cannot be downloaded
#     through a black-holed route. Idempotent.
if [ -d /etc/NetworkManager ]; then
  install -d -m 0755 /etc/NetworkManager/conf.d
  cat > /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf <<'NMEOF'
# usb0 is owned exclusively by the xorgate sim7600-lte daemon (its own
# dhclient + health checks); NetworkManager must not manage it. Installed
# early by install.sh; replaced by the fully commented copy from the agent
# tarball later in the same run.
[keyfile]
unmanaged-devices=interface-name:usb0
NMEOF
  systemctl reload NetworkManager 2>/dev/null || true
fi

# 0. Resolve the user the agent will RUN as (the installer itself must be root).
#    Captured from $SUDO_USER so the operator who ran `sudo bash install.sh`
#    owns the cert/state and can inspect the agent without sudo. Falls back to
#    root when run as actual root (no SUDO_USER) — the agent only needs an
#    outbound mTLS connection, so it doesn't require root.
SVC_USER="root"
SVC_GROUP="root"
if [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ] && id "${SUDO_USER}" >/dev/null 2>&1; then
  SVC_USER="${SUDO_USER}"
  SVC_GROUP="$(id -gn "${SUDO_USER}")"
fi
log "agent will run as '$SVC_USER'"

# 1. Distro detection + dependency install.
if [ -r /etc/os-release ]; then
  . /etc/os-release
else
  fail "cannot read /etc/os-release (unsupported distro)"
fi

case "${ID:-}${ID_LIKE:-}" in
  *debian*|*ubuntu*)
    log "installing apt dependencies"
    export DEBIAN_FRONTEND=noninteractive
    # A fresh boot usually has apt-daily/unattended-upgrades holding the dpkg
    # lock. Make EVERY apt call in this script (and the NodeSource sub-installer)
    # WAIT for the lock instead of failing instantly. Without this, the later
    # GStreamer install (step 8) loses the race, hits its '|| log WARN' guard, and
    # the device ends up provisioned-but-unable-to-stream. 600s is well past any
    # boot-time apt job; honored from apt.conf.d by every apt-get invocation.
    install -d -m 0755 /etc/apt/apt.conf.d
    echo 'DPkg::Lock::Timeout "600";' > /etc/apt/apt.conf.d/99xorgate-lock-timeout
    apt-get update -qq
    apt-get install -y -qq curl ca-certificates jq build-essential python3
    if ! command -v node >/dev/null 2>&1 \
       || [ "$(node -p 'process.versions.node.split(".")[0]')" -lt 20 ]; then
      log "installing Node.js 20 via NodeSource"
      curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
      apt-get install -y -qq nodejs
    fi
    ;;
  *)
    fail "unsupported distro (${ID:-unknown}); only Debian/Ubuntu supported"
    ;;
esac

# 1b. IMU (BNO085) tooling — best-effort so a device without the sensor (or a
#     non-Pi host) still installs cleanly. i2cdetect + the Adafruit CircuitPython
#     stack back the agent's real IMU collector (src/hardware/imu.ts); the agent
#     spawns bare `python3`, so the libs go system-wide. The IMU collector
#     self-gates when none of this is present, so failures here only disable IMU.
log "installing IMU tooling (i2c-tools, adafruit-circuitpython-bno08x)"
case "${ID:-}${ID_LIKE:-}" in
  *debian*|*ubuntu*)
    apt-get install -y -qq i2c-tools python3-pip python3-dev \
      || log "WARN: i2c-tools/python3 pip install failed; IMU disabled"
    # PEP 668 (Bookworm+) blocks system-wide pip installs without an override;
    # older pip lacks the flag, so try with it and fall back to a plain install.
    python3 -m pip install --break-system-packages -q \
        adafruit-circuitpython-bno08x adafruit-blinka 2>/dev/null \
      || python3 -m pip install -q adafruit-circuitpython-bno08x adafruit-blinka \
      || log "WARN: adafruit python libs failed to install; IMU disabled until installed"
    ;;
esac

# Enable the I2C bus and let the (non-root) agent user reach /dev/i2c-*.
if command -v raspi-config >/dev/null 2>&1; then
  raspi-config nonint do_i2c 0 || log "WARN: 'raspi-config do_i2c' failed; enable I2C manually"
else
  modprobe i2c-dev 2>/dev/null || true
fi
if getent group i2c >/dev/null 2>&1 && [ "$SVC_USER" != "root" ]; then
  usermod -aG i2c "$SVC_USER" || log "WARN: could not add $SVC_USER to the i2c group"
fi

# 2. Claim the registration code. The serial is a stable host identifier so a
#    re-run on the same machine maps to the same device (the backend rejects a
#    duplicate serial in the workspace). machine-id is preferred; fall back to the
#    DMI product UUID, then the hostname.
#    The claim also carries a hardware probe (cm4-support plan, D3): the
#    device-tree model string picks the right device model row server-side
#    (CM4 vs CM5), and the camera count narrows further when cameras are
#    already bound. On a FRESH install the count is 0 (the overlays land in
#    step 8b and bind on the next boot) — the backend treats 0 as unknown.
HOSTNAME_VAL=$(hostname || echo unknown)
PLATFORM_VAL=$(uname -m || echo unknown)
SERIAL_VAL=$(cat /etc/machine-id 2>/dev/null \
  || cat /sys/class/dmi/id/product_uuid 2>/dev/null \
  || echo "$HOSTNAME_VAL")
DT_MODEL=$(tr -d '\0' < /proc/device-tree/model 2>/dev/null || echo unknown)
CAM_COUNT=$( { rpicam-hello --list-cameras 2>/dev/null || true; } | grep -cE '^[0-9]+ +:' || true)
CAM_COUNT="${CAM_COUNT:-0}"

log "claiming registration code via $API_BASE (model: $DT_MODEL, cameras: $CAM_COUNT)"
CLAIM_RESPONSE=$(curl -fsSL --retry 3 --retry-delay 2 \
  -X POST -H 'Content-Type: application/json' \
  -d "$(jq -n --arg s "$SERIAL_VAL" --arg h "$HOSTNAME_VAL" --arg p "$PLATFORM_VAL" \
        --arg d "$DT_MODEL" --argjson c "${CAM_COUNT:-0}" \
        '{serial:$s, hostname:$h, platform:$p, dtModel:$d, cameras:$c}')" \
  "$API_BASE/device-registrations/$REG_CODE/claim") \
  || fail "claim failed (code may be expired, already used, or the device already registered)"

DEVICE_ID=$(echo "$CLAIM_RESPONSE"   | jq -r '.deviceId // empty')
ENDPOINT=$(echo "$CLAIM_RESPONSE"    | jq -r '.dataEndpoint // empty')
CA_URL=$(echo "$CLAIM_RESPONSE"      | jq -r '.caUrl // empty')

[ -n "$DEVICE_ID" ] || fail "claim response missing deviceId"
[ -n "$ENDPOINT" ]  || fail "claim response missing dataEndpoint"

# 3. Drop certs + agent.env into /etc/xorgate. Mode 0600, owned by the service
#    user (not root) so the non-root agent can read them and they stay private
#    to that user.
install -d -m 0700 -o "$SVC_USER" -g "$SVC_GROUP" "$CONF_DIR"

echo "$CLAIM_RESPONSE" | jq -r '.certPem'     \
  | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/device.cert.pem"
echo "$CLAIM_RESPONSE" | jq -r '.privateKey'  \
  | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/device.key.pem"

# The Amazon Root CA is public; fetch it by URL rather than embedding a copy.
if [ -n "$CA_URL" ]; then
  curl -fsSL --retry 3 --retry-delay 2 "$CA_URL" \
    | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/root-ca.pem"
else
  curl -fsSL --retry 3 --retry-delay 2 \
    https://www.amazontrust.com/repository/AmazonRootCA1.pem \
    | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/root-ca.pem"
fi

install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/agent.env" <<EOF
DEVICE_ID=$DEVICE_ID
IOT_DATA_ENDPOINT=$ENDPOINT
CERT_PATH=$CONF_DIR/device.cert.pem
KEY_PATH=$CONF_DIR/device.key.pem
CA_PATH=$CONF_DIR/root-ca.pem
MODE=real
# KVS WebRTC media runtime (supervised by the agent's media collector; it reuses
# the IoT cert above for KVS creds — channels/endpoints live in kvs.json). On for
# every provisioned device; turn a camera off with
# 'xorgate set recording.video.cameras.<streamKey> off' or from the web UI.
# (No backticks in this heredoc: its delimiter is unquoted, so a backtick
# would command-substitute at install time and mangle the written file.)
KVS_RUNTIME_BIN=$INSTALL_PREFIX/media/xorgate-kvs-webrtc
KVS_RUNTIME_LIB_DIR=$INSTALL_PREFIX/media/lib
KVS_MANIFEST=$CONF_DIR/kvs.json
# Device hardware block (cm4-support): written in step 8 when the claim
# resolves a v2 device model; the agent falls back to legacy CM5 behavior
# when the file is absent.
HARDWARE_MANIFEST=$CONF_DIR/hardware.json
# Per-stream GStreamer pipelines: drop <streamKey>.gst here (e.g. cam0.gst), or
# manage them with 'xorgate pipeline set <streamKey> <file>'.
KVS_PIPELINE_DIR=$CONF_DIR/pipelines
# Where 'xorgate update' pulls new builds from.
DEVICE_DIST_BUCKET_URL=$DIST_BUCKET_URL
DEVICE_DIST_VERSION=$DIST_VERSION
EOF

# 4. Fetch + extract the device tarball.
TARBALL="xorgate-agent-$DIST_VERSION.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

log "downloading $DIST_BUCKET_URL/$TARBALL"
curl -fsSL --retry 3 --retry-delay 2 "$DIST_BUCKET_URL/$TARBALL" -o "$TMP/$TARBALL"

install -d -m 0755 "$INSTALL_PREFIX"
tar -xzf "$TMP/$TARBALL" -C "$INSTALL_PREFIX"

# 5. Install runtime node modules for the target arch. Files stay root-owned
#    (the agent runs as root); no chown needed.
log "installing node modules in $INSTALL_PREFIX"
(cd "$INSTALL_PREFIX" && npm ci --omit=dev --no-audit --no-fund)

# 5b. Install the `xorgate` CLI as a thin wrapper that defaults XORGATE_HOME to
#     the systemd agent's state dir, so `xorgate status`/`monitor` reflect the
#     running service (no sudo needed when the agent runs as you — see the
#     drop-in below). Use systemctl/journalctl for lifecycle — `xorgate start`
#     would launch a SECOND worker that fights the systemd one for the IoT clientId.
# rm first: `cat >` follows an existing symlink and would clobber its target
# (e.g. a prior `xorgate -> dist/cli.js` symlink).
rm -f /usr/local/bin/xorgate
cat > /usr/local/bin/xorgate <<'WRAP'
#!/usr/bin/env bash
exec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" node /opt/xorgate/dist/cli.js "$@"
WRAP
chmod +x /usr/local/bin/xorgate

# 6. systemd.
install -m 0644 "$INSTALL_PREFIX/systemd/xorgate-agent.service" "$UNIT_DEST"

# Run the service as the provisioning user via a drop-in (keeps the shipped unit
# generic). StateDirectory=xorgate then makes systemd create/own /var/lib/xorgate
# as this user, so its pidfile + state are readable without sudo. systemd reads
# EnvironmentFile + sets up StateDirectory as root before dropping privileges.
install -d -m 0755 "$UNIT_DEST.d"
cat > "$UNIT_DEST.d/10-user.conf" <<EOF
[Service]
User=$SVC_USER
Group=$SVC_GROUP
EOF

# Let the agent run the two privileged actions the cloud can trigger, and only
# those. Without this the reboot/power-off commands depend on the distro's
# stock `010_pi-nopasswd` (NOPASSWD: ALL for the `pi` user), which grants far
# more than we need and is absent entirely when provisioning as any other user.
# validate before install: a malformed sudoers file locks out sudo machine-wide.
SUDOERS_TMP="$(mktemp)"
cat > "$SUDOERS_TMP" <<EOF
# Installed by xorgate. Allows the agent (running as $SVC_USER) to act on
# cloud-issued reboot / power-off commands. See device-agent src/agent/commands.ts.
$SVC_USER ALL=(root) NOPASSWD: /usr/bin/systemctl reboot, /usr/bin/systemctl poweroff
EOF
if visudo -cf "$SUDOERS_TMP" >/dev/null 2>&1; then
  install -m 0440 -o root -g root "$SUDOERS_TMP" /etc/sudoers.d/020_xorgate-power
else
  log "WARN: sudoers drop-in failed validation; reboot/power-off commands will not work"
fi
rm -f "$SUDOERS_TMP"

# 6a. Offline time sources (optimize-system-process-cycle plan, Phase 4): the
#     root clock-step helper (agent invokes it via sudo to correct a wrong
#     boot clock from GPS/NITZ while NTP is unsynced), its sudoers grant, and
#     a cron drop-in that tightens fake-hwclock's save cadence to 5 min so a
#     hard crash restores a clock at most 5 min stale.
if [ -f "$INSTALL_PREFIX/system/xorgate-setclock" ]; then
  install -m 0755 -o root -g root "$INSTALL_PREFIX/system/xorgate-setclock" /usr/local/bin/xorgate-setclock
  SUDOERS_TMP="$(mktemp)"
  cat > "$SUDOERS_TMP" <<EOF
# Installed by xorgate. Allows the agent (running as $SVC_USER) to apply an
# offline GPS/NITZ clock step. See device-agent src/agent/clock-step.ts.
$SVC_USER ALL=(root) NOPASSWD: /usr/local/bin/xorgate-setclock
EOF
  if visudo -cf "$SUDOERS_TMP" >/dev/null 2>&1; then
    install -m 0440 -o root -g root "$SUDOERS_TMP" /etc/sudoers.d/021_xorgate-clock
  else
    log "WARN: clock sudoers drop-in failed validation; offline clock steps will not work"
  fi
  rm -f "$SUDOERS_TMP"
fi
if [ -f "$INSTALL_PREFIX/system/xorgate-fake-hwclock.cron" ]; then
  install -m 0644 -o root -g root "$INSTALL_PREFIX/system/xorgate-fake-hwclock.cron" /etc/cron.d/xorgate-fake-hwclock
fi

# 6a1. Camera mounting rotation (cam-sensor-level-rotate plan): the root helper
#      that sets the `rotation` parameter on a camera's dtoverlay line, plus a
#      sudoers grant for it. /boot/firmware is root-owned and the agent is not,
#      so this helper is the whole privilege surface — it takes sensor/camN/deg
#      arguments and re-validates all three, never a path or a body to write.
#      The rotation is a device-tree parameter: it applies on the NEXT BOOT and
#      the agent never reboots the device itself.
if [ -f "$INSTALL_PREFIX/system/xorgate-camrotate" ]; then
  install -m 0755 -o root -g root "$INSTALL_PREFIX/system/xorgate-camrotate" /usr/local/bin/xorgate-camrotate
  SUDOERS_TMP="$(mktemp)"
  cat > "$SUDOERS_TMP" <<EOF
# Installed by xorgate. Allows the agent (running as $SVC_USER) to set the
# camera overlay's rotation parameter in the boot config, and nothing else.
# See device-agent src/hardware/boot-config.ts + system/xorgate-camrotate.
$SVC_USER ALL=(root) NOPASSWD: /usr/local/bin/xorgate-camrotate
EOF
  if visudo -cf "$SUDOERS_TMP" >/dev/null 2>&1; then
    install -m 0440 -o root -g root "$SUDOERS_TMP" /etc/sudoers.d/023_xorgate-camrotate
  else
    log "WARN: camrotate sudoers drop-in failed validation; camera rotation will not apply"
  fi
  rm -f "$SUDOERS_TMP"
fi

# 6a2. Remote update rail (device_update_management plan, Phase 3): a oneshot
#      unit that runs `xorgate update` as root, plus a sudoers grant letting the
#      unprivileged agent START THAT ONE UNIT and nothing else. The agent cannot
#      update itself in-process (it is not root) and cannot spawn the updater as
#      a child (the child would die with its own `systemctl restart
#      xorgate-agent`), so systemd owns the job. Never enabled — started on
#      demand only.
if [ -f "$INSTALL_PREFIX/systemd/xorgate-updater.service" ]; then
  install -m 0644 -o root -g root "$INSTALL_PREFIX/systemd/xorgate-updater.service" \
    /etc/systemd/system/xorgate-updater.service
  SUDOERS_TMP="$(mktemp)"
  cat > "$SUDOERS_TMP" <<EOF
# Installed by xorgate. Allows the agent (running as $SVC_USER) to start the
# one-shot self-update unit on a cloud-issued 'update' command, and nothing
# else. Exact-args on purpose: no wildcard, so this cannot become "run any unit
# as root". See device-agent src/agent/commands.ts.
$SVC_USER ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater.service
EOF
  if visudo -cf "$SUDOERS_TMP" >/dev/null 2>&1; then
    install -m 0440 -o root -g root "$SUDOERS_TMP" /etc/sudoers.d/022_xorgate-update
  else
    log "WARN: update sudoers drop-in failed validation; remote updates will not work"
  fi
  rm -f "$SUDOERS_TMP"
fi

# 6b. Keep POSIX shared memory alive across ssh logouts. logind's default
#     RemoveIPC=yes deletes a user's shm segments when their last login session
#     ends — that destroys the shmsink bridge areas of bridged camera pipelines
#     mid-flight, and any later-connecting KVS master then shm_open-fails
#     forever (black streams; hardware-verified, offline-recording plan spike
#     notes gotcha #5). A drop-in keeps the distro's logind.conf pristine;
#     restarting logind applies it and does not disturb existing sessions.
install -d -m 0755 /etc/systemd/logind.conf.d
cat > /etc/systemd/logind.conf.d/10-xorgate-removeipc.conf <<'EOF'
# Installed by xorgate. Bridged camera pipelines publish frames through POSIX
# shared memory owned by the agent user; logind must not remove it when the
# last ssh session closes.
[Login]
RemoveIPC=no
EOF
systemctl try-restart systemd-logind 2>/dev/null \
  || log "WARN: could not restart systemd-logind; RemoveIPC=no applies on next boot"

systemctl daemon-reload
systemctl enable --now xorgate-agent

# 7. SIM7600 LTE connection manager (cellular uplink + GPS NMEA source).
#    Best-effort and self-gating like the IMU step (1b): a device without the
#    modem still installs cleanly. The daemon owns the modem AT port, brings up
#    the cellular link, and publishes signal telemetry the agent's `lte`
#    collector reads (/run/sim7600-lte/telemetry.json); it also enables GNSS so
#    the agent's `gps` collector can read NMEA on /dev/ttyUSB1.
log "installing SIM7600 connection manager (LTE + GPS)"
case "${ID:-}${ID_LIKE:-}" in
  *debian*|*ubuntu*)
    apt-get install -y -qq isc-dhcp-client usbutils \
      || log "WARN: isc-dhcp-client/usbutils install failed; LTE may not come up"
    ;;
esac

# ModemManager claims the AT port and fights the daemon for it — disable it.
systemctl disable --now ModemManager.service 2>/dev/null || true

# The agent reads NMEA on /dev/ttyUSB1; the non-root agent user needs 'dialout'.
if getent group dialout >/dev/null 2>&1 && [ "$SVC_USER" != "root" ]; then
  usermod -aG dialout "$SVC_USER" || log "WARN: could not add $SVC_USER to the dialout group"
fi

# Install the daemon + its systemd unit + udev rule from the extracted tarball.
if [ -f "$INSTALL_PREFIX/modem/sim7600-lte.sh" ]; then
  install -m 0755 "$INSTALL_PREFIX/modem/sim7600-lte.sh" /usr/local/bin/sim7600-lte.sh
  install -m 0644 "$INSTALL_PREFIX/modem/sim7600-lte.service" /etc/systemd/system/sim7600-lte.service
  install -m 0644 "$INSTALL_PREFIX/modem/99-sim7600-lte.rules" /etc/udev/rules.d/99-sim7600-lte.rules
  udevadm control --reload-rules 2>/dev/null || true
  # usb0 is owned exclusively by the sim7600-lte daemon (its own dhclient +
  # end-to-end health checks). NetworkManager must not manage it, or it races
  # the daemon for the modem's internal DHCP lease and leaves stale duplicate
  # addresses that wedge reconnection after an LTE outage.
  if [ -f "$INSTALL_PREFIX/modem/99-sim7600-unmanaged.conf" ]; then
    install -d -m 0755 /etc/NetworkManager/conf.d
    install -m 0644 "$INSTALL_PREFIX/modem/99-sim7600-unmanaged.conf" /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf
    systemctl reload NetworkManager 2>/dev/null || true
  fi
  systemctl daemon-reload
  # Enable so it persists + starts now if the modem is already enumerated; the
  # udev rule also starts it on hotplug. Failure here only disables LTE/GPS.
  systemctl enable --now sim7600-lte.service 2>/dev/null \
    || systemctl enable sim7600-lte.service 2>/dev/null \
    || log "WARN: could not enable sim7600-lte.service; LTE/GPS disabled"
else
  log "WARN: modem assets missing from build ($INSTALL_PREFIX/modem); LTE/GPS disabled"
fi

# 7b. Uplink priority: prefer WiFi over the LTE modem for the IPv4 default route.
#     usb0 is unmanaged by NM (see 7); the sim7600-lte daemon installs the LTE
#     default route itself at metric 100. Dropping WiFi's metric to 50 makes WiFi
#     win whenever it is associated, with LTE as the fallback (used only when
#     WiFi is down/absent). Without this, a dead-but-up LTE link (antenna out /
#     no signal, USB net iface still up) could win the route and black-hole
#     IPv4 — including KVS WebRTC video, whose endpoints are IPv4-only.
#     Best-effort and self-gating: only applied when NetworkManager is the
#     active stack. The SIM7600 manager is unaffected — its health checks
#     bind to usb0 directly (ping -I usb0), independent of the default route.
#     NOTE: this is link-based, not connectivity-based; it assumes the device only
#     joins trusted WiFi that has real internet (a captive/dead AP would black-hole).
if command -v nmcli >/dev/null 2>&1 && systemctl is-active --quiet NetworkManager 2>/dev/null; then
  log "setting WiFi to outrank the LTE modem for the IPv4 default route"
  install -d -m 0755 /etc/NetworkManager/conf.d
  cat > /etc/NetworkManager/conf.d/10-uplink-priority.conf <<'NMEOF'
# Prefer WiFi over the LTE modem (usb0) for the IPv4 default route.
# WiFi connections get route-metric 50; usb0 is unmanaged by NM and the
# sim7600-lte daemon installs its default route at metric 100, so WiFi wins
# whenever it is associated. LTE stays the fallback, used only when WiFi is
# down or absent. Interface-bound health checks in the SIM7600 manager
# (ping -I usb0) are unaffected by this.
[connection-wifi-priority]
match-device=type:wifi
ipv4.route-metric=50
NMEOF
  systemctl reload NetworkManager 2>/dev/null || true
else
  log "NOTE: NetworkManager not active; skipping WiFi/LTE route-priority config"
fi

# 8. KVS WebRTC media runtime — deps + manifest. Best-effort and self-gating like
#    the IMU (1b) and SIM7600 (7) steps: a device that won't stream still installs
#    cleanly. The aarch64 `xorgate-kvs-webrtc` binary ships in the tarball under
#    media/; its KVS SDK .so libs are supplied separately into media/lib. The media
#    collector self-gates on arch/binary/manifest, so it stays inert until the libs land.
log "installing KVS WebRTC media runtime deps"
case "${ID:-}${ID_LIKE:-}" in
  *debian*|*ubuntu*)
    # apt-provided runtime libs the binary links + the GStreamer elements its
    # pipeline needs at runtime (videotestsrc from -base, x264enc from -ugly,
    # libcamerasrc from -libcamera). glib + its deps (libffi, pcre2, …) and the
    # gst base/app libs come in transitively with these.
    apt-get install -y -qq \
        libglib2.0-0 libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 \
        gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-tools \
      || log "WARN: GStreamer runtime install failed; media streaming disabled. Re-run: sudo apt-get install -y gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly"
    # libcamera source (Raspberry Pi cameras); absent on some distros — ignore.
    apt-get install -y -qq gstreamer1.0-libcamera 2>/dev/null || true
    # NOTE: the rest are NOT apt packages — the KVS SDK libs, its vendored
    # OpenSSL 1.1 (libssl/libcrypto.so.1.1, which differ from the system
    # OpenSSL 3 on current Pi OS), and libwebsockets ship in the bundle at
    # $INSTALL_PREFIX/media/lib and are found via LD_LIBRARY_PATH.
    ;;
esac

# Write the KVS manifest the media collector reads — region, IoT credential
# endpoint, role alias, and per-channel names, all straight from the claim
# response. Mode 0600, owned by the service user (matches the cert files).
echo "$CLAIM_RESPONSE" \
  | jq '{deviceId, region, credentialEndpoint, roleAlias, channels: (.channels // []), uploadBucket}' \
  | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/kvs.json"

# Hardware block (cm4-support plan): the device-facing projection of the
# resolved device model — platform, encoder token, connector/sensor list,
# LED/rails/IMU. Seeds the agent until the retained cloud-config message
# (which also carries it) arrives. Absent for v1 models: the agent then keeps
# its legacy CM5 behavior, matching what those devices run today.
if echo "$CLAIM_RESPONSE" | jq -e '.hardware' >/dev/null 2>&1; then
  echo "$CLAIM_RESPONSE" | jq '.hardware' \
    | install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/hardware.json"
  log "hardware block written ($CONF_DIR/hardware.json)"
fi

# Per-stream pipeline dir (operator drops <streamKey>.gst files here for cameras).
install -d -m 0755 -o "$SVC_USER" -g "$SVC_GROUP" "$CONF_DIR/pipelines"

# Per-stream pipelines (cm4-support plan, Phase 3).
#
# HARDWARE-BLOCK DEVICES (the claim resolved a schemaVersion-2 model —
# hardware.json was written above): the installer writes NO pipelines. The
# agent owns them now: on the first post-reboot start it enumerates cameras,
# pairs them to the block's connector list, and renders the bridged trio from
# its tested per-encoder templates (device-agent media/hardware-reconcile.ts).
# Install-time rendering is impossible anyway — the overlays land in step 8b
# and the cameras only exist after the reboot.
#
# LEGACY FALLBACK (no hardware block: a v1 model, or an old backend): keep
# writing the historical CM5 trio VERBATIM, else a fresh legacy device would
# boot with no pipelines and no agent machinery to render them. Per camera:
#   camN.gst         shmsrc consumer the KVS master loads
#   camN.camera      libcamera identity the capture daemon opens
#   camN.gst.direct  direct pipeline `xorgate pipeline direct camN` restores
# Written only if the .gst is absent so a later `xorgate pipeline ...`
# customization is never clobbered.
# shm sockets live under XORGATE_HOME/shm (device-agent recording/config.ts);
# XORGATE_HOME is the agent's systemd StateDirectory, /var/lib/xorgate.
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
if [ ! -f "$CONF_DIR/hardware.json" ]; then
  log "no hardware block (legacy model); writing the historical CM5 pipeline defaults"
  write_bridged_pipeline cam0 /base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a
  write_bridged_pipeline cam1 /base/axi/pcie@1000120000/rp1/i2c@70000/imx290@1a
else
  log "hardware block present; the agent renders pipelines on first start after reboot"
fi

# Stage the runtime binary (tar preserves the +x bit, but be defensive) and warn
# if the user-supplied KVS SDK .so libs aren't present yet.
if [ -f "$INSTALL_PREFIX/media/xorgate-kvs-webrtc" ]; then
  chmod 0755 "$INSTALL_PREFIX/media/xorgate-kvs-webrtc" || true
  log "media runtime staged; streams start once the KVS SDK libs and kvs.json are in place"
  if ! ls "$INSTALL_PREFIX"/media/lib/*.so* >/dev/null 2>&1; then
    log "NOTE: KVS SDK .so libs missing in $INSTALL_PREFIX/media/lib; supply them to enable streaming"
  fi
else
  log "WARN: media runtime binary missing from build ($INSTALL_PREFIX/media); streaming disabled"
fi

# 8b. Camera device-tree overlays. Sensors like the IMX290 have no camera
#     EEPROM, so camera_auto_detect never finds them; they only bind with
#     explicit overlays. The overlay LIST comes from the resolved model's
#     hardware block (sensor + connector per camera — cm4-support plan); a
#     device with no block falls back to the historical CM5 dual-IMX290 lines.
#     Idempotent + backed up + best-effort. Takes effect on the NEXT BOOT, so
#     a fresh device needs one reboot after install before streaming works.
CONFIG_TXT=/boot/firmware/config.txt
[ -f "$CONFIG_TXT" ] || CONFIG_TXT=/boot/config.txt
if [ -f "$CONFIG_TXT" ]; then
  cp -n "$CONFIG_TXT" "$CONFIG_TXT.bak-xorgate" 2>/dev/null || true
  CAM_OVERLAY_ADDED=0
  if grep -qE '^camera_auto_detect=' "$CONFIG_TXT"; then
    sed -i 's/^camera_auto_detect=.*/camera_auto_detect=0/' "$CONFIG_TXT"
  else
    echo 'camera_auto_detect=0' >> "$CONFIG_TXT"
  fi
  # One "<sensor>,<camN>" per declared camera: csi0 -> cam0, csi1 -> cam1.
  # Legacy fallback when no hardware block was written.
  OVERLAY_LINES=$(echo "$CLAIM_RESPONSE" | jq -r     '.hardware.video // [] | .[] | "dtoverlay=\(.sensor),\(.connector | sub("^csi"; "cam"))"' 2>/dev/null)
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

log "registered device $DEVICE_ID"
log "tail logs: sudo journalctl -u xorgate-agent -f"
