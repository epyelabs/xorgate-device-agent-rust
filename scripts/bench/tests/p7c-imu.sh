#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015,SC2012  # test assertions: `[ … ]; expect $?` and `a && pass || failc`
# Phase 7c WP3 (phase-7c-rust-imu.md §Design 3, gate row C8): the bench
# scripts learn the Rust IMU reader `xorgate-imu`. The shared profile pins
# XORGATE_IMU_BACKEND=python AND points XORGATE_IMU_READER_BIN at a blocking
# shim; the verifier refuses either missing; the env file may carry neither;
# `exclusive.sh status` counts the SHTP clients. Run as ROOT in a throwaway
# container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/p7c-imu.sh
# Dry-run / stubbed only (tests/_stubs.sh): nothing touches a bench.
set -uo pipefail
# shellcheck source=scripts/bench/tests/_stubs.sh
source "$(dirname "$0")/_stubs.sh"
fixture_bench; reset_units
echo "KVS_MANIFEST=/var/lib/xorgate-rs/etc/kvs.json" >> "$RS_ENV"
sed -i '/^KVS_RUNTIME_BIN=/d' "$RS_ENV"
install -d -o pi -g pi -m 0700 "$RS_ETC"
echo '{"v":2,"imu":null}' > "$RS_ETC/hardware.json"
echo '{"deviceId":"x","channels":[]}' > "$RS_ETC/kvs.json"
grep -q "^HARDWARE_MANIFEST=" "$RS_ENV" || echo "HARDWARE_MANIFEST=$RS_ETC/hardware.json" >> "$RS_ENV"
D=/etc/systemd/system/xorgate-agent-rs.service.d/95-inputs.conf
X=/run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf
SHIMS=/var/lib/xorgate-rs/shared-shims

echo "== (1) dry run of the shared profile writes nothing"
snapshot > "$LOGS/s1-before"
bash "$B/profile.sh" shared --dry-run >"$LOGS/dry.log" 2>&1; echo "  rc=$?"
snapshot > "$LOGS/s1-after"
diff -q "$LOGS/s1-before" "$LOGS/s1-after" >/dev/null; expect $? "profile.sh shared --dry-run wrote nothing"
grep -q "xorgate-imu" "$LOGS/dry.log"; expect $? "the dry run announces the xorgate-imu shim"

echo "== (2) the shared profile: both IMU pins + the blocking reader shim; the verifier passes"
bash "$B/profile.sh" shared >"$LOGS/shared.log" 2>&1; rc=$?
expect $((rc != 0)) "profile.sh shared rc=0"
grep -qx 'Environment=XORGATE_IMU_BACKEND=python' "$D"; expect $? "95-inputs.conf pins XORGATE_IMU_BACKEND=python"
grep -qx "Environment=XORGATE_IMU_READER_BIN=$SHIMS/xorgate-imu" "$D"; expect $? "95-inputs.conf pins XORGATE_IMU_READER_BIN to the shared shim"
[ "$(grep -c '^Environment=XORGATE_IMU_' "$D")" = 2 ]; expect $? "exactly two XORGATE_IMU_* lines (the pins)"
out=$("$SHIMS/xorgate-imu" __imu-reader --address 0x4a 2>"$LOGS/shim.err"); rc=$?
[ "$rc" = 1 ] && [ -z "$out" ] && grep -q "xorgate-imu: disabled in the shared profile" "$LOGS/shim.err" && [ "$(stat -c '%U %a' "$SHIMS/xorgate-imu")" = "root 755" ]
expect $? "shim xorgate-imu: exit 1, no stdout (no sample, no I2C), 'disabled in the shared profile' on stderr, root 0755"
bash "$B/exclusive.sh" status >"$LOGS/st.log" 2>&1
grep -q "inputs: SAFE" "$LOGS/st.log"; expect $? "inputs_safe passes with both IMU pins" || tail -20 "$LOGS/st.log"

echo "== (3) the shared profile opens no I2C bus in EITHER backend"
# What the agent's env would be under this drop-in.
env_of() { sed -n "s/^Environment=$1=//p" "$D" | tail -n1; }
P=$(env_of PATH); BE=$(env_of XORGATE_IMU_BACKEND); RB=$(env_of XORGATE_IMU_READER_BIN); PY=$(env_of XORGATE_PYTHON_DIR)
[ "$BE" = python ] && [ ! -e "$PY/bno085_read.py" ] && ! env -i PATH="$P" bash -c 'command -v i2cdetect' >/dev/null
expect $? "backend python: no helper in XORGATE_PYTHON_DIR=$PY (spawn fails → parked), no i2cdetect on PATH"
[ "$RB" = "$SHIMS/xorgate-imu" ] && ! env -i PATH="$P" "$RB" __imu-reader --address 0x4a >/dev/null 2>&1
expect $? "backend rust (a stray XORGATE_IMU_BACKEND=rust): the reader program is the blocking shim (exit 1)"

echo "== (4) the verifier fails on every missing/extra piece"
for cut in XORGATE_IMU_BACKEND XORGATE_IMU_READER_BIN; do
  cp "$D" "$LOGS/d.bak"; sed -i "/^Environment=$cut=/d" "$D"
  bash "$B/exclusive.sh" status >"$LOGS/v-$cut.log" 2>&1
  grep -q "missing: $cut=" "$LOGS/v-$cut.log" && grep -q "inputs: NOT SAFE" "$LOGS/v-$cut.log"; expect $? "inputs_safe fails without the $cut pin"
  cp "$LOGS/d.bak" "$D"
done
cp "$D" "$LOGS/d.bak"; echo "Environment=XORGATE_IMU_BACKEND=rust" >> "$D"
bash "$B/exclusive.sh" status >"$LOGS/v-extra.log" 2>&1
grep -q "UNSAFE: .*XORGATE_IMU_\* exactly twice" "$LOGS/v-extra.log" && grep -q "inputs: NOT SAFE" "$LOGS/v-extra.log"; expect $? "inputs_safe fails on a later XORGATE_IMU_BACKEND=rust line"
cp "$LOGS/d.bak" "$D"
cp "$D" "$LOGS/d.bak"; sed -i 's#^Environment=XORGATE_IMU_BACKEND=python$#Environment=XORGATE_IMU_BACKEND=rust#' "$D"
bash "$B/exclusive.sh" status >"$LOGS/v-flip.log" 2>&1
grep -q "missing: XORGATE_IMU_BACKEND=python" "$LOGS/v-flip.log" && grep -q "inputs: NOT SAFE" "$LOGS/v-flip.log"; expect $? "inputs_safe fails when the pin says rust"
cp "$LOGS/d.bak" "$D"
printf '#!/bin/sh\nexit 0\n' > "$SHIMS/xorgate-imu"
bash "$B/exclusive.sh" status >"$LOGS/v-shim.log" 2>&1
grep -q "UNSAFE: shim .*xorgate-imu exits 0" "$LOGS/v-shim.log"; expect $? "inputs_safe fails on a reader shim that exits 0"
rm -f "$SHIMS/xorgate-imu"
bash "$B/exclusive.sh" status >"$LOGS/v-noshim.log" 2>&1
grep -q "missing: shim .*xorgate-imu" "$LOGS/v-noshim.log"; expect $? "inputs_safe fails without the reader shim"
bash "$B/profile.sh" shared >"$LOGS/repair.log" 2>&1
bash "$B/profile.sh" shared >"$LOGS/repair2.log" 2>&1
grep -q "== done: 0 change(s)" "$LOGS/repair2.log" && [ -x "$SHIMS/xorgate-imu" ]; expect $? "profile.sh shared repairs the shim and is idempotent"
for k in XORGATE_IMU_BACKEND=rust XORGATE_IMU_READER_BIN=/x XORGATE_IMU_FUTURE=1; do
  cp "$RS_ENV" "$LOGS/env.bak"; echo "$k" >> "$RS_ENV"
  bash "$B/profile.sh" shared >"$LOGS/env-${k%%=*}.log" 2>&1; rc=$?
  [ "$rc" = 1 ] && grep -q "REFUSING: .*${k%%=*}" "$LOGS/env-${k%%=*}.log"; expect $? "profile.sh shared refuses $RS_ENV carrying ${k%%=*} (an EnvironmentFile beats the pins)"
  bash "$B/exclusive.sh" status >"$LOGS/envst-${k%%=*}.log" 2>&1
  grep -q "inputs: NOT SAFE" "$LOGS/envst-${k%%=*}.log" && grep -q "XORGATE_IMU_\*" "$LOGS/envst-${k%%=*}.log"; expect $? "inputs_safe fails with ${k%%=*} in $RS_ENV"
  cp "$LOGS/env.bak" "$RS_ENV"
done

echo "== (5) the exclusive drop-in: the reader re-execs the packaged binary (no shim); the backend stays the pin"
echo inactive > "$STATE/xorgate-agent"
cp "$DEV_BACKUP/kvs.json" "$RS_ETC/kvs-exclusive.json"; echo '{"v":2,"imu":"bno085"}' > "$RS_ETC/hardware-exclusive.json"
bash "$B/profile.sh" exclusive >"$LOGS/excl.log" 2>&1; rc=$?
expect $((rc != 0)) "profile.sh exclusive rc=0 (JS stopped)"
grep -qx "UnsetEnvironment=XORGATE_IMU_READER_BIN" "$X" && ! grep -qE "^(Environment|UnsetEnvironment)=.*XORGATE_IMU_BACKEND" "$X"; expect $? "99-exclusive.conf unsets READER_BIN only (the backend stays the shared pin)"
grep -qx 'Environment=XORGATE_IMU_BACKEND=python' "$D"; expect $? "the shared pin is still in 95-inputs.conf during the window"
bash "$B/profile.sh" shared >"$LOGS/back.log" 2>&1
[ ! -e "$X" ]; expect $? "back to shared: the exclusive drop-in is gone"
reset_units

echo "== (6) exclusive.sh status counts the SHTP clients (python helpers + xorgate-imu)"
FK=$(mktemp -d); cp /bin/sleep "$FK/xorgate-imu"
printf 'import time\ntime.sleep(1000)\n' > "$FK/bno085_read.py"
bash "$B/exclusive.sh" status >"$LOGS/imu0.log" 2>&1
grep -q "IMU readers (SHTP clients): 0" "$LOGS/imu0.log" && ! grep -q "SHTP clients on the BNO085" "$LOGS/imu0.log"; expect $? "no reader: 0, no UNSAFE line"
"$FK/xorgate-imu" 1000 & RI=$!
sleep 0.3
[ "$(cat "/proc/$RI/comm")" = xorgate-imu ]; expect $? "fake Rust reader running (comm xorgate-imu)"
bash "$B/exclusive.sh" status >"$LOGS/imu1.log" 2>&1
grep -q "IMU readers (SHTP clients): 1 -> other:xorgate-imu:$RI" "$LOGS/imu1.log" && ! grep -q "SHTP clients on the BNO085" "$LOGS/imu1.log"; expect $? "one Rust reader: 1, named xorgate-imu, no UNSAFE line" || cat "$LOGS/imu1.log"
python3 "$FK/bno085_read.py" --address 0x4a & PI=$!
sleep 0.3
bash "$B/exclusive.sh" status >"$LOGS/imu2.log" 2>&1
grep -q "IMU readers (SHTP clients): 2" "$LOGS/imu2.log" && grep -q "other:python:$PI" "$LOGS/imu2.log" && grep -q "UNSAFE: 2 SHTP clients on the BNO085" "$LOGS/imu2.log"
expect $? "a python helper beside it: 2 clients and an UNSAFE line" || cat "$LOGS/imu2.log"
kill "$RI" "$PI" 2>/dev/null; wait "$RI" "$PI" 2>/dev/null

echo "== (7) the tarball still requires the Python helper (the default backend)"
grep -q 'python/bno085_read.py missing (the IMU helper)' "$REPO/scripts/pack.sh" && grep -qE '^for required in .*python/bno085_read\.py' "$REPO/scripts/pack.sh"
expect $? "pack.sh refuses a tree without python/bno085_read.py and requires it in the tarball"

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)  (logs in $LOGS)"
exit $fail
