#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015,SC2251  # test assertions: `[ … ]; expect $?` and `a && pass || failc`
# Phase 5b (WP4) bench overlay + CF76, run as ROOT in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/p5b-overlay.sh
# p5b-overlay.sh apply|revert|status with the emulator / FakeModem replaced by
# stand-ins (XORGATE_TEST_P5B_*): refuses outside the shared profile, writes
# ONLY /run, /tmp and the DEV manifest copy, restarts ONLY xorgate-agent-rs,
# reverts byte-for-byte; CF76: the OBD knobs never in the env file, the DEV
# manifest never declares peripherals.obd outside the overlay.
set -uo pipefail
# shellcheck source=scripts/bench/tests/_stubs.sh
source "$(dirname "$0")/_stubs.sh"
id pi >/dev/null 2>&1 || useradd -m pi
command -v runuser >/dev/null || { echo "runuser missing (util-linux)"; exit 2; }
HW=/var/lib/xorgate-rs/etc/hardware.json
DROP=/run/systemd/system/xorgate-agent-rs.service.d/97-p5b.conf
export P5B_NO_COPY=1 XORGATE_TEST_P5B_SKIP_VENV=1
# Stand-ins: the "emulator" writes its pty path and sleeps; the "FakeModem"
# makes its link and sleeps (both as pi, like the real ones).
cat > /usr/local/bin/p5b-fake-emu <<'S'
#!/bin/sh
echo /dev/pts/77 > "$1.tmp" && mv "$1.tmp" "$1"; while :; do sleep 1; done
S
cat > /usr/local/bin/p5b-fake-modem <<'S'
#!/bin/sh
ln -sfn /dev/null "$1"; while :; do sleep 1; done
S
chmod 0755 /usr/local/bin/p5b-fake-emu /usr/local/bin/p5b-fake-modem
export XORGATE_TEST_P5B_EMU_MATCH=p5b-fake-emu XORGATE_TEST_P5B_MODEM_MATCH=p5b-fake-modem
export XORGATE_TEST_P5B_EMULATOR_CMD="/usr/local/bin/p5b-fake-emu /run/xorgate-rs-p5b/obd-pty"
export XORGATE_TEST_P5B_MODEM_CMD="/usr/local/bin/p5b-fake-modem /run/xorgate-rs-p5b/sms-pty"
fake_install() {
  mkdir -p /opt/xorgate-rs/python/obd
  : > /opt/xorgate-rs/python/obd_read.py; : > /opt/xorgate-rs/python/obd_scan.py
  rm -rf /tmp/xorgate-p5b; mkdir -p /tmp/xorgate-p5b
}

fixture_bench; reset_units; fake_install
bash "$B/dev-manifests.sh" >"$LOGS/dm.log" 2>&1 || { failc "dev-manifests.sh"; tail -5 "$LOGS/dm.log"; }
bash "$B/profile.sh" shared >"$LOGS/s1.log" 2>&1; expect $? "profile.sh shared"

echo "== refusals"
echo inactive > "$STATE/xorgate-agent"
bash "$B/p5b-overlay.sh" apply >"$LOGS/r1.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q "REFUSING: xorgate-agent is inactive" "$LOGS/r1.log" && [ ! -e "$DROP" ]
expect $? "apply refuses while the JS prod agent is not active"
echo active > "$STATE/xorgate-agent"
mkdir -p /run/systemd/system/xorgate-agent-rs.service.d; : > /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf
bash "$B/p5b-overlay.sh" apply >"$LOGS/r2.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q "REFUSING: an exclusive window is open" "$LOGS/r2.log"; expect $? "apply refuses inside an exclusive window"
rm -f /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf
mv /opt/xorgate-rs/python/obd_read.py /opt/xorgate-rs/python/obd_read.py.x
bash "$B/p5b-overlay.sh" apply >"$LOGS/r3.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q "obd_read.py missing" "$LOGS/r3.log"; expect $? "apply refuses without the installed helper"
mv /opt/xorgate-rs/python/obd_read.py.x /opt/xorgate-rs/python/obd_read.py

echo "== apply: only /run, /tmp and the DEV manifest; restart only the Rust unit"
cp -p "$HW" "$LOGS/hw.orig"
# (the manifest's own dir changes its mtime when the file is replaced)
snap_outside() { snapshot | grep -v -e '^/run' -e '  /run/' -e "^$HW " -e " $HW\$" -e '^/var/lib/xorgate-rs/etc d '; }
snap_outside > "$LOGS/before.snap"
: > "$STATE/systemctl.log"
bash "$B/p5b-overlay.sh" apply >"$LOGS/a1.log" 2>&1; expect $? "apply rc=0"
snap_outside > "$LOGS/after.snap"
diff -q "$LOGS/before.snap" "$LOGS/after.snap" >/dev/null; expect $? "nothing outside /run, /tmp and $HW changed"
! grep -E 'systemctl (restart|start|stop)' "$STATE/systemctl.log" | grep -qv 'restart xorgate-agent-rs$' && grep -q 'restart xorgate-agent-rs$' "$STATE/systemctl.log"
expect $? "the only (re)start is 'systemctl restart xorgate-agent-rs'"
[ "$(js_state)" = active ] && [ "$(cat "$STATE/sim7600-lte")" = active ]; expect $? "JS agent + sim7600-lte untouched"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["peripherals"]["obd"]=={"adapter":"elm327","transport":"bt-spp"}; assert d["imu"] is None' "$HW"
expect $? "$HW gains peripherals.obd (elm327/bt-spp), imu still null"
grep -qx 'Environment=XORGATE_OBD_SERIAL=/dev/pts/77' "$DROP" && grep -qx 'Environment=XORGATE_PYTHON_DIR=/run/xorgate-rs-obd-python' "$DROP" && grep -qx 'Environment=XORGATE_SMS_PORT=/run/xorgate-rs-p5b/sms-pty' "$DROP"
expect $? "97-p5b.conf: python dir, emulator pty, FakeModem link"
[ "$(readlink /run/xorgate-rs-obd-python/obd_read.py)" = /opt/xorgate-rs/python/obd_read.py ] && [ ! -e /run/xorgate-rs-obd-python/bno085_read.py ]
expect $? "obd-only python dir (symlinks to the install, no bno085_read.py)"
p=$(cat /run/xorgate-rs-p5b/emulator.pid); [ "$(stat -c %U /proc/"$p")" = pi ]; expect $? "the emulator runs as pi"
bash "$B/exclusive.sh" status >"$LOGS/a2.log" 2>&1
grep -q "inputs: SAFE" "$LOGS/a2.log"; expect $? "inputs_safe SAFE while the overlay is applied (marker present)"
bash "$B/p5b-overlay.sh" apply >"$LOGS/a3.log" 2>&1
grep -q "already applied" "$LOGS/a3.log"; expect $? "a second apply is a no-op"
bash "$B/p5b-overlay.sh" status >"$LOGS/st.log" 2>&1
grep -q "marker:   applied" "$LOGS/st.log" && grep -q "emulator: running" "$LOGS/st.log"; expect $? "status reports the overlay"

echo "== revert: byte-for-byte"
: > "$STATE/systemctl.log"
bash "$B/p5b-overlay.sh" revert >"$LOGS/v1.log" 2>&1; rc=$?; expect $rc "revert rc=0"; [ "$rc" = 0 ] || sed 's/^/    v1: /' "$LOGS/v1.log" | tail -15
cmp -s "$HW" "$LOGS/hw.orig"; expect $? "$HW restored byte-for-byte"
[ ! -e "$DROP" ] && [ ! -e /run/xorgate-rs-p5b ] && [ ! -e /run/xorgate-rs-obd-python ] && [ ! -e /tmp/xorgate-p5b ]
expect $? "drop-in, /run dirs and /tmp files gone"
! kill -0 "$p" 2>/dev/null; expect $? "the emulator is stopped"
! grep -E 'systemctl (restart|start|stop)' "$STATE/systemctl.log" | grep -qv 'restart xorgate-agent-rs$'; expect $? "revert restarts only xorgate-agent-rs"
bash "$B/exclusive.sh" status >"$LOGS/v2.log" 2>&1
grep -q "inputs: SAFE" "$LOGS/v2.log"; expect $? "inputs_safe SAFE after revert"

echo "== review-wp4 R4-11: a failure partway through apply rolls back"
fake_install; cp -p "$HW" "$LOGS/hw.orig2"
XORGATE_TEST_P5B_SKIP_VENV=0 bash "$B/p5b-overlay.sh" apply >"$LOGS/f1.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q "FAILED (rc" "$LOGS/f1.log" && grep -q "reverting" "$LOGS/f1.log"; expect $? "apply failing at the venv step exits 1 and says it reverts"; [ "$fail" = 0 ] || sed 's/^/    f1: /' "$LOGS/f1.log" | tail -15
cmp -s "$HW" "$LOGS/hw.orig2"; expect $? "the DEV manifest is restored byte-for-byte (no stray peripherals.obd)"
[ ! -e /run/xorgate-rs-p5b ] && [ ! -e /run/xorgate-rs-obd-python ] && [ ! -e "$DROP" ]; expect $? "no /run leftovers, no drop-in"

echo "== review-wp4 R4-12: stop_pid kills only the overlay's own pi process"
fake_install
bash "$B/p5b-overlay.sh" apply >"$LOGS/i1.log" 2>&1; expect $? "apply rc=0"
emu=$(cat /run/xorgate-rs-p5b/emulator.pid)
sleep 300 & victim=$!
echo "$victim" > /run/xorgate-rs-p5b/emulator.pid
bash "$B/p5b-overlay.sh" revert >"$LOGS/i2.log" 2>&1
kill -0 "$victim" 2>/dev/null && grep -q "REFUSING to stop pid $victim" "$LOGS/i2.log"; expect $? "a recycled pid (a root sleep) is NOT killed"
kill "$victim" "$emu" 2>/dev/null; true

echo "== CF76: outside the overlay"
python3 - "$HW" <<'PY'
import json, sys
d = json.load(open(sys.argv[1])); d["peripherals"] = {"obd": {"adapter": "elm327", "transport": "bt-spp"}}
json.dump(d, open(sys.argv[1], "w"))
PY
bash "$B/exclusive.sh" status >"$LOGS/c1.log" 2>&1
grep -q "UNSAFE: $HW declares peripherals.obd outside the p5b overlay" "$LOGS/c1.log" && grep -q "inputs: NOT SAFE" "$LOGS/c1.log"
expect $? "NOT SAFE: the DEV manifest keeps peripherals.obd without the overlay"
bash "$B/profile.sh" shared >"$LOGS/c2.log" 2>&1
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert "peripherals" not in d' "$HW"; expect $? "profile.sh shared strips peripherals.obd from the DEV copy"
python3 - /etc/xorgate/hardware.json <<'PY'
import json, sys
d = json.load(open(sys.argv[1])); d["peripherals"] = {"obd": {"adapter": "elm327", "transport": "bt-spp"}}
json.dump(d, open(sys.argv[1], "w"))
PY
bash "$B/profile.sh" shared >"$LOGS/c3.log" 2>&1
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert "peripherals" not in d' "$HW"; expect $? "a prod manifest WITH peripherals.obd still gives a DEV copy without it"
mkdir -p /run/systemd/system/xorgate-agent-rs.service.d; printf '[Service]\nEnvironment=XORGATE_OBD_SERIAL=/dev/pts/9\n' > "$DROP"
bash "$B/exclusive.sh" status >"$LOGS/c4.log" 2>&1
grep -q "UNSAFE: $DROP present without" "$LOGS/c4.log"; expect $? "NOT SAFE: a stray 97-p5b.conf without the marker"
rm -f "$DROP"
for bad in XORGATE_OBD_SERIAL=/dev/rfcomm0 XORGATE_OBD_ADAPTER=00:1D:A5:68:98:8B XORGATE_OBD_POLL_HZ=2; do
  cp "$RS_ENV" "$LOGS/env.bak"; echo "$bad" >> "$RS_ENV"
  bash "$B/profile.sh" shared >"$LOGS/g.log" 2>&1; rc=$?
  [ "$rc" = 1 ] && grep -q "REFUSING: $RS_ENV sets ${bad%%=*}" "$LOGS/g.log"; expect $? "profile.sh shared refuses $RS_ENV carrying ${bad%%=*}"
  cp "$LOGS/env.bak" "$RS_ENV"
done
for f in /proc/[0-9]*/cmdline; do grep -q p5b-fake "$f" 2>/dev/null && kill "$(basename "$(dirname "$f")")" 2>/dev/null; done; true
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
