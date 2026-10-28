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
# Cross-side barrier: side a's k-th edge (`status`) waits for side b's k-th;
# an edge the partner never reaches releases after the 3 s timeout (logged).
mkdir -p "$W/bar" "$W/a" "$W/b"; cp /t/obd-gate.py "$W/a/obd_read.py"; cp /t/obd-gate.py "$W/b/obd_read.py"
printf 'import time, json, sys\nd=float(sys.argv[1])\ntime.sleep(d)\nprint(json.dumps({"type":"status","t":time.time()}), flush=True)\nprint(json.dumps({"type":"profile"}), flush=True) if sys.argv[2] == "a" else None\n' > "$W/a/obd_read_real.py"
cp "$W/a/obd_read_real.py" "$W/b/obd_read_real.py"
rm -f "$G"
( RIG_OBD_WAITLOG=$W/a/wait.ndjson RIG_OBD_BARRIER=$W/bar RIG_OBD_SIDE=a RIG_OBD_PEERS=a,b python3 "$W/a/obd_read.py" 0 a > "$W/a/out" 2> "$W/a/err"; date +%s.%N > "$W/a/done" ) &
( RIG_OBD_WAITLOG=$W/b/wait.ndjson RIG_OBD_BARRIER=$W/bar RIG_OBD_SIDE=b RIG_OBD_PEERS=a,b python3 "$W/b/obd_read.py" 1 b > "$W/b/out" 2> "$W/b/err" ) &
sleep 0.5; [ ! -s "$W/a/out" ] && ok "barrier: a's status waits for b's" || bad "barrier leaked: $(cat "$W/a/out")"
wait
[ "$(wc -l < "$W/a/out")" = 2 ] && [ "$(wc -l < "$W/b/out")" = 1 ] && ok "both sides release every line" || bad "lines a=$(wc -l < "$W/a/out") b=$(wc -l < "$W/b/out")"
grep -q '"timeout": true' "$W/a/wait.ndjson" && [ ! -s "$W/a/err" ] && ok "an edge the partner never reaches: timeout in the rig wait log, nothing on the helper's stderr" || bad "wait log: $(cat "$W/a/wait.ndjson" 2>&1) err: $(cat "$W/a/err")"
python3 -c 'import json,sys; w=[json.loads(l) for l in open(sys.argv[1])]; first=w[0]; sys.exit(0 if first["kind"]=="status" and first["wait_ms"]>=800 and not first["timeout"] else 1)' "$W/a/wait.ndjson" && ok "the wait for the 1 s-late partner is logged (>= 800 ms)" || bad "first wait: $(head -1 "$W/a/wait.ndjson")"
# Report hold: after a `status` line, a line due 2.1 s later waits until
# 3.25 s after it (the two agents' debounced report builds); none without a gate.
mkdir -p "$W/h"; cp /t/obd-gate.py "$W/h/obd_read.py"
printf 'import time, json\nprint(json.dumps({"type":"status"}), flush=True)\ntime.sleep(2.1)\nprint(json.dumps({"type":"profile"}), flush=True)\n' > "$W/h/obd_read_real.py"
echo 1 > "$G"
python3 - "$W/h/obd_read.py" <<'P'
import subprocess, sys, time, os
t0 = time.time()
p = subprocess.Popen([sys.executable, sys.argv[1]], stdout=subprocess.PIPE, env={**os.environ, "RIG_OBD_GATE": os.environ["RIG_OBD_GATE"]})
stamps = [time.time() - t0 for _ in iter(p.stdout.readline, b"")]
p.wait()
gap = stamps[1] - stamps[0]
print("ok   report hold: profile released %.2f s after the status line" % gap if 3.1 <= gap <= 3.6 else "FAIL report hold gap %.2f" % gap)
sys.exit(0 if 3.1 <= gap <= 3.6 else 1)
P
[ $? = 0 ] || fail=1
# Symmetric report hold (review 2026-10-28): side a's relay is descheduled
# (SIGSTOP) for 0.2 s while it waits at the first status edge, so it sees that
# edge's release 0.15 s after side b does. The second status reaches the
# barrier 1.80-1.85 s after the first edge's shared release — inside the hold
# window measured from b's view, outside a's own late view. Both relays must
# still make the SAME decision (held to the shared window's end). The old
# per-side wall clock released a at once and held b 1.4 s.
mkdir -p "$W/sy/bar" "$W/sy/a" "$W/sy/b"; echo 1 > "$W/sy/gate"
for s in a b; do cp /t/obd-gate.py "$W/sy/$s/obd_read.py"; done
cat > "$W/sy/helper.py" <<'P'
import json, os, time
t0 = float(os.environ["T0"]); d1, d2 = float(os.environ["D1"]), float(os.environ["D2"])
def at(t):
    time.sleep(max(0.0, t0 + t - time.time()))
at(d1); print(json.dumps({"type": "status", "link": 1, "n": 1}), flush=True)
at(d2); print(json.dumps({"type": "status", "link": 1, "n": 2}), flush=True)
time.sleep(0.2)
P
cp "$W/sy/helper.py" "$W/sy/a/obd_read_real.py"; cp "$W/sy/helper.py" "$W/sy/b/obd_read_real.py"
cat > "$W/sy/stamp.py" <<'P'
import sys, time
for l in sys.stdin:
    print(f"{time.time():.3f} {l.strip()}", flush=True)
P
T0=$(python3 -c 'import time; print(time.time() + 1.0)')
export T0 RIG_OBD_GATE="$W/sy/gate" RIG_OBD_BARRIER="$W/sy/bar" RIG_OBD_PEERS=a,b
RIG_OBD_SIDE=a RIG_OBD_WAITLOG="$W/sy/a/wait.ndjson" D1=0.0 D2=1.95 python3 "$W/sy/a/obd_read.py" > >(python3 "$W/sy/stamp.py" > "$W/sy/a/out") & PA=$!
RIG_OBD_SIDE=b RIG_OBD_WAITLOG="$W/sy/b/wait.ndjson" D1=0.1 D2=1.90 python3 "$W/sy/b/obd_read.py" > >(python3 "$W/sy/stamp.py" > "$W/sy/b/out") & PB=$!
python3 -c 'import os, time; time.sleep(max(0.0, float(os.environ["T0"]) + 0.05 - time.time()))'; kill -STOP $PA
python3 -c 'import os, time; time.sleep(max(0.0, float(os.environ["T0"]) + 0.25 - time.time()))'; kill -CONT $PA
wait $PA $PB; sleep 0.3
python3 - "$W/sy/a/out" "$W/sy/b/out" "$T0" <<'P' && ok "report hold: the same decision on both sides when one relay is descheduled" || bad "report hold asymmetric: a=$(tail -1 "$W/sy/a/out") b=$(tail -1 "$W/sy/b/out")"
import sys
t0 = float(sys.argv[3])
second = [float(next(l for l in open(f) if '"n": 2' in l).split()[0]) - t0 for f in sys.argv[1:3]]
print("second status delivered at", second)
sys.exit(0 if abs(second[0] - second[1]) < 0.1 and min(second) > 3.2 else 1)
P
unset T0 RIG_OBD_GATE RIG_OBD_BARRIER RIG_OBD_PEERS
# Shared tick-gate GO (review-wp4 R5-2): side a's relay is descheduled while
# it waits at an edge; b arrives, finds the gate open and delivers; the gate
# then closes (a tick-pair window) before a resumes. Both must still deliver
# the edge at the ONE published GO (a's lateness is logged); the old per-relay
# gate read held a's copy until the gate reopened (0.9 s apart).
mkdir -p "$W/gg/bar" "$W/gg/a" "$W/gg/b"
for s in a b; do cp /t/obd-gate.py "$W/gg/$s/obd_read.py"; cp "$W/sy/helper.py" "$W/gg/$s/obd_read_real.py"; done
T0=$(python3 -c 'import time; print(time.time() + 1.0)')
export T0 RIG_OBD_GATE="$W/gg/gate" RIG_OBD_BARRIER="$W/gg/bar" RIG_OBD_PEERS=a,b
echo 1 > "$W/gg/gate"
RIG_OBD_SIDE=a RIG_OBD_WAITLOG="$W/gg/a/wait.ndjson" D1=0.0 D2=3.0 python3 "$W/gg/a/obd_read.py" > >(python3 "$W/sy/stamp.py" > "$W/gg/a/out") & PA=$!
RIG_OBD_SIDE=b RIG_OBD_WAITLOG="$W/gg/b/wait.ndjson" D1=0.1 D2=3.0 python3 "$W/gg/b/obd_read.py" > >(python3 "$W/sy/stamp.py" > "$W/gg/b/out") & PB=$!
at() { python3 -c 'import os, sys, time; time.sleep(max(0.0, float(os.environ["T0"]) + float(sys.argv[1]) - time.time()))' "$1"; }
at 0.05; kill -STOP $PA
at 0.15; echo 0 > "$W/gg/gate.tmp" && mv "$W/gg/gate.tmp" "$W/gg/gate"
at 0.25; kill -CONT $PA
at 1.0; echo 1 > "$W/gg/gate.tmp" && mv "$W/gg/gate.tmp" "$W/gg/gate"
wait $PA $PB; sleep 0.3
python3 - "$W/gg/a/out" "$W/gg/b/out" "$T0" "$W/gg/a/wait.ndjson" <<'P' && ok "tick gate: one shared GO per edge (a descheduled relay delivers at the GO, late logged)" || bad "tick gate asymmetric: a=$(head -1 "$W/gg/a/out") b=$(head -1 "$W/gg/b/out")"
import json, sys
t0 = float(sys.argv[3])
first = [float(next(l for l in open(f) if '"n": 1' in l).split()[0]) - t0 for f in sys.argv[1:3]]
go = [json.loads(l) for l in open(sys.argv[4]) if '"go"' in l]
print("first status delivered at", first, "a's GO:", go[:1])
sys.exit(0 if abs(first[0] - first[1]) < 0.2 and max(first) < 0.5 and go and go[0]["late_ms"] >= 100 and not go[0]["decider"] else 1)
P
unset T0 RIG_OBD_GATE RIG_OBD_BARRIER RIG_OBD_PEERS
# Held helper lines before barrier inputs (review-wp4 R7-1): both helpers
# print a sample while the tick gate is closed; relay a is descheduled across
# the gate opening. The player (played here) waits until no relay flags a
# held line before it releases a barrier input; both samples must reach their
# agents BEFORE that input. The old relay flagged nothing: the input went out
# at the opening and a's sample 0.1 s after it.
mkdir -p "$W/hl/bar" "$W/hl/a" "$W/hl/b"
cat > "$W/hl/helper.py" <<'P'
import json, os, time
t0 = float(os.environ["T0"])
time.sleep(max(0.0, t0 - time.time()))
print(json.dumps({"type": "sample", "fields": {"rpm": 800}}), flush=True)
time.sleep(1.5)
P
for s in a b; do cp /t/obd-gate.py "$W/hl/$s/obd_read.py"; cp "$W/hl/helper.py" "$W/hl/$s/obd_read_real.py"; done
T0=$(python3 -c 'import time; print(time.time() + 1.0)')
export T0 RIG_OBD_GATE="$W/hl/gate" RIG_OBD_BARRIER="$W/hl/bar" RIG_OBD_PEERS=a,b
echo "0 1" > "$W/hl/gate"
RIG_OBD_SIDE=a RIG_OBD_WAITLOG="$W/hl/a/wait.ndjson" python3 "$W/hl/a/obd_read.py" > >(python3 "$W/sy/stamp.py" > "$W/hl/a/out") & PA=$!
RIG_OBD_SIDE=b RIG_OBD_WAITLOG="$W/hl/b/wait.ndjson" python3 "$W/hl/b/obd_read.py" > >(python3 "$W/sy/stamp.py" > "$W/hl/b/out") & PB=$!
at() { python3 -c 'import os, sys, time; time.sleep(max(0.0, float(os.environ["T0"]) + float(sys.argv[1]) - time.time()))' "$1"; }
at 0.2; echo "0 1" > "$W/hl/gate.tmp" && mv "$W/hl/gate.tmp" "$W/hl/gate"; kill -STOP $PA
at 0.3; echo "1 2" > "$W/hl/gate.tmp" && mv "$W/hl/gate.tmp" "$W/hl/gate"
( at 0.4; kill -CONT $PA ) &
# the player: no barrier input while a relay holds a line (cap 2 s)
python3 - "$W/hl/bar" > "$W/hl/input_at" <<'P'
import os, sys, time
bar = sys.argv[1]; start = time.time()
def holding():
    for s in "ab":
        try:
            if time.time() - float(open(os.path.join(bar, s + ".held")).read()) <= 2.0:
                return True
        except (OSError, ValueError):
            pass
    return False
while holding() and time.time() - start < 2.0:
    time.sleep(0.005)
print(f"{time.time():.3f}")
P
wait $PA $PB; sleep 0.3
python3 - "$W/hl/a/out" "$W/hl/b/out" "$W/hl/input_at" <<'P' && ok "held helper lines reach both agents before the barrier input" || bad "a held line after the barrier input: a=$(head -1 "$W/hl/a/out") b=$(head -1 "$W/hl/b/out") input=$(cat "$W/hl/input_at")"
import sys
inp = float(open(sys.argv[3]).read())
got = [float(next(l for l in open(f) if '"sample"' in l).split()[0]) for f in sys.argv[1:3]]
print("samples at", [round(g - inp, 3) for g in got], "relative to the input")
sys.exit(0 if all(g <= inp for g in got) else 1)
P
unset T0 RIG_OBD_GATE RIG_OBD_BARRIER RIG_OBD_PEERS
# Adapter drain (orchestrator ruling 2026-10-27): a reply queued on the pty
# for the PREVIOUS helper (and one arriving 0.15 s after the relay starts) never
# reaches the next helper; the drain is logged with its byte count. Without the
# drain the helper reads the stale "OK>" as the answer to its first command.
mkdir -p "$W/dr"; cp /t/obd-gate.py "$W/dr/obd_read.py"
cat > "$W/dr/obd_read_real.py" <<'P'
import os, sys, time, tty
p = sys.argv[sys.argv.index("--serial") + 1]
fd = os.open(p, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK); tty.setraw(fd)
time.sleep(0.2)
try: got = os.read(fd, 4096)
except BlockingIOError: got = b""
print(repr(got), flush=True)
P
cat > "$W/dr/drive.py" <<'P'
import os, pty, subprocess, sys, threading, time, tty
m, s = pty.openpty(); tty.setraw(s); path = os.ttyname(s)
os.write(m, b"OK\r\r>")                         # the old helper's unread reply
threading.Timer(0.15, lambda: os.write(m, b"ELM327 v1.5\r\r>")).start()  # still in flight
env = dict(os.environ, RIG_OBD_WAITLOG=sys.argv[2], RIG_OBD_SIDE="a")
out = subprocess.run([sys.executable, sys.argv[1], "--serial", path], env=env,
                     capture_output=True, text=True, timeout=10).stdout.strip()
os.close(s)  # held open meanwhile: the queued reply stays in the input queue
print(out)
P
rm -f "$G"
got=$(python3 "$W/dr/drive.py" "$W/dr/obd_read.py" "$W/dr/wait.ndjson")
[ "$got" = "b''" ] && ok "drain: the next helper reads no stale reply" || bad "drain: helper read $got"
python3 -c 'import json,sys; w=[json.loads(l) for l in open(sys.argv[1])]; d=[x for x in w if x["kind"]=="drain"]; sys.exit(0 if len(d)==1 and d[0]["bytes"]==len("OK\r\r>")+len("ELM327 v1.5\r\r>") and d[0]["quiet"] and d[0]["ms"]>=300 else 1)' "$W/dr/wait.ndjson" && ok "drain logged: both stale replies' bytes, quiet" || bad "drain log: $(cat "$W/dr/wait.ndjson")"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"; exit $fail
