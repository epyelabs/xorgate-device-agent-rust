#!/usr/bin/env bash
# obd-gate.py self-test (Phase 5b WP4), in any python3 image:
#   docker run --rm -v "$PWD/tools/replay:/t:ro" python:3.11-slim bash /t/obd-gate-test.sh
# A closed gate holds the helper's lines; open releases them in order; a
# SIGTERM is forwarded and the exit waits for the gate; a stale / missing gate
# file is open; a helper killed by a signal is re-raised (the agent sees the
# same signal exit).
set -u
W=$(mktemp -d); cp /t/obd-gate.py "$W/obd_read.py"; cd "$W"; G=$W/gate; export RIG_OBD_GATE=$G
fail=0; ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fail=1; }
cat > obd_read_real.py <<'P'
import sys, time, signal
signal.signal(signal.SIGTERM, lambda *_: (print("bye", flush=True), sys.exit(143)))
print("ready", " ".join(sys.argv[1:]), flush=True)
for i in range(100):
    print("sample", i, flush=True); time.sleep(0.2)
P
echo 0 > "$G"; python3 obd_read.py --x 1 > out 2>&1 & P=$!
sleep 1; [ "$(wc -l < out)" = 0 ] && ok "closed gate holds every line" || bad "closed gate leaked $(wc -l < out) lines"
echo 1 > "$G"; sleep 0.5; [ "$(head -1 out)" = "ready --x 1" ] && [ "$(wc -l < out)" -ge 5 ] && ok "open releases in order (argv passed)" || bad "open: $(head -2 out)"
echo 0 > "$G"; kill -TERM $P; sleep 0.5; kill -0 $P 2>/dev/null && ok "exit waits for the gate" || bad "exited while closed"
echo 1 > "$G"; wait $P; rc=$?; [ $rc = 143 ] && [ "$(tail -1 out)" = bye ] && ok "SIGTERM forwarded, rc 143" || bad "rc=$rc tail=$(tail -1 out)"
echo 0 > "$G"; touch -d '-5 seconds' "$G"; timeout 2 python3 obd_read.py > out2; [ "$(head -1 out2)" = "ready " ] && ok "stale gate = open" || bad "stale: $(head -1 out2)"
rm -f "$G"; timeout 2 python3 obd_read.py > out3; [ -s out3 ] && ok "missing gate = open" || bad "missing gate held"
printf 'import os, signal\nprint("x", flush=True); os.kill(os.getpid(), signal.SIGTERM)\n' > obd_read_real.py
python3 obd_read.py > /dev/null; rc=$?; [ $rc = 143 ] && ok "signal death re-raised" || bad "rc=$rc"
# Cross-side barrier: side a's k-th `status` waits for side b's k-th; a kind
# the partner never reaches releases after the 3 s timeout (logged).
mkdir -p "$W/bar" "$W/a" "$W/b"; cp /t/obd-gate.py "$W/a/obd_read.py"; cp /t/obd-gate.py "$W/b/obd_read.py"
printf 'import time, json, sys\nd=float(sys.argv[1])\ntime.sleep(d)\nprint(json.dumps({"type":"status","t":time.time()}), flush=True)\nprint(json.dumps({"type":"only_"+sys.argv[2]}), flush=True)\n' > "$W/a/obd_read_real.py"
cp "$W/a/obd_read_real.py" "$W/b/obd_read_real.py"
rm -f "$G"
( RIG_OBD_BARRIER=$W/bar RIG_OBD_SIDE=a RIG_OBD_PEERS=a,b python3 "$W/a/obd_read.py" 0 a > "$W/a/out" 2> "$W/a/err"; date +%s.%N > "$W/a/done" ) &
( RIG_OBD_BARRIER=$W/bar RIG_OBD_SIDE=b RIG_OBD_PEERS=a,b python3 "$W/b/obd_read.py" 1 b > "$W/b/out" 2> "$W/b/err" ) &
sleep 0.5; [ ! -s "$W/a/out" ] && ok "barrier: a's status waits for b's" || bad "barrier leaked: $(cat "$W/a/out")"
wait
[ "$(wc -l < "$W/a/out")" = 2 ] && [ "$(wc -l < "$W/b/out")" = 2 ] && ok "both sides release every line" || bad "lines a=$(wc -l < "$W/a/out") b=$(wc -l < "$W/b/out")"
grep -q 'barrier timeout: a only_a #1' "$W/a/err" && ok "a kind the partner never reaches: timeout, logged" || bad "no timeout log: $(cat "$W/a/err")"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"; exit $fail
