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
# WP-R1 (phase-8/provision-rs @ ac3dcae): enablement is a boot GENERATOR, not `systemctl enable`
mkdir -p /etc/systemd/system-generators
printf '%s\n' '#!/bin/sh' '[ -e /opt/xorgate ] && exit 0' 'mkdir -p "$1/multi-user.target.wants" || exit 0' \
  'ln -sf /etc/systemd/system/xorgate-agent-rs.service "$1/multi-user.target.wants/xorgate-agent-rs.service"' > /etc/systemd/system-generators/xorgate-agent-rs-native
chmod 0755 /etc/systemd/system-generators/xorgate-agent-rs-native
systemctl daemon-reload
systemctl start xorgate-agent-rs
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
cat > /root/fake-provision.sh <<'EOF'
#!/bin/bash
# WP-R1 MUST-2 stand-in: the provisioner that keeps writing the DEV identity
# into /etc/xorgate and a build into /opt. "ignore" ignores SIGTERM.
[ "${1:-term}" = ignore ] && trap '' TERM
# As provision-rs.sh: one run at a time, refused while the lock is held.
exec 9>/run/xorgate-provision-rs.lock
flock -n 9 || { echo "refused $(date +%s.%N)" >> /var/tmp/p8prov.refused; exit 3; }
date +%s.%N > /var/tmp/p8prov.first
while :; do
  mkdir -p /etc/xorgate /opt/xorgate-rs.new
  head -c 64 /dev/urandom > /etc/xorgate/.dev-key.tmp && mv -f /etc/xorgate/.dev-key.tmp /etc/xorgate/device.key.pem
  date +%s.%N >> /opt/xorgate-rs.new/stamp
  echo "tick" >> /var/log/xorgate-provision-rs.log
  date +%s.%N > /var/tmp/p8prov.last
  sleep 0.2
done
EOF
cat > /root/fake-install-handoff.sh <<'EOF'
#!/bin/bash
# provision-rs.sh's hand-off shape: a SIBLING unit, then a polling client.
systemd-run --unit=xorgate-provision-rs --collect --quiet --no-block /bin/bash /root/fake-provision.sh "${1:-term}"
echo "handed off"
while :; do
  case $(systemctl is-active xorgate-provision-rs 2>/dev/null) in active | activating | deactivating) sleep 1 ;; *) break ;; esac
done
echo "provision ended"
EOF
cat > /root/fake-install-late.sh <<'EOF'
#!/bin/bash
# The hand-off comes LATE: the client is mid-apt (holds the dpkg frontend lock
# for 14 s) when the restore starts and lists the units, and hands off 7 s in,
# while the restore is still in its F3 dpkg wait.
python3 -I -c 'import fcntl, time; f = open("/var/lib/dpkg/lock-frontend", "w"); fcntl.lockf(f, fcntl.LOCK_EX); print("locked", flush=True); time.sleep(14); fcntl.lockf(f, fcntl.LOCK_UN); print("released", flush=True)' & hp=$!
until [ -e /var/tmp/p8m2l.go ]; do sleep 0.1; done; sleep 7
systemd-run --unit=xorgate-provision-rs --collect --quiet --no-block /bin/bash /root/fake-provision.sh term
echo "handed off late $(date +%s.%N)"
wait "$hp"
while :; do
  case $(systemctl is-active xorgate-provision-rs 2>/dev/null) in active | activating | deactivating) sleep 1 ;; *) break ;; esac
done
EOF
chmod 0755 /root/fake-install-js.sh /root/fake-install-rs.sh /root/fake-install-dpkg.sh /root/fake-provision.sh /root/fake-install-handoff.sh /root/fake-install-late.sh

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
m1=$(systemctl show p8-deadman.timer -p NextElapseUSecMonotonic --value); tm=$(systemctl show p8-deadman.timer -p TimersMonotonic --value)
systemctl daemon-reload; m2=$(systemctl show p8-deadman.timer -p NextElapseUSecMonotonic --value)
span_s() { python3 -I -c 'import re,sys; u={"d":86400,"h":3600,"min":60,"s":1,"ms":0.001,"us":1e-6}; print(sum(float(n)*u[k] for n,k in re.findall(r"([0-9.]+)(d|h|min|ms|us|s)", sys.argv[1])))' "$1"; }
d=$(python3 -I -c 'import sys; print(abs(float(sys.argv[1]) - float(sys.argv[2])))' "$(span_s "$m1")" "$(span_s "$m2")")
case $tm in *OnActiveUSec*) [ -n "$m1" ] && [ "$m1" != infinity ] && python3 -I -c 'import sys; sys.exit(0 if float(sys.argv[1]) <= 2 else 1)' "$d" && ok "T4: a monotonic OnActiveSec elapse ($m1) next to OnCalendar, kept by daemon-reload (delta ${d}s)" || bad "T4: monotonic $m1 -> $m2" ;; *) bad "T4: no OnActiveUSec in TimersMonotonic ($tm)" ;; esac
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
grep -q 'self=p8-restore-20261114T000001Z.service' "$W/restore.log" && grep -q 'running as the transient unit p8-restore-' "$W/restore.out" && ok "the manual restore ran as the transient unit p8-restore-<ts> (ruling)" || bad "manual restore not in a unit"
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
out=$(bash "$W/bin/p8-deadman.sh" arm --window "$W" --cap-min 60 --label w1 2>&1); [ $? = 3 ] && [ ! -e /root/p8-window.active ] && ok "T1: the finished window cannot be re-armed" || bad "T1 re-arm: $out"
out=$(bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w1 -- /root/fake-install-js.sh 2>&1); rc=$?
[ "$rc" = 3 ] && ! systemctl is-active --quiet xorgate-agent && [ "$(sha256sum /etc/xorgate/device.key.pem | cut -c1-64)" != "$dev_key" ] && ok "T1: the finished window cannot be re-installed over PROD" || bad "T1 re-install: rc=$rc $out"

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
[ -L /run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service ] && [ ! -L "$SDIR/multi-user.target.wants/xorgate-agent-rs.service" ] \
  && ok "B2: the native install's generator wants the test unit (no /etc wants link, as provision-rs.sh)" || bad "B2: generator did not run"
boot B2
wait_active xorgate-agent-rs 120 && ok "B2: rust active after the boot" || bad "B2: not active"
sleep 3
since_boot "BOOT B2" | grep -q '^test-rs start' && bad "B2: the TEST rust unit started at boot" || ok "B2: the test unit never started at boot"
since_boot "BOOT B2" | grep -q '^prod-rs start' && [ "$(readlink "/proc/$(systemctl show -p MainPID --value xorgate-agent-rs)/exe")" = /opt/xorgate-rs/bin/xorgate-rs ] \
  && [ "$(sha256sum /opt/xorgate-rs/bin/xorgate-rs | cut -c1-64)" = "$CERT" ] && ok "B2: the PROD binary runs (certified sha)" || bad "B2: wrong binary"
grep -q 'F1: cancelling queued job [0-9]* (xorgate-agent-rs.service)' "$W/restore.log" && ok "B2: the queued test rust job was cancelled" || bad "B2: no cancel"
[ ! -e "$SDIR/xorgate-agent-rs.service.d/20-native.conf" ] && [ -e "$W/test-w2/etc/systemd/system/xorgate-agent-rs.service.d/20-native.conf" ] && ok "B2: 20-native.conf went with the test surface" || bad "B2: 20-native.conf"
[ ! -e /etc/systemd/system-generators/xorgate-agent-rs-native ] && [ -e "$W/test-w2/etc/systemd/system-generators/xorgate-agent-rs-native" ] \
  && [ ! -L /run/systemd/generator/multi-user.target.wants/xorgate-agent-rs.service ] && [ -L "$SDIR/multi-user.target.wants/xorgate-agent-rs.service" ] \
  && ok "B2: the generator and its /run link are gone; PROD is enabled by its own /etc link again" || bad "B2: generator survived the restore"
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

echo "== P1/P2: the ssh session dies during a MANUAL restore (fix-check probes)"
open_window 20261114T000007Z w1 --cap-min 60
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w1 -- /root/fake-install-js.sh > "$W/ri.out" 2>&1 || bad "P1 install"
bash "$W/bin/p8-restore.sh" --window "$W" 2>&1 | true      # P1: the reader is gone at once (a closed ssh channel, no SIGHUP)
wait_grep "$W/restore.done" verify= 120 && wait_active xorgate-agent-rs 60 && grep -q 'verify=PASS' "$W/restore.done" \
  && ok "P1: a closed client pipe: the restore unit finished, PROD started, verify PASS" || { bad "P1: PROD not started"; tail -5 "$W/restore.log"; }
bash "$W/bin/p8-modem-sampler.sh" stop >/dev/null
open_window 20261114T000008Z w1 --cap-min 60
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w1 -- /root/fake-install-js.sh > "$W/ri.out" 2>&1 || bad "P2 install"
script -qefc "bash $W/bin/p8-restore.sh --window $W" /dev/null > /dev/null 2>&1 & sp=$!
sleep 2; kill -KILL "$sp" 2>/dev/null       # P2: the pty master dies: the client gets SIGHUP + EIO
wait_grep "$W/restore.done" verify= 120 && wait_active xorgate-agent-rs 60 && grep -q 'verify=PASS' "$W/restore.done" \
  && ok "P2: a pty hang-up: the restore unit finished, PROD started, verify PASS" || { bad "P2: PROD not started"; tail -5 "$W/restore.log"; }
bash "$W/bin/p8-modem-sampler.sh" stop >/dev/null
bash "$W/bin/p8-verify.sh" post --window "$W" > "$W/post.out" 2>&1 && ok "P2: verify post PASS" || { bad "P2: verify post"; grep FAIL "$W/post.out"; }

echo "== P3: Ctrl-C (SIGINT) on the manual-restore client mid-restore never stops the restore unit"
open_window 20261114T000009Z w1 --cap-min 60
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w1 -- /root/fake-install-js.sh > "$W/ri.out" 2>&1 || bad "P3 install"
{ for _ in $(seq 400); do grep -q 'restore start' "$W/restore.log" 2>/dev/null && break; sleep 0.05; done
  date +%s.%N > /root/p3-ctrlc.epoch; [ -e "$W/restore.done" ] && echo late > /root/p3-ctrlc.late; printf '\003'; sleep 20; } | script -qefc "bash $W/bin/p8-restore.sh --window $W" /dev/null > "$W/p3-client.out" 2>&1 & sp=$!
wait_grep "$W/restore.done" verify= 120 && wait_active xorgate-agent-rs 60 && grep -q 'verify=PASS' "$W/restore.done" \
  && ok "P3: after a real ^C on the client's pty the restore unit finished, PROD started, verify PASS" || { bad "P3: PROD not started"; tail -5 "$W/restore.log"; }
[ -s /root/p3-ctrlc.epoch ] && [ ! -e /root/p3-ctrlc.late ] && ok "P3: the ^C landed mid-restore (after 'restore start', before restore.done)" || bad "P3: the ^C was not mid-restore"
rm -f /root/p3-ctrlc.late
grep -q 'self=p8-restore-20261114T000009Z.service' "$W/restore.log" && [ "$(cat "$W/restore.rc")" = 0 ] && ok "P3: the unit's own exit code recorded (restore.rc 0)" || bad "P3: restore.rc"
wait "$sp" 2>/dev/null; rm -f /root/p3-ctrlc.epoch
bash "$W/bin/p8-modem-sampler.sh" stop >/dev/null

must2_check() { # <case> <label>: the provisioner stopped BEFORE any move back, the restore exact
  local c=$1 vl bl pk
  vl=$(grep -n 'window unit xorgate-provision-rs.service inactive (verified)' "$W/restore.log" | head -1 | cut -d: -f1)
  bl=$(grep -n 'PROD back:' "$W/restore.log" | head -1 | cut -d: -f1)
  [ -n "$vl" ] && [ -n "$bl" ] && [ "$vl" -lt "$bl" ] && ok "$c: the provisioner was verified inactive (log line $vl) before the first move back (line $bl)" || bad "$c: order vl=$vl bl=$bl"
  python3 -I -c 'import sys; sys.exit(0 if float(open(sys.argv[1]).read()) < float(open(sys.argv[2]).read()) else 1)' /var/tmp/p8prov.last "$W/units-stopped.epoch" \
    && ok "$c: its last write came before the stop was verified" || bad "$c: the provisioner wrote after the stop"
  systemctl is-active --quiet xorgate-provision-rs && bad "$c: the provisioner unit is still active" || ok "$c: the provisioner unit is inactive"
  pk=$(awk -F '\t' '$1 == "/etc/xorgate/device.key.pem" {print $6}' "$W/pre.manifest"); sleep 2
  [ "$(sha256sum /etc/xorgate/device.key.pem | cut -c1-64)" = "$pk" ] && [ ! -e /opt/xorgate-rs.new ] && grep -q '^DA15 pre: PASS' "$W/verify-pre.out" \
    && [ ! -e /var/log/xorgate-provision-rs.log ] && [ -e "$W/test-$2/var/log/xorgate-provision-rs.log" ] \
    && ok "$c: the restored state is exact (PROD key, no /opt/xorgate-rs.new, verify pre PASS); the provisioner log is in test-$2" || { bad "$c: restored state"; cat "$W/verify-pre.out"; }
  rm -f /var/tmp/p8prov.last
}
echo "== M2D: WP-R1 MUST-2, the dead-man fires while a SIBLING xorgate-provision-rs keeps writing"
open_window 20261114T000010Z w2 --deadline-in-s 25
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w2 -- /root/fake-install-handoff.sh > "$W/ri.out" 2>&1 & rip=$!
wait_grep /var/log/xorgate-provision-rs.log tick 30 && systemctl is-active --quiet xorgate-provision-rs && ok "M2D: the sibling provisioner runs and writes" || bad "M2D: no provisioner"
wait_grep "$W/restore.log" 'restore done' 150 && wait_active xorgate-agent-rs 60 && ok "M2D: the timer's restore ran and PROD is active" || bad "M2D: no timer restore"
wait "$rip" 2>/dev/null
must2_check M2D w2
grep -q 'verify post: PASS' "$W/restore.log" && ok "M2D: the --auto verify post PASSed (no xorgate-provision-rs leftover)" || { bad "M2D: verify post"; grep FAIL "$W/verify-post.out"; }
echo "== M2M: WP-R1 MUST-2, a MANUAL restore while the provisioner IGNORES SIGTERM (SIGKILL escalation)"
open_window 20261114T000011Z w2 --cap-min 75
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w2 -- /root/fake-install-handoff.sh ignore > "$W/ri.out" 2>&1 & rip=$!
wait_grep /var/log/xorgate-provision-rs.log tick 30 || bad "M2M: no provisioner"
P8_UNIT_STOP_WAIT_S=8 bash "$W/bin/p8-restore.sh" --window "$W" > "$W/restore.out" 2>&1 && ok "M2M: the manual restore exit 0" || { bad "M2M: restore"; tail -8 "$W/restore.out"; }
wait "$rip" 2>/dev/null
grep -q 'xorgate-provision-rs.service still running after 8s: SIGKILL' "$W/restore.log" && ok "M2M: the SIGTERM-ignoring provisioner was SIGKILLed" || bad "M2M: no SIGKILL escalation"
must2_check M2M w2
bash "$W/bin/p8-modem-sampler.sh" stop >/dev/null
bash "$W/bin/p8-verify.sh" post --window "$W" > "$W/post.out" 2>&1 && ok "M2M: verify post PASS" || { bad "M2M: verify post"; grep FAIL "$W/post.out"; }

echo "== M2L: a LATE hand-off (after the dpkg wait) is caught; the held lock refuses a provisioner started mid-restore"
open_window 20261114T000012Z w2 --cap-min 75
bash "$W/bin/p8-backup.sh" run-install --window "$W" --label w2 -- /root/fake-install-late.sh > "$W/ri.out" 2>&1 & rip=$!
wait_grep "$W/w2-install.log" locked 20 || bad "M2L: the client did not take the dpkg lock"
systemctl is-active --quiet xorgate-provision-rs && bad "M2L: the provisioner started too early" || ok "M2L: no provisioner yet when the restore starts"
rm -f /var/tmp/p8prov.refused /var/tmp/p8m2l.held /var/tmp/p8prov.first
touch /var/tmp/p8m2l.go; rs=$(date +%s)
{ for _ in $(seq 600); do
    if [ -e "$W/units-stopped.epoch" ]; then
      [ -e /run/xorgate-provision-rs.lock ] && ! flock -n /run/xorgate-provision-rs.lock true && echo held > /var/tmp/p8m2l.held
      systemd-run --unit=xorgate-provision-rs-mid --collect --quiet --no-block /bin/bash /root/fake-provision.sh term
      break
    fi; sleep 0.05
  done; } & pp=$!
bash "$W/bin/p8-restore.sh" --window "$W" > "$W/restore.out" 2>&1 && ok "M2L: the manual restore exit 0" || { bad "M2L: restore"; tail -8 "$W/restore.out"; }
wait "$rip" 2>/dev/null; wait "$pp" 2>/dev/null
w=$(sed -n 's/.*F3: dpkg locks free (waited \([0-9]*\)s).*/\1/p' "$W/restore.log")
python3 -I -c 'import sys; sys.exit(0 if float(open(sys.argv[1]).read()) >= float(sys.argv[2]) + 5 else 1)' /var/tmp/p8prov.first "$rs" && [ "${w:-0}" -ge 10 ] \
  && ok "M2L: the provisioner was handed off ~7 s INTO the restore's dpkg wait (waited ${w}s), after the first unit listing" || bad "M2L: hand-off timing (waited '${w}')"
must2_check M2L w2
[ -e /var/tmp/p8m2l.held ] && ok "M2L: the provisioner lock was held by the restore right after the units were stopped" || bad "M2L: the lock was not held after the stop"
grep -q refused /var/tmp/p8prov.refused 2>/dev/null && ! systemctl is-active --quiet xorgate-provision-rs-mid && ok "M2L: a provisioner started mid-restore REFUSED on the held lock (and never ran)" || bad "M2L: the mid-restore provisioner was not refused"
[ ! -e /run/xorgate-provision-rs.lock ] && ok "M2L: the lock is released and removed at the restore's exit" || bad "M2L: lock left"
rm -f /var/tmp/p8prov.refused /var/tmp/p8m2l.held /var/tmp/p8m2l.go /var/tmp/p8prov.first
bash "$W/bin/p8-modem-sampler.sh" stop >/dev/null
bash "$W/bin/p8-verify.sh" post --window "$W" > "$W/post.out" 2>&1 && ok "M2L: verify post PASS" || { bad "M2L: verify post"; grep FAIL "$W/post.out"; }

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
