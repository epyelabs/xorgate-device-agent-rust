#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015,SC2012  # test assertions: `[ … ]; expect $?` and `a && pass || failc`
# Phase 7a WP3 (phase-7a-inprocess-gstreamer.md §Design 10): the bench scripts
# learn the capture host `xorgate-capture`. Run as ROOT in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/p7a-capture-host.sh
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
grep -q "xorgate-capture" "$LOGS/dry.log"; expect $? "the dry run announces the xorgate-capture shim"

echo "== (2) the shared profile: both pins + the blocking host shim; the verifier passes"
bash "$B/profile.sh" shared >"$LOGS/shared.log" 2>&1; rc=$?
expect $((rc != 0)) "profile.sh shared rc=0"
grep -qx 'Environment=XORGATE_CAPTURE_BACKEND=gst-launch' "$D"; expect $? "95-inputs.conf pins XORGATE_CAPTURE_BACKEND=gst-launch"
grep -qx "Environment=XORGATE_CAPTURE_HOST_BIN=$SHIMS/xorgate-capture" "$D"; expect $? "95-inputs.conf pins XORGATE_CAPTURE_HOST_BIN to the shared shim"
out=$("$SHIMS/xorgate-capture" __capture-host -e libcamerasrc camera-name=x 2>&1 >/dev/null); rc=$?
[ "$rc" = 1 ] && grep -q "xorgate-capture: disabled in the shared profile" <<<"$out" && [ "$(stat -c '%U %a' "$SHIMS/xorgate-capture")" = "root 755" ]
expect $? "shim xorgate-capture: exit 1, 'disabled in the shared profile', root 0755"
bash "$B/exclusive.sh" status >"$LOGS/st.log" 2>&1
grep -q "inputs: SAFE" "$LOGS/st.log"; expect $? "inputs_safe passes with both pins" || tail -20 "$LOGS/st.log"

echo "== (3) the shared profile blocks capture in BOTH backends"
# What the agent's env would be under this drop-in: PATH and the two pins.
env_of() { sed -n "s/^Environment=$1=//p" "$D" | tail -n1; }
P=$(env_of PATH); BE=$(env_of XORGATE_CAPTURE_BACKEND); HB=$(env_of XORGATE_CAPTURE_HOST_BIN)
G=$(env -i PATH="$P" bash -c 'command -v gst-launch-1.0')
[ "$BE" = gst-launch ] && [ "$G" = "$SHIMS/gst-launch-1.0" ] && ! env -i PATH="$P" gst-launch-1.0 -e videotestsrc ! fakesink 2>/dev/null
expect $? "backend gst-launch: gst-launch-1.0 resolves to the blocking shim (exit 1)"
[ "$HB" = "$SHIMS/xorgate-capture" ] && ! env -i PATH="$P" "$HB" __capture-host -e videotestsrc ! fakesink 2>/dev/null
expect $? "backend host (a stray XORGATE_CAPTURE_BACKEND=host): the host program is the blocking shim (exit 1)"

echo "== (4) the verifier fails on every missing/extra piece"
for cut in XORGATE_CAPTURE_BACKEND XORGATE_CAPTURE_HOST_BIN; do
  cp "$D" "$LOGS/d.bak"; sed -i "/^Environment=$cut=/d" "$D"
  bash "$B/exclusive.sh" status >"$LOGS/v-$cut.log" 2>&1
  grep -q "missing: $cut=" "$LOGS/v-$cut.log" && grep -q "inputs: NOT SAFE" "$LOGS/v-$cut.log"; expect $? "inputs_safe fails without the $cut pin"
  cp "$LOGS/d.bak" "$D"
done
cp "$D" "$LOGS/d.bak"; echo "Environment=XORGATE_CAPTURE_BACKEND=host" >> "$D"
bash "$B/exclusive.sh" status >"$LOGS/v-extra.log" 2>&1
grep -q "UNSAFE: .*XORGATE_CAPTURE_\* exactly twice" "$LOGS/v-extra.log" && grep -q "inputs: NOT SAFE" "$LOGS/v-extra.log"; expect $? "inputs_safe fails on a later XORGATE_CAPTURE_BACKEND=host line"
cp "$LOGS/d.bak" "$D"
printf '#!/bin/sh\nexit 0\n' > "$SHIMS/xorgate-capture"
bash "$B/exclusive.sh" status >"$LOGS/v-shim.log" 2>&1
grep -q "UNSAFE: shim .*xorgate-capture exits 0" "$LOGS/v-shim.log"; expect $? "inputs_safe fails on a host shim that exits 0"
rm -f "$SHIMS/xorgate-capture"
bash "$B/exclusive.sh" status >"$LOGS/v-noshim.log" 2>&1
grep -q "missing: shim .*xorgate-capture" "$LOGS/v-noshim.log"; expect $? "inputs_safe fails without the host shim"
bash "$B/profile.sh" shared >"$LOGS/repair.log" 2>&1
bash "$B/profile.sh" shared >"$LOGS/repair2.log" 2>&1
grep -q "== done: 0 change(s)" "$LOGS/repair2.log" && [ -x "$SHIMS/xorgate-capture" ]; expect $? "profile.sh shared repairs the shim and is idempotent"
for k in XORGATE_CAPTURE_BACKEND=host XORGATE_CAPTURE_HOST_BIN=/x XORGATE_SIDECAR_TIMING=exact; do
  cp "$RS_ENV" "$LOGS/env.bak"; echo "$k" >> "$RS_ENV"
  bash "$B/profile.sh" shared >"$LOGS/env-${k%%=*}.log" 2>&1; rc=$?
  [ "$rc" = 1 ] && grep -q "REFUSING: .*${k%%=*}" "$LOGS/env-${k%%=*}.log"; expect $? "profile.sh shared refuses $RS_ENV carrying ${k%%=*} (an EnvironmentFile beats the pins)"
  bash "$B/exclusive.sh" status >"$LOGS/envst-${k%%=*}.log" 2>&1
  grep -q "inputs: NOT SAFE" "$LOGS/envst-${k%%=*}.log"; expect $? "inputs_safe fails with ${k%%=*} in $RS_ENV"
  cp "$LOGS/env.bak" "$RS_ENV"
done

echo "== (5) the exclusive drop-in: the host re-execs the packaged binary (no shim)"
echo inactive > "$STATE/xorgate-agent"
cp "$DEV_BACKUP/kvs.json" "$RS_ETC/kvs-exclusive.json"; echo '{"v":2,"imu":"bno085"}' > "$RS_ETC/hardware-exclusive.json"
bash "$B/profile.sh" exclusive >"$LOGS/excl.log" 2>&1; rc=$?
expect $((rc != 0)) "profile.sh exclusive rc=0 (JS stopped)"
grep -qx "UnsetEnvironment=XORGATE_CAPTURE_HOST_BIN" "$X" && ! grep -qE "^(Environment|UnsetEnvironment)=.*XORGATE_CAPTURE_BACKEND" "$X"; expect $? "99-exclusive.conf unsets HOST_BIN only (the backend stays the shared pin)"
grep -qx 'Environment=XORGATE_CAPTURE_BACKEND=gst-launch' "$D"; expect $? "the shared pin is still in 95-inputs.conf during the window"
bash "$B/profile.sh" shared >"$LOGS/back.log" 2>&1
[ ! -e "$X" ]; expect $? "back to shared: the exclusive drop-in is gone"
reset_units

echo "== (6) leftovers + shm audit name the host"
FK=$(mktemp -d); cp /bin/sleep "$FK/sleep"
# A Rust host: argv[0] xorgate-capture, child of the Rust MainPID; it reacts to SIGINT.
bash -c "python3 -c 'import os,signal,sys; signal.signal(signal.SIGINT, signal.SIG_DFL); os.execv(sys.argv[1], [\"xorgate-capture\", \"1000\"])' $FK/sleep & wait" & RM=$!
# A JS-owned process with the same name (child of the JS MainPID): never signalled.
bash -c "exec -a xorgate-capture $FK/sleep 1000 & wait" & JM=$!
sleep 0.5
kid() { local f st pp; for f in /proc/[0-9]*/stat; do st=$(cat "$f" 2>/dev/null) || continue; st=${st##*) }; read -r _ pp _ <<<"$st"; [ "$pp" = "$1" ] && { f=${f#/proc/}; echo "${f%/stat}"; return; }; done; }
RH=$(kid "$RM"); JH=$(kid "$JM")
[ -n "$RH" ] && [ -n "$JH" ] && [ "$(tr '\0' '\n' < "/proc/$RH/cmdline" | head -n1)" = xorgate-capture ]; expect $? "fake hosts running (argv[0] xorgate-capture)"
echo "$RM" > "$STATE/xorgate-agent-rs.mainpid"; echo "$JM" > "$STATE/xorgate-agent.mainpid"
: > "/dev/shm/shmpipe.$RH.    0"
bash "$B/shm-audit.sh" >"$LOGS/shm.log" 2>&1
grep -q "RUST .*pid=$RH .*prog=xorgate-capture" "$LOGS/shm.log"; expect $? "shm-audit: a host segment is RUST, prog=xorgate-capture" || cat "$LOGS/shm.log"
bash "$B/shm-audit.sh" --json >"$LOGS/shm.json" 2>/dev/null
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); r=[x for x in d["shm"] if x["pid"]==int(sys.argv[2])]; assert r and r[0]["owner"]=="RUST" and r[0]["program"]=="xorgate-capture", d' "$LOGS/shm.json" "$RH"
expect $? "shm-audit --json carries program=xorgate-capture"
echo inactive > "$STATE/xorgate-agent"
bash "$B/exclusive.sh" end >"$LOGS/end.log" 2>&1; rc=$?
expect $((rc != 0)) "exclusive.sh end rc=0"
sleep 0.3
dead() { local s; s=$(cat "/proc/$1/stat" 2>/dev/null) || return 0; s=${s##*) }; [ "${s%% *}" = Z ]; }
dead "$RH"; expect $? "end SIGINTs the Rust xorgate-capture leftover"
grep -q "SIGINT -> Rust xorgate-capture pid $RH" "$LOGS/end.log"; expect $? "end names it xorgate-capture"
! dead "$JH" && ! grep -q "pid $JH" "$LOGS/end.log"; expect $? "a JS-owned xorgate-capture is never signalled"
kill "$JH" "$JM" "$RM" 2>/dev/null; rm -f /dev/shm/shmpipe.*
[ "$(js_state)" = active ]; expect $? "end leaves JS active"

echo "== (7) soak sampler carries the helpers that know xorgate-capture"
bash "$B/soak.sh" --minutes 1 --interval 1 >"$LOGS/soak.log" 2>&1
grep -q "xorgate-capture" /run/xorgate-rs-soak/sampler.sh && grep -q "is_media_name" /run/xorgate-rs-soak/sampler.sh; expect $? "the sampler's is_media_name knows xorgate-capture"
bash "$B/soak.sh" stop >/dev/null 2>&1

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)  (logs in $LOGS)"
exit $fail
