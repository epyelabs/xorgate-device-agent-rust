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
  # (P5B_NO_COPY: what apply's copy step would stage)
  cp "$B/obd-drain.py" /tmp/xorgate-p5b/
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
[ "$(readlink /run/xorgate-rs-obd-python/obd_scan.py)" = /opt/xorgate-rs/python/obd_scan.py ] && [ "$(readlink /run/xorgate-rs-obd-python/obd)" = /opt/xorgate-rs/python/obd ] && [ ! -e /run/xorgate-rs-obd-python/bno085_read.py ]
expect $? "obd-only python dir (symlinks to the install, no bno085_read.py)"
[ ! -L /run/xorgate-rs-obd-python/obd_read.py ] && cmp -s /run/xorgate-rs-obd-python/obd_read.py "$B/obd-drain.py" && [ "$(stat -c %U /run/xorgate-rs-obd-python/obd_read.py)" = pi ]
expect $? "obd_read.py is the pty-drain launcher (scripts/bench/obd-drain.py), owned by pi"

echo "== WP5: every helper start drains the emulator pty first (the rig's obd-gate.py drain)"
# A real pty: the "emulator" holds the master and queues the replies to the
# PREVIOUS helper's last commands; the helper (a stand-in for the installed
# obd_read.py) reads its serial for 0.2 s and reports what it got. Spawned the
# way the agent does: python3 <XORGATE_PYTHON_DIR>/obd_read.py <argv>.
cat > "$LOGS/fake_obd_read.py" <<'P'
import os, select, sys, time, json
a = sys.argv[1:]; ser = a[a.index("--serial") + 1]
fd = os.open(ser, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
import termios, tty
tty.setraw(fd, termios.TCSANOW)
got = b""; t0 = time.time()
while time.time() - t0 < 0.2:
    r, _, _ = select.select([fd], [], [], 0.02)
    if r:
        try: got += os.read(fd, 4096)
        except BlockingIOError: pass
print(json.dumps({"pid": os.getpid(), "argv": a, "got": got.decode()}), flush=True)
P
cat > "$LOGS/pty_case.py" <<'P'
# <launcher> <real helper> <drain log>: queue two stale replies, then one
# that is still "in flight" (written 150 ms after the launcher starts).
import json, os, pty, subprocess, sys, time, termios, tty
launcher, real, log = sys.argv[1:4]
m, s = pty.openpty(); name = os.ttyname(s)
tty.setraw(s, termios.TCSANOW)
os.write(m, b"OK\r\r>"); os.write(m, b"ELM327 v1.5\r\r>")
env = dict(os.environ, XORGATE_P5B_REAL_HELPER=real, XORGATE_P5B_DRAIN_LOG=log)
p = subprocess.Popen(["python3", launcher, "--serial", name, "--poll-hz", "1"], stdout=subprocess.PIPE, env=env)
time.sleep(0.15); os.write(m, b"41 00 BE 3F A8 13\r\r>")
out, _ = p.communicate(timeout=10)
print(json.dumps({"rc": p.returncode, "launcher_pid": p.pid, "helper": json.loads(out.decode() or "null")}))
P
L=/run/xorgate-rs-obd-python/obd_read.py
python3 "$LOGS/pty_case.py" "$L" "$LOGS/fake_obd_read.py" "$LOGS/drain.ndjson" > "$LOGS/drain-case.json" 2>"$LOGS/drain-case.err"
python3 - "$LOGS/drain-case.json" "$LOGS/drain.ndjson" <<'P'
import json, sys
r = json.load(open(sys.argv[1])); d = [json.loads(l) for l in open(sys.argv[2])]
h = r["helper"]
assert r["rc"] == 0 and h["got"] == "", r               # no stale reply reached the helper
assert h["pid"] == r["launcher_pid"], r                  # exec: same pid (agent signals reach the helper)
assert h["argv"][1:] == ["--poll-hz", "1"] or h["argv"][2:] == ["--poll-hz", "1"], r  # argv passed through
want = len("OK\r\r>") + len("ELM327 v1.5\r\r>") + len("41 00 BE 3F A8 13\r\r>")
assert len(d) == 1 and d[0]["bytes"] == want and d[0]["quiet"] and d[0]["ms"] >= 300, d
P
expect $? "drain: queued + in-flight stale replies never reach the helper; exec keeps pid + argv; logged (bytes, quiet)"
# Negative: the pre-WP5 overlay (obd_read.py = a symlink to the installed
# helper, no drain) hands the next helper the stale replies.
python3 "$LOGS/pty_case.py" "$LOGS/fake_obd_read.py" "$LOGS/fake_obd_read.py" "$LOGS/drain-none.ndjson" > "$LOGS/nodrain-case.json" 2>&1
python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); sys.exit(0 if r["helper"]["got"].startswith("OK\r\r>ELM327") else 1)' "$LOGS/nodrain-case.json"
expect $? "negative: without the launcher the helper reads the previous helper's stale replies"
python3 - "$L" "$LOGS/fake_obd_read.py" <<'P'
import json, os, subprocess, sys
# no --serial / an unopenable one: no drain, the helper still starts
for argv in ([], ["--serial", "/nonexistent/pty"]):
    env = dict(os.environ, XORGATE_P5B_REAL_HELPER=sys.argv[2], XORGATE_P5B_DRAIN_LOG="/nonexistent/dir/log")
    if not argv:
        r = subprocess.run(["python3", sys.argv[1]], env=env, capture_output=True, text=True, timeout=10)
        assert "--serial" in r.stderr or r.returncode != 0  # the stand-in needs --serial: it was exec'd
    else:
        r = subprocess.run(["python3", sys.argv[1]] + argv, env=env, capture_output=True, text=True, timeout=10)
        assert "FileNotFoundError" in r.stderr, r.stderr     # exec'd the helper (it failed to open), no drain hang
P
expect $? "drain: no --serial / unopenable serial = no drain, helper still exec'd; an unwritable log never blocks"
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
echo "== wheels: the container command builds an offline-installable wheelhouse (gate 2026-10-28)"
# Found by the Phase 5b gate: (1) bookworm's pip 23.0.1 has no
# --break-system-packages on `download` (install-only option): "no such option"
# before anything was fetched; (2) `pip3 download` leaves ELM327-emulator 4.0.0
# as an sdist, and the bench's offline `pip install --no-index --find-links`
# (apply) cannot build it (no setuptools>=61 in the wheelhouse). `pip3 wheel`
# builds every requirement into a wheel, so the offline install never builds.
WSTUB=$(mktemp -d)
cat > "$WSTUB/docker" <<'S'
#!/bin/sh
printf '%s\n' "$@" > "$DOCKER_ARGV"
S
chmod 0755 "$WSTUB/docker"
DOCKER_ARGV="$LOGS/wheels.argv" PATH="$WSTUB:$PATH" P5B_WHEELS="$LOGS/wheels" bash "$B/p5b-overlay.sh" wheels >"$LOGS/w1.log" 2>&1
expect $? "wheels rc=0 (docker stubbed)"
WCMD=$(tail -n 1 "$LOGS/wheels.argv")
printf '%s' "$WCMD" | grep -q 'pip3 wheel .*-w /w "ELM327-emulator==4.0.0"' && ! printf '%s' "$WCMD" | grep -q -e '--break-system-packages' -e 'pip3 download'
expect $? "wheels: 'pip3 wheel -w /w' (wheels only), no 'pip3 download', no install-only --break-system-packages"
rm -rf "$WSTUB"

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
