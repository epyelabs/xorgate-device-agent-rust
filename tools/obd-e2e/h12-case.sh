#!/usr/bin/env bash
# H12 inside xorgate-obd-py:bookworm (see run-h12.sh): the Rust agent in REAL
# mode, hardware block with `peripherals.obd`, `XORGATE_OBD_SERIAL` → an
# ELM327-emulator pty (taos-2022), the VENDORED helper, a local TLS mosquitto.
# Prints PASS/FAIL lines; exit 1 on any FAIL.
set -uo pipefail
BIN=/in/xorgate-rs
W=/work
RUN_S=${RUN_S:-150}
fail=0
pass() { echo "PASS $*"; }
failc() { echo "FAIL $*"; fail=1; }
mkdir -p $W/home $W/certs $W/shims $W/tmp
cp -R /vendored $W/python
export TMPDIR=$W/tmp
DEV=00000000-0000-4000-8000-0000000000e2

# --- throwaway PKI + broker -------------------------------------------------
cd $W/certs
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 2 -keyout ca.key -out ca.pem \
  -subj "/CN=h12-ca" -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
issue() { # name cn eku [san]
  openssl req -newkey rsa:2048 -nodes -sha256 -keyout "$1.key" -out "$1.csr" -subj "/CN=$2" 2>/dev/null
  { echo "basicConstraints=critical,CA:FALSE"; echo "keyUsage=critical,digitalSignature,keyEncipherment"
    echo "extendedKeyUsage=$3"; [ -n "${4:-}" ] && echo "subjectAltName=$4"; } > "$1.ext"
  openssl x509 -req -in "$1.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -out "$1.crt" -days 2 -sha256 -extfile "$1.ext" 2>/dev/null
}
issue server localhost serverAuth "DNS:localhost"
issue device $DEV clientAuth
issue sub h12-sub clientAuth
cat > $W/mosquitto.conf <<C
per_listener_settings false
persistence false
log_dest file $W/mosquitto.log
listener 8883
cafile $W/certs/ca.pem
certfile $W/certs/server.crt
keyfile $W/certs/server.key
require_certificate true
use_identity_as_username true
allow_anonymous true
C
chmod 0644 $W/certs/*
mosquitto -c $W/mosquitto.conf -d
sleep 0.5
mosquitto_sub -h localhost -p 8883 --cafile $W/certs/ca.pem --cert $W/certs/sub.crt --key $W/certs/sub.key \
  -t "xorgate/devices/$DEV/#" -v -V mqttv5 > $W/mqtt.log 2>$W/sub.err &
SUB=$!

# --- emulator ----------------------------------------------------------------
python3 /e2e/emulator.py $W/python taos-2022 $W/pty &
EMU=$!
for _ in $(seq 1 100); do [ -s $W/pty ] && break; sleep 0.2; done
PTY=$(cat $W/pty 2>/dev/null)
[ -n "$PTY" ] && pass "emulator on $PTY" || { failc "emulator did not start"; exit 1; }

# --- the agent ---------------------------------------------------------------
printf '#!/bin/sh\necho yes\n' > $W/shims/timedatectl; chmod 0755 $W/shims/timedatectl
cat > $W/hardware.json <<J
{"v":2,"platform":"rpi-cm5","encoder":"hw-h264","rails":false,"statusLed":null,"imu":null,"video":[],
 "peripherals":{"obd":{"adapter":"elm327","transport":"bt-spp"}}}
J
env -i PATH="$W/shims:/usr/local/bin:/usr/bin:/bin" HOME=$W XORGATE_HOME=$W/home \
  DEVICE_ID=$DEV IOT_DATA_ENDPOINT=localhost MODE=real \
  CERT_PATH=$W/certs/device.crt KEY_PATH=$W/certs/device.key CA_PATH=$W/certs/ca.pem \
  HARDWARE_MANIFEST=$W/hardware.json XORGATE_PYTHON_DIR=$W/python XORGATE_OBD_SERIAL="$PTY" \
  GPS_NMEA_PORT=/nonexistent LTE_TELEMETRY_FILE=/nonexistent RUST_LOG=info \
  $BIN run > $W/agent.out 2>&1 &
AG=$!
sleep "$RUN_S"
STATUS_LINE=$(env -i PATH=/usr/bin:/bin XORGATE_HOME=$W/home $BIN status 2>&1 | grep '^  obd ' || true)
OBD_STATUS=$(env -i PATH=/usr/bin:/bin XORGATE_HOME=$W/home $BIN obd status 2>&1)
HELPER_PID=$(pgrep -f "$W/python/obd_read.py" | head -1)
kill -TERM $AG; wait $AG; AGENT_RC=$?
sleep 1
kill $SUB 2>/dev/null; kill -TERM $EMU 2>/dev/null; wait $EMU 2>/dev/null

echo "== agent rc=$AGENT_RC; journal lines [obd]:"
grep -a '\[obd\]' $W/agent.out | head -40
echo "== xorgate-rs status obd line: $STATUS_LINE"
echo "== xorgate-rs obd status:"; echo "$OBD_STATUS"

# --- checks ------------------------------------------------------------------
[ "$AGENT_RC" = 0 ] && pass "agent exit 0 on SIGTERM" || failc "agent rc $AGENT_RC"
TEL=$(grep -a "/telemetry " $W/mqtt.log | tail -1 | cut -d' ' -f2-)
echo "$TEL" | python3 -c '
import json,sys
p=json.loads(sys.stdin.read()); o=p["metrics"]["obd"]
print("last telemetry obd group:", json.dumps(o))
assert o["link"]=={"value":2}, o["link"]
assert o["rpm"]["unit"]=="rpm" and o["rpm"]["value"]>0, o.get("rpm")
assert o["coolant_temp"]["unit"]=="°C"
assert all(isinstance(l["value"],(int,float)) for l in o.values())
assert list(p["metrics"]).index("obd")==len(p["metrics"])-1, list(p["metrics"])
' && pass "telemetry obd group at link 2 with taos values" || failc "telemetry obd group"
REP=$(grep -a "/config/reported " $W/mqtt.log | tail -1 | cut -d' ' -f2-)
echo "$REP" | python3 -c '
import json,sys
raw=sys.stdin.read().strip(); r=json.loads(raw); e=r["effective"]["obd"]
print("report bytes", len(raw.encode()), "effective.obd bytes", len(json.dumps(e,separators=(",",":"),ensure_ascii=False).encode()))
print("effective.obd keys", list(e))
assert list(e)[:10]==["enabled","adapterMac","pin","pollHz","fields","powerSource","status","link","source","adapter"], list(e)
assert e["status"]=="live" and e["link"]==2 and e["source"]=="env", (e["status"],e["link"],e["source"])
assert e["adapter"]["serial"].startswith("/dev/pts/")
assert e["profile"]["protocol"]["dpn"]=="A6", e.get("profile")
assert "audit" in e and e["audit"]["counters"]["requests"]>0, e.get("audit")
assert e["helper_version"] and e["allowlist_sha256"]
assert len(raw.encode()) <= 32768
# No cloud `…/config` document here, so `status` has no namespaces at all
# (status.obd needs a desired doc — H13 checks it against the dev cloud).
print("status", json.dumps(r["status"]))
' && pass "config report carries effective.obd (live, link 2, profile, audit) under MAX_REPORT_BYTES" || failc "effective.obd in the report"
echo "$STATUS_LINE" | grep -qE '^  obd        live  link 2  [0-9.]+ req/s  err 0  forbidden 0  [0-9.—]+ V  adapter /dev/pts/' \
  && pass "xorgate-rs status obd line" || failc "status obd line: $STATUS_LINE"
echo "$OBD_STATUS" | head -1 | grep -q '^obd        live  link 2' && echo "$OBD_STATUS" | grep -q '^  profile    AUTO, ISO 15765-4 (CAN 11/500) · ECUs ' \
  && pass "xorgate-rs obd status" || failc "obd status"
[ -n "$HELPER_PID" ] && pass "helper ran as python3 obd_read.py (pid $HELPER_PID)" || failc "no helper process"
grep -aq 'Traceback' $W/agent.out && failc "a Python traceback in the journal" || pass "no Python traceback in the journal"
grep -aE '\[obd\] (helper exited|python:)' $W/agent.out && failc "helper exit/stderr lines" || pass "no helper exit or stderr lines (clean SIGINT stop)"
grep -aq '\[obd\] live, ECU answering' $W/agent.out && pass "[obd] live event" || failc "no live event"
grep -aqE '\[obd\] audit #1: req [0-9]+ ' $W/agent.out && pass "[obd] audit journal line" || failc "no audit line"
ls $W/home/obd/audit/ 2>/dev/null | grep -q . && pass "audit JSONL under \$XORGATE_HOME/obd/audit/: $(ls $W/home/obd/audit | head -3 | tr '\n' ' ')" || failc "no audit dir"
AUD=$(ls $W/home/obd/audit/* 2>/dev/null | head -1)
[ -n "$AUD" ] && python3 -c 'import json,sys; [json.loads(l) for l in open(sys.argv[1]) if l.strip()]' "$AUD" && pass "audit file is JSON lines" || failc "audit file not JSONL"
STOPPED=$(grep -a '"stop"\|stop' "$AUD" 2>/dev/null | tail -1)
echo "== last audit record: $(tail -1 "$AUD" 2>/dev/null)"
python3 - "$W/home/state.json" <<'P' && pass "state.json obd right after recording (TS insertion order)" || failc "state.json obd"
import json,sys
s=json.load(open(sys.argv[1])); k=list(s)
print("state.json obd:", json.dumps(s.get("obd")))
assert "obd" in s and k.index("obd")==k.index("recording")+1, k
P
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
