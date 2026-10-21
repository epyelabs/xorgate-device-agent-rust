#!/bin/bash
# Recreate a standard JS-provisioned device inside a convert-e2e container, as
# the backend install script leaves it. Every block cites
#   be = backend/xorgate-core-service/src/assets/install-script.ts @ 96a91f2
# (a TS template string: `\$`/`\\` there are `$`/`\` in the served script).
# The JS tree comes from the JS repo @ $JS_REF via `git archive` (run.sh puts
# it at /in/js: package.json package-lock.json systemd python modem media
# system), as the JS tarball ships it (JS .github/workflows/device-deploy-dev.yml:98).
#
#   provision-js.sh [--svc pi|root] [--no-user-dropin] [--dist-version <v>]
#                   [--js enabled-active|enabled-inactive|disabled-inactive]
#                   [--bucket <url>]
#
#   --svc            SVC_USER (be:74-85: $SUDO_USER, else root). Default pi.
#   --no-user-dropin no xorgate-agent.service.d/10-user.conf at all (the
#                    WP2 brief's root-svc device; be:268-273 writes one even
#                    for root — that shape is --svc root without this flag)
#   --dist-version   DEVICE_DIST_VERSION in agent.env (be:228). Default latest.
#   --js             the JS unit's state after provisioning. Default
#                    enabled-active (be:386 `enable --now`); enabled-inactive =
#                    the bench exclusive-window shape; disabled-inactive.
#   --bucket         DEVICE_DIST_BUCKET_URL (be:227). Default http://127.0.0.1:8000
#
# Not simulated (outside every I3 root and the converter's reach): apt/node/pip
# installs (be:87-147), the claim (be:149-181; replaced by a local CA +
# device cert so `xorgate-rs run` can parse them), /boot/firmware/config.txt
# (be:567-605), usermod (be:144-146, 405-407).
set -euo pipefail
SVC=pi
USER_DROPIN=1
DIST_VERSION=latest
JS_STATE=enabled-active
BUCKET=http://127.0.0.1:8000
while [ $# -gt 0 ]; do
  case "$1" in
    --svc) SVC=$2; shift 2 ;;
    --no-user-dropin) USER_DROPIN=0; shift ;;
    --dist-version) DIST_VERSION=$2; shift 2 ;;
    --js) JS_STATE=$2; shift 2 ;;
    --bucket) BUCKET=$2; shift 2 ;;
    *) echo "provision-js: unknown arg $1" >&2; exit 2 ;;
  esac
done
JS=/in/js
CONF_DIR=/etc/xorgate
INSTALL_PREFIX=/opt/xorgate
UNIT_DEST=/etc/systemd/system/xorgate-agent.service
umask 022
log() { echo "[provision-js] $*"; }

# Raspberry Pi OS ships this grant for `pi` (the bootstrap one-liner's
# `sudo bash install-rs.sh` relies on it); not written by be, but present on
# every Pi OS device the backend provisions (G6 shows the bench's).
if id pi >/dev/null 2>&1; then
  printf 'pi ALL=(ALL) NOPASSWD: ALL\n' >/etc/sudoers.d/010_pi-nopasswd
  chmod 0440 /etc/sudoers.d/010_pi-nopasswd
fi

# be:52-72 (0a) — the early minimal NM conf; replaced by the tarball copy in 7.
install -d -m 0755 /etc/NetworkManager/conf.d
cat >/etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf <<'NMEOF'
# usb0 is owned exclusively by the xorgate sim7600-lte daemon (its own
# dhclient + health checks); NetworkManager must not manage it. Installed
# early by install.sh; replaced by the fully commented copy from the agent
# tarball later in the same run.
[keyfile]
unmanaged-devices=interface-name:usb0
NMEOF

# be:74-85 (0) — the service user.
SVC_USER=root
SVC_GROUP=root
if [ "$SVC" != root ]; then
  SVC_USER=$SVC
  SVC_GROUP=$(id -gn "$SVC")
fi
log "agent will run as '$SVC_USER'"

# be:104-105 (1) — apt lock timeout (outside the I3 roots; for fidelity).
install -d -m 0755 /etc/apt/apt.conf.d
echo 'DPkg::Lock::Timeout "600";' >/etc/apt/apt.conf.d/99xorgate-lock-timeout

# be:183-201 (3) — /etc/xorgate 0700 svc + cert/key/CA 0600 svc. The claim's
# PEMs are replaced by a throwaway local CA + device cert (X.509 v3: rustls
# rejects v1), so `xorgate-rs run` parses them and stays up, reconnecting to a
# closed 127.0.0.1:8883.
install -d -m 0700 -o "$SVC_USER" -g "$SVC_GROUP" "$CONF_DIR"
T=$(mktemp -d)
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$T/ca.key" \
  -out "$T/ca.pem" -days 3650 -subj /CN=convert-e2e-ca 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$T/dev.key" \
  -out "$T/dev.csr" -subj /CN=e2e-device 2>/dev/null
printf 'basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=clientAuth\n' >"$T/ext"
openssl x509 -req -in "$T/dev.csr" -CA "$T/ca.pem" -CAkey "$T/ca.key" -CAcreateserial \
  -out "$T/dev.pem" -days 3650 -extfile "$T/ext" 2>/dev/null
install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" "$T/dev.pem" "$CONF_DIR/device.cert.pem"
install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" "$T/dev.key" "$CONF_DIR/device.key.pem"
install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" "$T/ca.pem" "$CONF_DIR/root-ca.pem"
rm -rf "$T"

# be:203-229 (3) — agent.env, the exact heredoc (unquoted delimiter: values
# substituted at install time).
DEVICE_ID=e2e00000-0000-4000-8000-00000000c0de
ENDPOINT=127.0.0.1
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
DEVICE_DIST_BUCKET_URL=$BUCKET
DEVICE_DIST_VERSION=$DIST_VERSION
EOF

# be:231-240 (4) — extract the JS tarball into /opt/xorgate (root, as root's
# `tar -xzf` restores the archive's owners; git archive records 0:0).
install -d -m 0755 "$INSTALL_PREFIX"
cp -a "$JS"/. "$INSTALL_PREFIX"/
install -d -m 0755 "$INSTALL_PREFIX/dist"
# dist/ is the tsc build (not in git): stubs — the node-stub only needs the paths.
printf '// convert-e2e stub of the JS agent worker (the real one is tsc output)\n' >"$INSTALL_PREFIX/dist/worker.js"
printf '// convert-e2e stub of the JS agent CLI\n' >"$INSTALL_PREFIX/dist/cli.js"
printf '{ "version": "89b5c64", "channel": "dev", "builtAt": "2026-10-09T00:00:00Z", "packageVersion": "0.1.2" }\n' \
  >"$INSTALL_PREFIX/dist/build-info.json"
# be:242-245 (5) — `npm ci --omit=dev`: a stub node_modules/ (root-owned).
install -d -m 0755 "$INSTALL_PREFIX/node_modules/mqtt" "$INSTALL_PREFIX/node_modules/.bin"
printf '{ "name": "mqtt", "version": "5.0.0" }\n' >"$INSTALL_PREFIX/node_modules/mqtt/package.json"
printf 'module.exports = {};\n' >"$INSTALL_PREFIX/node_modules/mqtt/index.js"
ln -sfn ../mqtt/index.js "$INSTALL_PREFIX/node_modules/.bin/mqtt"

# be:247-259 (5b) — the `xorgate` wrapper (rm first, quoted heredoc, chmod +x).
rm -f /usr/local/bin/xorgate
cat >/usr/local/bin/xorgate <<'WRAP'
#!/usr/bin/env bash
exec env XORGATE_HOME="${XORGATE_HOME:-/var/lib/xorgate}" node /opt/xorgate/dist/cli.js "$@"
WRAP
chmod +x /usr/local/bin/xorgate

# be:261-262 (6) — the JS unit.
install -m 0644 "$INSTALL_PREFIX/systemd/xorgate-agent.service" "$UNIT_DEST"

# be:264-273 (6) — 10-user.conf (unconditional in be, even for root).
if [ "$USER_DROPIN" = 1 ]; then
  install -d -m 0755 "$UNIT_DEST.d"
  cat >"$UNIT_DEST.d/10-user.conf" <<EOF
[Service]
User=$SVC_USER
Group=$SVC_GROUP
EOF
fi

# sudoers_drop <dest> <warn>: be's visudo-before-install idiom (be:279-291 …).
sudoers_drop() {
  local dest=$1 tmp=$SUDOERS_TMP
  if visudo -cf "$tmp" >/dev/null 2>&1; then
    install -m 0440 -o root -g root "$tmp" "$dest"
  else
    log "WARN: $2"
  fi
  rm -f "$tmp"
}

# be:275-291 (6) — 020_xorgate-power.
SUDOERS_TMP="$(mktemp)"
cat >"$SUDOERS_TMP" <<EOF
# Installed by xorgate. Allows the agent (running as $SVC_USER) to act on
# cloud-issued reboot / power-off commands. See device-agent src/agent/commands.ts.
$SVC_USER ALL=(root) NOPASSWD: /usr/bin/systemctl reboot, /usr/bin/systemctl poweroff
EOF
sudoers_drop /etc/sudoers.d/020_xorgate-power "sudoers drop-in failed validation; reboot/power-off commands will not work"

# be:293-315 (6a) — setclock helper + 021 + the fake-hwclock cron.
if [ -f "$INSTALL_PREFIX/system/xorgate-setclock" ]; then
  install -m 0755 -o root -g root "$INSTALL_PREFIX/system/xorgate-setclock" /usr/local/bin/xorgate-setclock
  SUDOERS_TMP="$(mktemp)"
  cat >"$SUDOERS_TMP" <<EOF
# Installed by xorgate. Allows the agent (running as $SVC_USER) to apply an
# offline GPS/NITZ clock step. See device-agent src/agent/clock-step.ts.
$SVC_USER ALL=(root) NOPASSWD: /usr/local/bin/xorgate-setclock
EOF
  sudoers_drop /etc/sudoers.d/021_xorgate-clock "clock sudoers drop-in failed validation; offline clock steps will not work"
fi
if [ -f "$INSTALL_PREFIX/system/xorgate-fake-hwclock.cron" ]; then
  install -d -m 0755 /etc/cron.d
  install -m 0644 -o root -g root "$INSTALL_PREFIX/system/xorgate-fake-hwclock.cron" /etc/cron.d/xorgate-fake-hwclock
fi

# be:317-339 (6a1) — camrotate helper + 023.
if [ -f "$INSTALL_PREFIX/system/xorgate-camrotate" ]; then
  install -m 0755 -o root -g root "$INSTALL_PREFIX/system/xorgate-camrotate" /usr/local/bin/xorgate-camrotate
  SUDOERS_TMP="$(mktemp)"
  cat >"$SUDOERS_TMP" <<EOF
# Installed by xorgate. Allows the agent (running as $SVC_USER) to set the
# camera overlay's rotation parameter in the boot config, and nothing else.
# See device-agent src/hardware/boot-config.ts + system/xorgate-camrotate.
$SVC_USER ALL=(root) NOPASSWD: /usr/local/bin/xorgate-camrotate
EOF
  sudoers_drop /etc/sudoers.d/023_xorgate-camrotate "camrotate sudoers drop-in failed validation; camera rotation will not apply"
fi

# be:341-365 (6a2) — the JS updater unit + 022.
if [ -f "$INSTALL_PREFIX/systemd/xorgate-updater.service" ]; then
  install -m 0644 -o root -g root "$INSTALL_PREFIX/systemd/xorgate-updater.service" \
    /etc/systemd/system/xorgate-updater.service
  SUDOERS_TMP="$(mktemp)"
  cat >"$SUDOERS_TMP" <<EOF
# Installed by xorgate. Allows the agent (running as $SVC_USER) to start the
# one-shot self-update unit on a cloud-issued 'update' command, and nothing
# else. Exact-args on purpose: no wildcard, so this cannot become "run any unit
# as root". See device-agent src/agent/commands.ts.
$SVC_USER ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater.service
EOF
  sudoers_drop /etc/sudoers.d/022_xorgate-update "update sudoers drop-in failed validation; remote updates will not work"
fi

# be:367-383 (6b) — logind RemoveIPC=no drop-in.
install -d -m 0755 /etc/systemd/logind.conf.d
cat >/etc/systemd/logind.conf.d/10-xorgate-removeipc.conf <<'EOF'
# Installed by xorgate. Bridged camera pipelines publish frames through POSIX
# shared memory owned by the agent user; logind must not remove it when the
# last ssh session closes.
[Login]
RemoveIPC=no
EOF

# be:385-386 (6) — daemon-reload; enable --now xorgate-agent (the systemctl
# shim makes the wants/ symlink from WantedBy= and starts the node-stub).
systemctl daemon-reload
systemctl enable --now xorgate-agent 2>/dev/null

# be:388-433 (7) — the SIM7600 daemon, unit, udev rule, full NM conf;
# daemon-reload; enable --now sim7600-lte (the shim only marks it active,
# never runs the daemon).
if [ -f "$INSTALL_PREFIX/modem/sim7600-lte.sh" ]; then
  install -m 0755 "$INSTALL_PREFIX/modem/sim7600-lte.sh" /usr/local/bin/sim7600-lte.sh
  install -m 0644 "$INSTALL_PREFIX/modem/sim7600-lte.service" /etc/systemd/system/sim7600-lte.service
  install -d -m 0755 /etc/udev/rules.d
  install -m 0644 "$INSTALL_PREFIX/modem/99-sim7600-lte.rules" /etc/udev/rules.d/99-sim7600-lte.rules
  if [ -f "$INSTALL_PREFIX/modem/99-sim7600-unmanaged.conf" ]; then
    install -d -m 0755 /etc/NetworkManager/conf.d
    install -m 0644 "$INSTALL_PREFIX/modem/99-sim7600-unmanaged.conf" /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf
  fi
  systemctl daemon-reload
  systemctl enable --now sim7600-lte.service 2>/dev/null
fi

# be:435-464 (7b) — WiFi outranks LTE (NM is the active stack on a Pi OS
# Bookworm device, so be writes it).
install -d -m 0755 /etc/NetworkManager/conf.d
cat >/etc/NetworkManager/conf.d/10-uplink-priority.conf <<'NMEOF'
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

# be:491-497 (8) — kvs.json (jq's pretty-printed projection of the claim).
install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/kvs.json" <<EOF
{
  "deviceId": "$DEVICE_ID",
  "region": "us-east-1",
  "credentialEndpoint": "127.0.0.1",
  "roleAlias": "e2e-role-alias",
  "channels": [],
  "uploadBucket": "e2e-upload-bucket"
}
EOF
# be:499-508 (8) — hardware.json (a v2 model: the CM5 dual-IMX290 bench shape).
install -m 0600 -o "$SVC_USER" -g "$SVC_GROUP" /dev/stdin "$CONF_DIR/hardware.json" <<'EOF'
{
  "v": 2,
  "platform": "rpi-cm5",
  "encoder": "sw-x264",
  "rails": true,
  "statusLed": null,
  "imu": "bno085",
  "video": [
    {
      "key": "cam0",
      "connector": "csi0",
      "sensor": "imx290",
      "codec": "h264",
      "label": "Front"
    }
  ]
}
EOF
# be:510-511 (8) — the pipelines dir 0755 svc. With a hardware block be
# writes no pipelines (be:528-530, 551-553); the agent renders them later.
install -d -m 0755 -o "$SVC_USER" -g "$SVC_GROUP" "$CONF_DIR/pipelines"
# be:557-559 (8) — chmod the KVS runtime binary.
[ -f "$INSTALL_PREFIX/media/xorgate-kvs-webrtc" ] && chmod 0755 "$INSTALL_PREFIX/media/xorgate-kvs-webrtc"

# The running JS agent's state (JS unit StateDirectory=xorgate, mode 0700,
# owned by the service user): a small settings.json + config-sync.json.
install -d -m 0700 -o "$SVC_USER" -g "$SVC_GROUP" /var/lib/xorgate
printf '{\n  "recording": { "enabled": false }\n}\n' >/var/lib/xorgate/settings.json
printf '{ "rev": 7, "appliedAt": "2026-10-20T00:00:00.000Z" }\n' >/var/lib/xorgate/config-sync.json
chown "$SVC_USER:$SVC_GROUP" /var/lib/xorgate/settings.json /var/lib/xorgate/config-sync.json
chmod 0600 /var/lib/xorgate/settings.json /var/lib/xorgate/config-sync.json

# The JS state the case asks for.
case "$JS_STATE" in
  enabled-active) ;;
  enabled-inactive) systemctl stop xorgate-agent ;;
  disabled-inactive) systemctl disable --now xorgate-agent 2>/dev/null ;;
  *) echo "provision-js: bad --js $JS_STATE" >&2; exit 2 ;;
esac
log "JS-provisioned: svc=$SVC_USER dropin=$USER_DROPIN dist=$DIST_VERSION js=$(systemctl is-enabled xorgate-agent)/$(systemctl is-active xorgate-agent)"
