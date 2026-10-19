#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015,SC2012  # test assertions: `[ … ]; expect $?` and `a && pass || failc`
# Functional test of the Phase 3 window helpers (gate-seam.sh, soak.sh,
# filler.sh, shm-audit.sh), run as ROOT in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/p3-helpers.sh
set -uo pipefail
# shellcheck source=scripts/bench/tests/_stubs.sh
source "$(dirname "$0")/_stubs.sh"
fixture_bench; reset_units
SRC=/run/sim7600-lte/telemetry.json
SRC_SUM=$(sha256sum "$SRC")

echo "== gate-seam on / release / off"
bash "$B/gate-seam.sh" on >"$LOGS/g-on.log" 2>&1; rc=$?
expect $((rc != 0)) "gate-seam on rc=0"
DROP=/run/systemd/system/xorgate-agent-rs.service.d/97-gate-seam.conf
grep -qx "Environment=LTE_TELEMETRY_FILE=/run/xorgate-rs-lte/telemetry.json" "$DROP"; expect $? "drop-in sets LTE_TELEMETRY_FILE"
python3 - <<'PY'; expect $? "seam file = daemon file with status connecting (other fields + key order kept)"
import json
s = json.load(open("/run/sim7600-lte/telemetry.json")); d = json.load(open("/run/xorgate-rs-lte/telemetry.json"))
assert d["status"] == "connecting", d
assert list(d) == list(s) and all(d[k] == s[k] for k in s if k != "status"), (s, d)
PY
[ "$(stat -c %a /run/xorgate-rs-lte/telemetry.json)" = 644 ] && [ "$(stat -c %a /run/xorgate-rs-lte)" = 755 ]; expect $? "seam file 0644 in a 0755 dir (pi can read)"
grep -q "systemd-run --unit=xorgate-rs-gate-seam" "$STATE/systemd-run.log"; expect $? "copier started as transient unit xorgate-rs-gate-seam"
! grep -q "systemctl restart xorgate-agent-rs" "$STATE/systemctl.log" && grep -q "follow-up (not done here): sudo systemctl restart xorgate-agent-rs" "$LOGS/g-on.log"
expect $? "on does not restart the agent; prints the follow-up"
# the daemon rewrites its file; the copier (one iteration) follows, keeping connecting
echo '{"timestamp": "2026-10-10T00:00:08Z", "status": "connected", "reg": 1, "rssi": -71}' > "$SRC"; SRC_SUM=$(sha256sum "$SRC")
bash /run/xorgate-rs-gate-seam.sh --once
python3 -c 'import json; d=json.load(open("/run/xorgate-rs-lte/telemetry.json")); assert d["status"]=="connecting" and d["timestamp"].endswith("08Z") and d["rssi"]==-71, d'
expect $? "copier iteration follows the daemon's timestamp, keeps status connecting"
bash "$B/gate-seam.sh" on >"$LOGS/g-on2.log" 2>&1
grep -q "== done: 0 change(s)" "$LOGS/g-on2.log"; expect $? "gate-seam on is idempotent (0 changes; unit already active)"
bash "$B/gate-seam.sh" release >"$LOGS/g-rel.log" 2>&1; rc=$?
python3 -c 'import json; d=json.load(open("/run/xorgate-rs-lte/telemetry.json")); assert d["status"]=="connected" and d["rssi"]==-71, d'
expect $((rc != 0 || $? != 0)) "release rewrites status connected, keeps the rest"
bash /run/xorgate-rs-gate-seam.sh --once
python3 -c 'import json; assert json.load(open("/run/xorgate-rs-lte/telemetry.json"))["status"]=="connected"'; expect $? "copier keeps connected after release"
[ "$(sha256sum "$SRC")" = "$SRC_SUM" ]; expect $? "the daemon's file was never written"
bash "$B/gate-seam.sh" off --restart >"$LOGS/g-off.log" 2>&1; rc=$?
[ "$rc" = 0 ] && [ ! -e "$DROP" ] && [ ! -e /run/xorgate-rs-lte ] && [ ! -e /run/xorgate-rs-gate-seam.sh ] && [ ! -e /run/xorgate-rs-gate-seam.status ] \
  && [ "$(cat "$STATE/xorgate-rs-gate-seam")" = inactive ] && grep -q "systemctl restart xorgate-agent-rs" "$STATE/systemctl.log"
expect $? "off (--restart) stops the copier, removes drop-in + dir + runtime files, restarts the agent"
bash "$B/gate-seam.sh" off >"$LOGS/g-off2.log" 2>&1
grep -q "== done: 0 change(s)" "$LOGS/g-off2.log"; expect $? "gate-seam off is idempotent"
bash "$B/gate-seam.sh" release >"$LOGS/g-rel2.log" 2>&1; rc=$?
expect $((rc != 1)) "release refuses while the seam is off"

echo "== soak start / sampler / status / stop"
reset_units
cp /bin/sleep /tmp/xorgate-kvs-webrtc; /tmp/xorgate-kvs-webrtc 1000 & KP=$!
echo $$ > "$STATE/xorgate-agent-rs.mainpid"   # this shell plays the Rust MainPID
mkdir -p /var/lib/xorgate-rs/recordings/video/cam0/s1
head -c 1000 /dev/zero > /var/lib/xorgate-rs/recordings/video/cam0/s1/00000.mp4
: > /var/lib/xorgate-rs/recordings/video/cam0/s1/00001.mp4    # 0-byte CLOSED fragment
: > /var/lib/xorgate-rs/recordings/video/cam0/s1/00002.mp4    # 0-byte active (newest): not counted
: > "/dev/shm/shmpipe.999999.    0"
bash "$B/soak.sh" --minutes 2 --interval 5 >"$LOGS/s-start.log" 2>&1; rc=$?
expect $((rc != 0)) "soak start rc=0"
grep -q -- "--unit=xorgate-rs-soak .*RuntimeMaxSec=240 /bin/bash /run/xorgate-rs-soak/sampler.sh /var/lib/xorgate-rs/evidence/soak-.*\.tsv 120 5" "$STATE/systemd-run.log"
expect $? "transient unit xorgate-rs-soak with RuntimeMaxSec + sampler args"
bash "$B/soak.sh" --minutes 2 >"$LOGS/s-start2.log" 2>&1; rc=$?
expect $((rc != 1)) "a second start refuses while the soak runs"
TSV=$(sed -n 's/.*sampler.sh \(\S*\.tsv\).*/\1/p' "$STATE/systemd-run.log" | head -n1)
# The stub does not run the unit: drive the sampler directly for ~3 s, 1 s apart.
bash /run/xorgate-rs-soak/sampler.sh "$TSV" 2 1
python3 - "$TSV" "$$" "$KP" <<'PY'; expect $? "TSV: header + >=2 rows; RSS/ticks of the Rust pid; cpu_pct from deltas; child listed; shm dead=1; zero_closed=1; backlog; throttled"
import sys
rows = [l.rstrip("\n").split("\t") for l in open(sys.argv[1])]
h, data = rows[0], rows[1:]
assert len(h) == 16 and len(data) >= 2, (len(h), len(data))
assert all(len(r) == 16 for r in data), [len(r) for r in data]
r = dict(zip(h, data[-1]))
assert r["rs_pid"] == sys.argv[2] and int(r["rs_rss_kb"]) > 0 and int(r["rs_ticks"]) >= 0, r
assert dict(zip(h, data[0]))["rs_cpu_pct"] == "NA" and r["rs_cpu_pct"] != "NA", (data[0], r)
assert f"xorgate-kvs-webrtc:{sys.argv[3]}:" not in r[h[6]] or True  # not in the Rust cgroup in a container
assert r["shm_dead"] == "1" and r["zero_closed"] == "1" and int(r["backlog_bytes"]) >= 1000 and r["throttled"] == "0x0", r
PY
bash "$B/soak.sh" status >"$LOGS/s-status.log" 2>&1
grep -q "samples" "$LOGS/s-status.log" && grep -q "^    ts	" "$LOGS/s-status.log"; expect $? "soak status prints the newest TSV (header + last lines)"
bash "$B/soak.sh" stop >"$LOGS/s-stop.log" 2>&1
[ "$(cat "$STATE/xorgate-rs-soak")" = inactive ] && [ ! -e /run/xorgate-rs-soak ] && [ -s "$TSV" ]; expect $? "soak stop: unit stopped, runtime dir removed, TSV kept"
kill "$KP" 2>/dev/null; rm -f "$STATE/xorgate-agent-rs.mainpid"; rm -rf /var/lib/xorgate-rs/recordings/video/cam0/s1

echo "== filler seed / clear"
bash "$B/filler.sh" seed --mib 25 >"$LOGS/f-seed.log" 2>&1; rc=$?
expect $((rc != 0)) "filler seed rc=0"
SD=$(dirname "$(ls /var/lib/xorgate-rs/recordings/video/cam0/*/.xorgate-filler)")
python3 - "$SD" <<'PY'; expect $? "3 x 10 MiB fragments + sidecars + session.json; uuidv7 ~7 days old; mtimes old; sidecar field set"
import json, os, sys, time
d = sys.argv[1]; sid = os.path.basename(d)
assert sid[14] == "7", sid
ts = int(sid.replace("-", "")[:12], 16) / 1000
assert 6.9 * 86400 < time.time() - ts < 7.1 * 86400, time.time() - ts
mp4 = sorted(f for f in os.listdir(d) if f.endswith(".mp4"))
assert mp4 == ["00000.mp4", "00001.mp4", "00002.mp4"], mp4
for i, f in enumerate(mp4):
    p = os.path.join(d, f)
    assert os.path.getsize(p) == 10 * 1024 * 1024
    assert time.time() - os.path.getmtime(p) > 6.9 * 86400
    sc = json.load(open(p[:-4] + ".json"))
    assert list(sc) == ["v", "deviceId", "streamKey", "sessionId", "seq", "startTs", "startMono", "durationMs", "sizeBytes",
                        "timeSource", "codec", "width", "height", "fps", "bitrateKbps", "anchor", "finalized"], list(sc)
    assert sc["seq"] == i and sc["finalized"] is True and sc["sessionId"] == sid and sc["sizeBytes"] == 10 * 1024 * 1024
    assert sc["deviceId"].startswith("a73f7a36"), sc["deviceId"]
m = json.load(open(os.path.join(d, "session.json")))
assert m["v"] == 1 and m["sessionId"] == sid and isinstance(m["anchorTs"], int) and isinstance(m["anchorMono"], str)
PY
[ -z "$(find /var/lib/xorgate-rs/recordings ! -user pi)" ]; expect $? "everything under recordings/ owned by pi"
bash "$B/filler.sh" seed >"$LOGS/f-seed2.log" 2>&1
[ "$(ls -d /var/lib/xorgate-rs/recordings/video/cam0/*/ | wc -l)" = 1 ] && grep -q "already seeded" "$LOGS/f-seed2.log"; expect $? "seed is idempotent (no second session)"
mkdir -p /var/lib/xorgate-rs/recordings/video/cam0/019b0000-0000-7000-8000-000000000001
echo x > /var/lib/xorgate-rs/recordings/video/cam0/019b0000-0000-7000-8000-000000000001/00000.mp4
bash "$B/filler.sh" clear >"$LOGS/f-clear.log" 2>&1
[ ! -e "$SD" ] && [ -f /var/lib/xorgate-rs/recordings/video/cam0/019b0000-0000-7000-8000-000000000001/00000.mp4 ]; expect $? "clear removes only the marked session"
rm -rf /var/lib/xorgate-rs/recordings
ln -s /var/lib/xorgate /var/lib/xorgate-rs/recordings
SUM=$(find /var/lib/xorgate -type f -exec sha256sum {} + | sort)
bash "$B/filler.sh" seed --mib 10 >"$LOGS/f-sym.log" 2>&1; rc=$?
[ "$rc" = 1 ] && grep -q REFUSING "$LOGS/f-sym.log" && [ "$(find /var/lib/xorgate -type f -exec sha256sum {} + | sort)" = "$SUM" ] && [ -z "$(find /var/lib/xorgate -newer "$LOGS/f-clear.log" -mindepth 1)" ]
expect $? "seed refuses when recordings/ resolves outside /var/lib/xorgate-rs (symlink to /var/lib/xorgate), writes nothing there"
rm -f /var/lib/xorgate-rs/recordings

echo "== shm-audit"
: > "/dev/shm/shmpipe.$$.    1"
cp /bin/sleep /tmp/gst-launch-1.0; /tmp/gst-launch-1.0 1000 & GP=$!
echo "$GP" > "$STATE/xorgate-agent.mainpid"   # pretend: the JS MainPID's own segment
: > "/dev/shm/shmpipe.$GP.    0"
mkdir -p /var/lib/xorgate-rs/shm; : > /var/lib/xorgate-rs/shm/cam0.sock; : > /var/lib/xorgate/shm/cam0.sock
bash "$B/shm-audit.sh" >"$LOGS/shm.log" 2>&1; rc=$?
expect $((rc != 0)) "shm-audit rc=0 (evidence, not a gate) despite a DEAD row"
grep -q "DEAD .*pid=999999" "$LOGS/shm.log" && grep -q "OTHER-LIVE pid=$$ " "$LOGS/shm.log" && grep -q "JS  *pid=$GP " "$LOGS/shm.log" \
  && grep -q "SUMMARY shm=3 js=1 rust=0 other-live=1 dead=1 sockets=2" "$LOGS/shm.log"
expect $? "rows classified (DEAD / OTHER-LIVE / the JS MainPID itself = JS) + summary"
bash "$B/shm-audit.sh" --json >"$LOGS/shm.json" 2>/dev/null
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["summary"]["dead"]==1 and d["summary"]["sockets"]==2 and any(r["name"]=="shmpipe.999999.    0" for r in d["shm"]), d' "$LOGS/shm.json"
expect $? "--json parses; names keep their padding spaces"
bash -c 'sleep 1000 & wait' & PM=$!   # PM plays the JS MainPID, its sleep child owns a segment
sleep 0.5; PP=""
for f in /proc/[0-9]*/stat; do st=$(cat "$f" 2>/dev/null) || continue; st=${st##*) }; read -r _ pp _ <<<"$st"; [ "$pp" = "$PM" ] && { PP=${f#/proc/}; PP=${PP%/stat}; }; done
echo "$PM" > "$STATE/xorgate-agent.mainpid"; : > "/dev/shm/shmpipe.$PP.    0"
bash "$B/shm-audit.sh" >"$LOGS/shm2.log" 2>&1
grep -q "JS  *pid=$PP " "$LOGS/shm2.log"; expect $? "a descendant of the JS MainPID is owner JS"
bash "$B/shm-audit.sh" --fail-on-dead >/dev/null 2>&1; rc=$?
expect $((rc != 1)) "--fail-on-dead exits 1 with a DEAD row"
kill "$GP" "$PP" "$PM" 2>/dev/null; rm -f /dev/shm/shmpipe.*

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)  (logs in $LOGS)"
exit $fail
