#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015,SC2012  # test assertions: `[ … ]; expect $?` and `a && pass || failc`; ls of fixture dirs
# The drive blackbox (scripts/bench/drive-blackbox.py + drive-blackbox-ctl.sh +
# scripts/bench/systemd/xorgate-drive-blackbox.service) against a FAKE root:
# /proc, /sys, /run and the agent's state.json live under $FAKE
# (DRIVE_BB_ROOT), and vcgencmd / systemctl / journalctl are fixture-driven
# fakes that log every exec. Covers: the line schema, NO position data even
# when state.json and the modem file carry it, CPU deltas across a recorder
# restart, the change markers (nrestarts, mainpid, throttled bits, boot_id,
# mqtt, js_active), the journal snapshot, missing-key degrade, rotation +
# caps + the torn-line repair, the min-free guard, forks per tick, read-only
# toward the fake agent tree, `summary`, the unit (systemd-analyze verify),
# and the ctl verbs (via the shared ssh/sudo/systemctl stubs). Run as root in
# a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/drive-blackbox.sh
set -uo pipefail
# shellcheck source=scripts/bench/tests/_stubs.sh
source "$(dirname "$0")/_stubs.sh"
PY=$B/drive-blackbox.py
CTL=$B/drive-blackbox-ctl.sh
UNIT_SRC=$B/systemd/xorgate-drive-blackbox.service
FAKE=$(mktemp -d); FB=$(mktemp -d); FB2=$(mktemp -d); CTLD=$(mktemp -d); W=$(mktemp -d)
export FAKE_CTL=$CTLD
BOOT1=11111111-2222-4333-8444-555555555555
BOOT2=99999999-8888-4777-8666-555555555555

# --- fakes -------------------------------------------------------------------
cat > "$FB/vcgencmd" <<'S'
#!/usr/bin/env bash
echo "vcgencmd $*" >> "$FAKE_CTL/exec.log"
case "${1:-}" in
  get_throttled) echo "throttled=$(cat "$FAKE_CTL/throttled")" ;;
  pmic_read_adc) cat "$FAKE_CTL/adc" ;;
  *) exit 1 ;;
esac
S
cat > "$FB/systemctl" <<'S'
#!/usr/bin/env bash
echo "systemctl $*" >> "$FAKE_CTL/exec.log"
[ "${1:-}" = show ] || exit 0
first=1
for a in "$@"; do
  case "$a" in
    *.service) [ $first = 1 ] || echo; first=0
      if [ -f "$FAKE_CTL/units/$a" ]; then cat "$FAKE_CTL/units/$a"
      else printf 'Id=%s\nActiveState=inactive\nSubState=dead\nMainPID=0\nNRestarts=0\n' "$a"; fi ;;
  esac
done
S
cat > "$FB/journalctl" <<'S'
#!/usr/bin/env bash
echo "journalctl $*" >> "$FAKE_CTL/exec.log"
case " $* " in *" -n 1 "*) cat "$FAKE_CTL/jcursor" ;; *) cat "$FAKE_CTL/jlines" ;; esac
S
chmod +x "$FB"/*
cp "$FB/systemctl" "$FB/journalctl" "$FB2/"   # FB2: no vcgencmd at all

unit_props() { # <unit> <active> <sub> <pid> <nrestarts>
  mkdir -p "$CTLD/units"
  printf 'Id=%s\nActiveState=%s\nSubState=%s\nMainPID=%s\nNRestarts=%s\n' "$@" > "$CTLD/units/$1"
}
mkproc() { # <pid> <comm> <utime> <stime> <rss pages>
  mkdir -p "$FAKE/proc/$1"
  echo "$1 ($2) S 1 $1 $1 0 -1 4194560 100 0 0 0 $3 $4 0 0 20 0 5 0 777 100000000 $5 0" > "$FAKE/proc/$1/stat"
  echo "0::/system.slice/xorgate-agent-rs.service" > "$FAKE/proc/$1/cgroup"
}
cgroup_procs() { printf '%s\n' "$@" > "$FAKE/sys/fs/cgroup/system.slice/xorgate-agent-rs.service/cgroup.procs"; }
set_uptime() { echo "$1 4000.00" > "$FAKE/proc/uptime"; }
state_json() { # <connection> : a full Rust state.json, WITH position data (must never leak)
  local now; now=$(($(date +%s) * 1000))
  cat > "$FAKE/var/lib/xorgate/state.json" <<J
{"v":1,"writtenBy":"xorgate-device-agent-rs@0.0.1","deviceId":"58b51737-0000-4000-8000-000000000002","mode":"real",
 "mediaEnabled":true,"rateHz":1,"endpoint":"broker","offline":false,"pid":4242,"startedAt":$((now - 600000)),
 "lastWriteAt":$((now - 1000)),"connection":"$1","connectionSince":$((now - 300000)),"seq":598,"published":599,
 "publishErrors":2,"lastPublishAt":$((now - 2000)),"lastError":"TLS: I/O: tls handshake eof",
 "latest":{"v":1,"deviceId":"58b51737","ts":$((now - 2000)),"seq":598,
   "metrics":{"imu":{"quat_r":{"value":0.70711},"accel_x":{"value":0.31337}},
     "gps":{"fix_quality":{"value":1},"satellites":{"value":9},"satellites_in_view":{"value":14},"hdop":{"value":0.9},
            "sky_state":{"value":0},"lat":{"value":10.123456,"unit":"deg"},"lon":{"value":124.987654,"unit":"deg"},
            "alt":{"value":12.5,"unit":"m"},"speed":{"value":33.3,"unit":"km/h"},"course":{"value":270.1,"unit":"deg"},"ttff_s":{"value":31}},
     "lte":{"quality":{"value":18},"rssi":{"value":-77,"unit":"dBm"},"gnss_health":{"value":0},"resets_24h":{"value":0},"modem_temp":{"value":41,"unit":"C"}},
     "system":{"cpu_usage":{"value":21.5,"unit":"%"},"rail_5v":{"value":5.08,"unit":"V"}}},
   "media":{"kvs":{"value":true},"recording":{"value":true},
     "streams":[{"streamKey":"cam0","channelName":"prod-cam0","state":"running","recording":true},
                {"streamKey":"cam1","channelName":"prod-cam1","state":"error","recording":false}]},
   "recording":{"enabled":true,"sessionId":"019f95d1-aaaa","backlogBytes":1234567,"backlogSegments":12,"droppedSegments":0,
     "uploading":true,"lastUploadAt":$((now - 30000)),"bootDefer":"off","powerShed":"cam1"}},
 "recentEvents":[{"ts":$((now - 5000)),"level":"info","msg":"gps fix at 10.123456,124.987654"}],
 "recording":{"enabled":true,"sessionId":"019f95d1-aaaa","backlogBytes":1234567,"backlogSegments":12,"droppedSegments":0,
   "uploading":true,"lastUploadAt":$((now - 30000)),"bootDefer":"off","powerShed":"cam1","liveBlock":{"reason":"power","detail":"rail low","since":1}}}
J
}
modem_json() {
  cat > "$FAKE/run/sim7600-lte/telemetry.json" <<J
{"timestamp":"$(date -u +%Y-%m-%dT%H:%M:%SZ)","csq":18,"rssi_dbm":-77,"status":"connected","reg":1,"iface":"usb0","ip":"10.64.1.2",
 "gnss":{"lat":10.55555,"lon":124.44444,"fix":1},"sim":{"present":1,"state":"ready","iccid":"8963012345678901234","msisdn":"+639171234567",
 "carrier":"Smart","act":"LTE","registration":"home"},"cellular":{"state":"up","apn_applied":"internet"},"uplink":{"primary":"usb0","modem_only":0}}
J
}
fixture() {
  rm -rf "${FAKE:?}"/* "$CTLD"/*
  mkdir -p "$FAKE/proc/sys/kernel/random" "$FAKE/proc/net" "$FAKE/sys/class/thermal/thermal_zone0" \
    "$FAKE/sys/class/net/usb0" "$FAKE/sys/class/net/wlan0" "$FAKE/sys/fs/cgroup/system.slice/xorgate-agent-rs.service" \
    "$FAKE/var/lib/xorgate/recordings/video/s1/cam0" "$FAKE/var/lib/xorgate/recordings/telemetry/s1" \
    "$FAKE/run/sim7600-lte" "$FAKE/run/systemd/timesync"
  echo "$BOOT1" > "$FAKE/proc/sys/kernel/random/boot_id"
  set_uptime 1000.00
  echo "0.52 0.48 0.40 2/300 12345" > "$FAKE/proc/loadavg"
  printf 'MemTotal:        4045000 kB\nMemFree:          100000 kB\nMemAvailable:    2500000 kB\n' > "$FAKE/proc/meminfo"
  printf 'Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n' > "$FAKE/proc/net/route"
  printf 'wlan0\t00000000\t0108A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n' >> "$FAKE/proc/net/route"
  printf 'usb0\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n' >> "$FAKE/proc/net/route"
  printf 'usb0\t0000400A\t00000000\t0001\t0\t0\t100\t00C0FFFF\t0\t0\t0\n' >> "$FAKE/proc/net/route"
  echo 55200 > "$FAKE/sys/class/thermal/thermal_zone0/temp"
  echo 0x1003 > "$FAKE/sys/class/net/usb0/flags"; echo unknown > "$FAKE/sys/class/net/usb0/operstate"
  echo 0x1003 > "$FAKE/sys/class/net/wlan0/flags"; echo up > "$FAKE/sys/class/net/wlan0/operstate"
  : > "$FAKE/run/systemd/timesync/synchronized"
  mkproc 4242 xorgate-rs 600 400 10000
  mkproc 4300 gst-launch-1.0 300 200 5000
  mkproc 4301 python3 50 50 3000
  printf 'XORGATE_HOME=/var/lib/xorgate\0LTE_TELEMETRY_FILE=/run/sim7600-lte/telemetry.json\0' > "$FAKE/proc/4242/environ"
  cgroup_procs 4242 4300 4301
  head -c 2048 /dev/zero > "$FAKE/var/lib/xorgate/recordings/video/s1/cam0/00001.mp4"
  head -c 512 /dev/zero > "$FAKE/var/lib/xorgate/recordings/telemetry/s1/00001.ndjson"
  state_json connected; modem_json
  echo 0x0 > "$CTLD/throttled"
  printf '     3V7_WL_SW_A current(0)=0.00390372A\n         VDD_CORE_A current(7)=1.23456000A\n         EXT5V_V volt(24)=5.08000000V\n' > "$CTLD/adc"
  unit_props xorgate-agent-rs.service active running 4242 0
  unit_props xorgate-agent.service inactive dead 0 0
  unit_props sim7600-lte.service active running 777 2
  local b=${BOOT1//-/}
  echo "{\"__CURSOR\":\"s=abc;i=10;b=$b;m=1;t=5f0;x=1\",\"__REALTIME_TIMESTAMP\":\"1762500000000000\",\"__MONOTONIC_TIMESTAMP\":\"1\",\"_BOOT_ID\":\"$b\"}" > "$CTLD/jcursor"
  { echo '{"PRIORITY":"4","__REALTIME_TIMESTAMP":"1762499999000000"}'
    echo '{"PRIORITY":"4","__REALTIME_TIMESTAMP":"1762499999500000"}'
    echo '{"PRIORITY":"3","__REALTIME_TIMESTAMP":"1762499999900000"}'
    echo '{"PRIORITY":"4","__REALTIME_TIMESTAMP":"1762500000500000"}'; } > "$CTLD/jlines"
  : > "$CTLD/exec.log"
}
tree_sum() { (cd "$FAKE" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1); }
RO_BAD=0; RO_RUNS=0
rec() { # <out dir> [args…] : one recorder run, also checking it changed NOTHING under $FAKE
  local out=$1 before rc; shift
  before=$(tree_sum)
  DRIVE_BB_ROOT=$FAKE PATH="${REC_PATH:-$FB:$PATH}" python3 -I "$PY" run --out-dir "$out" --interval 0.01 "$@"; rc=$?
  [ "$(tree_sum)" = "$before" ] || RO_BAD=$((RO_BAD + 1)); RO_RUNS=$((RO_RUNS + 1))
  return $rc
}
newest() { ls -1t "$1"/*.ndjson 2>/dev/null | head -n1; }
# check <label> <python body> : runs the body with L = the JSON lines of the
# newest file in $OUT (L[-1] = last), prints PASS/FAIL lines.
check() {
  local label=$1 body=$2 out
  out=$(OUTF="$(newest "$OUT")" python3 - <<PY 2>&1
import json, os
L = [json.loads(x) for x in open(os.environ["OUTF"]) if x.strip()]
last = L[-1]
def ck(c, m):
    print(("PASS: " if c else "FAIL: ") + "$label: " + m)
def evk(l, k):
    return [e for e in l.get("ev", []) if e.get("k") == k]
$body
PY
)
  echo "$out"
  grep -q '^PASS' <<<"$out" || failc "$label: the checker printed no PASS ($out)"
  ! grep -qv '^PASS' <<<"$out" || fail=1
}

# =============================================================================
echo "== schema (one line, every section)"
fixture; OUT=$W/a
rec "$OUT" --ticks 1; expect $? "recorder exits 0 after --ticks 1"
[ "$(ls "$OUT"/*.ndjson | wc -l)" = 1 ] && [ "$(wc -l < "$(newest "$OUT")")" = 1 ]; expect $? "exactly one file with one line"
case "$(basename "$(newest "$OUT")")" in [0-9]*T[0-9]*Z_11111111.ndjson) pass "file name <boot-start-utc>_<boot8>.ndjson ($(basename "$(newest "$OUT")"))" ;; *) failc "file name $(basename "$(newest "$OUT")")" ;; esac
check schema '
ck(last["v"] == 1 and isinstance(last["ts"], int) and last["ts"] > 1.7e12, "v=1, ts is UTC ms")
ck(last["boot"] == "'"$BOOT1"'" and last["up"] == 1000.0 and last["n"] == 1 and last["dt"] is None and last["ntp"] is True, "boot/up/n/dt/ntp")
p = last["pwr"]
ck(p["rail_v"] == 5.08 and p["core_a"] == 1.235 and p["thr"] == "0x0" and p["thr_now"] == [] and p["thr_since"] == [] and p["temp_c"] == 55.2, "pwr %r" % p)
ck(last["load"] == {"avg": [0.52, 0.48, 0.4], "mem_avail_kb": 2500000, "mem_total_kb": 4045000}, "load %r" % last["load"])
d = last["disk"]
ck(all(isinstance(d[k], int) for k in ("root_free_mb", "xorgate_free_mb", "out_free_mb")), "disk free MB ints %r" % d)
n = last["net"]
ck(n["usb0"] == {"present": True, "up": True, "oper": "unknown"} and n["wlan0"]["up"] and n["def"] == ["usb0", "wlan0"] and n["usb0_default"], "net %r" % n)
s = last["svc"]
ck(s["rs"] == {"active": "active", "sub": "running", "pid": 4242, "nrestarts": 0, "rss_kb": 40000, "ticks": 1000, "cpu": None}, "svc.rs %r" % s["rs"])
ck(s["js"] == {"active": "inactive", "sub": "dead", "pid": 0}, "svc.js inactive %r" % s["js"])
ck(s["lte"] == {"active": "active", "sub": "running", "pid": 777, "nrestarts": 2}, "svc.lte %r" % s["lte"])
ck([(k["comm"], k["pid"], k["rss_kb"], k["ticks"]) for k in last["kids"]] == [("gst-launch-1.0", 4300, 20000, 500), ("python3", 4301, 12000, 100)], "kids %r" % last["kids"])
a = last["ag"]
ck(a["src"] == "/var/lib/xorgate/state.json" and a["by"].startswith("xorgate-device-agent-rs@") and a["mqtt"] == "connected", "ag src/by/mqtt")
ck(0 < a["age_s"] < 5 and 299 < a["mqtt_for_s"] < 305 and a["pub"] == 599 and a["pub_err"] == 2 and 1 < a["pub_age_s"] < 6 and 1 < a["tel_age_s"] < 6, "ag ages/counters")
ck(a["gps"] == {"fix": 1, "sats": 9, "siv": 14, "hdop": 0.9, "sky": 0, "ttff_s": 31}, "ag.gps (no coordinates) %r" % a["gps"])
ck(a["lte"] == {"q": 18, "rssi": -77, "gnss_health": 0, "resets_24h": 0, "modem_temp": 41}, "ag.lte %r" % a["lte"])
ck(a["kvs"] is True and a["media_rec"] is True and a["cams"] == [{"key": "cam0", "state": "running", "rec": True}, {"key": "cam1", "state": "error", "rec": False}], "ag cams %r" % a.get("cams"))
r = a["rec"]
ck(r["en"] is True and r["backlog_b"] == 1234567 and r["backlog_n"] == 12 and r["dropped"] == 0 and r["uploading"] is True and 29 < r["upload_age_s"] < 35 and r["shed"] == "cam1" and r["defer"] == "off" and r["block"] == "power", "ag.rec %r" % r)
l = last["lte"]
ck({k: l[k] for k in ("csq", "reg", "status", "op", "act", "sim_reg", "sim_state", "cell", "uplink")} == {"csq": 18, "reg": 1, "status": "connected", "op": "Smart", "act": "LTE", "sim_reg": "home", "sim_state": "ready", "cell": "up", "uplink": "usb0"} and 0 <= l["age_s"] < 5, "lte (modem file) %r" % l)
ck(evk(last, "start") and evk(last, "start")[0].get("first") is True and len(last["ev"]) == 1, "ev = [start first] %r" % last.get("ev"))
ck(last["jr"] == {"cursor": "s=abc;i=10;b='"${BOOT1//-/}"';m=1;t=5f0;x=1", "warn": 2, "err": 1, "from": "boot"}, "jr on the first line, the entry after the cursor not counted %r" % last.get("jr"))
ck(last["recfs"] == {"video": {"n": 1, "b": 2048}, "telemetry": {"n": 1, "b": 512}}, "recfs %r" % last.get("recfs"))
'
grep -q -- 'journalctl -q -u xorgate-agent-rs.service -p warning -o json --output-fields=PRIORITY --no-pager -b' "$CTLD/exec.log"; expect $? "first journal count: this boot (-b), warning+ only, PRIORITY field only (no message text)"

echo "== privacy: no position data anywhere"
ALL=$(cat "$OUT"/*.ndjson)
for s in 10.123456 124.987654 10.55555 124.44444 12.5 33.3 270.1 '"lat"' '"lon"' '"alt"' '"speed"' '"course"' '"imu"' 0.70711 0.31337 \
         8963012345678901234 639171234567 10.64.1.2 prod-cam0 'gps fix at' 'tls handshake' 58b51737; do
  grep -qF -- "$s" <<<"$ALL" && failc "output carries '$s'" || pass "no '$s' in the output"
done

echo "== CPU deltas across a recorder restart (same boot)"
set_uptime 1005.00; mkproc 4242 xorgate-rs 650 450 10000; mkproc 4300 gst-launch-1.0 310 215 5000
printf 'XORGATE_HOME=/var/lib/xorgate\0LTE_TELEMETRY_FILE=/run/sim7600-lte/telemetry.json\0' > "$FAKE/proc/4242/environ"
rec "$OUT" --ticks 1; expect $? "second run exits 0"
[ "$(ls "$OUT"/*.ndjson | wc -l)" = 1 ]; expect $? "same boot: appended to the same file"
check cpu '
ck(len(L) == 2 and last["up"] == 1005.0, "2 lines")
ck(last["svc"]["rs"]["cpu"] == 20.0, "agent cpu = 100 ticks / 5 s = 20.0 %% (got %r)" % last["svc"]["rs"]["cpu"])
ck([k["cpu"] for k in last["kids"]] == [5.0, 0.0], "kids cpu %r" % [k["cpu"] for k in last["kids"]])
ck([e["k"] for e in last["ev"]] == ["start"] and "first" not in last["ev"][0], "only a (non-first) start event %r" % last["ev"])
ck(last["jr"]["from"] == "cursor", "journal window from the saved cursor %r" % last["jr"])
'
grep -q -- "--after-cursor=s=abc;i=10;b=${BOOT1//-/};m=1;t=5f0;x=1" "$CTLD/exec.log"; expect $? "second journal count uses --after-cursor=<saved cursor>"
[ -f "$OUT/.journal-cursor" ]; expect $? "journal cursor saved in the recorder's own dir"

echo "== change markers"
set_uptime 1010.00
unit_props xorgate-agent-rs.service active running 4243 1
unit_props xorgate-agent.service active running 5000 0
unit_props sim7600-lte.service active running 778 3
mkproc 4243 xorgate-rs 10 5 9000; cgroup_procs 4243 4300 4301
printf 'XORGATE_HOME=/var/lib/xorgate\0' > "$FAKE/proc/4243/environ"
echo 0x50005 > "$CTLD/throttled"; state_json reconnecting
rec "$OUT" --ticks 1; expect $? "third run exits 0"
check markers '
ev = {(e["k"], e.get("u")): e for e in last["ev"]}
ck(ev.get(("nrestarts", "rs")) == {"k": "nrestarts", "u": "rs", "from": 0, "to": 1}, "rs nrestarts 0 -> 1")
ck(ev.get(("mainpid", "rs")) == {"k": "mainpid", "u": "rs", "from": 4242, "to": 4243}, "rs mainpid 4242 -> 4243")
ck(ev.get(("nrestarts", "lte")) == {"k": "nrestarts", "u": "lte", "from": 2, "to": 3} and ev.get(("mainpid", "lte"))["to"] == 778, "lte nrestarts + mainpid")
ck(ev.get(("js_active", None)) == {"k": "js_active", "from": "inactive", "to": "active"}, "js_active inactive -> active")
t = ev.get(("throttled", None))
ck(t == {"k": "throttled", "from": "0x0", "to": "0x50005", "set": ["uv", "thr", "uv_since", "thr_since"], "clr": []}, "throttled set bits %r" % t)
ck(last["pwr"]["thr_now"] == ["uv", "thr"] and last["pwr"]["thr_since"] == ["uv", "thr"], "decoded now/since %r" % last["pwr"])
ck(ev.get(("mqtt", None)) == {"k": "mqtt", "from": "connected", "to": "reconnecting"}, "mqtt connected -> reconnecting")
ck(last["svc"]["rs"]["cpu"] is None, "new MainPID: no cpu delta against the old pid")
'
echo 0x50000 > "$CTLD/throttled"; set_uptime 1015.00
rec "$OUT" --ticks 1
check thr-clear '
t = [e for e in last["ev"] if e["k"] == "throttled"]
ck(t and t[0]["set"] == [] and t[0]["clr"] == ["uv", "thr"], "throttled now-bits clear, since-bits stay %r" % t)
ck(not [e for e in last["ev"] if e["k"] in ("nrestarts", "mainpid", "mqtt", "js_active")], "no repeat markers %r" % last["ev"])
'
echo "$BOOT2" > "$FAKE/proc/sys/kernel/random/boot_id"; set_uptime 30.00
rec "$OUT" --ticks 1
[ "$(ls "$OUT"/*.ndjson | wc -l)" = 2 ] && ls "$OUT" | grep -q '_99999999\.ndjson$'; expect $? "new boot: a new file _99999999.ndjson"
check boot '
b = evk(last, "boot")
ck(b and b[0]["prev_boot"] == "'"$BOOT1"'" and b[0]["prev_up"] == 1015.0 and isinstance(b[0]["prev_ts"], int), "boot marker with the previous boot %r" % b)
ck(sorted(e["k"] for e in last["ev"]) == ["boot", "start"], "no stale-state markers across a boot %r" % last["ev"])
ck(last["jr"]["from"] == "boot", "journal: the saved cursor is from another boot -> -b")
'

echo "== missing keys degrade"
fixture; OUT=$W/b
echo '{"v":1,"connection":"connected"}' > "$FAKE/var/lib/xorgate/state.json"
rm -f "$FAKE/run/sim7600-lte/telemetry.json" "$FAKE/proc/4242/environ" "$FAKE/run/systemd/timesync/synchronized"
rm -rf "$FAKE/sys/class/net/usb0" "$FAKE/sys/class/thermal"
REC_PATH="$FB2:/usr/bin:/bin" rec "$OUT" --ticks 1; expect $? "exit 0 with no vcgencmd, no modem file, minimal state.json"
check degrade '
ck(last["pwr"] == {"rail_v": None, "core_a": None, "thr": None, "temp_c": None}, "pwr all null %r" % last["pwr"])
a = last["ag"]
ck(a["mqtt"] == "connected" and a["gps"] is None and "err" not in a and a["src"] == "/var/lib/xorgate/state.json", "ag: mqtt kept, gps null, default home %r" % a)
ck(not any(k in a for k in ("rec", "cams", "lte", "pub")), "absent keys stay absent %r" % a)
ck(last["lte"] == {"err": "missing"}, "lte err missing")
ck(last["net"]["usb0"] == {"present": False, "up": False} and last["ntp"] is False, "usb0 absent, ntp false")
ck(last["svc"]["rs"]["pid"] == 4242, "svc still read")
'
! grep -q vcgencmd "$CTLD/exec.log"; expect $? "no vcgencmd exec when it is not on PATH"
echo '{not json' > "$FAKE/var/lib/xorgate/state.json"
rec "$OUT" --ticks 1
check unparseable 'ck(last["ag"] == {"src": "/var/lib/xorgate/state.json", "err": "unparseable"}, "ag err unparseable %r" % last["ag"])'
rm -f "$FAKE/var/lib/xorgate/state.json"; unit_props xorgate-agent-rs.service inactive dead 0 1
rec "$OUT" --ticks 1
check agent-down '
ck(last["ag"]["err"] == "missing" and last["svc"]["rs"]["pid"] == 0 and "rss_kb" not in last["svc"]["rs"], "agent down: state missing, no crash %r" % last["svc"]["rs"])
ck([k["pid"] for k in last["kids"]] == [4242, 4300, 4301], "agent down: the default unit cgroup path is still read %r" % last["kids"])
ck([e for e in last["ev"] if e["k"] == "nrestarts"], "nrestarts 0 -> 1 marked")
'

echo "== forks per tick + the journal every 12 ticks"
fixture; OUT=$W/c
rec "$OUT" --ticks 24; expect $? "24 ticks"
[ "$(wc -l < "$(newest "$OUT")")" = 24 ]; expect $? "24 lines"
v=$(grep -c '^vcgencmd' "$CTLD/exec.log"); s=$(grep -c '^systemctl' "$CTLD/exec.log"); j=$(grep -c '^journalctl' "$CTLD/exec.log")
[ "$v" = 48 ] && [ "$s" = 24 ] && [ "$j" = 4 ]; expect $? "execs: vcgencmd $v (want 48), systemctl $s (24), journalctl $j (4 = 2 snapshots)"
check ticks '
ck([l["n"] for l in L if "jr" in l] == [1, 13], "jr on lines 1 and 13")
ck(all(l["dt"] is not None and l["dt"] < 1 for l in L[1:]), "dt set after the first tick")
ck(L[12]["jr"]["warn"] == 2 and L[12]["jr"]["from"] == "cursor", "second snapshot counts from the cursor")
'
SAMPLE=$(head -n 3 "$(newest "$OUT")")

echo "== rotation + caps"
fixture; OUT=$W/d; mkdir -p "$OUT"
for o in 20261101T000000Z_aaaaaaaa 20261102T000000Z_bbbbbbbb; do head -c 3000 /dev/zero | tr '\0' 'x' > "$OUT/$o.ndjson"; done
touch -d '2 days ago' "$OUT/20261101T000000Z_aaaaaaaa.ndjson"; touch -d '1 day ago' "$OUT/20261102T000000Z_bbbbbbbb.ndjson"
echo keep > "$OUT/notes.txt"
rec "$OUT" --ticks 40 --max-file-mb 0.004 --keep 3 --total-mb 0.012; expect $? "40 ticks with tiny caps"
nf=$(ls "$OUT"/*.ndjson | wc -l); tot=$(cat "$OUT"/*.ndjson | wc -c)
[ "$nf" -le 3 ]; expect $? "file count $nf <= --keep 3"
[ "$tot" -le $((12582 + 4194 + 4096)) ]; expect $? "total $tot bytes <= total cap + one file + one line"
[ ! -e "$OUT/20261101T000000Z_aaaaaaaa.ndjson" ] && [ ! -e "$OUT/20261102T000000Z_bbbbbbbb.ndjson" ]; expect $? "older boots' files pruned first"
[ -f "$OUT/notes.txt" ] && [ -f "$OUT/.journal-cursor" ]; expect $? "non-.ndjson files untouched"
hi=$(ls "$OUT" | sed -n 's/.*_11111111\.\([0-9]*\)\.ndjson$/\1/p' | sort -n | tail -n1)
[ "${hi:-0}" -ge 5 ]; expect $? "rotated through parts (highest .$hi)"
python3 -c 'import json,sys; [json.loads(l) for f in sys.argv[1:] for l in open(f) if l.strip()]' "$OUT"/*.ndjson; expect $? "every kept line parses"
n=$(cat "$OUT"/*.ndjson | wc -l); last_n=$(tail -n1 "$(newest "$OUT")" | python3 -c 'import json,sys; print(json.load(sys.stdin)["n"])')
[ "$last_n" = 40 ] && [ "$n" -lt 40 ]; expect $? "the newest line is n=40, older lines were dropped ($n kept)"
echo "== torn last line (power cut mid-write) + the min-free guard"
f=$(newest "$OUT"); printf '{"ts":17625' >> "$f"
rec "$OUT" --ticks 1 --max-file-mb 50 --keep 3 --total-mb 300
python3 - "$f" <<'PY'; expect $? "torn line closed by a newline: one malformed line, the new line parses"
import json, sys
bad = good = 0
for l in open(sys.argv[1]):
    try: json.loads(l); good += 1
    except ValueError: bad += 1
last = json.loads(open(sys.argv[1]).read().splitlines()[-1])
sys.exit(0 if bad == 1 and last["n"] == 1 else 1)
PY
python3 -I "$PY" summary "$f" | grep -q ', 1 malformed$'; expect $? "summary counts the torn line as malformed"
fixture; OUT=$W/e
rec "$OUT" --ticks 2 --min-free-mb 999999999; expect $? "exit 0 below min free"
[ -z "$(ls "$OUT"/*.ndjson 2>/dev/null)" ]; expect $? "below --min-free-mb nothing is written"

echo "== summary"
SF=$W/drive.ndjson
python3 - "$SF" "$BOOT1" "$BOOT2" <<'PY'
import json, sys
out, b1, b2 = sys.argv[1:]
T = 1762500000000
def line(ts, boot, up, rail, thr, mqtt, fix, rs, kids, ev=None, jr=None, backlog=0):
    l = {"v": 1, "ts": T + ts * 1000, "boot": boot, "up": up, "pwr": {"rail_v": rail, "core_a": 1.0, "thr": thr,
         "thr_now": ["uv", "thr"] if thr == "0x50005" else [], "temp_c": 60.0},
         "svc": {"rs": rs}, "kids": kids, "ag": {"mqtt": mqtt, "gps": None if fix is None else {"fix": fix, "sats": 7},
         "rec": {"backlog_b": backlog}}}
    if ev: l["ev"] = ev
    if jr: l["jr"] = jr
    return json.dumps(l)
rs = lambda pid, rss, cpu: {"active": "active", "pid": pid, "nrestarts": 0, "rss_kb": rss, "cpu": cpu}
g = lambda rss, cpu: [{"comm": "gst-launch-1.0", "pid": 50, "rss_kb": rss, "cpu": cpu}]
L = [line(0, b1, 100, 5.10, "0x0", "connected", 1, rs(10, 60000, 5.0), g(30000, 12.5), [{"k": "start", "first": True}]),
     line(5, b1, 105, 4.71, "0x50005", "reconnecting", 0, rs(10, 61000, 6.0), g(31000, 12.0),
          [{"k": "throttled", "from": "0x0", "to": "0x50005", "set": ["uv", "thr", "uv_since", "thr_since"], "clr": []},
           {"k": "mqtt", "from": "connected", "to": "reconnecting"}]),
     line(10, b1, 110, 5.12, "0x50000", "reconnecting", 0, rs(11, 62000, None), g(32000, 11.0),
          [{"k": "nrestarts", "u": "rs", "from": 0, "to": 1}, {"k": "mainpid", "u": "rs", "from": 10, "to": 11}], backlog=500),
     line(50, b1, 150, 5.05, "0x50000", "connected", 1, rs(11, 80000, 15.0), g(45000, 30.0), jr={"cursor": "c", "warn": 2, "err": 1, "from": "cursor"}, backlog=9000),
     line(200, b2, 20, 5.07, "0x0", "connecting", None, rs(12, 50000, None), [], [{"k": "start"}, {"k": "boot", "prev_boot": b1, "prev_ts": T + 50000}]),
     line(205, b2, 25, 5.06, "0x0", "connected", 1, rs(12, 51000, 3.0), [], backlog=100)]
open(out, "w").write("\n".join(L) + "\n")
PY
SUM=$(python3 -I "$PY" summary "$SF"); rc=$?
echo "$SUM" | sed 's/^/    | /'
expect "$rc" "summary exits 0"
for want in '6 line(s) in 1 file(s), 0 malformed' 'boots: 2' 'rail_v min 4.71' 'max 5.12' 'under-voltage NOW in 1 line(s)' \
            'throttled transitions: 1' 'set=uv,thr,uv_since,thr_since' 'rs nrestarts 0 -> 1' 'rs mainpid 10 -> 11' 'new boot (previous boot 11111111' \
            'gaps > 15 s or boot changes: 2' '40 s  same boot' 'BOOT CHANGE' 'mqtt disconnect windows: 2' 'gps fix-loss windows: 2' \
            'journal (xorgate-agent-rs): 2 warning(s), 1 error(s) over 1 snapshot(s)' 'upload backlog: max 9000 bytes, last 100 bytes'; do
  grep -qF -- "$want" <<<"$SUM" && pass "summary: $want" || failc "summary lacks: $want"
done
grep -qE '^  gst-launch-1\.0 +peak rss 45000 kB  peak cpu 30\.0 %  \(4 sample' <<<"$SUM"; expect $? "summary: gst-launch-1.0 peak rss 45000 kB, cpu 30.0 %"
grep -qE '^  xorgate-agent-rs \(main\) +peak rss 80000 kB  peak cpu 15\.0 %  \(6 sample' <<<"$SUM"; expect $? "summary: agent peak rss 80000 kB, cpu 15.0 %"
SUM2=$(python3 -I "$PY" summary "$W/a"); expect $? "summary of a directory (the schema/markers run)"
for s in 10.123456 124.987654 '"lat"' lat= lon=; do grep -qF -- "$s" <<<"$SUM$SUM2" && failc "summary carries $s" || pass "summary has no $s"; done
grep -q 'boots: 2' <<<"$SUM2" && grep -q 'throttled transitions: 2' <<<"$SUM2"; expect $? "directory summary: both boots, both throttled transitions"

echo "== read-only toward the agent"
[ "$RO_BAD" = 0 ]; expect $? "the fake agent tree is byte-identical after each of $RO_RUNS recorder runs"
for pat in 'ttyUSB' 'AT+' 'os.kill' 'signal.SIGKILL' 'import socket' 'urllib' 'http' 'serial'; do
  ! grep -qF -- "$pat" "$PY"; expect $? "recorder source has no '$pat'"
done
grep -qE '^ExecStart=/usr/bin/python3 -I /usr/local/bin/xorgate-drive-blackbox run' "$UNIT_SRC" && grep -qx 'ProtectSystem=strict' "$UNIT_SRC" \
  && grep -qx 'ReadWritePaths=-/var/lib/xorgate-rs/drive-blackbox' "$UNIT_SRC"; expect $? "unit: read-only filesystem except its own data dir"

echo "== unit"
for k in 'Restart=always' 'Nice=10' 'IOSchedulingClass=idle' 'After=xorgate-agent-rs.service' 'WantedBy=multi-user.target' 'StartLimitIntervalSec=0'; do
  grep -qx -- "$k" "$UNIT_SRC"; expect $? "unit has $k"
done
awk '/^\[/{s=$0} /^StartLimit/{if (s!="[Unit]") exit 1}' "$UNIT_SRC"; expect $? "StartLimit* in [Unit] (O-P5G-1)"
command -v systemd-analyze >/dev/null || { apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq systemd >/dev/null 2>&1; }
if command -v systemd-analyze >/dev/null; then
  d=$(mktemp -d); cp "$UNIT_SRC" "$d/"
  o=$(SYSTEMD_UNIT_PATH="$d:" systemd-analyze verify "$d/xorgate-drive-blackbox.service" 2>&1)
  [ -z "$o" ]; expect $? "systemd-analyze verify prints nothing (${o:-ok})"
else failc "systemd-analyze unavailable"; fi
if command -v pyflakes3 >/dev/null || { apt-get install -y -qq python3-pyflakes >/dev/null 2>&1 && command -v pyflakes3 >/dev/null; }; then
  pyflakes3 "$PY"; expect $? "pyflakes3 drive-blackbox.py"
else echo "NOTE: pyflakes3 not installable — skipped"; fi

echo "== ctl (ssh/sudo/systemctl stubs)"
reset_units; rm -rf /var/lib/xorgate-rs /usr/local/bin/xorgate-drive-blackbox /etc/systemd/system/xorgate-drive-blackbox.service
install -d -o pi -g pi -m 0700 /var/lib/xorgate-rs; mkdir -p /usr/local/bin /etc/systemd/system
printf '[Service]\nExecStart=/usr/local/bin/xorgate-blackbox.sh\n' > /etc/systemd/system/xorgate-blackbox.service
printf '#!/bin/bash\n# old blackbox\n' > /usr/local/bin/xorgate-blackbox.sh
echo active > "$STATE/xorgate-blackbox.service"
BB_WAIT_S=0 bash "$CTL" install --dry-run > "$LOGS/dry" 2>&1; expect $? "install --dry-run exits 0"
[ ! -e /usr/local/bin/xorgate-drive-blackbox ] && [ ! -e /var/lib/xorgate-rs/drive-blackbox ] && [ "$(cat "$STATE/xorgate-blackbox.service")" = active ]; expect $? "dry run writes nothing, old unit untouched"
BB_WAIT_S=0 bash "$CTL" install > "$LOGS/inst" 2>&1; rc=$?; expect "$rc" "install exits 0"; [ "$rc" = 0 ] || cat "$LOGS/inst"
cmp -s "$PY" /usr/local/bin/xorgate-drive-blackbox && [ "$(stat -c %a /usr/local/bin/xorgate-drive-blackbox)" = 755 ]; expect $? "recorder installed 0755, byte-identical"
cmp -s "$UNIT_SRC" /etc/systemd/system/xorgate-drive-blackbox.service; expect $? "unit installed byte-identical"
[ "$(stat -c '%U %a' /var/lib/xorgate-rs/drive-blackbox)" = "root 755" ] && [ "$(stat -c %U /var/lib/xorgate-rs)" = pi ]; expect $? "data dir root 0755; /var/lib/xorgate-rs still pi"
[ "$(cat "$STATE/xorgate-blackbox.service")" = inactive ] && [ "$(cat "$STATE/xorgate-blackbox.service.enabled")" = disabled ]; expect $? "old xorgate-blackbox stopped + disabled"
[ -f /etc/systemd/system/xorgate-blackbox.service ] && [ -f /usr/local/bin/xorgate-blackbox.sh ]; expect $? "old unit file + script kept"
[ "$(cat "$STATE/xorgate-drive-blackbox.service")" = active ] && [ "$(cat "$STATE/xorgate-drive-blackbox.service.enabled")" = enabled ]; expect $? "new unit enabled + active"
grep -q 'systemctl daemon-reload' "$STATE/systemctl.log" && grep -q 'systemctl enable --now xorgate-drive-blackbox.service' "$STATE/systemctl.log"; expect $? "daemon-reload + enable --now"
for u in xorgate-agent xorgate-agent-rs sim7600-lte; do [ "$(cat "$STATE/$u")" = active ] || failc "ctl changed $u"; done
! grep -qE 'systemctl (start|stop|restart|enable|disable)( --now)? (xorgate-agent|xorgate-agent-rs|sim7600-lte)(\.service)?$' "$STATE/systemctl.log"; expect $? "ctl never starts/stops/enables the agents or the modem unit"
: > "$STATE/systemctl.log"
BB_WAIT_S=0 bash "$CTL" install > "$LOGS/inst2" 2>&1; expect $? "second install exits 0"
grep -q 'already enabled + active' "$LOGS/inst2" && grep -q 'already stopped + disabled' "$LOGS/inst2" && ! grep -q 'enable --now\|restart' "$STATE/systemctl.log"; expect $? "second install is a no-op"
bash "$CTL" status > "$LOGS/status" 2>&1; expect $? "status exits 0"
grep -q 'xorgate-drive-blackbox.service *active / enabled' "$LOGS/status" && grep -q 'retired' "$LOGS/status"; expect $? "status shows both units"
cp "$SF" /var/lib/xorgate-rs/drive-blackbox/
bash "$CTL" summary all > "$LOGS/sum" 2>&1; expect $? "summary all (on the bench) exits 0"
grep -q 'boots: 2' "$LOGS/sum"; expect $? "bench summary over the data dir"
bash "$CTL" summary "$SF" | grep -q 'rail_v min 4.71'; expect $? "summary of a local file runs locally"
bash "$CTL" summary /nonexistent/x >/dev/null 2>&1; [ $? != 0 ]; expect $? "summary of a missing path fails"
bash "$CTL" uninstall > "$LOGS/un" 2>&1; expect $? "uninstall exits 0"
[ ! -e /etc/systemd/system/xorgate-drive-blackbox.service ] && [ ! -e /usr/local/bin/xorgate-drive-blackbox ]; expect $? "unit + recorder removed"
[ -f "/var/lib/xorgate-rs/drive-blackbox/$(basename "$SF")" ]; expect $? "data kept"
[ "$(cat "$STATE/xorgate-blackbox.service")" = inactive ] && [ "$(cat "$STATE/xorgate-blackbox.service.enabled")" = disabled ] && ! grep -q 'enable.*xorgate-blackbox.service' "$STATE/systemctl.log"; expect $? "old unit NOT re-enabled"
[ "$(cat "$STATE/xorgate-drive-blackbox.service")" = inactive ]; expect $? "new unit stopped"

echo "== sample lines (fixture run)"
echo "$SAMPLE"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
