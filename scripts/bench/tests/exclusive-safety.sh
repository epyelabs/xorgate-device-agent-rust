#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015,SC2012  # test assertions: `[ … ]; expect $?` and `a && pass || failc`
# Safety test for scripts/bench/{exclusive,profile,dev-manifests}.sh (review-wp7
# fix 6 + Phase 3 WP6), run as ROOT in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/exclusive-safety.sh
# Stubs ssh/sudo/systemctl (tests/_stubs.sh) so the real script bodies run
# locally; asserts the PROD JS unit (xorgate-agent) is active after every
# failure path, the media-off shared profile, the camera-release wait, an
# idempotent end and side-effect-free dry runs.
set -uo pipefail
# shellcheck source=scripts/bench/tests/_stubs.sh
source "$(dirname "$0")/_stubs.sh"
check() { local want="$1" msg="$2"; local got; got=$(js_state)
  if [ "$got" = "$want" ]; then pass "$msg (xorgate-agent=$got)"; else failc "$msg (xorgate-agent=$got, want $want)"; fi; }
reset() { reset_units; }
fixture_bench

sed -i '/^KVS_/d' "$RS_ENV"   # cases 1-3 predate the Phase 3 env guard
echo "== case 1: end with JS stopped and the Rust restart failing (daemon-reload fails)"
reset; echo inactive > "$STATE/xorgate-agent"; touch "$STATE/FAIL_RELOAD"
bash "$B/exclusive.sh" end >"$LOGS/c1.log" 2>&1; echo "  exclusive.sh end rc=$?"
check active "end starts JS even though the later Rust restart fails"

echo "== case 2: end run twice (idempotent)"
reset; echo inactive > "$STATE/xorgate-agent"
bash "$B/exclusive.sh" end >"$LOGS/c2a.log" 2>&1; bash "$B/exclusive.sh" end >"$LOGS/c2b.log" 2>&1; echo "  rc=$?"
check active "double end leaves JS active"

echo "== case 3: begin aborts after JS was stopped (preflight passes, exclusive profile step fails)"
reset
echo "KVS_MANIFEST=/var/lib/xorgate-rs/etc/kvs.json" >> "$RS_ENV"
mkdir -p "$RS_ETC"
C15=/var/lib/xorgate-rs/evidence/c15-baseline.sha256   # = _lib.sh C15_BASELINE
(cd / && sha256sum etc/xorgate/agent.env etc/xorgate/kvs.json etc/xorgate/hardware.json) > "$C15"
echo '{"v":2,"imu":"bno085"}' > "$RS_ETC/hardware-exclusive.json"   # = HW_EXCLUSIVE
echo '{"v":2,"imu":null}' > "$RS_ETC/hardware.json"                 # = HW_SHARED
cp "$DEV_BACKUP/kvs.json" "$RS_ETC/kvs-exclusive.json"              # = KVS_EXCLUSIVE
touch "$STATE/FAIL_RELOAD"
bash "$B/exclusive.sh" begin >"$LOGS/c3.log" 2>&1; echo "  exclusive.sh begin rc=$?"
check active "aborted begin restores JS via the trap"
if grep -q "begin aborted" "$LOGS/c3.log"; then pass "trap fired after JS was stopped"; else failc "trap did not fire (begin refused before stopping JS?) — log:"; tail -8 "$LOGS/c3.log"; fi
bash "$B/exclusive.sh" end >/dev/null 2>&1

# ---------------------------------------------------------------- Phase 3 (WP6)
echo "== case (e1): every dry run on an UNCONFIGURED bench writes nothing"
fixture_bench; fixture_phase2_copies; reset
snapshot > "$LOGS/snap-e1-before"
for cmd in "dev-manifests.sh --dry-run" "profile.sh shared --dry-run" "profile.sh exclusive --dry-run" \
           "exclusive.sh begin --dry-run" "exclusive.sh end --dry-run" "exclusive.sh tape-off --dry-run" \
           "gate-seam.sh on --dry-run" "gate-seam.sh release --dry-run" "gate-seam.sh off --dry-run" \
           "soak.sh --dry-run" "soak.sh start --minutes 2 --interval 5 --dry-run" "soak.sh stop --dry-run" \
           "filler.sh seed --mib 30 --dry-run" "filler.sh clear --dry-run" "shm-audit.sh" "shm-audit.sh --json"; do
  # shellcheck disable=SC2086
  bash "$B/"$cmd >"$LOGS/e1-${cmd//[ \/]/_}.log" 2>&1; echo "  $cmd rc=$?"
done
snapshot > "$LOGS/snap-e1-after"
if diff "$LOGS/snap-e1-before" "$LOGS/snap-e1-after" >"$LOGS/e1.diff"; then pass "dry runs (unconfigured) wrote nothing"; else failc "dry runs wrote:"; head -20 "$LOGS/e1.diff"; fi
grep -q "would write /var/lib/xorgate-rs/etc/kvs.json\|would: install -o pi -g pi -m 0600 .*kvs.json" "$LOGS/e1-dev-manifests.sh_--dry-run.log" \
  && pass "dev-manifests dry run announces the kvs copies" || { failc "dev-manifests dry run did not announce kvs.json"; tail -20 "$LOGS/e1-dev-manifests.sh_--dry-run.log"; }
check active "dry runs leave JS active"

echo "== dev-manifests (real): kvs copies, pipelines cleanup"
reset
bash "$B/dev-manifests.sh" >"$LOGS/dm1.log" 2>&1; rc=$?
expect $((rc != 0)) "dev-manifests.sh rc=0"
python3 - "$RS_ETC/kvs.json" "$RS_ETC/kvs-exclusive.json" "$DEV_BACKUP/kvs.json" <<'PY' >"$LOGS/dm-kvs.txt" 2>&1
import json, sys
s, x, src = (json.load(open(p)) for p in sys.argv[1:4])
assert s["channels"] == [], s
assert list(s.keys()) == list(src.keys()), (list(s.keys()), list(src.keys()))
for k in ("deviceId", "region", "credentialEndpoint", "roleAlias", "uploadBucket"):
    assert s[k] == src[k], k
assert open(sys.argv[2]).read() == open(sys.argv[3]).read()
print("ok")
PY
expect $? "kvs.json = DEV with channels [] (key order + credential fields kept); kvs-exclusive.json verbatim"
[ ! -e "$RS_ETC/pipelines/cam0.gst" ] && [ ! -e "$RS_ETC/pipelines/cam0.camera" ] && [ ! -e "$RS_ETC/pipelines/cam1.gst" ]
expect $? "copied JS renders removed from $RS_ETC/pipelines (exact copies + old JS-socket copy)"
[ -f "$RS_ETC/pipelines/cam0.gst.direct" ] && [ -d "$RS_ETC/pipelines" ]
expect $? "the Rust render + the pipelines dir are kept"
[ "$(stat -c '%U %a' "$RS_ETC/pipelines")" = "pi 700" ]; expect $? "pipelines dir pi 0700"
grep -qx "KVS_MANIFEST=$RS_ETC/kvs.json" "$RS_ENV"; expect $? "env file KVS_MANIFEST -> the channels-[] copy"
! grep -qE '^KVS_(RUNTIME_BIN|RUNTIME_LIB_DIR|CACERT_PATH)=' "$RS_ENV" && grep -q '^DEVICE_ID=' "$RS_ENV"; expect $? "env file: the JS-install KVS_RUNTIME_BIN/LIB_DIR/CACERT_PATH keys dropped, the rest kept"
bash "$B/dev-manifests.sh" >"$LOGS/dm2.log" 2>&1
grep -q "== done: 0 change(s)" "$LOGS/dm2.log"; expect $? "dev-manifests.sh second run: 0 changes"

echo "== case (a): profile.sh shared refuses exclusive/media-on env-file values"
for bad in "KVS_RUNTIME_BIN=/opt/xorgate-rs/media/xorgate-kvs-webrtc" "KVS_RUNTIME_BIN=/var/lib/xorgate-rs/no-kvs" \
           "PATH=/usr/bin:/bin" "KVS_MANIFEST=$RS_ETC/kvs-exclusive.json" "KVS_MANIFEST=/etc/xorgate/kvs.json"; do
  reset; cp "$RS_ENV" "$LOGS/env.bak"
  key=${bad%%=*}
  if grep -q "^$key=" "$RS_ENV"; then sed -i "s|^$key=.*|$bad|" "$RS_ENV"; else echo "$bad" >> "$RS_ENV"; fi
  rm -f /etc/systemd/system/xorgate-agent-rs.service.d/95-inputs.conf
  bash "$B/profile.sh" shared >"$LOGS/a.log" 2>&1; rc=$?
  if [ "$rc" = 1 ] && grep -q REFUSING "$LOGS/a.log" && [ ! -e /etc/systemd/system/xorgate-agent-rs.service.d/95-inputs.conf ]; then
    pass "profile.sh shared refuses $bad (rc=1, nothing written)"
  else failc "profile.sh shared with $bad: rc=$rc"; tail -5 "$LOGS/a.log"; fi
  cp "$LOGS/env.bak" "$RS_ENV"
done
reset; sed -i '/^KVS_MANIFEST=/d' "$RS_ENV"
bash "$B/profile.sh" shared >"$LOGS/a-unset.log" 2>&1; rc=$?
expect $((rc != 1)) "profile.sh shared refuses an UNSET KVS_MANIFEST (agent default = PROD manifest)"
cp "$LOGS/env.bak" "$RS_ENV"
touch /var/lib/xorgate-rs/no-kvs
bash "$B/profile.sh" shared >"$LOGS/a-nokvs.log" 2>&1; rc=$?
expect $((rc != 1)) "profile.sh shared refuses when /var/lib/xorgate-rs/no-kvs exists"
rm -f /var/lib/xorgate-rs/no-kvs
bash "$B/profile.sh" shared >"$LOGS/a-ok.log" 2>&1; rc=$?
expect $((rc != 0)) "profile.sh shared accepts KVS_MANIFEST=$RS_ETC/kvs.json (rc=$rc)"

echo "== case (b): the shared drop-in + shims; inputs_safe passes"
D=/etc/systemd/system/xorgate-agent-rs.service.d/95-inputs.conf
grep -qx 'Environment=PATH=/var/lib/xorgate-rs/shared-shims:/usr/local/bin:/usr/bin:/bin' "$D"; expect $? "95-inputs.conf: PATH with shared-shims first"
grep -qx 'Environment=KVS_RUNTIME_BIN=/var/lib/xorgate-rs/no-kvs' "$D"; expect $? "95-inputs.conf: KVS_RUNTIME_BIN=/var/lib/xorgate-rs/no-kvs"
[ "$(grep -c '^Environment=PATH=' "$D")" = 1 ]; expect $? "95-inputs.conf: exactly one PATH line"
for s in rpicam-hello gst-launch-1.0; do
  out=$(/var/lib/xorgate-rs/shared-shims/$s --list-cameras 2>&1 >/dev/null); rc=$?
  [ "$rc" = 1 ] && grep -q "disabled in the shared profile" <<<"$out" && [ "$(stat -c '%U %a' /var/lib/xorgate-rs/shared-shims/$s)" = "root 755" ]
  expect $? "shim $s: exit 1, 'disabled in the shared profile' on stderr, root 0755"
done
bash "$B/exclusive.sh" status >"$LOGS/b.log" 2>&1
grep -q "inputs: SAFE" "$LOGS/b.log"; expect $? "inputs_safe passes in the shared profile"
cp "$RS_ETC/kvs.json" "$LOGS/kvs.bak"; cp "$DEV_BACKUP/kvs.json" "$RS_ETC/kvs.json"
bash "$B/exclusive.sh" status >"$LOGS/b2.log" 2>&1
grep -q "missing: channels: \[\]" "$LOGS/b2.log" && grep -q "inputs: NOT SAFE" "$LOGS/b2.log"; expect $? "inputs_safe fails when kvs.json has channels"
cp "$LOGS/kvs.bak" "$RS_ETC/kvs.json"
printf '#!/bin/sh\nexit 0\n' > /var/lib/xorgate-rs/shared-shims/gst-launch-1.0
bash "$B/exclusive.sh" status >"$LOGS/b3.log" 2>&1
grep -q "UNSAFE: shim .*gst-launch-1.0 exits 0" "$LOGS/b3.log"; expect $? "inputs_safe fails on a shim that exits 0"
bash "$B/profile.sh" shared >"$LOGS/b4.log" 2>&1
bash "$B/profile.sh" shared >"$LOGS/b5.log" 2>&1
grep -q "== done: 0 change(s)" "$LOGS/b5.log"; expect $? "profile.sh shared repaired the shim and is idempotent (0 changes on rerun)"

echo "== case (c1): begin captures the JS media + waits for the camera release, then opens the window"
reset
C15=/var/lib/xorgate-rs/evidence/c15-baseline.sha256
(cd / && sha256sum /etc/xorgate/agent.env /etc/xorgate/kvs.json /etc/xorgate/hardware.json) > "$C15"
FK=$(mktemp -d); cp /bin/sleep "$FK/gst-launch-1.0"; cp /bin/sleep "$FK/xorgate-kvs-webrtc"
"$FK/gst-launch-1.0" 1000 & G=$!
env SECRET_TOKEN=hunter2 AWS_IOT_CORE_CERT=/x "$FK/xorgate-kvs-webrtc" 1000 & K=$!
echo "$G $K" > "$STATE/js-children"
export XORGATE_TEST_JS_MEDIA_PIDS_CMD="cat $STATE/js-children"
bash "$B/exclusive.sh" begin >"$LOGS/c1b.log" 2>&1; rc=$?
WIN=$(ls -1d /var/lib/xorgate-rs/evidence/window-* | tail -n1)
expect $((rc != 0)) "begin rc=0 once the JS children are gone"
grep -q "camera released" "$LOGS/c1b.log"; expect $? "begin waited for the camera release"
[ "$(js_state)" = inactive ] && [ -f /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf ]; expect $? "window open (JS stopped, exclusive drop-in present)"
[ "$(head -n1 "$WIN/js-gst-$G.cmdline")" = "$FK/gst-launch-1.0" ] && [ "$(sed -n 2p "$WIN/js-gst-$G.cmdline")" = 1000 ]; expect $? "js-gst-<pid>.cmdline saved (NUL -> newline)"
grep -qx SECRET_TOKEN "$WIN/js-kvs-$K.envkeys" && ! grep -q hunter2 "$WIN/js-kvs-$K.envkeys" && [ -f "$WIN/js-kvs-$K.cmdline" ]; expect $? "js-kvs-<pid>.envkeys holds KEYS only (+ cmdline)"
[ "$(wc -l < "$WIN/pipelines.sha256")" = 3 ] && [ -s "$WIN/shm-before.txt" ] && [ -f "$WIN/js-media-released" ] && grep -q "streams: cam0=running" "$WIN/js-media-before.txt"
expect $? "pipelines.sha256 (3 files), shm-before.txt, js-media-before.txt, js-media-released saved"
grep -q "KVS_MANIFEST=$RS_ETC/kvs-exclusive.json" /run/xorgate-agent-rs-exclusive.env; expect $? "exclusive env file: KVS_MANIFEST -> kvs-exclusive.json"
grep -qx "Environment=KVS_RUNTIME_BIN=/opt/xorgate-rs/media/xorgate-kvs-webrtc" /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf \
  && grep -qx "Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf; expect $? "99-exclusive.conf: full PATH (no shims) + packaged KVS_RUNTIME_BIN"
grep -q "gate-seam.sh" "$LOGS/c1b.log" && grep -q "soak.sh" "$LOGS/c1b.log" && grep -q "filler.sh" "$LOGS/c1b.log"; expect $? "begin hints name gate-seam/soak/filler"
unset XORGATE_TEST_JS_MEDIA_PIDS_CMD
wait "$G" "$K" 2>/dev/null

echo "== case (d): end tears the window down, is idempotent and never signals a JS pid"
# Window helpers left on: a gate seam, a soak, a filler session, Rust leftovers.
bash "$B/gate-seam.sh" on >"$LOGS/d-seam.log" 2>&1
bash "$B/soak.sh" --minutes 1 --interval 1 >"$LOGS/d-soak.log" 2>&1
bash "$B/filler.sh" seed --mib 20 >"$LOGS/d-fill.log" 2>&1
[ -f /run/systemd/system/xorgate-agent-rs.service.d/97-gate-seam.conf ] && [ "$(cat "$STATE/xorgate-rs-soak")" = active ] \
  && [ -n "$(ls /var/lib/xorgate-rs/recordings/video/cam0/*/.xorgate-filler 2>/dev/null)" ]; expect $? "seam on, soak started, filler seeded before end"
mkdir -p /var/lib/xorgate-rs/recordings/video/cam1/019a0000-0000-7000-8000-000000000001
echo real > /var/lib/xorgate-rs/recordings/video/cam1/019a0000-0000-7000-8000-000000000001/00000.mp4
# Rust leftovers: one stops on SIGINT, one ignores it (needs SIGKILL). A
# non-interactive bash starts `&` jobs with SIGINT ignored, so R1 re-arms the
# default disposition before exec'ing the fake gst.
cp /bin/sleep "$FK/gst-launch-1.0"
python3 -c 'import os, signal, sys; signal.signal(signal.SIGINT, signal.SIG_DFL); os.execv(sys.argv[1], sys.argv[1:])' "$FK/gst-launch-1.0" 1000 & R1=$!
bash -c 'trap "" INT; while :; do sleep 0.2; done' & R2=$!
# A "JS" media process (child of the JS MainPID) that a buggy scan lists as Rust: must survive.
bash -c "\"$FK/gst-launch-1.0\" 1000 & wait" & JM=$!
sleep 0.5; JC=""
for f in /proc/[0-9]*/stat; do st=$(cat "$f" 2>/dev/null) || continue; st=${st##*) }; read -r _ pp _ <<<"$st"; [ "$pp" = "$JM" ] && { JC=${f#/proc/}; JC=${JC%/stat}; }; done
[ -n "$JC" ] || failc "could not find the fake JS child of $JM"
echo "$JM" > "$STATE/xorgate-agent.mainpid"
python3 - "$JM" <<'PY'
import json, sys
p = "/var/lib/xorgate/state.json"; d = json.load(open(p)); d["pid"] = int(sys.argv[1]); json.dump(d, open(p, "w"))
PY
export XORGATE_TEST_RS_MEDIA_PIDS_CMD="echo $R1 $R2 $JC | tr ' ' '\n'"
bash "$B/exclusive.sh" end >"$LOGS/d1.log" 2>&1; rc1=$?
bash "$B/exclusive.sh" end >"$LOGS/d2.log" 2>&1; rc2=$?
unset XORGATE_TEST_RS_MEDIA_PIDS_CMD
expect $((rc1 != 0 || rc2 != 0)) "end rc=0 twice (rc1=$rc1 rc2=$rc2)" || true
[ "$rc1" = 0 ] || { echo "  --- d1.log tail"; tail -15 "$LOGS/d1.log"; }
[ "$rc2" = 0 ] || { echo "  --- d2.log tail"; tail -15 "$LOGS/d2.log"; }
check active "end leaves JS active"
grep -q "== done: 0 change(s)" "$LOGS/d2.log"; expect $? "second end: 0 changes"
[ ! -e /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf ] && [ ! -e /run/systemd/system/xorgate-agent-rs.service.d/98-tape.conf ] \
  && [ ! -e /run/systemd/system/xorgate-agent-rs.service.d/97-gate-seam.conf ] && [ ! -e /run/xorgate-rs-lte ] && [ ! -e /run/xorgate-agent-rs-exclusive.env ]
expect $? "runtime drop-ins, exclusive env, gate seam gone"
[ "$(cat "$STATE/xorgate-rs-soak")" = inactive ]; expect $? "soak unit stopped"
[ -z "$(ls /var/lib/xorgate-rs/recordings/video/cam0/*/.xorgate-filler 2>/dev/null)" ] && [ -f /var/lib/xorgate-rs/recordings/video/cam1/019a0000-0000-7000-8000-000000000001/00000.mp4 ]
expect $? "filler cleared, a real (unmarked) session kept"
dead() { local s; s=$(cat "/proc/$1/stat" 2>/dev/null) || return 0; s=${s##*) }; [ "${s%% *}" = Z ]; }
dead "$R1" && dead "$R2"; expect $? "Rust leftovers gone (SIGINT one, SIGKILL the INT-ignoring one)"
grep -q "SIGKILL -> Rust pid $R2" "$LOGS/d1.log" && ! grep -q "SIGKILL -> Rust pid $R1 " "$LOGS/d1.log"; expect $? "SIGINT stopped one leftover; the INT-ignoring one got SIGKILL after 5 s"
! dead "$JC" && grep -q "pid $JC belongs to xorgate-agent — NOT signalled" "$LOGS/d1.log"; expect $? "a JS-unit pid listed as Rust is NOT signalled"
grep -q "JS media after .*\[all running\] streams: cam0=running" "$LOGS/d1.log"; expect $? "end reports the JS streams running (state.json of the current JS pid)"
[ -s "$WIN/shm-after.txt" ] && grep -q "pipelines: /etc/xorgate/pipelines/\* sha256 UNCHANGED" "$LOGS/d1.log" && grep -q "C15: .* UNCHANGED" "$LOGS/d1.log"
expect $? "shm-after.txt written; C15 + pipelines.sha256 UNCHANGED"
grep -q "== 1b\." "$LOGS/d1.log" && [ "$(grep -n '== 1b\.' "$LOGS/d1.log" | cut -d: -f1)" -lt "$(grep -n '== 2\. JS agent' "$LOGS/d1.log" | cut -d: -f1)" ]
expect $? "leftover sweep ran right after the Rust stop"
echo '{"pid": 0, "latest": {"media": {"kvs": {"value": false}, "streams": [{"streamKey": "cam0", "state": "stopped"}]}}}' > /var/lib/xorgate/state.json
rm -f "$STATE/xorgate-agent.mainpid"
echo "  JS media after 0s: pid=0 (fresh) kvs=False [x] streams: cam0=stopped" > "$WIN/js-media-before.txt"
bash "$B/exclusive.sh" end >"$LOGS/d3.log" 2>&1
grep -q "JS media after 0s: .*\[= pre-window baseline\] streams: cam0=stopped" "$LOGS/d3.log"; expect $? "end accepts JS streams equal to the pre-window baseline (no 90 s wait)"
kill -KILL "$JC" "$JM" 2>/dev/null
echo '{"pid": 0, "latest": {"metrics": {"system": {}}, "media": {"kvs": {"value": true}, "streams": [{"streamKey": "cam0", "state": "running"}]}}}' > /var/lib/xorgate/state.json

echo "== case (c2): begin aborts (JS restored) when the JS media processes linger"
reset
export XORGATE_TEST_JS_MEDIA_PIDS_CMD="echo 4242" XORGATE_TEST_JS_RELEASE_SECS=3
bash "$B/exclusive.sh" begin >"$LOGS/c2.log" 2>&1; rc=$?
unset XORGATE_TEST_JS_MEDIA_PIDS_CMD XORGATE_TEST_JS_RELEASE_SECS
expect $((rc == 0)) "begin rc!=0 when the camera is never released (rc=$rc)"
check active "lingering JS media: the trap restores JS"
grep -q "camera not released" "$LOGS/c2.log" && grep -q "begin aborted" "$LOGS/c2.log"; expect $? "abort names the camera release + the trap fired"
[ ! -e /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf ]; expect $? "no exclusive drop-in left behind"
grep -q "systemctl stop xorgate-agent-rs" "$STATE/systemctl.log"; expect $? "the trap stopped Rust before restoring JS"

echo "== case (e2): dry runs on a CONFIGURED bench (seam on, filler seeded, window open) write nothing"
reset
bash "$B/gate-seam.sh" on >/dev/null 2>&1; bash "$B/filler.sh" seed --mib 20 >/dev/null 2>&1
echo active > "$STATE/xorgate-rs-soak"
snapshot > "$LOGS/snap-e2-before"
for cmd in "exclusive.sh end --dry-run" "gate-seam.sh off --dry-run" "gate-seam.sh release --dry-run" "soak.sh stop --dry-run" \
           "filler.sh clear --dry-run" "filler.sh seed --dry-run" "profile.sh shared --dry-run" "dev-manifests.sh --dry-run"; do
  # shellcheck disable=SC2086
  bash "$B/"$cmd >"$LOGS/e2-${cmd//[ \/]/_}.log" 2>&1; echo "  $cmd rc=$?"
done
snapshot > "$LOGS/snap-e2-after"
if diff "$LOGS/snap-e2-before" "$LOGS/snap-e2-after" >"$LOGS/e2.diff"; then pass "dry runs (configured) wrote nothing"; else failc "dry runs wrote:"; head -20 "$LOGS/e2.diff"; fi
grep -q "would: rm -rf -- /var/lib/xorgate-rs/recordings/video/cam0/" "$LOGS/e2-filler.sh_clear_--dry-run.log"; expect $? "filler clear dry run names the marked session"
grep -q "would: systemctl stop xorgate-rs-gate-seam\|would: rm -f /run/systemd/system/xorgate-agent-rs.service.d/97-gate-seam.conf" "$LOGS/e2-gate-seam.sh_off_--dry-run.log"; expect $? "gate-seam off dry run names the drop-in"
[ "$(cat "$STATE/xorgate-rs-soak")" = active ]; expect $? "soak stop dry run left the unit running"
bash "$B/exclusive.sh" end >/dev/null 2>&1

echo "== case (g): the exclusive-window DEAD-MAN (review-wp6 B1): begin arms, it fires without end, end cancels"
DM=xorgate-rs-window-deadman
reset
(cd / && sha256sum /etc/xorgate/agent.env /etc/xorgate/kvs.json /etc/xorgate/hardware.json) > "$C15"
bash "$B/exclusive.sh" begin --deadman-min 95 >"$LOGS/g1.log" 2>&1; rc=$?
expect $((rc != 0)) "begin rc=0 (rc=$rc)"
[ "$(cat "$STATE/$DM.timer" 2>/dev/null)" = active ] && grep -q -- "--unit=$DM --on-active=5700s" "$STATE/systemd-run.log" && [ -x /run/$DM.sh ]
expect $? "begin armed $DM.timer (--on-active = 95 min) with its restore script"
grep -n "== 2b. dead-man\|== 3. stop" "$LOGS/g1.log" | head -1 | grep -q "2b. dead-man"; expect $? "the dead-man is armed BEFORE the JS stop"
[ "$(js_state)" = inactive ] && [ -f /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf ]; expect $? "window open (JS stopped)"
# end never runs: the timer fires.
WING=$(ls -1d /var/lib/xorgate-rs/evidence/window-* | tail -n1)
eval "$(cat "$STATE/$DM.cmd")" >"$LOGS/g-fire.log" 2>&1; rc=$?
expect $((rc != 0)) "the fired dead-man exits 0 (rc=$rc)"
check active "the fired dead-man restored the PROD JS agent"
[ "$(cat "$STATE/xorgate-agent-rs")" = inactive ] && [ ! -e /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf ] \
  && [ ! -e /run/systemd/system/xorgate-agent-rs.service.d/98-tape.conf ] && [ ! -e /run/xorgate-agent-rs-exclusive.env ] && [ -f "$WING/deadman-fired" ]
expect $? "fire: Rust stopped, runtime drop-ins + exclusive env gone, window records deadman-fired"
grep -q "systemctl daemon-reload" "$STATE/systemctl.log"; expect $? "fire: daemon-reload before the JS start"
eval "$(cat "$STATE/$DM.cmd")" >"$LOGS/g-fire2.log" 2>&1; rc=$?
expect $((rc != 0)) "a second fire is a no-op success (idempotent, rc=$rc)"; check active "after the second fire"
echo inactive > "$STATE/xorgate-agent"; touch "$STATE/FAIL_START_xorgate-agent"
timeout 120 bash -c "eval \"\$(cat '$STATE/$DM.cmd')\"" >"$LOGS/g-fire3.log" 2>&1; rc=$?
rm -f "$STATE/FAIL_START_xorgate-agent"
expect $((rc == 0)) "a fire whose JS start keeps failing exits non-zero and says so (rc=$rc)"
grep -q "FAILED TO START" "$LOGS/g-fire3.log"; expect $? "…naming the PROD agent DOWN"
bash "$B/exclusive.sh" end >"$LOGS/g-end0.log" 2>&1
# Normal path: begin re-arms, end cancels; a second end changes nothing.
reset
bash "$B/exclusive.sh" begin >"$LOGS/g2.log" 2>&1
[ "$(cat "$STATE/$DM.timer" 2>/dev/null)" = active ] && grep -q -- "--on-active=10800s" "$STATE/systemd-run.log"; expect $? "default dead-man = 180 min"
bash "$B/exclusive.sh" begin >"$LOGS/g2b.log" 2>&1 || true
grep -q "systemctl stop $DM.timer" "$STATE/systemctl.log" && [ "$(cat "$STATE/$DM.timer")" = active ]; expect $? "a second begin replaces (re-arms) the armed timer"
bash "$B/exclusive.sh" end >"$LOGS/g3.log" 2>&1
[ "$(cat "$STATE/$DM.timer")" = inactive ] && [ ! -e /run/$DM.sh ] && grep -q "$DM cancelled" "$LOGS/g3.log"; expect $? "end cancelled the dead-man (timer stopped, script removed)"
check active "end after the cancel"
bash "$B/exclusive.sh" end >"$LOGS/g4.log" 2>&1
grep -q "= $DM not armed" "$LOGS/g4.log" && grep -q "== done: 0 change(s)" "$LOGS/g4.log"; expect $? "a second end: dead-man not armed, 0 changes"
# end whose JS start fails leaves the dead-man ARMED (it retries later).
reset; bash "$B/exclusive.sh" begin >"$LOGS/g5.log" 2>&1; touch "$STATE/FAIL_START_xorgate-agent"
timeout 200 bash "$B/exclusive.sh" end >"$LOGS/g6.log" 2>&1
rm -f "$STATE/FAIL_START_xorgate-agent"
[ "$(cat "$STATE/$DM.timer")" = active ] && grep -q "dead-man left ARMED" "$LOGS/g6.log"; expect $? "end with JS failing to start leaves the dead-man armed"
bash "$B/exclusive.sh" end >/dev/null 2>&1
# An aborted begin (the trap) cancels it: nothing fires hours later.
reset; touch "$STATE/FAIL_RELOAD"
bash "$B/exclusive.sh" begin >"$LOGS/g7.log" 2>&1; rm -f "$STATE/FAIL_RELOAD"
grep -q "begin aborted" "$LOGS/g7.log" && [ "$(cat "$STATE/$DM.timer")" = inactive ] && [ ! -e /run/$DM.sh ]; expect $? "an aborted begin cancels the dead-man"
check active "after the aborted begin"
bash "$B/exclusive.sh" begin --deadman-min 5 >"$LOGS/g8.log" 2>&1; rc=$?
expect $((rc != 2)) "begin refuses --deadman-min < 10 (rc=$rc)"; check active "the refused begin touched nothing"
bash "$B/exclusive.sh" end >/dev/null 2>&1

# ---------------------------------------------------------------- Phase 5 (WP3)
# A CONVERSION window (phase-5-distribution.md §12, I6): install-rs.sh installs
# the SHIPPED unit (Conflicts=xorgate-agent.service) and DISABLES the JS unit.
# CF64: end / the begin trap / the dead-man re-enable JS; CF65: nothing on the
# bench (re)starts a Conflicts=-bearing Rust unit; the dead-man reverts a
# conversion still in place.
U=/etc/systemd/system/xorgate-agent-rs.service
REC=/var/lib/xorgate-rs-install/record
INST=/opt/xorgate-rs/install-rs.sh
I6="shipped (converted) unit installed — run scripts/bench/install.sh first; NOT starting xorgate-agent-rs (it would stop the PROD JS agent)"
converted_unit() { printf '[Unit]\nDescription=Xorgate device agent (Rust)\nConflicts=xorgate-agent.service\n[Service]\nType=notify\nExecStart=/opt/xorgate-rs/bin/xorgate-rs run\n' > "$U"; }
bench_unit() { printf '[Unit]\nDescription=Xorgate device agent (Rust)\n# BENCH (scripts/bench/install.sh): Conflicts=xorgate-agent.service stripped — both agents coexist on the bench\n[Service]\nType=notify\n' > "$U"; }
js_en() { cat "$STATE/xorgate-agent.enabled" 2>/dev/null || echo enabled; }
js_disabled_stopped() { echo disabled > "$STATE/xorgate-agent.enabled"; echo inactive > "$STATE/xorgate-agent"; }
# A stand-in for install-rs.sh --revert (keep mode, I1): logs its argv, then
# does what the real one does to the units (+ moves the record away).
fake_installer() { # <rc>
  mkdir -p /opt/xorgate-rs
  cat > "$INST" <<STUB
#!/bin/bash
echo "\$*" >> $LOGS/installer.argv
[ "$1" = 0 ] || exit $1
systemctl disable --now xorgate-agent-rs
systemctl enable xorgate-agent
systemctl start xorgate-agent
mv $REC $REC.reverted-test
STUB
  chmod 0755 "$INST"; rm -f "$LOGS/installer.argv"
}
record() { mkdir -p /var/lib/xorgate-rs-install; chmod 0700 /var/lib/xorgate-rs-install; printf 'state=%s\nstartedAt=2026-10-21T00:00:00Z\njsEnabled=enabled\njsActive=inactive\nrsPreexisting=yes\n' "$1" > "$REC"; }
no_record() { rm -rf /var/lib/xorgate-rs-install; }
line_of() { grep -n -- "$1" "$STATE/systemctl.log" | head -n1 | cut -d: -f1; }

echo "== case (p1): end on a CONVERTED bench (shipped unit, JS disabled) re-enables + starts JS and refuses the Rust restart"
reset; converted_unit; js_disabled_stopped; record converted; fake_installer 0; : > "$STATE/systemctl.log"
echo active > "$STATE/$DM.timer"; echo enabled > "$STATE/xorgate-agent-rs.enabled"   # the window's dead-man; install-rs.sh enabled Rust
bash "$B/exclusive.sh" end >"$LOGS/p1.log" 2>&1; rc=$?; echo "  exclusive.sh end rc=$rc"
check active "end with the shipped unit: JS active"
# review-wp3 M1: (a) Rust disabled, (b) dead-man left ARMED, (c) non-zero exit.
[ "$(cat "$STATE/xorgate-agent-rs.enabled")" = disabled ] && grep -q "^systemctl disable xorgate-agent-rs$" "$STATE/systemctl.log"
expect $? "M1(a): end DISABLED xorgate-agent-rs (no boot race with the PROD JS unit)"
[ "$(cat "$STATE/$DM.timer")" = active ] && ! grep -q "stop $DM.timer" "$STATE/systemctl.log" && grep -q "dead-man left ARMED" "$LOGS/p1.log"
expect $? "M1(b): end left the dead-man ARMED"
[ "$rc" = 1 ] && grep -q "end INCOMPLETE: a conversion is in place" "$LOGS/p1.log"; expect $? "M1(c): end exits 1 and says why (rc=$rc)"
s1=$(line_of "^systemctl start xorgate-agent$"); d1=$(line_of "^systemctl disable xorgate-agent-rs$")
[ -n "$s1" ] && [ -n "$d1" ] && [ "$s1" -lt "$d1" ]; expect $? "M1: JS restored (start) BEFORE the Rust disable (lines $s1 < $d1)"
[ "$(js_en)" = enabled ]; expect $? "end re-enabled the disabled JS unit (CF64)"
e=$(line_of "^systemctl enable xorgate-agent$"); st=$(line_of "^systemctl start xorgate-agent$")
[ -n "$e" ] && [ -n "$st" ] && [ "$e" -lt "$st" ]; expect $? "enable ran before start (systemctl log lines $e < $st)"
! grep -Eq "^systemctl (restart|start|enable) xorgate-agent-rs" "$STATE/systemctl.log" && [ "$(cat "$STATE/xorgate-agent-rs")" = inactive ]
expect $? "the shipped Rust unit was NOT (re)started or enabled; it stays stopped (CF65)"
grep -qF "$I6" "$LOGS/p1.log"; expect $? "end prints the I6 refusal text"
grep -q "xorgate-agent enabled (was disabled)" "$LOGS/p1.log"; expect $? "end names the enable as a change"
grep -q "state=converted — run: sudo /opt/xorgate-rs/install-rs.sh --revert" "$LOGS/p1.log" && [ ! -e "$LOGS/installer.argv" ]
expect $? "end WARNS about the unreverted conversion and does not run the installer itself"
no_record; bench_unit; bash "$B/exclusive.sh" end >/dev/null 2>&1

echo "== case (p1b): end with an UNREADABLE/unknown record (no state= line) on the bench flavour — treated as converted"
reset; bench_unit; js_disabled_stopped; mkdir -p /var/lib/xorgate-rs-install; printf 'startedAt=x\n' > "$REC"; echo active > "$STATE/$DM.timer"; : > "$STATE/systemctl.log"
bash "$B/exclusive.sh" end >"$LOGS/p1b.log" 2>&1; rc=$?
[ "$rc" = 1 ] && [ "$(js_state)" = active ] && [ "$(js_en)" = enabled ] && [ "$(cat "$STATE/xorgate-agent-rs.enabled")" = disabled ] \
  && [ "$(cat "$STATE/$DM.timer")" = active ] && grep -q "state=unknown" "$LOGS/p1b.log" && ! grep -q "^systemctl restart xorgate-agent-rs" "$STATE/systemctl.log"
expect $? "unknown record: JS enabled + active, Rust disabled + not restarted, dead-man armed, rc=1 (rc=$rc)"
bash "$B/exclusive.sh" status >"$LOGS/p1b-st.log" 2>&1; grep -q "conversion record: state=unknown" "$LOGS/p1b-st.log"; expect $? "status reports state=unknown"
echo "== case (p1c): end with the shipped unit and NO record — same safe outcome"
reset; no_record; converted_unit; echo active > "$STATE/$DM.timer"
bash "$B/exclusive.sh" end >"$LOGS/p1c.log" 2>&1; rc=$?
[ "$rc" = 1 ] && [ "$(cat "$STATE/xorgate-agent-rs.enabled")" = disabled ] && [ "$(cat "$STATE/$DM.timer")" = active ]; expect $? "shipped unit, no record: Rust disabled, dead-man armed, rc=1 (rc=$rc)"
check active "shipped unit, no record: JS active"
no_record; bench_unit; bash "$B/exclusive.sh" end >/dev/null 2>&1
[ "$(cat "$STATE/$DM.timer")" = inactive ]; expect $? "after the revert + bench flavour, end cancels the dead-man again"

echo "== case (p2): end with JS disabled + stopped (bench flavour) — enabled, started, Rust restarted; second end 0 changes"
reset; bench_unit; js_disabled_stopped; : > "$STATE/systemctl.log"
bash "$B/exclusive.sh" end >"$LOGS/p2a.log" 2>&1; rc=$?
expect $((rc != 0)) "end rc=0 (rc=$rc)"
check active "disabled JS: end started it"; [ "$(js_en)" = enabled ]; expect $? "disabled JS: end enabled it"
grep -q "^systemctl restart xorgate-agent-rs" "$STATE/systemctl.log"; expect $? "bench flavour: Rust restarted as before"
bash "$B/exclusive.sh" end >"$LOGS/p2b.log" 2>&1
grep -q "== done: 0 change(s)" "$LOGS/p2b.log" && grep -q "= xorgate-agent active + enabled" "$LOGS/p2b.log"; expect $? "second end: active + enabled, 0 changes"

echo "== case (p3): end with JS ACTIVE but disabled — enabled, not counted as a start"
reset; bench_unit; echo disabled > "$STATE/xorgate-agent.enabled"
bash "$B/exclusive.sh" end >"$LOGS/p3.log" 2>&1
[ "$(js_en)" = enabled ] && grep -q "xorgate-agent enabled (was disabled)" "$LOGS/p3.log" && ! grep -q -- "- xorgate-agent started" "$LOGS/p3.log"
expect $? "active+disabled JS: enabled only"; check active "active+disabled JS stays active"

echo "== case (p4): a failing enable still starts JS and says so"
reset; bench_unit; js_disabled_stopped; touch "$STATE/FAIL_ENABLE_xorgate-agent"
bash "$B/exclusive.sh" end >"$LOGS/p4.log" 2>&1; rm -f "$STATE/FAIL_ENABLE_xorgate-agent"
check active "enable failed: JS started anyway"
grep -q "systemctl enable xorgate-agent FAILED" "$LOGS/p4.log"; expect $? "the enable failure is reported"
bash "$B/exclusive.sh" end >/dev/null 2>&1

echo "== case (p5): begin refuses a shipped (converted) unit BEFORE stopping JS"
reset; converted_unit; : > "$STATE/systemctl.log"; rm -f "$STATE/systemd-run.log"
bash "$B/exclusive.sh" begin >"$LOGS/p5.log" 2>&1; rc=$?
expect $((rc == 0)) "begin rc!=0 with the shipped unit (rc=$rc)"
grep -qF "refusing: $I6" "$LOGS/p5.log"; expect $? "begin preflight prints the I6 text"
check active "refused begin: JS untouched"
! grep -q "stop xorgate-agent$" "$STATE/systemctl.log" && [ ! -e "$STATE/systemd-run.log" ]; expect $? "refused begin: no JS stop, no dead-man armed"

echo "== case (p5b): begin refuses an unreverted (or unknown) conversion record beside a bench-flavour unit (review-wp3 N1)"
for st in converted converting unknown; do
  reset; bench_unit; : > "$STATE/systemctl.log"
  if [ $st = unknown ]; then mkdir -p /var/lib/xorgate-rs-install; printf 'x=1\n' > "$REC"; else record $st; fi
  bash "$B/exclusive.sh" begin >"$LOGS/p5b-$st.log" 2>&1; rc=$?
  [ "$rc" != 0 ] && grep -q "refusing: conversion record .* state=$st" "$LOGS/p5b-$st.log" && ! grep -q "stop xorgate-agent$" "$STATE/systemctl.log"
  expect $? "begin refuses state=$st before the JS stop (rc=$rc)"
done
no_record

echo "== case (p6): tape-off and profile.sh shared refuse to restart the shipped unit"
reset; converted_unit; : > "$STATE/systemctl.log"
bash "$B/exclusive.sh" tape-off >"$LOGS/p6a.log" 2>&1; rc=$?
[ "$rc" != 0 ] && grep -qF "$I6" "$LOGS/p6a.log" && ! grep -q "restart xorgate-agent-rs" "$STATE/systemctl.log"; expect $? "tape-off refuses (rc=$rc, I6 text, no restart)"
bash "$B/profile.sh" shared >"$LOGS/p6b.log" 2>&1; rc=$?
[ "$rc" != 0 ] && grep -qF "$I6" "$LOGS/p6b.log" && ! grep -q "restart xorgate-agent-rs" "$STATE/systemctl.log"; expect $? "profile.sh shared refuses (rc=$rc, I6 text, no restart)"
check active "after the refusals"

echo "== case (p7): start.sh refuses the shipped unit FIRST (and a Conflicts= drop-in), never enabling Rust"
reset; converted_unit; : > "$STATE/systemctl.log"
bash "$B/start.sh" >"$LOGS/p7a.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -qF "REFUSING: $I6" "$LOGS/p7a.log" && ! grep -q "xorgate-agent-rs" "$STATE/systemctl.log"; expect $? "start.sh: rc=1, I6 text, no systemctl call on Rust (rc=$rc)"
bench_unit; mkdir -p /etc/systemd/system/xorgate-agent-rs.service.d
printf '[Unit]\nConflicts = foo.service xorgate-agent.service\n' > /etc/systemd/system/xorgate-agent-rs.service.d/50-p7.conf
bash "$B/start.sh" >"$LOGS/p7b.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -qF "$I6" "$LOGS/p7b.log"; expect $? "start.sh refuses a drop-in whose Conflicts= list names xorgate-agent.service"
printf '[Unit]\nConflicts=xorgate-agent-rs-helper.service\n' > /etc/systemd/system/xorgate-agent-rs.service.d/50-p7.conf
bash "$B/start.sh" >"$LOGS/p7c.log" 2>&1
! grep -qF "$I6" "$LOGS/p7c.log"; expect $? "a Conflicts= on another unit (xorgate-agent-rs-helper) is not the shipped unit"
rm -f /etc/systemd/system/xorgate-agent-rs.service.d/50-p7.conf
check active "start.sh cases leave JS active"

echo "== case (p8): the DEAD-MAN on a converted bench runs install-rs.sh --revert first, then enable + start + verify"
reset; bench_unit; no_record
(cd / && sha256sum /etc/xorgate/agent.env /etc/xorgate/kvs.json /etc/xorgate/hardware.json) > "$C15"
bash "$B/exclusive.sh" begin --deadman-min 95 >"$LOGS/p8-begin.log" 2>&1; rc=$?
expect $((rc != 0)) "begin (bench flavour) rc=0 (rc=$rc)"
WINP=$(ls -1d /var/lib/xorgate-rs/evidence/window-* | tail -n1)
fire() { eval "$(cat "$STATE/$DM.cmd")"; }
# Inside the window the gate runner converts: shipped unit, JS disabled, Rust active.
converted_unit; js_disabled_stopped; echo active > "$STATE/xorgate-agent-rs"; record converted; fake_installer 0; : > "$STATE/systemctl.log"
fire >"$LOGS/p8-fire.log" 2>&1; rc=$?
expect $((rc != 0)) "fired dead-man rc=0 (rc=$rc)"
[ "$(cat "$LOGS/installer.argv" 2>/dev/null)" = "--revert" ]; expect $? "it ran $INST with exactly --revert"
check active "dead-man after a conversion: JS active"; [ "$(js_en)" = enabled ]; expect $? "…and enabled"
[ "$(cat "$STATE/xorgate-agent-rs")" = inactive ] && [ "$(cat "$STATE/xorgate-agent-rs.enabled")" = disabled ]; expect $? "Rust stopped + disabled by the revert"
[ -f "$WINP/deadman-fired" ] && grep -q "conversion record state=converted" "$LOGS/p8-fire.log"; expect $? "deadman-fired recorded; log names the conversion"
! grep -Eq "^systemctl (restart|start|enable) xorgate-agent-rs" "$STATE/systemctl.log"; expect $? "the dead-man never starts the shipped Rust unit"

echo "== case (p9): dead-man, state=converting, the revert FAILS — JS is still enabled + started"
converted_unit; js_disabled_stopped; record converting; fake_installer 1; rm -f "$WINP/deadman-fired"
fire >"$LOGS/p9.log" 2>&1; rc=$?
expect $((rc != 0)) "fired dead-man rc=0 despite the failed revert (rc=$rc)"
[ "$(cat "$LOGS/installer.argv" 2>/dev/null)" = "--revert" ] && grep -q "install-rs.sh --revert exited 1" "$LOGS/p9.log"; expect $? "converting record: revert attempted, its failure named"
check active "failed revert: JS active"; [ "$(js_en)" = enabled ]; expect $? "failed revert: JS enabled by the dead-man itself (CF64)"

echo "== case (p10): dead-man, no record / reverted record / installer missing — never runs the installer, still enables JS"
for v in none reverted missing; do
  bench_unit; js_disabled_stopped; fake_installer 0
  case $v in none) no_record ;; reverted) record reverted ;; missing) record converted; rm -f "$INST" ;; esac
  fire >"$LOGS/p10-$v.log" 2>&1; rc=$?
  [ "$rc" = 0 ] && [ ! -e "$LOGS/installer.argv" ] && [ "$(js_state)" = active ] && [ "$(js_en)" = enabled ]
  expect $? "dead-man ($v): rc=0, installer not run, JS enabled + active (rc=$rc)"
done
grep -q "install-rs.sh is missing — cannot revert" "$LOGS/p10-missing.log"; expect $? "a converted record without the installer is named"
no_record; rm -rf /opt/xorgate-rs; bash "$B/exclusive.sh" end >/dev/null 2>&1

echo "== case (p11): an aborted begin with JS disabled re-enables it (the trap)"
reset; bench_unit; echo disabled > "$STATE/xorgate-agent.enabled"; touch "$STATE/FAIL_RELOAD"
bash "$B/exclusive.sh" begin >"$LOGS/p11.log" 2>&1; rm -f "$STATE/FAIL_RELOAD"
grep -q "begin aborted" "$LOGS/p11.log" && [ "$(js_en)" = enabled ]; expect $? "aborted begin: trap enabled JS"
check active "aborted begin: JS active"
bash "$B/exclusive.sh" end >/dev/null 2>&1

echo "== case (p13): ntp-unsync.sh break and gate-seam.sh --restart refuse the shipped unit (review-wp3 S1)"
reset; converted_unit; echo inactive > "$STATE/xorgate-agent"; mkdir -p /run/systemd/system/xorgate-agent-rs.service.d
touch /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf; rm -f /var/lib/xorgate-rs/ntp-unsync.state; : > "$STATE/systemctl.log"
bash "$B/ntp-unsync.sh" break >"$LOGS/p13a.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -qF "REFUSING: $I6" "$LOGS/p13a.log" && ! grep -q "xorgate-agent-rs" "$STATE/systemctl.log" && [ ! -e /var/lib/xorgate-rs/ntp-unsync.state ]
expect $? "ntp-unsync.sh break: rc=1, I6 text, no Rust stop/start, clock untouched (rc=$rc)"
rm -f /run/systemd/system/xorgate-agent-rs.service.d/99-exclusive.conf
bash "$B/gate-seam.sh" on --restart >"$LOGS/p13b.log" 2>&1; rc=$?
[ "$rc" != 0 ] && grep -qF "REFUSING the restart: $I6" "$LOGS/p13b.log" && ! grep -q "restart xorgate-agent-rs" "$STATE/systemctl.log"
expect $? "gate-seam.sh on --restart: refuses the Rust restart (rc=$rc)"
bash "$B/gate-seam.sh" off --restart >"$LOGS/p13c.log" 2>&1; rc=$?
[ "$rc" != 0 ] && grep -qF "$I6" "$LOGS/p13c.log" && ! grep -q "restart xorgate-agent-rs" "$STATE/systemctl.log" && [ ! -e /run/systemd/system/xorgate-agent-rs.service.d/97-gate-seam.conf ]
expect $? "gate-seam.sh off --restart: seam torn down, Rust restart refused (rc=$rc)"
bench_unit; bash "$B/gate-seam.sh" off >/dev/null 2>&1; reset

echo "== case (p14): the dead-man waits (bounded) for a RUNNING install-rs.sh before it reverts (review-wp3 S2)"
reset; bench_unit; no_record; LOCKD=/run/xorgate-rs-install.lock; rm -rf "$LOCKD"
XORGATE_TEST_DEADMAN_LOCK_SECS=4 bash "$B/exclusive.sh" begin >"$LOGS/p14-begin.log" 2>&1; rc=$?
expect $((rc != 0)) "begin (lock wait 4 s) rc=0 (rc=$rc)"
# A live install-rs.sh holds the lock; it finishes (state=converted) after ~2 s.
converted_unit; js_disabled_stopped; record converting; fake_installer 0
mkdir -p "$LOCKD"; sleep 1000 & LP=$!; echo "$LP" > "$LOCKD/pid"
( sleep 2; record converted; rm -rf "$LOCKD"; kill "$LP" ) &
fire >"$LOGS/p14a.log" 2>&1; rc=$?; wait "$LP" 2>/dev/null
w1=$(grep -n "waiting <= 4s" "$LOGS/p14a.log" | cut -d: -f1); f1=$(grep -n "install-rs.sh finished after" "$LOGS/p14a.log" | cut -d: -f1); r1=$(grep -n "running $INST --revert" "$LOGS/p14a.log" | cut -d: -f1)
[ "$rc" = 0 ] && [ -n "$w1" ] && [ -n "$f1" ] && [ -n "$r1" ] && [ "$f1" -lt "$r1" ] && grep -q "state=converted" "$LOGS/p14a.log" && [ "$(cat "$LOGS/installer.argv")" = --revert ]
expect $? "lock held: waited, then reverted the state the convert left (converted) (rc=$rc)"
check active "after the lock wait: JS active"; [ "$(js_en)" = enabled ]; expect $? "…and enabled"
converted_unit; js_disabled_stopped; record converted; fake_installer 0
mkdir -p "$LOCKD"; sleep 1000 & LP=$!; echo "$LP" > "$LOCKD/pid"
fire >"$LOGS/p14b.log" 2>&1; rc=$?; kill "$LP"; wait "$LP" 2>/dev/null; rm -rf "$LOCKD"
[ "$rc" = 0 ] && grep -q "still holds $LOCKD after 4s" "$LOGS/p14b.log" && [ "$(js_state)" = active ] && [ "$(js_en)" = enabled ]
expect $? "lock never released: bounded (4 s), then revert attempted + JS enabled + active anyway (rc=$rc)"
js_disabled_stopped; no_record; mkdir -p "$LOCKD"; echo 999999 > "$LOCKD/pid"
fire >"$LOGS/p14c.log" 2>&1; rc=$?; rm -rf "$LOCKD"
[ "$rc" = 0 ] && ! grep -q "waiting" "$LOGS/p14c.log" && [ "$(js_state)" = active ]; expect $? "a STALE lock (dead pid) is not waited for"
no_record; rm -rf /opt/xorgate-rs; bench_unit; bash "$B/exclusive.sh" end >/dev/null 2>&1

echo "== case (p12): status reports the conversion record + the shipped unit"
reset; converted_unit; record converted
bash "$B/exclusive.sh" status >"$LOGS/p12.log" 2>&1
grep -q "conversion record: state=converted" "$LOGS/p12.log" && grep -qF "UNSAFE: $I6" "$LOGS/p12.log"; expect $? "status names state=converted and the shipped unit"
no_record; bench_unit

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)  (logs in $LOGS)"
exit $fail
