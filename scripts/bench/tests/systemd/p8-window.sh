#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016  # test assertions: `a && ok || bad`; literal sed/printf bodies
# Phase 8 WP-T under a REAL systemd 252 (PID 1, privileged bookworm): the
# bench-window tools on a fake 58b51737 (fake PROD Rust agent + disabled PROD
# JS unit, a fake sim7600-lte daemon that turns settings.json's gnssAntBias
# into telemetry, a fake drive blackbox). Cases:
#   CTL  `systemctl start multi-user.target` queues an enabled, inactive unit
#        (proves the boot emulation below is a boot transaction)
#   BIR  backup-install-restore (plan DA6 rehearsal): baseline, arm (cap-10,
#        F5), backup, aside + seed, a JS-style install in p8-install-w1, the
#        manual restore, verify pre BEFORE the start, verify post PASS, the
#        sampler summary PASS, C3/C4/C5 byte-exact
#   B1   F1 boot path, W1 shape: the test JS unit is enabled when the box
#        "boots"; the boot oneshot cancels its queued job, restores, verifies
#        INSIDE the oneshot, starts PROD --no-block; neither JS unit ever starts
#   B2   F1 boot path, W2 shape: the TEST xorgate-agent-rs (native) is queued;
#        only the PROD binary starts, after the verify
#   B1M  MUTANT: B1 with the cancel removed -> a JS unit starts (caught)
#   T3   the Persistent dead-man TIMER fires (--auto), F3 with a REAL dpkg lock
#        held by the installer + a really half-unpacked package: the restore
#        waits for the lock, THEN stops the installer, then dpkg --configure -a
#   T3M  MUTANT: T3 without the wait -> the installer dies holding the lock (caught)
# Run (systemd must be PID 1; ~5 min):
#   docker run -d --name xg-p8 --label xorgate.agent=<tag> --privileged --cgroupns=private \
#     --tmpfs /run --tmpfs /run/lock -v "$PWD:/src:ro" debian:bookworm \
#     bash -c 'apt-get update -qq && apt-get install -y -qq systemd python3 sudo iproute2 >/dev/null && exec /lib/systemd/systemd'
#   docker exec xg-p8 bash /src/scripts/bench/tests/systemd/p8-window.sh
#   docker rm -f xg-p8
# Nothing here sets or reads a clock tool; the fixture has no setclock file.
set -uo pipefail
SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)
B=$SRC/scripts/bench
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
[ "$(cat /proc/1/comm)" = systemd ] || { echo "FAIL: systemd is not PID 1 (see the header)"; exit 2; }
for c in python3 ip dpkg-deb; do type -P "$c" >/dev/null || { echo "FAIL: $c missing (see the header)"; exit 2; }; done
for _ in $(seq 60); do s=$(systemctl is-system-running 2>/dev/null); case $s in running | degraded) break ;; esac; sleep 1; done
echo "  $(systemctl --version | head -1); system $s"
id pi >/dev/null 2>&1 || useradd -m pi
LOG=/var/log/p8sd.log; : > "$LOG"; chmod 0666 "$LOG"
SDIR=/etc/systemd/system
epoch() { python3 -I -c 'import calendar,sys,time; print(calendar.timegm(time.strptime(sys.argv[1], "%Y-%m-%dT%H:%M:%SZ")))' "$1"; }
wait_active() { local _i; for _i in $(seq "$2"); do systemctl is-active --quiet "$1" && return 0; sleep 1; done; return 1; }
wait_grep() { local _i; for _i in $(seq "$3"); do grep -q "$2" "$1" 2>/dev/null && return 0; sleep 1; done; return 1; }
since_boot() { awk -v m="$1" 'f; $0 == m {f = 1}' "$LOG"; }  # hl-clock-mocked ($0 is awk's record, not a command)

# --------------------------------------------------------------- the fake bench
install -d /usr/local/sbin
cat > /usr/local/sbin/p8sd-log <<'EOF'
#!/bin/bash
echo "$1 start $(date +%s.%N)" >> /var/log/p8sd.log
EOF
cat > /usr/local/bin/sim7600-lte.sh <<'EOF'
#!/bin/bash
# fake modem daemon: settings.json gnssAntBias -> telemetry gnss_ant_bias, 1 Hz
mkdir -p /run/sim7600-lte
while :; do
  b=0
  grep -q '"gnssAntBias": *true' /var/lib/xorgate/settings.json 2>/dev/null && b=1
  printf '{"timestamp":"%s","status":"connected","gnss_ant_bias":%s,"cellular":{"state":"ok","apn_applied":"bench.apn.test"},"recovery":{"connect_fails":0,"radio_cycles_since_ok":0,"resets_24h":0,"last_reset":null}}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$b" > /run/sim7600-lte/telemetry.json.tmp
  mv /run/sim7600-lte/telemetry.json.tmp /run/sim7600-lte/telemetry.json
  sleep 1
done
EOF
cat > /usr/local/bin/xorgate-drive-blackbox <<'EOF'
#!/bin/bash
mkdir -p /var/lib/xorgate-rs/drive-blackbox
while :; do echo '{"t":1}' >> /var/lib/xorgate-rs/drive-blackbox/b1.ndjson; sleep 1; done
EOF
printf '#!/bin/sh\necho throttled=0x0\n' > /usr/bin/vcgencmd
chmod 0755 /usr/local/sbin/p8sd-log /usr/local/bin/sim7600-lte.sh /usr/local/bin/xorgate-drive-blackbox /usr/bin/vcgencmd
unit() { # <name> <log tag> <ExecStart> [extra [Unit] line]
  printf '[Unit]\nDescription=fake %s\n%s\n[Service]\nExecStartPre=/usr/local/sbin/p8sd-log %s\nExecStart=%s\n[Install]\nWantedBy=multi-user.target\n' "$1" "${4:-}" "$2" "$3" > "$SDIR/$1.service"
}
mkdir -p /etc/xorgate/pipelines /var/lib/xorgate/recordings/cam0 /var/lib/xorgate-rs/tapes /var/lib/xorgate-rs-install \
  /opt/xorgate/node_modules/x /opt/xorgate-rs/bin /opt/xorgate-rs.prev/bin /boot/firmware /etc/NetworkManager/conf.d \
  /etc/systemd/logind.conf.d /etc/apt/apt.conf.d /etc/cron.d /etc/udev/rules.d /etc/modprobe.d \
  "$SDIR/xorgate-agent.service.d" "$SDIR/xorgate-agent-rs.service.d" "$SDIR/xorgate-updater-rs.service.d"
head -c 256 /dev/urandom > /etc/xorgate/device.key.pem; head -c 512 /dev/urandom > /etc/xorgate/device.cert.pem
echo ca > /etc/xorgate/root-ca.pem; echo DEVICE_ID=prod > /etc/xorgate/agent.env; chmod 0644 /etc/xorgate/agent.env
echo gst > /etc/xorgate/pipelines/cam0.gst
python3 -I -c 'import json; json.dump({"gnssAntBias": True, "lteRecovery": {"deadZoneProfile": "balanced", "gpsFixResetPolicy": "defer"}, "mode": "x"}, open("/var/lib/xorgate/settings.json", "w"))'
echo '{}' > /var/lib/xorgate/state.json; echo seg > /var/lib/xorgate/recordings/cam0/a.mp4
chown -R pi:pi /etc/xorgate /var/lib/xorgate; chmod 0700 /etc/xorgate /var/lib/xorgate
echo '{"state":"idle"}' > /var/lib/xorgate/apn-apply.json        # WP0 C3: root-owned inside the pi dir
echo tape > /var/lib/xorgate-rs/tapes/t1; chown -R pi:pi /var/lib/xorgate-rs
printf 'state=converted\nsource=fake\n' > /var/lib/xorgate-rs-install/record; chmod 0700 /var/lib/xorgate-rs-install
echo js > /opt/xorgate/node_modules/x/i.js
cp /usr/bin/sleep /opt/xorgate-rs/bin/xorgate-rs; echo prev > /opt/xorgate-rs.prev/bin/xorgate-rs
printf '{"version":"9bd55b9","channel":"bench"}\n' > /opt/xorgate-rs/build-info.json
CERT=$(sha256sum /opt/xorgate-rs/bin/xorgate-rs | cut -c1-64)
export P8_CERT_BIN_SHA=$CERT
systemctl set-environment "P8_CERT_BIN_SHA=$CERT"   # the dead-man units' restore sees the fake cert
unit xorgate-agent-rs prod-rs "/opt/xorgate-rs/bin/xorgate-rs infinity" $'After=xorgate-agent.service\nConflicts=xorgate-agent.service'
unit xorgate-agent prod-js "/usr/bin/sleep infinity"
printf '[Service]\nUser=pi\n' > "$SDIR/xorgate-agent-rs.service.d/10-user.conf"
printf '[Service]\nUser=pi\n' > "$SDIR/xorgate-agent.service.d/10-user.conf"
printf '[Service]\nWorkingDirectory=/opt/xorgate\n' > "$SDIR/xorgate-agent.service.d/20-workdir.conf"     # WP0 C5
printf '[Service]\nType=oneshot\nExecStart=/bin/true\n' > "$SDIR/xorgate-updater.service"
printf '[Service]\nType=oneshot\nExecStart=/bin/true\n' > "$SDIR/xorgate-updater-rs.service"
printf '[Service]\nEnvironment=DIST=x\n' > "$SDIR/xorgate-updater-rs.service.d/10-dist.conf"
printf '[Service]\nExecStart=/usr/local/bin/sim7600-lte.sh\n[Install]\nWantedBy=multi-user.target\n' > "$SDIR/sim7600-lte.service"
printf '[Service]\nExecStart=/usr/local/bin/xorgate-drive-blackbox\n[Install]\nWantedBy=multi-user.target\n' > "$SDIR/xorgate-drive-blackbox.service"
for f in 020_xorgate-power 021_xorgate-clock 023_xorgate-camrotate 024_xorgate-update-rs; do echo "# $f" > "/etc/sudoers.d/$f"; chmod 0440 "/etc/sudoers.d/$f"; done
printf '# the bench comment (WP0 C4)\n# pi rule\n' > /etc/sudoers.d/022_xorgate-update; chmod 0440 /etc/sudoers.d/022_xorgate-update
printf '#!/bin/sh\necho xorgate\n' > /usr/local/bin/xorgate; printf '#!/bin/sh\necho xorgate-rs\n' > /usr/local/bin/xorgate-rs
printf '#!/bin/sh\necho camrotate\n' > /usr/local/bin/xorgate-camrotate; chmod 0755 /usr/local/bin/xorgate /usr/local/bin/xorgate-rs /usr/local/bin/xorgate-camrotate
echo rule > /etc/udev/rules.d/99-sim7600-lte.rules; echo nm > /etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf
echo ipc > /etc/systemd/logind.conf.d/10-xorgate-removeipc.conf; echo 'DPkg::Lock::Timeout "60";' > /etc/apt/apt.conf.d/99xorgate-lock-timeout
echo cron > /etc/cron.d/xorgate-fake-hwclock  # hl-clock-mocked (an inert fixture file NAME; nothing runs it)
printf 'dtoverlay=imx290,cam0\ndtoverlay=imx290,cam1\n' > /boot/firmware/config.txt
echo i2c-dev > /etc/modules; : > /etc/modprobe.d/raspi-blacklist.conf
# a real, installed package whose re-unpack leaves dpkg "unpacked, not configured" (T3)
mkdir -p /root/p8dummy/DEBIAN
printf 'Package: p8dummy\nVersion: 1.0\nArchitecture: all\nMaintainer: p8 <p8@example.invalid>\nDescription: p8 test package\n' > /root/p8dummy/DEBIAN/control
dpkg-deb --build /root/p8dummy /root/p8dummy.deb >/dev/null && dpkg -i /root/p8dummy.deb >/dev/null || bad "p8dummy build/install"
systemctl daemon-reload
systemctl enable --now sim7600-lte xorgate-drive-blackbox xorgate-agent-rs >/dev/null 2>&1
wait_active xorgate-agent-rs 20 && wait_active sim7600-lte 10 && ok "fake PROD bench up (rust agent, modem daemon, blackbox)" || bad "fixture did not come up"
sleep 2

# ---------------------------------------------------------------- installers
cat > /root/fake-install-js.sh <<'EOF'
#!/bin/bash
# What the JS install.sh does to the moved-aside bench: a new agent surface,
# identical system bytes through `install` (unlink), a group, enable --now.
set -e
install -d -m 0700 -o pi -g pi /etc/xorgate
head -c 256 /dev/urandom > /etc/xorgate/device.key.pem; head -c 512 /dev/urandom > /etc/xorgate/device.cert.pem
printf 'DEVICE_ID=dev\n' > /etc/xorgate/agent.env; chmod 0600 /etc/xorgate/agent.env; chown -R pi:pi /etc/xorgate
install -d /opt/xorgate; echo js-dev > /opt/xorgate/agent.js
cp /usr/local/bin/sim7600-lte.sh /tmp/p8-modem.copy; install -m 0755 /tmp/p8-modem.copy /usr/local/bin/sim7600-lte.sh
printf '[Unit]\nDescription=test js\n[Service]\nExecStartPre=/usr/local/sbin/p8sd-log test-js\nExecStart=/usr/bin/sleep infinity\n[Install]\nWantedBy=multi-user.target\n' > /etc/systemd/system/xorgate-agent.service
mkdir -p /etc/systemd/system/xorgate-agent.service.d; printf '[Service]\nUser=pi\n' > /etc/systemd/system/xorgate-agent.service.d/10-user.conf
for f in 020_xorgate-power 021_xorgate-clock 023_xorgate-camrotate; do echo "# js $f" > "/etc/sudoers.d/$f"; chmod 0440 "/etc/sudoers.d/$f"; done
printf '# the TEMPLATE comment\n# pi rule\n' > /etc/sudoers.d/022_xorgate-update; chmod 0440 /etc/sudoers.d/022_xorgate-update
printf '#!/bin/sh\nexec node\n' > /usr/local/bin/xorgate; chmod 0755 /usr/local/bin/xorgate
groupadd -f p8sdgrp; usermod -aG p8sdgrp pi
printf '{"gnssAntBias": true}\n' > /var/lib/xorgate/settings.json.new; chown pi:pi /var/lib/xorgate/settings.json.new; mv /var/lib/xorgate/settings.json.new /var/lib/xorgate/settings.json
systemctl daemon-reload
systemctl enable --now xorgate-agent
echo "install done"
EOF
cat > /root/fake-install-rs.sh <<'EOF'
#!/bin/bash
# A native Rust install (W2 shape): the TEST xorgate-agent-rs on /opt/xorgate-rs.
set -e
install -d -m 0700 -o pi -g pi /etc/xorgate
head -c 256 /dev/urandom > /etc/xorgate/device.key.pem; chown -R pi:pi /etc/xorgate
install -d /opt/xorgate-rs/bin; cp /usr/bin/sleep /opt/xorgate-rs/bin/xorgate-rs; echo TESTBUILD >> /opt/xorgate-rs/bin/xorgate-rs
printf '{"version":"testB"}\n' > /opt/xorgate-rs/build-info.json
printf '[Unit]\nDescription=test rs\n[Service]\nExecStartPre=/usr/local/sbin/p8sd-log test-rs\nExecStart=/opt/xorgate-rs/bin/xorgate-rs infinity\n[Install]\nWantedBy=multi-user.target\n' > /etc/systemd/system/xorgate-agent-rs.service
mkdir -p /etc/systemd/system/xorgate-agent-rs.service.d /etc/systemd/system/xorgate-updater-rs.service.d
printf '[Service]\nUser=pi\n' > /etc/systemd/system/xorgate-agent-rs.service.d/10-user.conf
printf '[Unit]\nConditionPathExists=!/opt/xorgate\n' > /etc/systemd/system/xorgate-agent-rs.service.d/20-native.conf
printf '[Service]\nType=oneshot\nExecStart=/bin/true\n' > /etc/systemd/system/xorgate-updater-rs.service
printf '[Service]\nEnvironment=DIST=dev\n' > /etc/systemd/system/xorgate-updater-rs.service.d/10-dist.conf
for f in 020_xorgate-power 021_xorgate-clock 023_xorgate-camrotate 024_xorgate-update-rs; do echo "# rs $f" > "/etc/sudoers.d/$f"; chmod 0440 "/etc/sudoers.d/$f"; done
printf '#!/bin/sh\nexec /opt/xorgate-rs/bin/xorgate-rs\n' > /usr/local/bin/xorgate-rs; chmod 0755 /usr/local/bin/xorgate-rs
systemctl daemon-reload
systemctl enable --now xorgate-agent-rs
echo "install done"
EOF
cat > /root/fake-install-dpkg.sh <<'EOF'
#!/bin/bash
# T3: the installer is mid-apt when the dead-man fires: a package half-done,
# then the dpkg frontend lock held for 40 s.
dpkg --unpack /root/p8dummy.deb >/dev/null 2>&1
echo "unpacked"
python3 -I -c 'import fcntl, time; f = open("/var/lib/dpkg/lock-frontend", "w"); fcntl.lockf(f, fcntl.LOCK_EX); print("locked", flush=True); time.sleep(40); fcntl.lockf(f, fcntl.LOCK_UN); print("released", flush=True)'
sleep 600
EOF
chmod 0755 /root/fake-install-js.sh /root/fake-install-rs.sh /root/fake-install-dpkg.sh

# ------------------------------------------------------------- window helpers
W=""
stage() { # <ts> [mutant: cancel|wait]
  W=/root/p8-$1
  install -d -m 0700 "$W" "$W/bin"; install -m 0700 "$B"/p8-*.sh "$W/bin/"
  case ${2:-} in
    cancel) sed -i '/^p8_cancel_agent_jobs/d' "$W/bin/p8-restore.sh"; ! grep -q '^p8_cancel_agent_jobs' "$W/bin/p8-restore.sh" || bad "cancel mutant not applied" ;;
    wait) sed -i 's/^    while p8_dpkg_locked && \[ "\$waited" -lt "\$P8_DPKG_WAIT_S" \]; do$/    while false; do/' "$W/bin/p8-lib.sh"; grep -q '^    while false; do$' "$W/bin/p8-lib.sh" || bad "wait mutant not applied" ;;
  esac
}
open_window() { # <ts> <label> <arm args...>; PROD running on entry
  local ts=$1 label=$2; shift 2
  stage "$ts" "${MUT:-}"
  bash "$W/bin/p8-verify.sh" baseline --window "$W" > /dev/null || bad "$label baseline"
  bash "$W/bin/p8-deadman.sh" arm --window "$W" --label "$label" "$@" > "$W/arm.out" 2>&1 || bad "$label arm: $(cat "$W/arm.out")"
  systemctl stop xorgate-agent-rs
  bash "$W/bin/p8-backup.sh" backup --window "$W" > "$W/backup.out" 2>&1 || bad "$label backup: $(cat "$W/backup.out")"
  bash "$W/bin/p8-backup.sh" aside --window "$W" --no-pull-check > "$W/aside.out" 2>&1 || bad "$label aside: $(cat "$W/aside.out")"
  bash "$W/bin/p8-modem-sampler.sh" start --window "$W" --label "$label" > /dev/null || bad "$label sampler start"
}
prod_back() { # make sure the fake PROD runs again (after a mutant)
  systemctl stop xorgate-agent >/dev/null 2>&1; systemctl start xorgate-agent-rs; wait_active xorgate-agent-rs 30
}
boot() { # a power cut + boot: the agents are down, then ONE boot transaction
  local m="BOOT $1"
  systemctl stop p8-modem-sampler xorgate-agent xorgate-agent-rs >/dev/null 2>&1
  echo "$m" >> "$LOG"
  systemctl start --no-block multi-user.target
  BOOT_T0=$(date +%s)
}

# ============================================================================
echo "== CTL: a boot transaction queues enabled, inactive units"
printf '[Service]\nExecStartPre=/usr/local/sbin/p8sd-log ctl-probe\nExecStart=/usr/bin/sleep infinity\n[Install]\nWantedBy=multi-user.target\n' > "$SDIR/xg-ctl-probe.service"
systemctl daemon-reload; systemctl enable xg-ctl-probe >/dev/null 2>&1
systemctl start --no-block multi-user.target
wait_active xg-ctl-probe 20 && ok "CTL: start multi-user.target started the enabled unit" || bad "CTL: the emulation does not queue wanted units"
systemctl disable --now xg-ctl-probe >/dev/null 2>&1; rm -f "$SDIR/xg-ctl-probe.service"; systemctl daemon-reload

echo "== BIR: backup-install-restore (W1 shape, manual restore)"
open_window 20261114T000001Z w1 --cap-min 60
t=$(date +%s); n=$(systemctl show p8-deadman.timer -p NextElapseUSecRealtime --value)
d=$(( $(date -d "$n" +%s) - t ))
[ "$d" -ge 2990 ] && [ "$d" -le 3005 ] && ok "F5: the timer elapses at arm + 50 min for a 60-min cap ($d s)" || bad "F5: next elapse in $d s ($n)"
[ "$(systemctl is-enabled p8-deadman-boot.service)" = enabled ] && ok "the boot unit is enabled" || bad "boot unit not enabled"
v=$(systemd-analyze verify "$SDIR/p8-deadman.timer" "$SDIR/p8-deadman.service" "$SDIR/p8-deadman-boot.service" 2>&1 | grep 'p8-deadman' || true)
[ -z "$v" ] && ok "systemd-analyze verify: the three dead-man units are clean" || bad "systemd-analyze: $v"
[ "$(cat /var/lib/xorgate/settings.json)" = "$(cat "$W/seed-settings.json")" ] && ok "the seed is in place" || bad "no seed"
sleep 2
grep -q '"gnss_ant_bias":1' /run/sim7600-lte/telemetry.json && ok "the modem daemon keeps the antenna bias ON with the seed (H10)" || bad "bias off on the seed"
modem_ino=$(stat -c %i /usr/local/bin/sim7600-lte.sh)
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w1 -- /root/fake-install-js.sh > "$W/ri.out" 2>&1 && ok "run-install: the JS-style install ran in p8-install-w1 (exit 0)" || bad "run-install: $(cat "$W/ri.out")"
grep -q 'install done' "$W/w1-install.log" && [ "$(stat -c %a "$W/w1-install.log")" = 600 ] && ok "the installer's output is in W/w1-install.log (0600)" || bad "install log"
systemctl is-active --quiet xorgate-agent && ok "the test JS agent runs" || bad "test JS not active"
grep -q '^test-js start' "$LOG" && ! grep -q '^prod-js start' "$LOG" && ok "the TEST JS unit ran, never the PROD one" || bad "JS units: $(cut -d' ' -f1 "$LOG" | tr '\n' ' ')"
dev_key=$(sha256sum /etc/xorgate/device.key.pem | cut -c1-64)
before=$(systemctl show sim7600-lte -p ActiveEnterTimestamp --value)
bash "$W/bin/p8-restore.sh" --window "$W" > "$W/restore.out" 2>&1 && ok "manual restore exit 0" || { bad "restore"; tail -n 30 "$W/restore.out"; }
grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && ok "verify pre PASS before the start" || { bad "verify pre"; cat "$W/verify-pre.out"; }
vp=$(epoch "$(grep 'verify pre: PASS' "$W/restore.log" | head -1 | sed 's/^\[p8 \([^]]*\)\].*/\1/')")
st=$(grep '^prod-rs start' "$LOG" | tail -1 | awk '{print int($3)}')
[ -n "$st" ] && [ "$st" -ge "$vp" ] && ok "PROD started after the verify ($st >= $vp)" || bad "start/verify order ($st vs $vp)"
systemctl is-active --quiet xorgate-agent-rs && ! systemctl is-active --quiet xorgate-agent && [ "$(systemctl is-enabled xorgate-agent)" = disabled ] && ok "PROD rust active; JS disabled + inactive" || bad "units after restore"
[ "$(sha256sum "$W/test-w1/etc/xorgate/device.key.pem" | cut -c1-64)" = "$dev_key" ] && [ "$(sha256sum /etc/xorgate/device.key.pem | cut -c1-64)" != "$dev_key" ] && ok "PROD key back, DEV key in W/test-w1" || bad "keys"
grep -q 'the bench comment' /etc/sudoers.d/022_xorgate-update && ok "C4: the bench's own 022 back" || bad "C4"
[ "$(stat -c %U /var/lib/xorgate/apn-apply.json)" = root ] && [ -f "$SDIR/xorgate-agent.service.d/20-workdir.conf" ] && ok "C3 owner + C5 drop-in back" || bad "C3/C5"
id -nG pi | tr ' ' '\n' | grep -qx p8sdgrp && bad "pi still in the test group" || ok "pi groups restored"
[ "$(systemctl show sim7600-lte -p ActiveEnterTimestamp --value)" = "$before" ] && [ "$(stat -c %i /usr/local/bin/sim7600-lte.sh)" != "" ] && ok "the modem daemon was never restarted (script reinstalled by unlink: inode $modem_ino -> $(stat -c %i /usr/local/bin/sim7600-lte.sh))" || bad "modem restarted"
wait_active xorgate-agent-rs 10; sleep 3
bash "$W/bin/p8-modem-sampler.sh" stop >/dev/null
bash "$W/bin/p8-verify.sh" post --window "$W" > "$W/post.out" 2>&1 && ok "verify post PASS (DA15 bench rows)" || { bad "verify post"; grep FAIL "$W/post.out"; }
bash "$W/bin/p8-modem-sampler.sh" summary "$W/modem-w1.tsv" > "$W/sum.out" 2>&1 && ok "sampler: $(grep -o 'samples=[0-9]* bias_off_ticks=[0-9]*' "$W/sum.out"), VERDICT PASS" || { bad "sampler summary"; cat "$W/sum.out"; }
grep -q bench.apn "$W/modem-w1.tsv" "$W/baseline.env" "$W/post.out" && bad "an APN value was written" || ok "no APN value in the sampler/baseline/verify files"

echo "== B1: F1 boot path, the TEST JS unit enabled at boot"
open_window 20261114T000002Z w1 --cap-min 60
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w1 -- /root/fake-install-js.sh > "$W/ri.out" 2>&1 || bad "B1 install"
boot B1
wait_active xorgate-agent-rs 120 && ok "B1: PROD rust active after the boot, $(( $(date +%s) - BOOT_T0 )) s (no deadlock)" || bad "B1: PROD not active"
sleep 3
since_boot "BOOT B1" | grep -qE '^(test-js|prod-js) start' && bad "B1: a JS unit started at boot: $(since_boot "BOOT B1" | tr '\n' ' ')" || ok "B1: neither JS unit started at boot"
grep -q 'F1: cancelling queued job [0-9]* (xorgate-agent.service)' "$W/restore.log" && ok "B1: the boot oneshot cancelled the queued JS job" || { bad "B1: no cancel logged"; cat "$W/restore.log"; }
grep -q '^DA15 pre: PASS' "$W/verify-pre.out" && ok "B1: verify pre PASS inside the oneshot" || bad "B1: verify pre"
vp=$(epoch "$(grep 'verify pre: PASS' "$W/restore.log" | head -1 | sed 's/^\[p8 \([^]]*\)\].*/\1/')"); st=$(since_boot "BOOT B1" | awk '/^prod-rs start/ {print int($3)}' | head -1)
[ -n "$st" ] && [ "$st" -ge "$vp" ] && ok "B1: PROD rust started after the in-oneshot verify" || bad "B1: start before verify ($st vs $vp)"
grep -q 'boot path: the agent starts when this oneshot exits' "$W/restore.log" && ok "B1: --boot did not wait on the agent" || bad "B1: boot log"
[ ! -e /root/p8-window.active ] && [ ! -e "$SDIR/p8-deadman-boot.service" ] && [ ! -e /var/lib/systemd/timers/stamp-p8-deadman.timer ] && ok "B1: marker, units and stamp gone" || bad "B1: leftovers"
bash "$W/bin/p8-verify.sh" post --window "$W" > "$W/post.out" 2>&1 && ok "B1: verify post PASS" || { bad "B1: verify post"; grep FAIL "$W/post.out"; }

echo "== B2: F1 boot path, the TEST native xorgate-agent-rs queued at boot"
open_window 20261114T000003Z w2 --cap-min 75
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w2 -- /root/fake-install-rs.sh > "$W/ri.out" 2>&1 || bad "B2 install: $(cat "$W/ri.out")"
grep -q '^test-rs start' "$LOG" && ok "B2: the test rust unit ran during the window" || bad "B2: test rs never ran"
boot B2
wait_active xorgate-agent-rs 120 && ok "B2: rust active after the boot" || bad "B2: not active"
sleep 3
since_boot "BOOT B2" | grep -q '^test-rs start' && bad "B2: the TEST rust unit started at boot" || ok "B2: the test unit never started at boot"
since_boot "BOOT B2" | grep -q '^prod-rs start' && [ "$(readlink "/proc/$(systemctl show -p MainPID --value xorgate-agent-rs)/exe")" = /opt/xorgate-rs/bin/xorgate-rs ] \
  && [ "$(sha256sum /opt/xorgate-rs/bin/xorgate-rs | cut -c1-64)" = "$CERT" ] && ok "B2: the PROD binary runs (certified sha)" || bad "B2: wrong binary"
grep -q 'F1: cancelling queued job [0-9]* (xorgate-agent-rs.service)' "$W/restore.log" && ok "B2: the queued test rust job was cancelled" || bad "B2: no cancel"
[ ! -e "$SDIR/xorgate-agent-rs.service.d/20-native.conf" ] && [ -e "$W/test-w2/etc/systemd/system/xorgate-agent-rs.service.d/20-native.conf" ] && ok "B2: 20-native.conf went with the test surface" || bad "B2: 20-native.conf"
bash "$W/bin/p8-verify.sh" post --window "$W" > "$W/post.out" 2>&1 && ok "B2: verify post PASS" || { bad "B2: verify post"; grep FAIL "$W/post.out"; }

echo "== B1M: MUTANT, the boot restore without the job cancel"
MUT="cancel" open_window 20261114T000004Z w1 --cap-min 60
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w1 -- /root/fake-install-js.sh > "$W/ri.out" 2>&1 || bad "B1M install"
boot B1M
for _ in $(seq 60); do [ ! -e /root/p8-window.active ] && since_boot "BOOT B1M" | grep -q 'js start' && break; sleep 1; done
sleep 3
since_boot "BOOT B1M" | grep -qE '^(test-js|prod-js) start' && ok "B1M: mutant CAUGHT (a JS unit starts at boot: $(since_boot "BOOT B1M" | cut -d' ' -f1 | tr '\n' ' '))" || bad "B1M: mutant NOT caught (the cancel is not load-bearing)"
prod_back; bash "$W/bin/p8-modem-sampler.sh" stop >/dev/null

echo "== T3: the Persistent dead-man TIMER fires mid-apt (F3, real dpkg lock + a half-unpacked package)"
open_window 20261114T000005Z w3 --deadline-in-s 25
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w3 -- /root/fake-install-dpkg.sh > "$W/ri.out" 2>&1 & rip=$!
wait_grep "$W/w3-install.log" locked 20 && ok "T3: the installer holds the dpkg frontend lock (package unpacked)" || bad "T3: installer did not lock"
wait_grep "$W/restore.log" 'restore done' 150 && ok "T3: the timer's restore ran" || bad "T3: no timer restore"
wait "$rip" 2>/dev/null
grep -q '^released' "$W/w3-install.log" && ok "F3: the installer was stopped only AFTER it released the lock" || bad "F3: the installer was killed holding the lock"
w=$(sed -n 's/.*F3: dpkg locks free (waited \([0-9]*\)s).*/\1/p' "$W/restore.log"); [ "${w:-0}" -ge 5 ] && ok "F3: the restore waited ${w}s for the lock" || bad "F3: waited '${w}'"
grep -q 'F3: dpkg repaired by --configure -a' "$W/restore.log" && [ "$(dpkg-query -W -f '${db:Status-Abbrev}' p8dummy)" = "ii " ] && ok "F3: dpkg --audit -> --configure -a repaired the half-unpacked package" || bad "F3: repair ($(dpkg-query -W -f '${db:Status-Abbrev}' p8dummy))"
systemctl is-active --quiet xorgate-agent-rs && grep -q 'verify post: PASS' "$W/restore.log" && ok "T3: --auto restarted PROD and its own verify post PASSed" || { bad "T3: auto verify post"; grep FAIL "$W/verify-post.out"; }
[ ! -e "$SDIR/p8-deadman.timer" ] && [ ! -e /var/lib/systemd/timers/stamp-p8-deadman.timer ] && [ ! -e /root/p8-window.active ] && ok "T3: the timer, its stamp and the marker are gone" || bad "T3: leftovers"
bash "$W/bin/p8-restore.sh" --window "$W" > "$W/again.out" 2>&1 && grep -q 'already finished' "$W/again.out" && systemctl is-active --quiet xorgate-agent-rs && ok "T3: a late restore is a no-op on the running PROD" || bad "T3: late restore"

echo "== T3M: MUTANT, the installer stopped without waiting for the lock"
MUT="wait" open_window 20261114T000006Z w3 --deadline-in-s 25
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w3 -- /root/fake-install-dpkg.sh > "$W/ri.out" 2>&1 & rip=$!
wait_grep "$W/restore.log" 'restore done' 150 || bad "T3M: no timer restore"
wait "$rip" 2>/dev/null
grep -q '^released' "$W/w3-install.log" && bad "T3M: mutant NOT caught" || ok "T3M: mutant CAUGHT (the installer died holding the lock)"
prod_back

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
