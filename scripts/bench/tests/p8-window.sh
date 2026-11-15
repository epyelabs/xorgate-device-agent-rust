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
  list-units) pat=${args[0]:-*}; while read -r l; do u=${l%% *}; case $u in $pat) echo "$l" ;; esac; done < <(cat "$SD/units" 2>/dev/null) ;;
  list-jobs) cat "$SD/jobs" 2>/dev/null; : > "$SD/jobs" ;;
  cancel) echo "${args[0]}" >> "$SD/cancelled" ;;
  stop) for u in "${args[@]}"; do echo inactive > "$SD/active.$(n "$u")"; sed -i "/^${u%.service}\(\.service\)\? /d" "$SD/units" 2>/dev/null; done ;;
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
cat "$SD/dpkg-list" 2>/dev/null; true
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
           etc/modprobe.d root run/sim7600-lte var/lib/systemd/timers var/lib/dpkg; do mkdir -p "$R/$d"; done
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
  printf '{"timestamp":"%s","status":"connected","gnss_ant_bias":1,"cellular":{"state":"ok","apn_applied":"secret.apn.example"},"recovery":{"connect_fails":0,"radio_cycles_since_ok":0,"resets_24h":0,"last_reset":null}}\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$R/run/sim7600-lte/telemetry.json"
  touch "$R/var/lib/dpkg/lock-frontend" "$R/var/lib/dpkg/lock"
  echo active > "$SD/active.xorgate-agent-rs"; echo enabled > "$SD/enabled.xorgate-agent-rs"
  echo enabled > "$SD/enabled.sim7600-lte"; echo active > "$SD/active.sim7600-lte"; echo enabled > "$SD/enabled.NetworkManager"
  echo active > "$SD/active.xorgate-drive-blackbox"; echo enabled > "$SD/enabled.xorgate-drive-blackbox"
  echo "Thu 2026-11-12 19:26:41 UTC" > "$SD/prop.sim7600-lte.ActiveEnterTimestamp"; echo 0 > "$SD/prop.sim7600-lte.NRestarts"
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
echo "p8-install-w1.service loaded active running p8 installer" > "$SD/units"
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
mkbench; echo "p8-install-w1.service loaded active running p8 installer" > "$SD/units"; : > "$H"
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

echo "== unlink-check (WP0 §2 method) + run-install (HOME=/root nit) + lint"
bash "$B/p8-verify.sh" unlink-check > "$TMP/u.out" 2>&1 && grep -q '^PASS install replaces by unlink' "$TMP/u.out" && grep -q 'in place: yes' "$TMP/u.out" && ok "unlink-check: install unlinks, cp is in place" || { bad "unlink-check"; cat "$TMP/u.out"; }
mkbench; touch "$W/aside.done"; printf '#!/bin/bash\necho hi\n' > "$TMP/install.sh"; : > "$SD/systemd-run.log"
run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" --reprovision >/dev/null 2>&1
l=$(cat "$SD/systemd-run.log")
case $l in *--unit=p8-install-w1*--setenv=SUDO_USER=pi*--setenv=HOME=/root*--wait*--collect*StandardOutput=file:/root/p8-20261114T120000Z/w1-install.log*"/bin/bash $TMP/install.sh --reprovision"*) ok "run-install: transient p8-install-w1, SUDO_USER=pi, HOME=/root, output to the window log" ;; *) bad "run-install line: $l" ;; esac
echo 7 > "$SD/run-rc"; run p8-backup.sh run-install --window "$W" --label w1 -- "$TMP/install.sh" >/dev/null 2>&1; [ $? = 7 ] && ok "run-install returns the installer's exit" || bad "run-install rc"; rm -f "$SD/run-rc"
command -v shellcheck >/dev/null && { shellcheck -x "$REPO"/scripts/bench/p8-*.sh "$REPO/scripts/bench/tests/p8-window.sh" "$REPO/scripts/bench/tests/systemd/p8-window.sh" > "$TMP/sc" 2>&1 && ok "shellcheck clean (tools + both suites)" || { bad "shellcheck"; cat "$TMP/sc"; }; }
for f in "$REPO"/scripts/bench/p8-*.sh "$REPO/scripts/bench/tests/systemd/p8-window.sh"; do bash -n "$f" || bad "bash -n $f"; done

rm -rf "$TMP"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
