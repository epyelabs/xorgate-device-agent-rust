#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016,SC2181  # test assertions: `a && ok || bad`, literal stub bodies, `[ $? … ]` after a capture
# Phase 8 WP-T: the bench-window tools (scripts/bench/p8-*.sh) against a
# SANDBOX bench tree (P8_ROOT), with systemctl/systemd-run/dpkg/apt-get/
# vcgencmd/ip stubbed. The real-systemd half (dead-man boot path F1, the
# timer, F3 with a real dpkg lock, the backup-install-restore rehearsal) is
# scripts/bench/tests/systemd/p8-window.sh. Run as root in a throwaway
# bookworm container (hl.sh `suites` does):
#   docker run --rm -v "$PWD:/src:ro" debian:bookworm bash -c 'cp -r /src /w && bash /w/scripts/bench/tests/p8-window.sh'
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
B=$REPO/scripts/bench
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
[ "$(id -u)" = 0 ] || { echo "FAIL: run as root (in a container)"; exit 2; }
command -v python3 >/dev/null || { echo "FAIL: python3 missing"; exit 2; }
id pi >/dev/null 2>&1 || useradd -m pi
getent group p8grp >/dev/null || groupadd p8grp

TMP=$(mktemp -d)
STUB=$TMP/stub; SD=$TMP/sd; mkdir -p "$STUB" "$SD"
export SD PATH="$STUB:$PATH"

# ----------------------------------------------------------------- stubs
cat > "$STUB/systemctl" <<'S'
#!/usr/bin/env bash
n() { local u=${1%.service}; echo "${u//\//_}"; }
cmd=$1; shift
echo "$(date +%s.%N) systemctl $cmd $*" >> "$SD/log"
nb=0; now=0; args=()
for a in "$@"; do case $a in --no-block) nb=1 ;; --now) now=1 ;; --*|-q) ;; *) args+=("$a") ;; esac; done
case $cmd in
  is-active) s=$(cat "$SD/active.$(n "${args[0]}")" 2>/dev/null || echo inactive); echo "$s"; [ "$s" = active ] ;;
  is-enabled) s=$(cat "$SD/enabled.$(n "${args[0]}")" 2>/dev/null || echo disabled); echo "$s"; [ "$s" = enabled ] ;;
  show) p=""; u=""; set -- "$@"
        while [ $# -gt 0 ]; do case $1 in -p) p=$2; shift 2 ;; --value) shift ;; *) u=$1; shift ;; esac; done
        cat "$SD/prop.$(n "$u").$p" 2>/dev/null || echo "" ;;
  list-units) [ ${#args[@]} -gt 0 ] || args=('*'); while read -r l; do u=${l%% *}; for pat in "${args[@]}"; do case $u in $pat) echo "$l"; break ;; esac; done; done < <(cat "$SD/units" 2>/dev/null) ;;
  list-jobs) cat "$SD/jobs" 2>/dev/null; : > "$SD/jobs" ;;
  cancel) echo "${args[0]}" >> "$SD/cancelled" ;;
  stop) for u in "${args[@]}"; do [ -f "$SD/ON_STOP_ACTIVATE.$(n "$u")" ] && { echo active > "$SD/active.$(cat "$SD/ON_STOP_ACTIVATE.$(n "$u")")"; rm -f "$SD/ON_STOP_ACTIVATE.$(n "$u")"; }; [ -f "$SD/ON_STOP_ADD.$(n "$u")" ] && { cat "$SD/ON_STOP_ADD.$(n "$u")" >> "$SD/units"; echo active > "$SD/active.xorgate-provision-rs"; rm -f "$SD/ON_STOP_ADD.$(n "$u")"; }; [ -f "$SD/STOPFAIL.$(n "$u")" ] && continue; echo inactive > "$SD/active.$(n "$u")"; sed -i "/^${u%.service}\(\.service\)\? /d" "$SD/units" 2>/dev/null; done ;;
  kill) for u in "${args[@]}"; do [ -f "$SD/KILLFAIL.$(n "$u")" ] && continue; echo inactive > "$SD/active.$(n "$u")"; sed -i "/^${u%.service}\(\.service\)\? /d" "$SD/units" 2>/dev/null; done ;;
  start|restart) for u in "${args[@]}"; do echo active > "$SD/active.$(n "$u")"; done ;;
  enable|disable) for u in "${args[@]}"; do echo "${cmd}d" > "$SD/enabled.$(n "$u")"; [ "$now" = 1 ] && { [ "$cmd" = enable ] && echo active || echo inactive; } > "$SD/active.$(n "$u")"; done; true ;;
  *) true ;;
esac
S
cat > "$STUB/systemd-run" <<'S'
#!/usr/bin/env bash
echo "systemd-run $*" >> "$SD/systemd-run.log"
unit=""; for a in "$@"; do case $a in --unit=*) unit=${a#--unit=} ;; esac; done
[ -n "$unit" ] && echo active > "$SD/active.$unit"
exit "$(cat "$SD/run-rc" 2>/dev/null || echo 0)"
S
cat > "$STUB/dpkg" <<'S'
#!/usr/bin/env bash
echo "dpkg $*" >> "$SD/dpkg.log"
c=$(cat "$SD/audit_n" 2>/dev/null || echo 0)
case "$1" in
  --audit) [ "$c" -gt 0 ] && echo "The following packages are only half configured: p8dummy" ; exit 0 ;;
  --configure) [ "$(cat "$SD/configure_fixes" 2>/dev/null || echo 0)" = 1 ] && echo 0 > "$SD/audit_n"; exit 0 ;;
esac
S
cat > "$STUB/apt-get" <<'S'
#!/usr/bin/env bash
echo "apt-get $*" >> "$SD/dpkg.log"; echo 0 > "$SD/audit_n"
S
cat > "$STUB/dpkg-query" <<'S'
#!/usr/bin/env bash
case "$*" in *unattended-upgrades*) cat "$SD/uu" 2>/dev/null ;; *) cat "$SD/dpkg-list" 2>/dev/null ;; esac; true
S
cat > "$STUB/apt-config" <<'S'
#!/usr/bin/env bash
echo 'APT::Architecture "arm64";'; cat "$SD/aptconf" 2>/dev/null; true
S
printf '#!/bin/sh\necho throttled=0x0\n' > "$STUB/vcgencmd"
printf '#!/bin/sh\nif [ "$1" = route ] && [ "${2:-}" = get ]; then echo "1.1.1.1 via 10.0.0.1 dev wlan0 src 10.0.0.2"; else cat "$SD/routes" 2>/dev/null; fi\n' > "$STUB/ip"
printf '#!/bin/sh\nexit 0\n' > "$STUB/udevadm"
chmod 0755 "$STUB"/*
echo "default via 10.0.0.1 dev wlan0 metric 50" > "$SD/routes"
printf 'curl 1 ii \npython3 3.11 ii \n' > "$SD/dpkg-list"

# ------------------------------------------------------------ the fake bench
R=$TMP/r
export P8_ROOT=$R P8_SVC_USER=pi P8_LOCK=$TMP/p8.lock P8_START_WAIT_S=2 P8_DPKG_WAIT_S=10
mkbench() {
  rm -rf "$R"; rm -f "$SD"/active.* "$SD"/enabled.* "$SD/units" "$SD/jobs" "$SD/cancelled" "$SD/log"
  local d
  for d in etc/xorgate/pipelines var/lib/xorgate/recordings/cam0 var/lib/xorgate-rs/drive-blackbox var/lib/xorgate-rs/tapes \
           var/lib/xorgate-rs-install opt/xorgate/node_modules/x opt/xorgate-rs/bin opt/xorgate-rs.prev/bin \
           etc/systemd/system/xorgate-agent.service.d etc/systemd/system/xorgate-agent-rs.service.d \
           etc/systemd/system/xorgate-updater-rs.service.d etc/systemd/system/multi-user.target.wants \
           etc/systemd/system/timers.target.wants etc/sudoers.d usr/local/bin etc/udev/rules.d etc/NetworkManager/conf.d \
           etc/NetworkManager/system-connections etc/systemd/logind.conf.d etc/apt/apt.conf.d etc/cron.d boot/firmware \
           etc/modprobe.d root run/sim7600-lte var/lib/systemd/timers var/lib/dpkg var/log; do mkdir -p "$R/$d"; done
  chmod 0700 "$R/root" "$R/etc/xorgate" "$R/var/lib/xorgate" "$R/var/lib/xorgate-rs-install"
  head -c 256 /dev/urandom > "$R/etc/xorgate/device.key.pem"; head -c 512 /dev/urandom > "$R/etc/xorgate/device.cert.pem"
  echo "ca" > "$R/etc/xorgate/root-ca.pem"; echo "DEVICE_ID=prod" > "$R/etc/xorgate/agent.env"; chmod 0644 "$R/etc/xorgate/agent.env"
  echo gst > "$R/etc/xorgate/pipelines/cam0.gst"
  chown -R pi:pi "$R/etc/xorgate" "$R/var/lib/xorgate"
  printf '{"cameraMount":{"cam0":{"rotationDeg":0}},"gnssAntBias":true,"lteRecovery":{"deadZoneProfile":"balanced","gpsFixResetPolicy":"defer"},"mode":"x"}\n' > "$R/var/lib/xorgate/settings.json"
  echo '{}' > "$R/var/lib/xorgate/state.json"; echo seg > "$R/var/lib/xorgate/recordings/cam0/a.mp4"
  echo '{"state":"idle"}' > "$R/var/lib/xorgate/apn-apply.json"; chown root:root "$R/var/lib/xorgate/apn-apply.json"   # WP0 C3
  chown -R pi:pi "$R/var/lib/xorgate/recordings" "$R/var/lib/xorgate/state.json" "$R/var/lib/xorgate/settings.json"
  echo '{"t":1}' > "$R/var/lib/xorgate-rs/drive-blackbox/b1.ndjson"; echo tape > "$R/var/lib/xorgate-rs/tapes/t1"
  printf 'state=converted\nsource=x\n' > "$R/var/lib/xorgate-rs-install/record"
  echo js > "$R/opt/xorgate/node_modules/x/i.js"
  printf '#!/bin/sh\necho prod-rs\n' > "$R/opt/xorgate-rs/bin/xorgate-rs"; chmod 0755 "$R/opt/xorgate-rs/bin/xorgate-rs"
  printf '{"version":"9bd55b9","channel":"bench"}\n' > "$R/opt/xorgate-rs/build-info.json"
  echo prev > "$R/opt/xorgate-rs.prev/bin/xorgate-rs"
  for d in xorgate-agent xorgate-agent-rs xorgate-updater xorgate-updater-rs sim7600-lte xorgate-drive-blackbox; do echo "[Unit] $d" > "$R/etc/systemd/system/$d.service"; done
  echo "[Service] User=pi" > "$R/etc/systemd/system/xorgate-agent.service.d/10-user.conf"
  echo "[Service] WorkingDirectory=/opt/xorgate" > "$R/etc/systemd/system/xorgate-agent.service.d/20-workdir.conf"   # WP0 C5
  echo "[Service] User=pi" > "$R/etc/systemd/system/xorgate-agent-rs.service.d/10-user.conf"
  echo "[Service] Environment=X" > "$R/etc/systemd/system/xorgate-updater-rs.service.d/10-dist.conf"
  ln -s /etc/systemd/system/xorgate-agent-rs.service "$R/etc/systemd/system/multi-user.target.wants/xorgate-agent-rs.service"
  ln -s /etc/systemd/system/sim7600-lte.service "$R/etc/systemd/system/multi-user.target.wants/sim7600-lte.service"
  for d in 010_pi-nopasswd 020_xorgate-power 021_xorgate-clock 023_xorgate-camrotate 024_xorgate-update-rs; do echo "# $d" > "$R/etc/sudoers.d/$d"; chmod 0440 "$R/etc/sudoers.d/$d"; done
  printf '# bench comment (C4)\npi ALL=(root) NOPASSWD: /bin/true\n' > "$R/etc/sudoers.d/022_xorgate-update"; chmod 0440 "$R/etc/sudoers.d/022_xorgate-update"
  for d in xorgate xorgate-rs xorgate-camrotate sim7600-lte.sh; do printf '#!/bin/sh\n# %s\n' "$d" > "$R/usr/local/bin/$d"; chmod 0755 "$R/usr/local/bin/$d"; done
  echo rule > "$R/etc/udev/rules.d/99-sim7600-lte.rules"; echo nm > "$R/etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf"
  echo secret-ssid-profile > "$R/etc/NetworkManager/system-connections/MyHomeSSID.nmconnection"; chmod 0600 "$R/etc/NetworkManager/system-connections/"*
  echo ipc > "$R/etc/systemd/logind.conf.d/10-xorgate-removeipc.conf"; echo 'DPkg::Lock::Timeout "60";' > "$R/etc/apt/apt.conf.d/99xorgate-lock-timeout"
  echo cron > "$R/etc/cron.d/xorgate-fake-hwclock"; printf 'dtparam=i2c_arm=on\ndtoverlay=imx290,cam0\n' > "$R/boot/firmware/config.txt"
  echo i2c-dev > "$R/etc/modules"; : > "$R/etc/modprobe.d/raspi-blacklist.conf"
  mkdir -p "$R/etc/apt/sources.list.d" "$R/etc/apt/keyrings" "$R/usr/share/keyrings"
  echo "deb bookworm main" > "$R/etc/apt/sources.list"; echo "deb raspi" > "$R/etc/apt/sources.list.d/raspi.list"
  echo k1 > "$R/etc/apt/keyrings/x.gpg"; echo k2 > "$R/usr/share/keyrings/y.gpg"
  cp "$R/boot/firmware/config.txt" "$R/boot/firmware/config.txt.bak-xorgate"
  printf '#!/bin/sh\n# setclock fixture\n' > "$R/usr/local/bin/xorgate-setclock"; chmod 0755 "$R/usr/local/bin/xorgate-setclock"
  mkdir -p "$R/etc/systemd/system-generators"; printf '#!/bin/sh\nexit 0\n' > "$R/etc/systemd/system-generators/zz-fixture-gen"; chmod 0755 "$R/etc/systemd/system-generators/zz-fixture-gen"
  printf '{"timestamp":"%s","status":"connected","gnss_ant_bias":1,"cellular":{"state":"ok","apn_applied":"secret.apn.example"},"recovery":{"connect_fails":0,"radio_cycles_since_ok":0,"resets_24h":0,"last_reset":null}}\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$R/run/sim7600-lte/telemetry.json"
  touch "$R/var/lib/dpkg/lock-frontend" "$R/var/lib/dpkg/lock"
  echo 0123456789abcdef0123456789abcdef > "$R/etc/machine-id"; mkdir -p "$R/proc/device-tree"; printf '6a1b2c3d4e5f6071\0' > "$R/proc/device-tree/serial-number"
  echo active > "$SD/active.xorgate-agent-rs"; echo enabled > "$SD/enabled.xorgate-agent-rs"
  echo enabled > "$SD/enabled.sim7600-lte"; echo active > "$SD/active.sim7600-lte"; echo enabled > "$SD/enabled.NetworkManager"
  echo active > "$SD/active.xorgate-drive-blackbox"; echo enabled > "$SD/enabled.xorgate-drive-blackbox"
  echo "Thu 2026-11-12 19:26:41 UTC" > "$SD/prop.sim7600-lte.ActiveEnterTimestamp"; echo 0 > "$SD/prop.sim7600-lte.NRestarts"
  echo "Tue 2030-01-01 00:00:00 UTC" > "$SD/prop.xorgate-agent-rs.ActiveEnterTimestamp"   # "started after the restore"
  export P8_CERT_BIN_SHA; P8_CERT_BIN_SHA=$(sha256sum "$R/opt/xorgate-rs/bin/xorgate-rs" | cut -c1-64)
  W=$R/root/p8-20261114T120000Z
  install -d -m 0700 "$W" "$W/bin"; install -m 0700 "$B"/p8-*.sh "$W/bin/"
}
# A JS-style test install on the moved-aside bench (what install.sh writes).
fake_js_install() {
  mkdir -p "$R/etc/xorgate" "$R/opt/xorgate/node_modules/y" "$R/etc/systemd/system/xorgate-agent.service.d" "$R/opt/xorgate-rs.new"
  head -c 256 /dev/urandom > "$R/etc/xorgate/device.key.pem"; head -c 512 /dev/urandom > "$R/etc/xorgate/device.cert.pem"
  echo "DEVICE_ID=dev" > "$R/etc/xorgate/agent.env"; chmod 0600 "$R/etc/xorgate/agent.env"
  echo y > "$R/opt/xorgate/node_modules/y/i.js"
  echo "[Unit] test-js" > "$R/etc/systemd/system/xorgate-agent.service"; echo "[Service] User=pi" > "$R/etc/systemd/system/xorgate-agent.service.d/10-user.conf"
  ln -s /etc/systemd/system/xorgate-agent.service "$R/etc/systemd/system/multi-user.target.wants/xorgate-agent.service"
  printf '# template comment\npi ALL=(root) NOPASSWD: /bin/true\n' > "$R/etc/sudoers.d/022_xorgate-update"; chmod 0440 "$R/etc/sudoers.d/022_xorgate-update"
  echo "# 020" > "$R/etc/sudoers.d/020_xorgate-power"; printf '#!/bin/sh\nexec node\n' > "$R/usr/local/bin/xorgate"
  echo nm2 > "$R/etc/NetworkManager/conf.d/50-test-new.conf"                      # a NEW system file
  echo ipc2 > "$R/etc/systemd/logind.conf.d/10-xorgate-removeipc.conf"           # T6: logind, apt, cron rewritten
  echo "deb nodesource" > "$R/etc/apt/sources.list.d/nodesource.list"; echo cron2 > "$R/etc/cron.d/xorgate-fake-hwclock"
  printf 'dtparam=i2c_arm=on\ndtoverlay=imx290,cam0\ndtoverlay=extra\n' > "$R/boot/firmware/config.txt.tmp"; mv "$R/boot/firmware/config.txt.tmp" "$R/boot/firmware/config.txt"
  usermod -aG p8grp pi
  echo enabled > "$SD/enabled.xorgate-agent"; echo active > "$SD/active.xorgate-agent"
}
sha() { sha256sum "$1" | cut -c1-64; }
run() { bash "$W/bin/$1" "${@:2}"; }
# pipefail-safe: the output of a verify that is EXPECTED to fail, for grep
vo() { bash "$W/bin/p8-verify.sh" "$@" 2>&1 || true; }

# ======================================================================
echo "== deadman: arm at cap-10 (F5), the units, refuse re-arm, disarm (stamp nit)"
mkbench
t0=$(date -u +%s)
out=$(run p8-deadman.sh arm --window "$W" --cap-min 60 --label w1 2>&1) && ok "arm exit 0" || bad "arm: $out"
cal=$(sed -n 's/^OnCalendar=//p' "$R/etc/systemd/system/p8-deadman.timer")
dl=$(date -u -d "$cal" +%s 2>/dev/null || echo 0)
d=$((dl - t0)); [ "$d" -ge 2995 ] && [ "$d" -le 3005 ] && ok "F5: OnCalendar = arm time + 50 min for a 60-min cap ($d s)" || bad "F5: deadline offset $d s (want 3000)"
oas=$(sed -n 's/^OnActiveSec=\([0-9]*\)s$/\1/p' "$R/etc/systemd/system/p8-deadman.timer")
[ -n "$oas" ] && [ "$oas" -ge 2995 ] && [ "$oas" -le 3001 ] && ok "T4: OnActiveSec=${oas}s (monotonic) next to OnCalendar" || bad "T4: OnActiveSec '$oas'"
grep -qx 'Persistent=true' "$R/etc/systemd/system/p8-deadman.timer" && ok "timer Persistent=true (a reboot past the deadline fires at once)" || bad "no Persistent=true"
grep -qx 'Before=xorgate-agent.service xorgate-agent-rs.service xorgate-updater.service xorgate-updater-rs.service' "$R/etc/systemd/system/p8-deadman-boot.service" \
  && grep -qx 'ConditionPathExists=/root/p8-window.active' "$R/etc/systemd/system/p8-deadman-boot.service" \
  && grep -qx 'WantedBy=multi-user.target' "$R/etc/systemd/system/p8-deadman-boot.service" && ok "boot unit: Before= both agents + updaters, ConditionPathExists=marker, enabled target" || bad "boot unit shape"
grep -q -- '--auto --boot --window /root/p8-20261114T120000Z' "$R/etc/systemd/system/p8-deadman-boot.service" && grep -q -- '--auto --window /root/p8-' "$R/etc/systemd/system/p8-deadman.service" \
  && ok "units run p8-restore.sh --auto [--boot] from the window's bin" || bad "ExecStart lines"
[ "$(grep -c '^Environment=HOME=/root$' "$R/etc/systemd/system/p8-deadman.service" "$R/etc/systemd/system/p8-deadman-boot.service" | awk -F: '{s+=$2} END {print s}')" = 2 ] && ok "HOME=/root in both units (nit)" || bad "HOME=/root missing"
grep -qx 'After=p8-deadman-boot.service' "$R/etc/systemd/system/p8-deadman.service" && ok "timer restore ordered After= the boot restore (no race at boot)" || bad "After= boot missing"
grep -q 'enable.*p8-deadman-boot.service' "$SD/log" && grep -q 'enable --now p8-deadman.timer' "$SD/log" && ok "arm enables the boot unit + starts the timer" || bad "arm systemctl calls"
[ "$(stat -c %a "$R/root/p8-window.active")" = 600 ] && grep -qx 'label=w1' "$R/root/p8-window.active" && ok "marker 0600 with window/label/deadline" || bad "marker"
out=$(run p8-deadman.sh arm --window "$W" --cap-min 60 2>&1); [ $? = 3 ] && ok "re-arm refused (exit 3)" || bad "re-arm: $out"
touch "$R/var/lib/systemd/timers/stamp-p8-deadman.timer"
run p8-deadman.sh disarm >/dev/null
[ ! -e "$R/etc/systemd/system/p8-deadman.timer" ] && [ ! -e "$R/etc/systemd/system/p8-deadman-boot.service" ] && [ ! -e "$R/root/p8-window.active" ] \
  && [ ! -e "$R/var/lib/systemd/timers/stamp-p8-deadman.timer" ] && ok "disarm removes units, marker AND the timer stamp" || bad "disarm left something"
grep -q 'systemctl stop p8-deadman.service\|systemctl stop p8-deadman-boot' "$SD/log" && bad "disarm stopped a dead-man SERVICE (it may be running inside it)" || ok "disarm never stops the service it may run in"
P8_ROOT=$R bash "$W/bin/p8-deadman.sh" arm --window "$R/root/p8-bad" --cap-min 60 >/dev/null 2>&1 && bad "arm accepted a missing window" || ok "arm refuses a missing window"

echo "== baseline + backup: manifest (volatile list), tar without /var/lib/xorgate-rs, 0600"
mkbench
out=$(run p8-verify.sh baseline --window "$W" 2>&1) && ok "baseline exit 0" || bad "baseline: $out"
grep -q '^boot_id=' "$W/baseline.env" && grep -q '^tele_bias=1$' "$W/baseline.env" && grep -q '^record_state=converted$' "$W/baseline.env" && ok "baseline: boot id (C6), bias, record state" || bad "baseline content"
grep -q 'secret.apn' "$W/baseline.env" && bad "baseline holds the APN value" || ok "baseline holds the APN as a sha only"
printf '%s' "$out" | grep -q 'secret' && bad "baseline output printed a secret" || ok "baseline output has no APN"
printf '%s' "$out" | grep -qE 'apn_sha=[0-9a-f]' && bad "T5: baseline printed an APN sha prefix" || ok "T5: baseline prints 'apn_sha recorded' only"
out=$(run p8-backup.sh backup --window "$W" 2>&1); [ $? = 3 ] && ok "backup refused while xorgate-agent-rs is active (T0 first)" || bad "backup ran with PROD active: $out"
echo inactive > "$SD/active.xorgate-agent-rs"
mkdir -p "$R/etc/systemd/system/p8-x.service.d"; touch "$R/etc/systemd/system/p8-deadman.timer" "$R/etc/apt/apt.conf.d/00p8-no-upgrade"
out=$(run p8-backup.sh backup --window "$W" 2>&1) && ok "backup exit 0: $(printf '%s' "$out" | grep '^backup:' | cut -c1-90)" || bad "backup: $out"
rm -rf "$R/etc/systemd/system/p8-x.service.d" "$R/etc/systemd/system/p8-deadman.timer" "$R/etc/apt/apt.conf.d/00p8-no-upgrade"
M=$W/pre.manifest
grep -q '^/var/lib/xorgate-rs/drive-blackbox' "$M" && bad "manifest holds the blackbox (volatile, M4)" || ok "manifest excludes /var/lib/xorgate-rs/drive-blackbox"
grep -q '^/var/lib/xorgate-rs/tapes/t1	' "$M" && ok "manifest keeps the rest of /var/lib/xorgate-rs" || bad "tapes row missing"
grep -q 'p8-' "$M" && bad "manifest holds p8-* units" || ok "manifest excludes the p8-* units"
grep -q '00p8-no-upgrade' "$M" && bad "manifest holds the H1 conf" || ok "manifest excludes the H1 conf"
grep -q $'^/var/lib/xorgate/apn-apply.json\tf\t0644\t0:0\t' "$M" && ok "C3: apn-apply.json in the manifest, root-owned" || bad "C3 row: $(grep apn-apply "$M")"
grep -q '^/etc/systemd/system/xorgate-agent.service.d/20-workdir.conf' "$M" && grep -q '^/etc/sudoers.d/022_xorgate-update' "$M" && grep -q '^/etc/modules' "$M" \
  && grep -q '^/etc/modprobe.d/raspi-blacklist.conf' "$M" && grep -q '^@groups:pi' "$M" && grep -q '^@enabled:ModemManager.service' "$M" \
  && ok "manifest: 20-workdir.conf (C5), 022 (C4), /etc/modules, raspi-blacklist, pi groups, ModemManager enablement (S8)" || bad "manifest rows missing"
[ "$(stat -c %a "$W/backup.tgz")" = 600 ] && [ "$(stat -c %a "$W/pre.manifest")" = 600 ] && ok "backup.tgz + pre.manifest are 0600 in the 0700 window" || bad "modes"
tar -tzf "$W/backup.tgz" > "$TMP/members"; tar -tvzf "$W/backup.tgz" --numeric-owner > "$TMP/members-v"
grep -q '^var/lib/xorgate-rs/' "$TMP/members" && bad "tar holds /var/lib/xorgate-rs (location tapes, M4)" || ok "tar excludes /var/lib/xorgate-rs"
grep -q '^var/lib/xorgate-rs-install/record$' "$TMP/members" && grep -q '^etc/xorgate/device.key.pem$' "$TMP/members" && ok "tar holds the record and the PROD key" || bad "tar members"
[ "$(wc -l < "$TMP/members")" = "$(cat "$W/backup.count")" ] && [ "$(cat "$W/backup.count")" = "$(awk -F '\t' '$2 != "p" && $1 !~ "^/var/lib/xorgate-rs(/|$)"' "$M" | wc -l)" ] \
  && ok "member count == manifest rows (minus /var/lib/xorgate-rs) == $(cat "$W/backup.count")" || bad "member count"
grep -q ' 0/0 .*var/lib/xorgate/apn-apply.json' "$TMP/members-v" && ok "tar keeps the numeric owner of apn-apply.json (C3)" || bad "tar owner of apn-apply.json"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d=={"gnssAntBias":True,"lteRecovery":{"deadZoneProfile":"balanced","gpsFixResetPolicy":"defer"}}, d' "$W/seed-settings.json" \
  && ok "seed = {gnssAntBias, lteRecovery} from PROD, nothing else" || bad "seed content"
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))=={"gnssAntBias":True}' "$W/align.json" && ok "align.json = gnssAntBias only (WP0 C1)" || bad "align.json"
cp "$R/var/lib/xorgate/settings.json" "$TMP/s.json"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); d["cellular"]={"apn":"x.y"}; json.dump(d,open(sys.argv[1],"w"))' "$R/var/lib/xorgate/settings.json"
out=$(run p8-backup.sh seed --window "$W" 2>&1)
grep -q cellular "$W/seed-settings.json" && bad "seed carried cellular (H10)" || ok "seed never carries cellular, even when PROD has it ($(printf '%s' "$out" | grep -o 'cellular=[a-z]*'))"
printf '%s' "$out" | grep -q 'x\.y' && bad "seed printed an APN" || ok "seed output: booleans/names only"
cp "$TMP/s.json" "$R/var/lib/xorgate/settings.json"; run p8-backup.sh seed --window "$W" >/dev/null 2>&1

echo "== pull (Mac side, ssh/scp stubbed): sha on both ends, count, 0700, pulled.ok"
cat > "$STUB/fakessh" <<'S'
#!/usr/bin/env bash
while [ $# -gt 0 ]; do case "$1" in -o) shift 2 ;; -*) shift ;; *) shift; break ;; esac; done
exec bash -c "$*"
S
cat > "$STUB/fakescp" <<'S'
#!/usr/bin/env bash
a=(); for x in "$@"; do case $x in -o) skip=1; continue ;; -q) continue ;; esac; [ "${skip:-0}" = 1 ] && { skip=0; continue; }; a+=("${x#*:}"); done
cp "${a[0]}" "${a[1]}"
S
chmod 0755 "$STUB/fakessh" "$STUB/fakescp"
out=$(P8_SSH=fakessh P8_PRIVATE=$TMP/priv bash "$B/p8-backup.sh" pull --host x --window "$W" 2>&1) && ok "pull exit 0" || bad "pull: $out"
P=$TMP/priv/20261114T120000Z
[ "$(stat -c %a "$TMP/priv")" = 700 ] && [ "$(stat -c %a "$P")" = 700 ] && [ "$(stat -c %a "$P/backup.tgz")" = 600 ] && ok "private copy 0700/0600" || bad "private modes"
[ "$(sha "$P/backup.tgz")" = "$(cat "$W/backup.sha256")" ] && [ "$(cut -c1-64 "$W/pulled.ok")" = "$(cat "$W/backup.sha256")" ] && ok "sha equal both ends; W/pulled.ok marks it" || bad "pull sha/marker"
printf '%s' "$out" | grep -q 'H8' && ok "pull names the H8 hazard" || bad "no H8 note"
echo corrupt >> "$P/backup.tgz"
out=$(P8_SSH=fakessh P8_PRIVATE=$TMP/priv2 bash "$B/p8-backup.sh" pull --host x --window "$W" 2>&1) && ok "re-pull into a fresh dir ok" || bad "re-pull: $out"
out=$(P8_SSH=fakessh P8_PRIVATE=$TMP/x/plans/p bash "$B/p8-backup.sh" pull --host x --window "$W" 2>&1); [ $? = 2 ] && ok "pull refuses a destination under plans/" || bad "plans/ dest accepted"
rm -rf /root/p8-20261114T130000Z
out=$(P8_SSH=fakessh P8_SCP=fakescp bash "$B/p8-backup.sh" stage --host x --ts 20261114T130000Z 2>&1) && [ "$(stat -c %a /root/p8-20261114T130000Z)" = 700 ] \
  && [ "$(find /root/p8-20261114T130000Z/bin -name 'p8-*.sh' -perm 0700 | wc -l)" = 6 ] && ok "stage: W 0700 with the 6 tools (0700)" || bad "stage: $out"
rm -rf /root/p8-20261114T130000Z

echo "== aside: preconditions, the mv -T guard, seed, H1"
rm -f "$W/pulled.ok"
out=$(run p8-backup.sh aside --window "$W" 2>&1); [ $? = 3 ] && ok "aside refused without a verified Mac copy" || bad "aside without pull: $out"
sha "$W/backup.tgz" > "$W/pulled.ok"
out=$(run p8-backup.sh aside --window "$W" 2>&1); [ $? = 3 ] && printf '%s' "$out" | grep -q 'not armed' && ok "aside refused when the dead-man is not armed" || bad "aside unarmed: $out"
run p8-deadman.sh arm --window "$W" --cap-min 60 --label w1 >/dev/null 2>&1
mkdir -p "$W/aside/opt/xorgate"
out=$(run p8-backup.sh aside --window "$W" 2>&1); rc=$?
[ "$rc" = 3 ] && [ -d "$R/var/lib/xorgate/recordings" ] && [ -d "$R/etc/xorgate" ] && [ ! -e "$W/aside.list" ] && ok "mv -T guard: a pre-existing destination refuses BEFORE anything moves (exit 3)" || bad "guard: rc=$rc $out"
rmdir "$W/aside/opt/xorgate"
out=$(run p8-backup.sh aside --window "$W" 2>&1) && ok "aside exit 0: $(printf '%s' "$out" | head -n1 | cut -c1-80)" || bad "aside: $out"
[ ! -e "$R/etc/xorgate" ] && [ ! -e "$R/opt/xorgate" ] && [ ! -e "$R/usr/local/bin/xorgate-rs" ] && [ ! -e "$R/etc/sudoers.d/022_xorgate-update" ] \
  && [ ! -e "$R/etc/systemd/system/xorgate-agent.service.d" ] && [ ! -L "$R/etc/systemd/system/multi-user.target.wants/xorgate-agent-rs.service" ] && ok "agent surface gone from its places" || bad "surface still present"
[ -e "$R/etc/sudoers.d/010_pi-nopasswd" ] && [ -e "$R/usr/local/bin/sim7600-lte.sh" ] && [ -e "$R/etc/systemd/system/sim7600-lte.service" ] && ok "system surface (modem, 010 sudoers) untouched" || bad "system surface moved"
[ "$(ls -A "$R/var/lib/xorgate")" = settings.json ] && [ "$(stat -c '%a %U' "$R/var/lib/xorgate")" = "700 pi" ] && [ "$(stat -c '%a %U' "$R/var/lib/xorgate/settings.json")" = "600 pi" ] \
  && cmp -s "$R/var/lib/xorgate/settings.json" "$W/seed-settings.json" && ok "/var/lib/xorgate = the seed only (0700/0600 pi)" || bad "seed dir"
grep -q 'seed: /var/lib/xorgate moved and seeded in [0-9]* ms' "$W/aside.log" && ok "seed timing logged ($(grep -o 'in [0-9]* ms' "$W/aside.log"))" || bad "no seed timing"
[ "$(stat -c '%U' "$W/aside/var/lib/xorgate/apn-apply.json")" = root ] && ok "C3: apn-apply.json moved with its root owner" || bad "C3 owner after move"
grep -qx 'APT::Get::Upgrade "false";' "$R/etc/apt/apt.conf.d/00p8-no-upgrade" && ok "H1 conf installed" || bad "H1 conf"
grep -c $'^moved\t' "$W/aside.list" | grep -qx 20 && grep -q $'^absent\t/opt/xorgate-rs.new' "$W/aside.list" && ok "aside.list: 20 moved, absent ones recorded" || bad "aside.list: $(cut -f1 "$W/aside.list" | sort | uniq -c | tr '\n' ' ')"
out=$(run p8-backup.sh aside --window "$W" 2>&1); [ $? = 3 ] && ok "a second aside is refused" || bad "second aside ran"

echo "== dev-settings (H10 STOP), sampler (booleans only, F2 summary)"
cp "$W/seed-settings.json" "$R/var/lib/xorgate/settings.json"
run p8-verify.sh dev-settings --window "$W" >/dev/null 2>&1 && ok "dev-settings PASS on the seed" || bad "dev-settings on the seed"
python3 -c 'import json,sys; json.dump({"gnssAntBias":True,"cellular":{"apn":"z"}},open(sys.argv[1],"w"))' "$R/var/lib/xorgate/settings.json"
run p8-verify.sh dev-settings --window "$W" >/dev/null 2>&1; [ $? = 1 ] && ok "dev-settings exit 1 (STOP) on a DEV cellular key" || bad "no STOP on cellular"
python3 -c 'import json,sys; json.dump({"gnssAntBias":True},open(sys.argv[1],"w"))' "$R/var/lib/xorgate/settings.json"
run p8-verify.sh dev-settings --window "$W" >/dev/null 2>&1 && ok "dev-settings PASS with lteRecovery absent (C1)" || bad "C1 absent lteRecovery"
cp "$W/seed-settings.json" "$R/var/lib/xorgate/settings.json"
T=$TMP/m.tsv
run p8-modem-sampler.sh run --out "$T" --baseline "$W/baseline.env" --ticks 2 && ok "sampler run (2 ticks)" || bad "sampler run"
sed -i 's/"gnss_ant_bias":1/"gnss_ant_bias":0/; s/"radio_cycles_since_ok":0/"radio_cycles_since_ok":2/' "$R/run/sim7600-lte/telemetry.json"
run p8-modem-sampler.sh run --out "$T" --baseline "$W/baseline.env" --ticks 3
sed -i 's/"gnss_ant_bias":0/"gnss_ant_bias":1/; s/"radio_cycles_since_ok":2/"radio_cycles_since_ok":0/' "$R/run/sim7600-lte/telemetry.json"
run p8-modem-sampler.sh run --out "$T" --baseline "$W/baseline.env" --ticks 1
[ "$(head -1 "$T" | tr '\t' ' ')" = "utc bias apn_same cell status rung fails cycles resets24 reset_path reset_new fresh" ] && [ "$(wc -l < "$T")" = 7 ] && ok "sampler TSV: header + 6 rows" || bad "sampler TSV shape"
awk -F '\t' 'NR>1 && ($3 != "1" || $5 != "connected" || $12 != "1")' "$T" | grep -q . && bad "sampler apn_same/status/fresh wrong" || ok "sampler: apn_same=1 vs the baseline sha, status, fresh"
awk -F '\t' 'NR==4 && $2 == "0" && $6 == "radio-cycle" && $8 == "2"' "$T" | grep -q . && ok "sampler: bias-off ticks and the derived rung (radio-cycle, count 2)" || bad "sampler rung/bias: $(sed -n 4p "$T")"
grep -q 'secret' "$T" && bad "sampler wrote the APN" || ok "sampler never writes the APN value"
s=$(run p8-modem-sampler.sh summary "$T" 2>&1); [ $? = 0 ] && printf '%s' "$s" | grep -q 'bias_off_ticks=3' && printf '%s' "$s" | grep -q 'final_bias=1' && ok "summary: 3 off ticks, final 1, VERDICT PASS" || bad "summary: $s"
s=$(run p8-modem-sampler.sh summary "$T" --claim-epoch "$(date -u +%s)" 2>&1); printf '%s' "$s" | grep -q 'F2:' && ok "summary --claim-epoch reports the F2 window" || bad "F2 line"

echo "== restore after a JS-style test install: exact, idempotent"
fake_js_install
dev_key=$(sha "$R/etc/xorgate/device.key.pem")
out=$(run p8-restore.sh --window "$W" 2>&1); rc=$?
[ "$rc" = 0 ] && ok "restore exit 0" || { bad "restore rc=$rc"; printf '%s\n' "$out" | tail -n 30; }
grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && ok "verify pre PASS (manifest == pre.manifest) BEFORE the start" || { bad "verify pre"; cat "$W/verify-pre.out"; }
pre_line=$(grep -n 'verify pre: PASS' "$W/restore.log" | head -1 | cut -d: -f1); st_line=$(grep -n 'start --no-block xorgate-agent-rs queued' "$W/restore.log" | head -1 | cut -d: -f1)
[ -n "$pre_line" ] && [ -n "$st_line" ] && [ "$pre_line" -lt "$st_line" ] && ok "the start comes after the verify" || bad "order verify/start"
grep -q 'systemctl start --no-block xorgate-agent-rs' "$SD/log" && ok "PROD started with --no-block" || bad "start not --no-block"
[ "$(sha "$R/etc/xorgate/device.key.pem")" != "$dev_key" ] && [ "$(sha "$W/test-w1/etc/xorgate/device.key.pem")" = "$dev_key" ] && ok "PROD key back; the DEV key is in W/test-w1" || bad "keys"
[ "$(sha "$R/etc/sudoers.d/022_xorgate-update")" = "$(awk -F '\t' '$1 == "/etc/sudoers.d/022_xorgate-update" {print $6}' "$M")" ] && ok "C4: the bench's own 022 back byte-exact" || bad "022"
[ "$(stat -c %U "$R/var/lib/xorgate/apn-apply.json")" = root ] && ok "C3: apn-apply.json back with its root owner" || bad "C3 after restore"
[ -e "$R/etc/systemd/system/xorgate-agent.service.d/20-workdir.conf" ] && ok "C5: 20-workdir.conf back" || bad "C5"
[ ! -e "$R/etc/NetworkManager/conf.d/50-test-new.conf" ] && [ -e "$W/test-w1/extra/etc/NetworkManager/conf.d/50-test-new.conf" ] && ok "a NEW system file moved to test-w1/extra" || bad "new file sweep"
grep -q 'reloading NetworkManager' "$W/restore.log" && ok "NM reload only because an NM file changed" || bad "no NM reload"
[ "$(sha "$R/boot/firmware/config.txt")" = "$(awk -F '\t' '$1 == "/boot/firmware/config.txt" {print $6}' "$M")" ] && ok "config.txt re-extracted from the tar" || bad "config.txt"
id -nG pi | tr ' ' '\n' | grep -qx p8grp && bad "pi still in the test-added group" || ok "pi groups restored"
[ ! -e "$R/opt/xorgate-rs.new" ] && [ -e "$W/test-w1/opt/xorgate-rs.new" ] && ok "/opt/xorgate-rs.new swept into test-w1" || bad ".new sweep"
[ ! -e "$R/etc/apt/apt.conf.d/00p8-no-upgrade" ] && [ ! -e "$R/root/p8-window.active" ] && [ ! -e "$R/etc/systemd/system/p8-deadman.timer" ] && ok "H1 conf, marker, dead-man gone" || bad "artifacts left"
grep -q 'MyHomeSSID' "$W/restore.log" "$W/verify-pre.out" && bad "an NM connection name was printed" || ok "no NM connection name in the logs"
out=$(run p8-restore.sh --window "$W" 2>&1)
[ $? = 0 ] && printf '%s' "$out" | grep -q 'already finished' && ok "a second restore is a no-op (restore.done)" || bad "rerun: $out"
echo active > "$SD/active.xorgate-agent-rs"
out=$(run p8-restore.sh --window "$W" --again 2>&1); [ $? != 0 ] && printf '%s' "$out" | grep -q 'refused' && ok "--again refused while PROD runs" || bad "--again ran over live PROD"

echo "== verify post (stubs) + leftovers + DEV key search"
echo inactive > "$SD/active.xorgate-agent"; echo 1 > "$SD/prop.xorgate-agent-rs.MainPID"
echo '{"t":2}' >> "$R/var/lib/xorgate-rs/drive-blackbox/b1.ndjson"
out=$(run p8-verify.sh post --window "$W" 2>&1)
printf '%s\n' "$out" | grep '^FAIL' | grep -v 'runs /opt/xorgate-rs/bin/xorgate-rs' | grep -q . && { bad "post: unexpected FAIL"; printf '%s\n' "$out" | grep '^FAIL'; } \
  || ok "post: every check PASS except the exe link (no real process in the sandbox)"
cp "$W/test-w1/etc/xorgate/device.key.pem" "$R/tmp-devkey.pem" 2>/dev/null || { mkdir -p "$R/tmp"; cp "$W/test-w1/etc/xorgate/device.key.pem" "$R/tmp/devkey.pem"; }
mkdir -p "$R/home/pi"; cp "$W/test-w1/etc/xorgate/device.key.pem" "$R/home/pi/stray.pem"
vo post --window "$W" | grep -q '^FAIL no DEV key/cert outside' && ok "post FAILs on a DEV key copy outside W/test-*" || bad "DEV key copy not found"
rm -f "$R/home/pi/stray.pem" "$R/tmp-devkey.pem" "$R/tmp/devkey.pem"
# W1 attempt 2: a PUBLIC CA bundle (the KVS SDK's media/cert.pem) in the test tree AND in PROD is not a DEV key
mkdir -p "$W/test-w1/opt/xorgate/media" "$R/opt/xorgate-rs/media"; printf 'PUBLIC CA BUNDLE\n' > "$W/test-w1/opt/xorgate/media/cert.pem"; cp "$W/test-w1/opt/xorgate/media/cert.pem" "$R/opt/xorgate-rs/media/cert.pem"
vo post --window "$W" | grep -q '^PASS no DEV key/cert outside' && ok "a public cert.pem (KVS CA bundle) in the test tree and PROD does NOT trip the DEV-key row" || bad "cert.pem false positive: $(vo post --window "$W" | grep 'DEV key')"
mkdir -p "$R/var/tmp"; cp "$W/test-w1/etc/xorgate/device.key.pem" "$R/var/tmp/stray"
vo post --window "$W" | grep -q '^FAIL no DEV key/cert outside' && ok "a stray DEV device.key.pem (any name, anywhere scanned) trips it" || bad "stray DEV key not found"
rm -rf "$R/var/tmp/stray" "$R/opt/xorgate-rs/media/cert.pem" "$W/test-w1/opt/xorgate/media"
rm -f "$R/home/pi/stray.pem" "$R/tmp-devkey.pem" "$R/tmp/devkey.pem"
touch "$R/var/lib/systemd/timers/stamp-p8-deadman.timer"
vo pre --window "$W" | grep -q '^FAIL no window artifact' && ok "pre FAILs on a leftover timer stamp (nit)" || bad "stamp not caught"
rm -f "$R/var/lib/systemd/timers/stamp-p8-deadman.timer"
echo x >> "$R/etc/xorgate/agent.env"
vo pre --window "$W" | grep -q '^FAIL manifest differs' && ok "pre FAILs on a one-byte change to agent.env" || bad "agent.env change not caught"
sed -i '$d' "$R/etc/xorgate/agent.env"
printf 'curl 2 ii \npython3 3.11 ii \n' > "$SD/dpkg-list"
vo post --window "$W" | grep -q '^FAIL dpkg list unchanged' && ok "post FAILs on a package upgrade (H1)" || bad "dpkg change not caught"
printf 'curl 1 ii \npython3 3.11 ii \n' > "$SD/dpkg-list"
echo "Fri 2026-11-14 10:00:00 UTC" > "$SD/prop.sim7600-lte.ActiveEnterTimestamp"
vo post --window "$W" | grep -q '^FAIL sim7600-lte ActiveEnterTimestamp' && ok "post FAILs on a modem restart" || bad "modem restart not caught"

echo "== restore after a PARTIAL aside: never-moved PROD paths stay put"
mkbench; echo inactive > "$SD/active.xorgate-agent-rs"
run p8-verify.sh baseline --window "$W" >/dev/null 2>&1; run p8-backup.sh backup --window "$W" >/dev/null 2>&1
mkdir -p "$W/aside/var/lib"; mv -T "$R/var/lib/xorgate" "$W/aside/var/lib/xorgate"; mkdir -p "$R/var/lib/xorgate"; echo seed > "$R/var/lib/xorgate/settings.json"
printf 'moving\t/var/lib/xorgate\nmoved\t/var/lib/xorgate\nmoving\t/etc/xorgate\n' > "$W/aside.list"   # crashed before /etc/xorgate moved
k=$(sha "$R/etc/xorgate/device.key.pem")
out=$(run p8-restore.sh --window "$W" --label w9 2>&1)
[ "$(sha "$R/etc/xorgate/device.key.pem")" = "$k" ] && [ ! -e "$W/test-w9/etc/xorgate" ] && ok "an un-moved /etc/xorgate ('moving' with nothing aside) is NOT taken as test state" || bad "partial aside: PROD /etc/xorgate moved"
grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && [ -e "$R/var/lib/xorgate/state.json" ] && ok "partial aside restored exactly" || { bad "partial restore"; cat "$W/verify-pre.out"; }

echo "== F3: the dpkg lock is waited for BEFORE the installer stops; then the repair"
mkbench
echo "p8-install-w1.service loaded active running p8 installer" > "$SD/units"; echo active > "$SD/active.p8-install-w1"
H=$TMP/holder.log; : > "$H"
python3 -c 'import fcntl,sys,time; f=open(sys.argv[1],"w"); fcntl.lockf(f, fcntl.LOCK_EX); print("held", flush=True); time.sleep(4); print("released %.3f" % time.time(), flush=True); f.close(); time.sleep(30)' \
  "$R/var/lib/dpkg/lock-frontend" >> "$H" 2>&1 &
hp=$!; for _ in $(seq 50); do grep -q held "$H" && break; sleep 0.1; done
echo 2 > "$SD/audit_n"; echo 0 > "$SD/configure_fixes"; : > "$SD/dpkg.log"
run p8-restore.sh --window "$W" --label w1 > "$TMP/r.out" 2>&1
kill "$hp" 2>/dev/null; wait "$hp" 2>/dev/null
rel=$(sed -n 's/^released //p' "$H"); stop=$(grep 'systemctl stop p8-install-w1' "$SD/log" | head -1 | cut -d' ' -f1)
[ -n "$rel" ] && [ -n "$stop" ] && python3 -c "import sys; sys.exit(0 if $stop >= $rel else 1)" && ok "F3: installer stopped only AFTER the lock was released (stop $stop >= release $rel)" || bad "F3 order: release=$rel stop=$stop"
grep -q 'F3: dpkg locks free (waited [1-9]' "$W/restore.log" && ok "F3: the wait is logged" || bad "F3 wait log"
grep -q '^dpkg --audit' "$SD/dpkg.log" && grep -q '^dpkg --configure -a' "$SD/dpkg.log" && grep -q '^apt-get -f install -y --no-upgrade' "$SD/dpkg.log" \
  && [ "$(grep -n '' "$SD/dpkg.log" | grep -m1 configure | cut -d: -f1)" -lt "$(grep -n '' "$SD/dpkg.log" | grep -m1 apt-get | cut -d: -f1)" ] \
  && ok "F3: --audit -> --configure -a -> apt-get -f install --no-upgrade (in order)" || bad "F3 repair: $(tr '\n' ';' < "$SD/dpkg.log")"
grep -q 'repaired by apt-get -f install --no-upgrade' "$W/restore.log" && ok "F3: the repair is logged" || bad "F3 repair log"
# the bounded wait: a lock held past P8_DPKG_WAIT_S still ends in a stop
mkbench; echo "p8-install-w1.service loaded active running p8 installer" > "$SD/units"; echo active > "$SD/active.p8-install-w1"; : > "$H"
python3 -c 'import fcntl,sys,time; f=open(sys.argv[1],"w"); fcntl.lockf(f, fcntl.LOCK_EX); print("held", flush=True); time.sleep(30)' "$R/var/lib/dpkg/lock-frontend" >> "$H" 2>&1 &
hp=$!; for _ in $(seq 50); do grep -q held "$H" && break; sleep 0.1; done
P8_DPKG_WAIT_S=2 run p8-restore.sh --window "$W" --label w1 > "$TMP/r.out" 2>&1
kill "$hp" 2>/dev/null; wait "$hp" 2>/dev/null
grep -q 'still held after 2s: stopping the installer anyway' "$W/restore.log" && grep -q 'systemctl stop p8-install-w1' "$SD/log" && ok "F3: the wait is bounded (P8_DPKG_WAIT_S), then the stop" || bad "bounded wait"

echo "== F1: the queued agent jobs are cancelled (boot path, stubbed; the real one is the systemd suite)"
mkbench; printf '41 xorgate-agent.service start waiting\n42 multi-user.target start waiting\n' > "$SD/jobs"
run p8-restore.sh --window "$W" --label w1 --auto --boot > "$TMP/r.out" 2>&1
grep -qx 41 "$SD/cancelled" && ! grep -qx 42 "$SD/cancelled" && ok "F1: only the agent's job (41) cancelled" || bad "cancel: $(tr '\n' ' ' < "$SD/cancelled" 2>/dev/null)"
grep -q 'boot path: the agent starts when this oneshot exits' "$W/restore.log" && ! grep -q 'xorgate-agent-rs active$' "$W/restore.log" && ok "F1: --boot does not wait on the agent (no deadlock)" || bad "boot wait"

echo "== cloud (fake PROD API) + cam0 hook"
cat > "$TMP/api.py" <<'PY'
import http.server, json, sys
state = json.load(open(sys.argv[2]))
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ok = self.headers.get("Authorization") == "Bearer tok" and self.headers.get("X-Organization-Id") == "org"
        body = json.dumps({"device": json.load(open(sys.argv[2]))}).encode() if ok else b"{}"
        self.send_response(200 if ok else 401); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
echo '{"status":"online","configRev":81,"agentVersion":"9bd55b9","reportedConfig":{"rev":81},"config":{"gnssAntBias":true,"b":[1,2]}}' > "$TMP/dev.json"
python3 "$TMP/api.py" 18931 "$TMP/dev.json" & ap=$!
sleep 1
export XG_TOKEN=tok XG_ORG=org XG_WS=ws
out=$(bash "$B/p8-verify.sh" cloud-baseline --device d --out "$TMP/cb" --api http://127.0.0.1:18931 2>&1) && grep -q '^configRev=81$' "$TMP/cb" && ok "cloud-baseline recorded (configRev 81)" || bad "cloud-baseline: $out"
printf '%s' "$out" | grep -q tok && bad "token printed" || ok "the token is never printed"
bash "$B/p8-verify.sh" cloud --device d --baseline "$TMP/cb" --api http://127.0.0.1:18931 --wait 0 --expect-rev 81 > "$TMP/c.out" 2>&1 && ok "cloud PASS" || { bad "cloud"; cat "$TMP/c.out"; }
echo '{"status":"online","configRev":82,"agentVersion":"9bd55b9","reportedConfig":{"rev":81},"config":{"gnssAntBias":false,"b":[1,2]}}' > "$TMP/dev.json"
bash "$B/p8-verify.sh" cloud --device d --baseline "$TMP/cb" --api http://127.0.0.1:18931 --wait 0 > "$TMP/c.out" 2>&1
[ "$(grep -c '^FAIL' "$TMP/c.out")" = 3 ] && ok "cloud FAILs configRev, reported rev and the settings sha on drift" || { bad "cloud drift"; cat "$TMP/c.out"; }
kill "$ap" 2>/dev/null; wait "$ap" 2>/dev/null
unset XG_TOKEN XG_ORG XG_WS
bash "$B/p8-verify.sh" cam0 >/dev/null 2>&1; [ $? = 3 ] && ok "cam0 PENDING without the hook (not green)" || bad "cam0 unset"
printf '#!/bin/sh\nexit 0\n' > "$TMP/hook"; chmod 0755 "$TMP/hook"
P8_CAM0_HOOK=$TMP/hook bash "$B/p8-verify.sh" cam0 >/dev/null 2>&1 && ok "cam0 PASS via the hook" || bad "cam0 hook pass"
printf '#!/bin/sh\nexit 1\n' > "$TMP/hook"
P8_CAM0_HOOK=$TMP/hook bash "$B/p8-verify.sh" cam0 >/dev/null 2>&1 && bad "cam0 passed a failing hook" || ok "cam0 FAIL via a failing hook"

echo "== T6: the manifest path set is pinned (one golden row per tree/glob); a dropped glob goes RED"
GOLD="/etc/xorgate /var/lib/xorgate /var/lib/xorgate-rs-install /var/lib/xorgate-rs /opt/xorgate /opt/xorgate-rs /opt/xorgate-rs.prev
/etc/systemd/system/xorgate-agent.service /etc/systemd/system/sim7600-lte.service /etc/systemd/system/multi-user.target.wants/sim7600-lte.service
/etc/sudoers.d/010_pi-nopasswd /usr/local/bin/xorgate /usr/local/bin/xorgate-rs /usr/local/bin/xorgate-setclock /usr/local/bin/xorgate-camrotate
/usr/local/bin/sim7600-lte.sh /etc/udev/rules.d/99-sim7600-lte.rules /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf
/etc/NetworkManager/system-connections/MyHomeSSID.nmconnection /etc/systemd/logind.conf.d/10-xorgate-removeipc.conf
/etc/apt/apt.conf.d/99xorgate-lock-timeout /etc/cron.d/xorgate-fake-hwclock /boot/firmware/config.txt /etc/modules
/etc/modprobe.d/raspi-blacklist.conf /etc/apt/sources.list /etc/apt/sources.list.d/raspi.list /etc/apt/keyrings/x.gpg
/usr/share/keyrings/y.gpg /boot/firmware/config.txt.bak-xorgate /etc/systemd/system-generators/zz-fixture-gen @groups:pi @enabled:ModemManager.service @enabled:NetworkManager.service
@enabled:sim7600-lte.service @enabled:xorgate-agent.service @enabled:xorgate-agent-rs.service @enabled:xorgate-drive-blackbox.service"
gold_missing() { local p n=0; for p in $GOLD; do awk -F '\t' -v p="$p" '$1 == p {f=1} END {exit !f}' "$1" || { n=$((n + 1)); echo "missing $p" >&2; }; done; echo "$n"; }
mkbench
# shellcheck source=/dev/null
( . "$W/bin/p8-lib.sh"; p8_manifest "$TMP/gold.m" )
[ "$(gold_missing "$TMP/gold.m" 2>/dev/null)" = 0 ] && ok "every golden path ($(echo "$GOLD" | wc -w)) is in the manifest" || bad "golden paths: $(gold_missing "$TMP/gold.m" 2>&1 >/dev/null | tr '\n' ' ')"
cp -r "$W/bin" "$TMP/mbin"; sed -i 's#"/etc/systemd/logind.conf.d/\*",##' "$TMP/mbin/p8-lib.sh"
grep -q 'logind.conf.d' "$TMP/mbin/p8-lib.sh" && bad "T6 mutant not applied" || true
# shellcheck source=/dev/null
( . "$TMP/mbin/p8-lib.sh"; p8_manifest "$TMP/gold-m.m" )
[ "$(gold_missing "$TMP/gold-m.m" 2>/dev/null)" != 0 ] && ok "T6 mutant (logind glob dropped) CAUGHT by the golden list" || bad "T6 mutant not caught"

ready() { # a fresh sandbox window with the surface aside (label $1); NOGEN=1: no /etc generators dir on "PROD"
  mkbench
  [ "${NOGEN:-0}" = 1 ] && rm -rf "$R/etc/systemd/system-generators"
  run p8-verify.sh baseline --window "$W" >/dev/null 2>&1 || bad "ready: baseline"
  run p8-deadman.sh arm --window "$W" --cap-min 60 --label "${1:-w1}" >/dev/null 2>&1 || bad "ready: arm"
  echo inactive > "$SD/active.xorgate-agent-rs"
  run p8-backup.sh backup --window "$W" >/dev/null 2>&1 || bad "ready: backup"
  sha "$W/backup.tgz" > "$W/pulled.ok"
  run p8-backup.sh aside --window "$W" >/dev/null 2>&1 || bad "ready: aside"
  [ -f "$W/aside.done" ] || bad "ready: no aside.done"
}

echo "== T6: the fake install's logind/apt/cron rewrites come back"
ready w1; fake_js_install
run p8-restore.sh --window "$W" >/dev/null 2>&1
grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && [ "$(cat "$R/etc/systemd/logind.conf.d/10-xorgate-removeipc.conf")" = ipc ] && [ "$(cat "$R/etc/cron.d/xorgate-fake-hwclock")" = cron ] \
  && [ ! -e "$R/etc/apt/sources.list.d/nodesource.list" ] && ok "logind + cron re-extracted, the new apt source moved out; verify pre PASS" || { bad "T6 logind/apt/cron"; cat "$W/verify-pre.out"; }
ls "$R/boot/firmware/"*.p8new >/dev/null 2>&1 && bad "T8: a .p8new staging file left" || ok "T8: config.txt put back by rename, no staging file left"

echo "== T1: a finished window can be neither re-armed nor re-installed"
out=$(run p8-deadman.sh arm --window "$W" --cap-min 60 --label w1 2>&1); [ $? = 3 ] && printf '%s' "$out" | grep -q 'already ran' && ok "T1: arm refuses a window that already ran" || bad "T1 arm: $out"
printf '#!/bin/bash\necho hi\n' > "$TMP/install.sh"
out=$(run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" 2>&1); [ $? = 3 ] && ok "T1: run-install refuses a finished window ($(printf '%s' "$out" | grep -o 'not armed\|already restored' | head -1))" || bad "T1 run-install finished: $out"
ready w1; W1=$W; W=$R/root/p8-20261114T120001Z; install -d -m 0700 "$W" "$W/bin"; install -m 0700 "$B"/p8-*.sh "$W/bin/"; touch "$W/aside.done" "$W/aside.list"
out=$(run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" 2>&1); [ $? = 3 ] && printf '%s' "$out" | grep -q 'not armed' && ok "T1: run-install refuses a window whose dead-man is not armed (a foreign/stale \$W)" || bad "T1 foreign: $out"
W=$W1; echo active > "$SD/active.xorgate-agent-rs"
out=$(run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" 2>&1); [ $? = 3 ] && printf '%s' "$out" | grep -q 'is active' && ok "T1: run-install refuses while PROD runs" || bad "T1 PROD active: $out"
echo inactive > "$SD/active.xorgate-agent-rs"
mv "$W/aside/etc/xorgate" "$TMP/xa"; out=$(run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" 2>&1); rc=$?; mv "$TMP/xa" "$W/aside/etc/xorgate"
[ $rc = 3 ] && ok "T1: run-install refuses when the PROD /etc/xorgate is not aside" || bad "T1 aside check: $out"
run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" >/dev/null 2>&1 && ok "T1: run-install runs on a live, armed aside" || bad "T1: run-install refused a valid window"

echo "== T3: backup/aside/run-install refuse while /run/p8.lock is held"
flock "$P8_LOCK" sleep 6 & lp=$!; sleep 1
out=$(run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" 2>&1); [ $? = 3 ] && printf '%s' "$out" | grep -q 'is held' && ok "T3: run-install refuses under the lock" || bad "T3 run-install: $out"
out=$(run p8-backup.sh aside --window "$W" 2>&1); [ $? = 3 ] && printf '%s' "$out" | grep -q 'is held' && ok "T3: aside refuses under the lock" || bad "T3 aside: $out"
out=$(run p8-backup.sh backup --window "$W" 2>&1); [ $? = 3 ] && printf '%s' "$out" | grep -q 'is held' && ok "T3: backup refuses under the lock" || bad "T3 backup: $out"
wait "$lp"

echo "== T2 + T9: a manual restore whose verify FAILS re-arms a failsafe dead-man; the failsafe starts PROD"
ready w1; fake_js_install
echo changed > "$R/var/lib/xorgate-rs/tapes/t1"      # an UNRESTORABLE row: verify pre must FAIL
: > "$SD/log"
out=$(run p8-restore.sh --window "$W" 2>&1); rc=$?
[ $rc = 1 ] && ! grep -q 'systemctl start --no-block xorgate-agent-rs' "$SD/log" && ok "T2: manual restore with a failing verify: exit 1, PROD not started" || bad "T2: rc=$rc"
oas=$(sed -n 's/^OnActiveSec=\([0-9]*\)s$/\1/p' "$R/etc/systemd/system/p8-deadman.timer" 2>/dev/null)
[ -e "$R/root/p8-window.active" ] && [ -e "$R/etc/systemd/system/p8-deadman-boot.service" ] && [ -n "$oas" ] && [ "$oas" -ge 595 ] && [ "$oas" -le 600 ] \
  && grep -q 'FAILSAFE --auto dead-man is armed for +10 min' "$W/restore.log" && ok "T2: a +10 min --auto failsafe dead-man + boot unit re-armed (OnActiveSec=${oas}s)" || bad "T2: failsafe not armed: $(tail -2 "$W/restore.log")"
grep -q 'label=w1' "$R/root/p8-window.active" && ok "T2: the failsafe keeps the window's label" || bad "T2 label"
out=$(run p8-restore.sh --auto --window "$W" 2>&1); rc=$?      # what the failsafe timer runs
grep -q 'systemctl start --no-block xorgate-agent-rs' "$SD/log" && grep -q 'verify=FAIL' "$W/restore.done" && [ ! -e "$R/root/p8-window.active" ] \
  && ok "T2: the failsafe --auto restore starts PROD (restore.done says verify=FAIL, rc=$rc)" || bad "T2: failsafe restore: $(tail -3 "$W/restore.log")"
out=$(run p8-restore.sh --window "$W" 2>&1); [ $? = 1 ] && printf '%s' "$out" | grep -q 'NOT PASS' && ok "T9: a no-op restore reports the earlier FAIL (exit 1)" || bad "T9: $out"

echo "== T6: an interrupted restore (after step 4 / after step 5) finishes on the re-run; the stale-restore.done mutant is CAUGHT"
ready w1; fake_js_install; pk=$(awk -F '\t' '$1 == "/etc/xorgate/device.key.pem" {print $6}' "$W/pre.manifest")
P8_TEST_STOP_AFTER=4 run p8-restore.sh --window "$W" >/dev/null 2>&1; [ $? = 99 ] && [ ! -e "$W/restore.done" ] && ok "stopped after step 4 (no restore.done)" || bad "stop after 4"
run p8-restore.sh --window "$W" >/dev/null 2>&1 && [ "$(sha "$R/etc/xorgate/device.key.pem")" = "$pk" ] && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" \
  && ok "manual re-run after step 4: full restore, verify PASS" || { bad "re-run after 4"; cat "$W/verify-pre.out"; }
ready w1; fake_js_install; pk=$(awk -F '\t' '$1 == "/etc/xorgate/device.key.pem" {print $6}' "$W/pre.manifest")
P8_TEST_STOP_AFTER=5 run p8-restore.sh --window "$W" >/dev/null 2>&1; [ $? = 99 ] && [ -e "$W/aside.restored" ] && [ ! -e "$W/restore.done" ] && ok "stopped after step 5 (aside back, no restore.done)" || bad "stop after 5"
run p8-restore.sh --auto --boot --window "$W" >/dev/null 2>&1; [ "$(sha "$R/etc/xorgate/device.key.pem")" = "$pk" ] && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && [ ! -e "$R/etc/NetworkManager/conf.d/50-test-new.conf" ] \
  && ok "--boot re-run after step 5: steps 6+ done, verify PASS" || { bad "re-run after 5"; cat "$W/verify-pre.out"; }
ready w1; fake_js_install; pk=$(awk -F '\t' '$1 == "/etc/xorgate/device.key.pem" {print $6}' "$W/pre.manifest")
sed -i 's|^p8_log "restore start: |echo "x verify=PASS" > "$W/restore.done"; p8_log "restore start: |' "$W/bin/p8-restore.sh"
grep -q 'echo "x verify=PASS" > "$W/restore.done"; p8_log' "$W/bin/p8-restore.sh" || bad "stale-restore.done mutant not applied"
P8_TEST_STOP_AFTER=4 run p8-restore.sh --window "$W" >/dev/null 2>&1; run p8-restore.sh --window "$W" >/dev/null 2>&1
[ "$(sha "$R/etc/xorgate/device.key.pem" 2>/dev/null)" != "$pk" ] && ok "T6 mutant (restore.done at restore start) CAUGHT: the re-run would skip a needed restore" || bad "T6 mutant not caught"

echo "== (c) preflight STOP on APT::Periodic keys or unattended-upgrades"
mkbench
run p8-verify.sh preflight >/dev/null 2>&1 && ok "preflight PASS on a clean apt config" || bad "preflight clean"
echo 'APT::Periodic::Update-Package-Lists "1";' > "$SD/aptconf"
out=$(run p8-verify.sh baseline --window "$W" 2>&1); [ $? = 3 ] && [ ! -e "$W/baseline.env" ] && printf '%s' "$out" | grep -q 'PREFLIGHT STOP' && ok "APT::Periodic key: baseline refused (exit 3), no baseline written" || bad "periodic STOP: $out"
rm -f "$SD/aptconf"; echo "ii " > "$SD/uu"
run p8-verify.sh preflight >/dev/null 2>&1; [ $? = 1 ] && ok "unattended-upgrades installed: preflight STOP" || bad "uu STOP"
rm -f "$SD/uu"

echo "== NITs: cam0 timeout, blackbox growth tolerant of pruning"
printf '#!/bin/sh\nsleep 10\n' > "$TMP/hook"; chmod 0755 "$TMP/hook"
out=$(P8_CAM0_TIMEOUT=2 P8_CAM0_HOOK=$TMP/hook bash "$B/p8-verify.sh" cam0 2>&1); printf '%s' "$out" | grep -q 'TIMED OUT' && ok "T11: a hung cam0 hook is cut at P8_CAM0_TIMEOUT (FAIL)" || bad "T11: $out"
ready w1; run p8-restore.sh --window "$W" >/dev/null 2>&1; echo 1 > "$SD/prop.xorgate-agent-rs.MainPID"
rm -f "$R/var/lib/xorgate-rs/drive-blackbox/b1.ndjson"; sleep 1.1; echo '{"t":9}' > "$R/var/lib/xorgate-rs/drive-blackbox/b2.ndjson"; touch -d '+2 seconds' "$R/var/lib/xorgate-rs/drive-blackbox/b2.ndjson"
vo post --window "$W" | grep -q '^PASS drive-blackbox writing' && ok "T12: a pruned old tape + a new tape reads as writing" || bad "T12 prune"
touch -d '-1 hour' "$R/var/lib/xorgate-rs/drive-blackbox/b2.ndjson"
vo post --window "$W" | grep -q '^FAIL drive-blackbox not writing' && ok "T12: no new write reads as NOT writing" || bad "T12 stale"

echo "== (a) p8-align.sh against a mocked DEV API"
cat > "$TMP/devapi.py" <<'PY'
import http.server, json, sys
port, state_f, tok, mid, hn = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
st = {"gets": 0, "patches": [], "cellular": False}
old = {"id": "11111111-0000-0000-0000-000000000000", "createdAt": "2026-11-14T00:00:00Z", "config": {}}
new = {"id": "22222222-0000-0000-0000-000000000000", "createdAt": None, "config": {"gnssAntBias": False}, "configRev": 1, "serial": mid}
other = {"id": "33333333-0000-0000-0000-000000000000", "createdAt": "2026-11-15T00:00:00Z", "config": {}, "serial": "someone-else"}
class H(http.server.BaseHTTPRequestHandler):
    def ok(self):
        return self.headers.get("Authorization") == "Bearer " + tok and self.headers.get("X-Workspace-Id") == "ws"
    def send(self, code, body):
        b = json.dumps(body).encode(); self.send_response(code); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if not self.ok(): return self.send(401, {})
        if self.path == "/v1/devices":
            st["gets"] += 1
            mode = open(state_f).read().strip()
            rows = [old] + ([new] if st["gets"] >= 3 else []) + ([other] if st["gets"] >= 3 and mode == "multi" else [])
            if st["gets"] == 3 and mode == "notbench": new["serial"] = "not-the-bench"
            if st["gets"] == 3 and mode == "hostonly": new["serial"] = "not-the-bench"; new["hostname"] = hn
            if st["gets"] == 3:
                import time; new["createdAt"] = time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(time.time() - 4))
                if open(state_f).read().strip() == "cellular": new["config"]["cellular"] = {"apn": "x"}
            return self.send(200, {"devices": rows})
        if self.path == "/v1/devices/" + new["id"]: return self.send(200, {"device": new})
        self.send(404, {})
    def do_PATCH(self):
        if not self.ok(): return self.send(401, {})
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        open(state_f + ".patches", "a").write(self.path + " " + json.dumps(body, sort_keys=True) + "\n")
        new["config"].update(body); new["configRev"] += 1
        self.send(200, {"device": new})
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
echo '{"gnssAntBias": true}' > "$TMP/align.json"
jwt() { python3 -c 'import base64,json,sys; b=lambda d: base64.urlsafe_b64encode(json.dumps(d).encode()).decode().rstrip("="); print(b({"alg":"none"}) + "." + b({"iss": sys.argv[1]}) + ".sig")' "$1"; }
DEVTOK=$(jwt https://cognito-idp.us-east-1.amazonaws.com/us-east-1_E6dkPNlZl)
PRODTOK=$(jwt https://cognito-idp.us-east-1.amazonaws.com/us-east-1_RPLv99S54)
run_api() { : > "$TMP/apistate.patches"; echo "$1" > "$TMP/apistate"; python3 "$TMP/devapi.py" "$2" "$TMP/apistate" "${3:-$DEVTOK}" "$(cat "$R/etc/machine-id")" "$(hostname)" & AP=$!; sleep 1; }
ready w1; cp "$W/seed-settings.json" "$R/var/lib/xorgate/settings.json"
run p8-modem-sampler.sh run --out "$W/modem-w1.tsv" --baseline "$W/baseline.env" --ticks 2 >/dev/null 2>&1
run_api ok 18941
out=$(XG_TOKEN="$DEVTOK" P8_DEV_API_HOSTS="127.0.0.1:18941" P8_SSH=fakessh P8_RSUDO="sudo -n -E" bash "$B/p8-align.sh" --api http://127.0.0.1:18941 --org org --workspace ws \
  --align "$TMP/align.json" --poll 0.2 --timeout 20 --host x --window "$W" --label w1 --f2-wait 0 --out "$TMP/align.env" 2>&1); rc=$?
kill "$AP" 2>/dev/null; wait "$AP" 2>/dev/null
[ $rc = 0 ] && ok "align: exit 0 (new device found, aligned, bench dev-settings PASS, F2 summary PASS)" || { bad "align rc=$rc"; printf '%s\n' "$out" | tail -n 12; }
[ "$(cat "$TMP/apistate.patches")" = '/v1/devices/22222222-0000-0000-0000-000000000000/config {"gnssAntBias": true}' ] && ok "align: exactly ONE PATCH, on the NEW device only, body = align.json (gnssAntBias)" || bad "align PATCHes: $(cat "$TMP/apistate.patches")"
grep -q '^claim_to_patch_s=[0-9]' "$TMP/align.env" && grep -q '^aligned=yes' "$TMP/align.env" && [ "$(stat -c %a "$TMP/align.env")" = 600 ] && ok "align: claim->PATCH delay recorded (F2), result file 0600" || bad "align result: $(cat "$TMP/align.env")"
printf '%s' "$out" | grep -qF "$DEVTOK" && bad "align printed the token" || ok "align never prints the token"
printf '%s' "$out" | grep -q 'F2:' && printf '%s' "$out" | grep -q 'bench dev-settings PASS' && ok "align ran the bench dev-settings check and the F2 summary" || bad "align bench part"
run_api cellular 18942
out=$(XG_TOKEN="$DEVTOK" P8_DEV_API_HOSTS="127.0.0.1:18942" bash "$B/p8-align.sh" --api http://127.0.0.1:18942 --org org --workspace ws --align "$TMP/align.json" --bench-id "$W/bench-id.env" --poll 0.2 --timeout 20 2>&1); rc=$?
kill "$AP" 2>/dev/null; wait "$AP" 2>/dev/null
[ $rc = 1 ] && [ ! -s "$TMP/apistate.patches" ] && printf '%s' "$out" | grep -q 'STOP (H10)' && ok "align: a DEV cellular key is a STOP, and nothing is PATCHed" || bad "align cellular: rc=$rc $(cat "$TMP/apistate.patches")"
out=$(XG_TOKEN="$DEVTOK" bash "$B/p8-align.sh" --api https://api.xorgate.io --org org --workspace ws --align "$TMP/align.json" 2>&1); [ $? = 2 ] && printf '%s' "$out" | grep -q 'PROD API host' && ok "align REFUSES the PROD API host (before any request)" || bad "align PROD: $out"
out=$(XG_TOKEN="$DEVTOK" P8_DEV_API_HOSTS="api.xorgate.io" bash "$B/p8-align.sh" --api https://api.xorgate.io --org org --workspace ws --align "$TMP/align.json" 2>&1); [ $? = 2 ] && ok "align refuses PROD even when it is listed as DEV" || bad "align PROD listed: $out"
out=$(XG_TOKEN="$DEVTOK" bash "$B/p8-align.sh" --api https://example.com --org org --workspace ws --align "$TMP/align.json" 2>&1); [ $? = 2 ] && ok "align refuses a non-DEV host" || bad "align other host"
out=$(XG_TOKEN="$DEVTOK" P8_POOL=us-east-1_RPLv99S54 bash "$B/p8-align.sh" --org org --workspace ws --align "$TMP/align.json" 2>&1); [ $? = 2 ] && ok "align refuses the PROD Cognito pool" || bad "align PROD pool"
for m in multi:18943 notbench:18944 hostonly:18946; do
  run_api "${m%%:*}" "${m#*:}"
  out=$(XG_TOKEN="$DEVTOK" P8_DEV_API_HOSTS="127.0.0.1:${m#*:}" bash "$B/p8-align.sh" --api "http://127.0.0.1:${m#*:}" --org org --workspace ws --align "$TMP/align.json" --bench-id "$W/bench-id.env" --poll 0.2 --timeout 20 2>&1); rc=$?
  kill "$AP" 2>/dev/null; wait "$AP" 2>/dev/null
  [ $rc = 1 ] && [ ! -s "$TMP/apistate.patches" ] && ok "align: '${m%%:*}' (two new devices / a serial that is not the bench's / a hostname match alone) is REFUSED with no PATCH" || bad "align ${m%%:*}: rc=$rc $(printf '%s' "$out" | tail -2)"
done
run_api ok 18945 "$PRODTOK"
out=$(XG_TOKEN="$PRODTOK" P8_DEV_API_HOSTS="127.0.0.1:18945" bash "$B/p8-align.sh" --api http://127.0.0.1:18945 --org org --workspace ws --align "$TMP/align.json" --bench-id "$W/bench-id.env" --poll 0.2 --timeout 5 2>&1); rc=$?
kill "$AP" 2>/dev/null; wait "$AP" 2>/dev/null
[ $rc = 2 ] && [ ! -s "$TMP/apistate.patches" ] && printf '%s' "$out" | grep -q 'not a DEV-pool token' && ok "align REFUSES a supplied XG_TOKEN whose iss is the PROD pool (no request)" || bad "align PROD iss: rc=$rc $out"
out=$(XG_TOKEN=not-a-jwt P8_DEV_API_HOSTS="127.0.0.1:1" bash "$B/p8-align.sh" --api http://127.0.0.1:1 --org org --workspace ws --align "$TMP/align.json" --bench-id "$W/bench-id.env" 2>&1); [ $? = 2 ] && ok "align refuses a token it cannot decode" || bad "align bad jwt"
out=$(XG_TOKEN="$DEVTOK" P8_DEV_API_HOSTS="127.0.0.1:1" bash "$B/p8-align.sh" --api http://127.0.0.1:1 --org org --workspace ws --align "$TMP/align.json" 2>&1); [ $? = 2 ] && printf '%s' "$out" | grep -q 'no bench identity' && ok "align refuses without a bench identity" || bad "align no bench id: $out"
grep -qE '^serial_sha_machineid=[0-9a-f]{64}$' "$W/bench-id.env" && grep -qE '^serial_sha_dt=[0-9a-f]{64}$' "$W/bench-id.env" && [ "$(stat -c %a "$W/bench-id.env")" = 600 ] \
  && ! grep -q 0123456789abcdef "$W/bench-id.env" && ok "backup wrote bench-id.env: shas of machine-id + DT serial + hostname only (0600)" || bad "bench-id.env"
echo '{"gnssAntBias": true, "lteRecovery": {}}' > "$TMP/align2.json"
out=$(XG_TOKEN="$DEVTOK" bash "$B/p8-align.sh" --org org --workspace ws --align "$TMP/align2.json" 2>&1); [ $? = 2 ] && ok "align refuses an align.json with more than gnssAntBias" || bad "align extra keys"

echo "== WP-R1 native generator: in the manifest, moved out with the test surface, its /run output removed, a leftover is caught"
ready w2
mkdir -p "$R/etc/systemd/system-generators" "$R/run/systemd/generator/multi-user.target.wants"
printf '#!/bin/sh\n[ -e /opt/xorgate ] && exit 0\n' > "$R/etc/systemd/system-generators/xorgate-agent-rs-native"; chmod 0755 "$R/etc/systemd/system-generators/xorgate-agent-rs-native"
ln -s /etc/systemd/system/xorgate-agent-rs.service "$R/run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service"
grep -q $'^absent\t/etc/systemd/system-generators/xorgate-agent-rs-native$' "$W/aside.list" && ok "the generator path is part of the move-aside surface" || bad "generator not in aside.list"
: > "$SD/log"; run p8-restore.sh --window "$W" >/dev/null 2>&1
[ ! -e "$R/etc/systemd/system-generators/xorgate-agent-rs-native" ] && [ -e "$W/test-w2/etc/systemd/system-generators/xorgate-agent-rs-native" ] \
  && [ ! -L "$R/run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service" ] && grep -q 'removed the native generator' "$W/restore.log" \
  && [ "$(grep -n 'daemon-reload' "$SD/log" | tail -1 | cut -d: -f1)" -gt 0 ] && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" \
  && ok "restore: the generator went to test-w2, its /run wants link removed, daemon-reload, verify pre PASS" || { bad "generator restore"; cat "$W/verify-pre.out"; }
[ -e "$R/etc/systemd/system-generators/zz-fixture-gen" ] && ok "an unrelated PROD generator stays" || bad "fixture generator lost"
ln -s /etc/systemd/system/xorgate-agent-rs.service "$R/run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service"
vo pre --window "$W" | grep -q '^FAIL no window artifact.*generator' && ok "verify: a leftover generator /run link is caught" || bad "generator leftover not caught"
rm -f "$R/run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service"

echo "== restore.done only AFTER the start is queued; a re-run never re-extracts over a running PROD"
ready w1; fake_js_install
P8_TEST_STOP_AFTER=start run p8-restore.sh --window "$W" >/dev/null 2>&1; [ $? = 99 ] && grep -q 'systemctl start --no-block xorgate-agent-rs' "$SD/log" && [ ! -e "$W/restore.done" ] \
  && ok "stopped right after the start was queued: no restore.done yet" || bad "stop after start"
echo '{"live":1}' > "$R/var/lib/xorgate/state.json"      # PROD writes its state
run p8-restore.sh --auto --window "$W" >/dev/null 2>&1
grep -q 'PROD found running' "$W/restore.done" && grep -q 'verify=PASS' "$W/restore.done" && [ "$(cat "$R/var/lib/xorgate/state.json")" = '{"live":1}' ] \
  && ok "the late --auto run records the verdict and never touches the live PROD state" || bad "PROD-running guard: $(cat "$W/restore.done" 2>/dev/null)"

echo "== WP-R1 MUST-2: the sibling provisioner unit is stopped FIRST and verified inactive before anything moves back"
ready w2; fake_js_install
printf 'xorgate-provision-rs.service loaded active running native provisioning\np8-install-w2.service loaded active running p8 installer\n' > "$SD/units"
echo active > "$SD/active.xorgate-provision-rs"; echo active > "$SD/active.p8-install-w2"
echo "step 3 dev claim" > "$R/var/log/xorgate-provision-rs.log"; mkdir -p "$R/run"; touch "$R/run/xorgate-provision-rs.lock" "$R/run/xorgate-provision-rs.rc" "$R/run/xorgate-provision-rs.rc.20261115T1"
: > "$SD/log"; run p8-restore.sh --window "$W" >/dev/null 2>&1
lp=$(grep -n 'stopping window unit xorgate-provision-rs.service' "$W/restore.log" | head -1 | cut -d: -f1)
li=$(grep -n 'stopping window unit p8-install-w2.service' "$W/restore.log" | head -1 | cut -d: -f1)
lb=$(grep -n 'PROD back:' "$W/restore.log" | head -1 | cut -d: -f1)
[ -n "$lp" ] && [ -n "$li" ] && [ -n "$lb" ] && [ "$lp" -lt "$li" ] && [ "$li" -lt "$lb" ] && grep -q 'window unit xorgate-provision-rs.service inactive (verified)' "$W/restore.log" \
  && ok "the provisioner is stopped first, then p8-install, both verified inactive, BEFORE the first move back" || bad "MUST-2 order: prov=$lp inst=$li back=$lb"
[ ! -e "$R/var/log/xorgate-provision-rs.log" ] && [ -e "$W/test-w2/var/log/xorgate-provision-rs.log" ] && [ ! -e "$R/run/xorgate-provision-rs.lock" ] && [ ! -e "$R/run/xorgate-provision-rs.rc" ] && [ ! -e "$R/run/xorgate-provision-rs.rc.20261115T1" ] \
  && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && ok "the provisioner log moved to test-w2; its /run lock + every rc.<id> removed; verify pre PASS" || { bad "MUST-2 artefacts"; cat "$W/verify-pre.out"; }
ready w2; fake_js_install
echo "xorgate-provision-rs.service loaded active running native provisioning" > "$SD/units"; echo active > "$SD/active.xorgate-provision-rs"; touch "$SD/STOPFAIL.xorgate-provision-rs"
: > "$SD/log"; P8_UNIT_STOP_WAIT_S=1 P8_KILL_WAIT_S=1 run p8-restore.sh --window "$W" >/dev/null 2>&1
grep -q 'still running after 1s: SIGKILL' "$W/restore.log" && grep -q 'systemctl kill --signal=SIGKILL xorgate-provision-rs.service' "$SD/log" && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" \
  && ok "a provisioner that ignores the stop is SIGKILLed, verified inactive, then the restore completes" || bad "MUST-2 SIGKILL escalation"
ready w2; fake_js_install; k=$(sha "$W/aside/etc/xorgate/device.key.pem")
echo "xorgate-provision-rs.service loaded active running native provisioning" > "$SD/units"; echo active > "$SD/active.xorgate-provision-rs"
touch "$SD/STOPFAIL.xorgate-provision-rs" "$SD/KILLFAIL.xorgate-provision-rs"
P8_UNIT_STOP_WAIT_S=1 P8_KILL_WAIT_S=1 run p8-restore.sh --window "$W" >/dev/null 2>&1; rc=$?
[ $rc != 0 ] && [ "$(sha "$W/aside/etc/xorgate/device.key.pem")" = "$k" ] && ! grep -q 'PROD back:' "$W/restore.log" && [ -e "$R/root/p8-window.active" ] \
  && grep -q 'moving NOTHING back' "$W/restore.log" && ok "an unkillable provisioner: the restore refuses to move anything back (PROD stays aside, dead-man still armed)" || bad "MUST-2 unkillable: rc=$rc"
rm -f "$SD/STOPFAIL.xorgate-provision-rs" "$SD/KILLFAIL.xorgate-provision-rs"; : > "$SD/units"
mkbench; run p8-verify.sh baseline --window "$W" >/dev/null 2>&1; echo inactive > "$SD/active.xorgate-agent-rs"; run p8-backup.sh backup --window "$W" >/dev/null 2>&1
echo x > "$R/var/log/xorgate-provision-rs.log"
vo pre --window "$W" | grep -q '^FAIL no window artifact.*xorgate-provision-rs.log' && ok "verify: a leftover provisioner log is a window artifact" || bad "prov log leftover not caught"
rm -f "$R/var/log/xorgate-provision-rs.log"; echo "xorgate-provision-rs.service loaded inactive dead native provisioning" > "$SD/units"
vo pre --window "$W" | grep -q '^FAIL no window artifact.*unit xorgate-provision-rs.service' && ok "verify: a still-loaded xorgate-provision-rs unit is a leftover" || bad "prov unit leftover not caught"
: > "$SD/units"

echo "== bench-tools-r1 review: re-list until none; the provisioner lock is held; a survivor re-arms then says REBOOT NEEDED"
ready w2; fake_js_install
echo "p8-install-w2.service loaded active running p8 installer" > "$SD/units"; echo active > "$SD/active.p8-install-w2"
echo "xorgate-provision-rs.service loaded active running native provisioning (late)" > "$SD/ON_STOP_ADD.p8-install-w2"
: > "$SD/log"; run p8-restore.sh --window "$W" >/dev/null 2>&1
lp=$(grep -n 'stopping window unit xorgate-provision-rs.service' "$W/restore.log" | head -1 | cut -d: -f1); lb=$(grep -n 'PROD back:' "$W/restore.log" | head -1 | cut -d: -f1)
grep -q 'pass 2: window units appeared after the last pass' "$W/restore.log" && [ -n "$lp" ] && [ -n "$lb" ] && [ "$lp" -lt "$lb" ] && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" \
  && ok "a provisioner handed off while its client was being stopped is caught by pass 2 and stopped before any move back" || bad "late hand-off: lp=$lp lb=$lb"
grep -q 'holding /run/xorgate-provision-rs.lock' "$W/restore.log" && [ ! -e "$R/run/xorgate-provision-rs.lock" ] && ok "the restore held the provisioner lock, and released + removed it at exit" || bad "prov lock hold/release"
ready w2; fake_js_install; mkdir -p "$R/run"
flock "$R/run/xorgate-provision-rs.lock" sleep 30 & fp=$!; sleep 0.5
P8_UNIT_PASSES=2 run p8-restore.sh --window "$W" >/dev/null 2>&1; rc=$?; kill "$fp" 2>/dev/null; wait "$fp" 2>/dev/null
[ $rc != 0 ] && grep -q 'held by a process outside any unit' "$W/restore.log" && ! grep -q 'PROD back:' "$W/restore.log" && ok "a provisioner process outside any unit (holding its lock): nothing moved back" || bad "lock held outside a unit: rc=$rc"
rm -f "$R/run/xorgate-provision-rs.lock"
ready w2; fake_js_install
echo "xorgate-provision-rs.service loaded active running native provisioning" > "$SD/units"; echo active > "$SD/active.xorgate-provision-rs"
touch "$SD/STOPFAIL.xorgate-provision-rs" "$SD/KILLFAIL.xorgate-provision-rs"
for i in 1 2 3; do
  P8_UNIT_STOP_WAIT_S=1 P8_KILL_WAIT_S=1 run p8-restore.sh --auto --window "$W" >/dev/null 2>&1
  oas=$(sed -n 's/^OnActiveSec=\([0-9]*\)s$/\1/p' "$R/etc/systemd/system/p8-deadman.timer")
  [ "$(cat "$W/survivor.retries")" = "$i" ] && grep -q "retry $i/3 armed for +5 min" "$W/restore.log" && [ "$oas" -ge 295 ] && [ "$oas" -le 300 ] && [ -e "$R/root/p8-window.active" ] \
    && ok "survivor --auto run $i: nothing moved back, a +5 min retry re-armed (OnActiveSec=${oas}s), marker kept" || bad "survivor run $i: oas=$oas $(tail -2 "$W/restore.log")"
done
P8_UNIT_STOP_WAIT_S=1 P8_KILL_WAIT_S=1 run p8-restore.sh --auto --window "$W" >/dev/null 2>&1
grep -q 'REBOOT NEEDED, PROD still aside' "$W/restore.log" && [ -e "$W/REBOOT-NEEDED" ] && ! grep -q 'PROD back:' "$W/restore.log" && [ -e "$R/root/p8-window.active" ] \
  && ok "survivor run 4: 'REBOOT NEEDED, PROD still aside' logged + W/REBOOT-NEEDED; the boot unit stays armed" || bad "survivor REBOOT NEEDED"
rm -f "$SD/STOPFAIL.xorgate-provision-rs" "$SD/KILLFAIL.xorgate-provision-rs"; : > "$SD/units"
mkbench; run p8-verify.sh baseline --window "$W" >/dev/null 2>&1; echo inactive > "$SD/active.xorgate-agent-rs"; run p8-backup.sh backup --window "$W" >/dev/null 2>&1
mkdir -p "$R/run"; touch "$R/run/xorgate-provision-rs.rc.20261115T2"
vo pre --window "$W" | grep -q '^FAIL no window artifact.*xorgate-provision-rs.rc.20261115T2' && ok "verify: a leftover per-run xorgate-provision-rs.rc.<id> is caught (glob)" || bad "rc.<id> leftover not caught"
rm -f "$R/run/xorgate-provision-rs.rc.20261115T2"

echo "== bench-tools-r2 MUST-1: the UPDATERS are stopped (verified) before the agents"
ready w2; fake_js_install
echo active > "$SD/active.xorgate-updater-rs"; echo active > "$SD/active.xorgate-agent-rs"
: > "$SD/log"; run p8-restore.sh --window "$W" >/dev/null 2>&1
lu=$(grep -n 'stopping window unit xorgate-updater-rs.service' "$W/restore.log" | head -1 | cut -d: -f1)
la=$(grep -n 'stopping window unit xorgate-agent-rs.service' "$W/restore.log" | head -1 | cut -d: -f1)
lb=$(grep -n 'PROD back:' "$W/restore.log" | head -1 | cut -d: -f1)
[ -n "$lu" ] && [ -n "$la" ] && [ -n "$lb" ] && [ "$lu" -lt "$la" ] && [ "$la" -lt "$lb" ] && grep -q 'window unit xorgate-updater-rs.service inactive (verified)' "$W/restore.log" \
  && ok "the updater is stopped + verified BEFORE the agent, both before the first move back" || bad "MUST-1 order: upd=$lu agent=$la back=$lb"
echo "== bench-tools-r2 SHOULD-3: a window-created empty /etc/systemd/system-generators goes; DA15 checks it"
NOGEN=1 ready w2
grep -qx 'generators_dir=absent' "$W/bench-id.env" && ok "backup recorded generators_dir=absent" || bad "bench-id generators_dir"
fake_js_install; mkdir -p "$R/etc/systemd/system-generators"
printf '#!/bin/sh\n' > "$R/etc/systemd/system-generators/xorgate-agent-rs-native"
run p8-restore.sh --window "$W" >/dev/null 2>&1
[ ! -e "$R/etc/systemd/system-generators" ] && grep -q 'removed the window-created empty /etc/systemd/system-generators' "$W/restore.log" && ok "the generators dir (absent at backup) is removed once emptied" || bad "SHOULD-3 rmdir"
echo 1 > "$SD/prop.xorgate-agent-rs.MainPID"
vo post --window "$W" | grep -q '^PASS /etc/systemd/system-generators as at backup (absent)' && ok "DA15 post: the generators dir matches the backup" || bad "SHOULD-3 post row"
mkdir -p "$R/etc/systemd/system-generators"
vo post --window "$W" | grep -q '^FAIL /etc/systemd/system-generators as at backup' && ok "DA15 post: a left-over generators dir FAILs" || bad "SHOULD-3 post FAIL row"
rmdir "$R/etc/systemd/system-generators"
echo "== fix-check b8d7e44 SHOULD 2: a failed / retried dead-man is NOT GREEN in post, even after a later fix"
echo "Tue 2030-01-01 00:00:00 UTC" > "$SD/prop.xorgate-agent-rs.ActiveEnterTimestamp"
vo post --window "$W" | grep -qE '^FAIL (no W/REBOOT-NEEDED|no dead-man|no FATAL|every dead-man)' && bad "post flags dead-man rows on a clean window" || ok "a clean window: no dead-man row FAILs"
echo "x REBOOT NEEDED" > "$W/REBOOT-NEEDED"; echo 2 > "$W/survivor.retries"
printf '[p8 t] restore start: window=x label=w2 auto=1 boot=0 self=p8-deadman.service\n[p8 t] FATAL: a window unit could not be stopped\n' >> "$W/restore.log"
echo "t self=p8-deadman.service rc=1" >> "$W/restore.rc.log"
o=$(vo post --window "$W")
for r in 'no W/REBOOT-NEEDED' 'no dead-man survivor retry' 'no FATAL in a dead-man restore run' 'every dead-man restore ended rc=0'; do
  printf '%s\n' "$o" | grep -q "^FAIL $r" && ok "post FAILs: $r" || bad "post did not flag: $r"
done
grep -q 'self=shell rc=' "$W/restore.rc.log" && [ "$(grep -c 'rc=' "$W/restore.rc.log")" -ge 2 ] && ok "restore.rc.log keeps a history (one line per run)" || bad "no restore.rc history"
rm -f "$W/REBOOT-NEEDED" "$W/survivor.retries"

echo "== F-P8-T-LOOP: the updater/agent stop loops until ONE pass finds all four inactive"
ready w2; fake_js_install
echo active > "$SD/active.xorgate-agent-rs"; echo xorgate-updater-rs > "$SD/ON_STOP_ACTIVATE.xorgate-agent-rs"   # stopping the DEV agent starts a NEW updater
: > "$SD/log"; run p8-restore.sh --window "$W" >/dev/null 2>&1
grep -q 'updater/agent pass 2: still running: xorgate-updater-rs.service' "$W/restore.log" && grep -q 'updater/agent pass 3: all four inactive' "$W/restore.log" \
  && [ "$(grep -n 'window unit xorgate-updater-rs.service inactive (verified)' "$W/restore.log" | tail -1 | cut -d: -f1)" -lt "$(grep -n 'PROD back:' "$W/restore.log" | head -1 | cut -d: -f1)" ] \
  && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && ok "an updater started by the stopping agent is caught by pass 2; pass 3 finds all four inactive, before any move back" || { bad "F-P8-T-LOOP"; grep 'updater/agent' "$W/restore.log"; }
echo "== F-P8-T-DELTEST: a deleted-but-running binary fails the live-exe check"
mkdir -p "$R/proc/1"; ln -sfn "/opt/xorgate-rs/bin/xorgate-rs (deleted)" "$R/proc/1/exe"; echo 1 > "$SD/prop.xorgate-agent-rs.MainPID"
vo post --window "$W" | grep -q '^FAIL xorgate-agent-rs runs a DELETED binary' && ok "a '(deleted)' /proc/<pid>/exe FAILs with its own reason" || bad "DELTEST: deleted exe not flagged"
rm -rf "$R/proc/1"

echo "== cam0 hook policy (mocked: the policy function over a request table)"
command -v node >/dev/null || { /usr/bin/apt-get update -qq >/dev/null 2>&1; /usr/bin/apt-get install -y -qq nodejs >/dev/null 2>&1; }  # the real apt-get: $STUB has a fake one
if command -v node >/dev/null; then
  out=$(P8_CAM0_SELFTEST=1 bash "$B/p8-cam0.sh" 2>&1); rc=$?
  [ $rc = 0 ] && ! printf '%s' "$out" | grep -q '^FAIL' && printf '%s' "$out" | grep -q '^PASS prod: the only non-GET that passes is POST /v1/live/access' \
    && printf '%s' "$out" | grep -q '^PASS prod DELETE https://api.xorgate.io/v1/live/access -> abort' && ok "cam0 (PROD): only POST /v1/live/access passes; DELETE of it and every other write abort ($(printf '%s' "$out" | grep -c '^PASS') rows)" || { bad "cam0 policy"; printf '%s\n' "$out"; }
  out=$(P8_CAM0_SELFTEST=1 bash "$B/p8-devcam0.sh" 2>&1); rc=$?
  [ $rc = 0 ] && ! printf '%s' "$out" | grep -q '^FAIL' && printf '%s' "$out" | grep -q '^PASS dev GET https://api.xorgate.io/v1/devices -> abort' \
    && printf '%s' "$out" | grep -q '^PASS dev: the only non-GET that passes is POST /v1/live/access' && ok "devcam0 (DEV): every PROD request aborts; on DEV only POST /v1/live/access passes ($(printf '%s' "$out" | grep -c '^PASS') rows)" || { bad "devcam0 policy"; printf '%s\n' "$out"; }
  mkdir -p /tmp/playwright 2>/dev/null
  out=$(XG_USER=u XG_PASS=p P8_PLAYWRIGHT_NODE_PATH=/tmp P8_CAM0_TEST=1 P8_CAM0_CONSOLE=https://console.xorgate.io P8_DEV_DEVICE=58b51737-0000-0000-0000-000000000001 bash "$B/p8-devcam0.sh" 2>&1); rc=$?
  [ $rc = 2 ] && printf '%s' "$out" | grep -q 'REFUSED: the DEV check against a PROD host' && ok "devcam0: REFUSES a PROD host before any browser starts" || { bad "devcam0 PROD refusal: rc=$rc $out"; }
  out=$(XG_USER=u XG_PASS=p P8_PLAYWRIGHT_NODE_PATH=/tmp P8_CAM0_TEST=1 P8_CAM0_API_BASE=https://api.xorgate.io P8_DEV_DEVICE=58b51737-0000-0000-0000-000000000001 bash "$B/p8-devcam0.sh" 2>&1); rc=$?
  [ $rc = 2 ] && printf '%s' "$out" | grep -q 'REFUSED' && ok "devcam0: REFUSES the PROD API host too" || bad "devcam0 PROD API refusal: rc=$rc"
  rmdir /tmp/playwright 2>/dev/null || true
  grep -q 'serviceWorkers: "block"' "$B/p8-cam0.sh" && ok "cam0: service workers blocked" || bad "cam0: no serviceWorkers block"
else bad "cam0 policy: no node (could not install nodejs)"; fi

echo "== T2 residual: SIGHUP between the disarm and the start arms the failsafe"
ready w1; fake_js_install; : > "$SD/log"
P8_TEST_PAUSE_S=6 bash "$W/bin/p8-restore.sh" --window "$W" > "$TMP/hup.out" 2>&1 & rp=$!
for _ in $(seq 100); do [ ! -e "$R/root/p8-window.active" ] && grep -q 'disarm' "$SD/log" 2>/dev/null && break; [ ! -e "$R/etc/systemd/system/p8-deadman.timer" ] && break; sleep 0.1; done
sleep 0.5; kill -HUP "$rp"; wait "$rp"; rc=$?
[ $rc = 1 ] && [ -e "$R/root/p8-window.active" ] && [ -e "$R/etc/systemd/system/p8-deadman-boot.service" ] && grep -q '^OnActiveSec=' "$R/etc/systemd/system/p8-deadman.timer" \
  && ! grep -q 'systemctl start --no-block xorgate-agent-rs' "$SD/log" && grep -q 'signal between the disarm and the start' "$W/restore.log" \
  && ok "SIGHUP after the disarm: exit 1, PROD not started, a failsafe dead-man + boot unit re-armed" || { bad "T2 residual: rc=$rc"; tail -3 "$W/restore.log"; }
run p8-restore.sh --auto --window "$W" >/dev/null 2>&1; grep -q 'systemctl start --no-block xorgate-agent-rs' "$SD/log" && ok "…and that failsafe's --auto restore starts PROD" || bad "T2 residual failsafe restore"

echo "== T8 NIT: no temp file NM could load; a stale one is removed; the fallback keeps mode + owner"
ready w1; fake_js_install
echo stale > "$R/etc/NetworkManager/conf.d/.p8new-99-sim7600-unmanaged.conf~"
printf '# changed\n' > "$R/etc/sudoers.d/010_pi-nopasswd"; chmod 0644 "$R/etc/sudoers.d/010_pi-nopasswd"; chown pi "$R/etc/sudoers.d/010_pi-nopasswd"
mkdir -p "$TMP/cpstub"; printf '#!/bin/bash\nfor a in "$@"; do [ "$a" = -a ] && exit 1; done\nexec /bin/cp "$@"\n' > "$TMP/cpstub/cp"; chmod 0755 "$TMP/cpstub/cp"
PATH="$TMP/cpstub:$PATH" run p8-restore.sh --window "$W" >/dev/null 2>&1
[ "$(stat -c '%a %U' "$R/etc/sudoers.d/010_pi-nopasswd")" = "440 root" ] && grep -q '^# 010_pi-nopasswd' "$R/etc/sudoers.d/010_pi-nopasswd" && ok "the install(1) fallback (cp -a failing) restores content AND mode 0440 + owner root" || bad "fallback mode/owner: $(stat -c '%a %U' "$R/etc/sudoers.d/010_pi-nopasswd")"
[ ! -e "$R/etc/NetworkManager/conf.d/.p8new-99-sim7600-unmanaged.conf~" ] && ! find "$R" -name '.p8new-*~' | grep -q . && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && ok "a stale .p8new-*~ temp is removed; none left; verify pre PASS" || { bad "T8 temp cleanup"; cat "$W/verify-pre.out"; }
grep -q 'p8new' "$W/bin/p8-restore.sh" && ! grep -qE '"\$d\.p8new"' "$W/bin/p8-restore.sh" && ok "temp names start with '.' and end with '~' (NM keyfile/conf.d, sudo, cron, apt, udev, systemd skip them)" || bad "temp naming"

echo "== unlink-check (WP0 §2 method) + run-install (HOME=/root nit) + lint"
bash "$B/p8-verify.sh" unlink-check > "$TMP/u.out" 2>&1 && grep -q '^PASS install replaces by unlink' "$TMP/u.out" && grep -q 'in place: yes' "$TMP/u.out" && ok "unlink-check: install unlinks, cp is in place" || { bad "unlink-check"; cat "$TMP/u.out"; }
ready w1; printf '#!/bin/bash\necho hi\n' > "$TMP/install.sh"; : > "$SD/systemd-run.log"
run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" --reprovision >/dev/null 2>&1
l=$(cat "$SD/systemd-run.log")
case $l in *--unit=p8-install-w1*--setenv=SUDO_USER=pi*--setenv=HOME=/root*--wait*--collect*StandardOutput=file:/root/p8-20261114T120000Z/w1-install.log*"/bin/bash $TMP/install.sh --reprovision"*) ok "run-install: transient p8-install-w1, SUDO_USER=pi, HOME=/root, output to the window log" ;; *) bad "run-install line: $l" ;; esac
echo 7 > "$SD/run-rc"; run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" >/dev/null 2>&1; [ $? = 7 ] && ok "run-install returns the installer's exit" || bad "run-install rc"; rm -f "$SD/run-rc"
command -v shellcheck >/dev/null && { (cd "$REPO" && shellcheck -x scripts/bench/p8-*.sh scripts/bench/tests/p8-window.sh scripts/bench/tests/systemd/p8-window.sh) > "$TMP/sc" 2>&1 && ok "shellcheck clean (tools + both suites)" || { bad "shellcheck"; cat "$TMP/sc"; }; }
for f in "$REPO"/scripts/bench/p8-*.sh "$REPO/scripts/bench/tests/systemd/p8-window.sh"; do bash -n "$f" || bad "bash -n $f"; done

rm -rf "$TMP"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
