#!/usr/bin/env bash
# p8-modem-sampler.sh: the 1 Hz modem-input sampler of a Phase 8 bench window
# (plan H10, DA12-DA15, PROGRESS F2; WP0 §5: telemetry has NO rung key).
# Root, on the bench. It runs as the transient unit p8-modem-sampler, so a
# sleeping Mac loses nothing, and it keeps running through the restore.
#
#   p8-modem-sampler.sh start --window W [--label w1]   -> W/modem-<label>.tsv
#   p8-modem-sampler.sh stop
#   p8-modem-sampler.sh run --out FILE [--baseline W/baseline.env] [--ticks N]   (the loop itself)
#   p8-modem-sampler.sh summary FILE [--claim-epoch E]
#
# One TSV row per second, from /run/sim7600-lte/telemetry.json. BOOLEANS,
# COUNTS AND NAMES ONLY: never an APN, ICCID, number, IP or coordinate.
#   utc             sample time
#   bias            gnss_ant_bias 1/0 (na = key missing or null)
#   apn_same        sha256(cellular.apn_applied, or "<absent>") == the baseline's apn_sha: 1/0 (na = no baseline)
#                   NOTE (fix-check M3): this is the daemon's CACHE (APN_APPLIED), which only the
#                   daemon writes; "unchanged" is not a fresh read of the modem
#   cell            the telemetry cellular block present 1/0
#   status          connected|connecting|disconnected|dead_zone|other
#   rung            derived (no rung key exists): ok | retry (connect_fails>0) |
#                   radio-cycle (radio_cycles_since_ok>0) | dead-zone (status) | unknown
#   fails cycles resets24   recovery.connect_fails / radio_cycles_since_ok / resets_24h (counts)
#   reset_path      recovery.last_reset.path: ladder|deadzone|config-drift|none|other
#   reset_new       1 when last_reset differs from the FIRST sample's (a reset happened since)
#   fresh           1 when telemetry.json's timestamp is <= 90 s old
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/bench/p8-lib.sh
. "$HERE/p8-lib.sh"

cmd=${1:-}; shift || true
W=""; LABEL=""; OUT=""; BASE=""; TICKS=0; CLAIM=""; FILE=""
while [ $# -gt 0 ]; do
  case $1 in
    --window) W=$2; shift 2 ;;
    --label) LABEL=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --baseline) BASE=$2; shift 2 ;;
    --ticks) TICKS=$2; shift 2 ;;
    --claim-epoch) CLAIM=$2; shift 2 ;;
    -*) echo "unknown argument $1" >&2; exit 2 ;;
    *) FILE=$1; shift ;;
  esac
done

run() {
  [ -n "$OUT" ] || { echo "run needs --out" >&2; exit 2; }
  python3 -I - "$P8_TELEMETRY" "$OUT" "$BASE" "$TICKS" <<'PY'
import calendar, hashlib, json, os, signal, sys, time
tele, out, base, ticks = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4] or 0)
apn_sha = None
if base and os.path.exists(base):
    for line in open(base):
        if line.startswith("apn_sha="):
            apn_sha = line.strip().split("=", 1)[1] or None
STATUS = {"connected", "connecting", "disconnected", "dead_zone"}
PATHS = {"ladder", "deadzone", "config-drift"}
stop = False
def on_term(*_):
    global stop
    stop = True
signal.signal(signal.SIGTERM, on_term)
def count(v):
    return str(v) if isinstance(v, int) and not isinstance(v, bool) and 0 <= v < 10**6 else "na"
first_reset = "unset"
new = not os.path.exists(out)
fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
f = os.fdopen(fd, "a", buffering=1)
if new:
    f.write("utc\tbias\tapn_same\tcell\tstatus\trung\tfails\tcycles\tresets24\treset_path\treset_new\tfresh\n")
n = 0
while not stop:
    now = time.time()
    try:
        with open(tele) as fh:
            d = json.load(fh)
    except Exception:
        d = None
    if not isinstance(d, dict):
        row = ["na"] * 11
    else:
        b = d.get("gnss_ant_bias")
        bias = "1" if b == 1 else "0" if b == 0 else "na"
        cell = d.get("cellular")
        apn = cell.get("apn_applied") if isinstance(cell, dict) else None
        sha = hashlib.sha256((apn if isinstance(apn, str) and apn else "<absent>").encode()).hexdigest()
        same = "na" if apn_sha is None else ("1" if sha == apn_sha else "0")
        st = d.get("status")
        st = st if st in STATUS else "other"
        rec = d.get("recovery") if isinstance(d.get("recovery"), dict) else {}
        fails, cyc, res = rec.get("connect_fails"), rec.get("radio_cycles_since_ok"), rec.get("resets_24h")
        if st == "dead_zone":
            rung = "dead-zone"
        elif isinstance(cyc, int) and cyc > 0:
            rung = "radio-cycle"
        elif isinstance(fails, int) and fails > 0:
            rung = "retry"
        elif isinstance(fails, int) and isinstance(cyc, int):
            rung = "ok"
        else:
            rung = "unknown"
        lr = rec.get("last_reset")
        rp = "none" if lr is None else (lr.get("path") if isinstance(lr, dict) and lr.get("path") in PATHS else "other")
        key = json.dumps(lr, sort_keys=True)
        if first_reset == "unset":
            first_reset = key
        fresh = "0"
        try:
            ts = calendar.timegm(time.strptime(d.get("timestamp", ""), "%Y-%m-%dT%H:%M:%SZ"))
            fresh = "1" if abs(now - ts) <= 90 else "0"
        except Exception:
            pass
        row = [bias, same, "1" if isinstance(cell, dict) else "0", st, rung, count(fails), count(cyc), count(res),
               rp, "1" if key != first_reset else "0", fresh]
    f.write(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now)) + "\t" + "\t".join(row) + "\n")
    n += 1
    if ticks and n >= ticks:
        break
    time.sleep(max(0.0, 1.0 - (time.time() - now)))
PY
}

summary() {
  [ -f "$FILE" ] || { echo "summary: no file $FILE" >&2; exit 2; }
  python3 -I - "$FILE" "$CLAIM" <<'PY'
import calendar, sys, time
rows = [l.rstrip("\n").split("\t") for l in open(sys.argv[1]) if l.strip()][1:]
claim = int(sys.argv[2]) if sys.argv[2] else None
def ep(s):
    return calendar.timegm(time.strptime(s, "%Y-%m-%dT%H:%M:%SZ"))
off_runs, cur = [], None
for r in rows:
    if r[1] == "0":
        cur = [ep(r[0]), ep(r[0])] if cur is None else [cur[0], ep(r[0])]
    elif cur is not None:
        off_runs.append(cur); cur = None
if cur is not None:
    off_runs.append(cur)
longest = max([b - a + 1 for a, b in off_runs], default=0)
print("samples=%d bias_off_ticks=%d bias_off_runs=%d longest_off_s=%d final_bias=%s"
      % (len(rows), sum(1 for r in rows if r[1] == "0"), len(off_runs), longest, rows[-1][1] if rows else "na"))
if claim is not None:
    late = [a for a, b in off_runs if a - claim > 60 or a < claim]
    print("F2: off periods starting outside [claim, claim+60s]=%d; every off period <= 60 s: %s"
          % (len(late), "yes" if longest <= 60 else "NO"))
print("apn_same_all=%s apn_changed_ticks=%d na_ticks=%d" % (
    "yes" if rows and all(r[2] == "1" for r in rows) else "no", sum(1 for r in rows if r[2] == "0"), sum(1 for r in rows if r[2] == "na")))
print("cellular_block_present_ticks=%d resets_seen=%s rungs=%s stale_ticks=%d" % (
    sum(1 for r in rows if r[3] == "1"), "yes" if any(r[10] == "1" for r in rows) else "no",
    ",".join(sorted({r[5] for r in rows})), sum(1 for r in rows if r[11] == "0")))
ok = bool(rows) and rows[-1][1] == "1" and all(r[2] in ("1",) for r in rows) and longest <= 60
print("VERDICT %s (final bias 1, APN cache unchanged, no off period > 60 s)" % ("PASS" if ok else "FAIL"))
sys.exit(0 if ok else 1)
PY
}

case $cmd in
  start)
    p8_need_root; p8_check_window "$W"
    [ -n "$LABEL" ] || LABEL=$(p8_marker_get label || echo w)
    [[ $LABEL =~ ^[a-z0-9-]+$ ]] || { echo "bad label" >&2; exit 2; }
    [ "$(p8_active "$P8_SAMPLER_UNIT")" != active ] || { echo "sampler already running"; exit 0; }
    systemctl reset-failed "$P8_SAMPLER_UNIT" >/dev/null 2>&1 || true
    systemd-run --unit="$P8_SAMPLER_UNIT" --collect -p Nice=10 -p MemoryMax=64M \
      /bin/bash "${HERE#"$P8_ROOT"}/p8-modem-sampler.sh" run --out "${W#"$P8_ROOT"}/modem-$LABEL.tsv" --baseline "${W#"$P8_ROOT"}/baseline.env" >/dev/null
    echo "sampler: $P8_SAMPLER_UNIT -> ${W#"$P8_ROOT"}/modem-$LABEL.tsv" ;;
  stop) systemctl stop "$P8_SAMPLER_UNIT" >/dev/null 2>&1 || true; echo "sampler: stopped" ;;
  run) run ;;
  summary) summary ;;
  *) sed -n '2,30p' "$0"; exit 2 ;;
esac
