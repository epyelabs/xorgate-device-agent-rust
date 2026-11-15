#!/usr/bin/env bash
# p8-align.sh: the Mac-side H10 align poller of a Phase 8 DEV window (plan
# §7.3.1, Q-8.21, F2; review-wp-t asked item 5; WP0 C1). Start it BEFORE the
# install. DEV ONLY: it refuses the PROD API host and the PROD Cognito pool.
#
#   p8-align.sh --org <DEV org id> --workspace <DEV ws id> --align <align.json>
#               [--api https://h357muxy3k.execute-api.us-east-1.amazonaws.com]
#               [--host pi@<bench> --window /root/p8-<ts> [--label w<n>]]
#               [--poll 2] [--timeout 1800] [--f2-wait 90] [--out <result.env>]
#
#  1. snapshot the workspace's device ids, then poll every --poll s for a NEW
#     device (the claim);
#  2. STOP (exit 1, no write) if that device's config carries `cellular` (H10);
#  3. PATCH ONLY the align.json keys (WP0 C1: {gnssAntBias}) on that device,
#     and check the response; print the claim -> PATCH delay (F2);
#  4. with --host/--window: poll the bench's `p8-verify.sh dev-settings` until it
#     PASSes (exit 1 = a cellular STOP), then after --f2-wait s print the
#     sampler summary with --claim-epoch (the bounded bias-off period, F2).
# Auth: XG_TOKEN (a DEV id token), or XG_USER + XG_PASS (Cognito SRP against
# the DEV pool, through amazon-cognito-identity-js from P8_COGNITO_NODE_PATH,
# default frontend/xorgate-web/node_modules). Never printed. Nothing else is
# written: only this one PATCH, on the new device only.
# Exit: 0 aligned (+ bench PASS when asked); 1 STOP/failure; 2 usage/refused.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
P8_SSH=${P8_SSH:-ssh}
P8_RSUDO=${P8_RSUDO:-sudo -n}
# DEV identity (WP-C harness, xorgate-web .env DEV block). PROD is refused below.
DEV_POOL=us-east-1_E6dkPNlZl
DEV_CLIENT=7v9p61d6286ap4gbfj0psfu9h6
PROD_POOL=us-east-1_RPLv99S54
PROD_HOST=api.xorgate.io
P8_DEV_API_HOSTS=${P8_DEV_API_HOSTS:-h357muxy3k.execute-api.us-east-1.amazonaws.com api-dev.xorgate.io}
P8_COGNITO_NODE_PATH=${P8_COGNITO_NODE_PATH:-$(cd "$HERE/../../../.." 2>/dev/null && pwd)/frontend/xorgate-web/node_modules}

API=https://h357muxy3k.execute-api.us-east-1.amazonaws.com; ORG=""; WS=""; ALIGN=""; HOST=""; W=""; LABEL=""
POLL=2; TIMEOUT=1800; F2WAIT=90; OUT=""
while [ $# -gt 0 ]; do
  case $1 in
    --api) API=$2; shift 2 ;;
    --org) ORG=$2; shift 2 ;;
    --workspace) WS=$2; shift 2 ;;
    --align) ALIGN=$2; shift 2 ;;
    --host) HOST=$2; shift 2 ;;
    --window) W=$2; shift 2 ;;
    --label) LABEL=$2; shift 2 ;;
    --poll) POLL=$2; shift 2 ;;
    --timeout) TIMEOUT=$2; shift 2 ;;
    --f2-wait) F2WAIT=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
say() { printf '[align %s] %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
{ [ -n "$ORG" ] && [ -n "$WS" ] && [ -f "$ALIGN" ]; } || { sed -n '2,24p' "$0"; exit 2; }

# ---- DEV-only guards (before any request)
host=$(printf '%s' "$API" | sed -E 's#^[a-z]+://##; s#/.*$##')
case $host in "$PROD_HOST" | "$PROD_HOST":*) say "REFUSED: $API is the PROD API host"; exit 2 ;; esac
case " $P8_DEV_API_HOSTS " in *" $host "*) ;; *) say "REFUSED: $host is not a DEV API host ($P8_DEV_API_HOSTS)"; exit 2 ;; esac
[ "${P8_POOL:-$DEV_POOL}" != "$PROD_POOL" ] || { say "REFUSED: the PROD Cognito pool"; exit 2; }
python3 -I - "$ALIGN" <<'PY' || exit 2
import json, sys
d = json.load(open(sys.argv[1]))
if not isinstance(d, dict) or not d or set(d) - {"gnssAntBias"}:
    sys.exit("align.json may carry only gnssAntBias (WP0 C1): got %s" % ",".join(sorted(d)) if isinstance(d, dict) else "not an object")
if not isinstance(d["gnssAntBias"], bool):
    sys.exit("gnssAntBias must be a boolean")
PY

# ---- token
if [ -z "${XG_TOKEN:-}" ]; then
  { [ -n "${XG_USER:-}" ] && [ -n "${XG_PASS:-}" ]; } || { say "set XG_TOKEN, or XG_USER + XG_PASS (the DEV test user)"; exit 2; }
  [ -d "$P8_COGNITO_NODE_PATH/amazon-cognito-identity-js" ] || { say "no amazon-cognito-identity-js under $P8_COGNITO_NODE_PATH"; exit 2; }
  XG_TOKEN=$(P8_POOL=${P8_POOL:-$DEV_POOL} P8_CLIENT=${P8_CLIENT:-$DEV_CLIENT} NODE_PATH=$P8_COGNITO_NODE_PATH node -e '
const { CognitoUserPool, CognitoUser, AuthenticationDetails } = require("amazon-cognito-identity-js");
const pool = new CognitoUserPool({ UserPoolId: process.env.P8_POOL, ClientId: process.env.P8_CLIENT });
const u = new CognitoUser({ Username: process.env.XG_USER, Pool: pool });
u.authenticateUser(new AuthenticationDetails({ Username: process.env.XG_USER, Password: process.env.XG_PASS }), {
  onSuccess: (s) => process.stdout.write(s.getIdToken().getJwtToken()),
  onFailure: (e) => { console.error("auth failed: " + (e.code || e.name)); process.exit(1); },
  newPasswordRequired: () => { console.error("auth failed: newPasswordRequired"); process.exit(1); },
});') || { say "DEV sign-in failed"; exit 1; }
  export XG_TOKEN
fi
export XG_TOKEN

# ---- poll, check, PATCH (python: the token stays in its environment)
res=$(mktemp)
trap 'rm -f "$res"' EXIT
rc=0
python3 -I - "$API" "$ORG" "$WS" "$ALIGN" "$POLL" "$TIMEOUT" > "$res" <<'PY' || rc=$?
import calendar, json, os, sys, time, urllib.error, urllib.request
api, org, ws, align_f, poll, timeout = sys.argv[1].rstrip("/"), sys.argv[2], sys.argv[3], sys.argv[4], float(sys.argv[5]), float(sys.argv[6])
align = json.load(open(align_f))
H = {"Authorization": "Bearer " + os.environ["XG_TOKEN"], "X-Organization-Id": org, "X-Workspace-Id": ws, "Content-Type": "application/json"}
def call(method, path, body=None):
    req = urllib.request.Request(api + "/v1" + path, method=method, headers=H, data=None if body is None else json.dumps(body).encode())
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read() or b"{}")
def ids():
    b = call("GET", "/devices")
    rows = next((b[k] for k in ("devices", "items", "data") if isinstance(b.get(k), list)), [])
    return {r["id"]: r for r in rows if isinstance(r, dict) and r.get("id")}
def log(m):
    print("[align %s] %s" % (time.strftime("%H:%M:%SZ", time.gmtime()), m), file=sys.stderr, flush=True)
def epoch(s):
    try:
        return calendar.timegm(time.strptime(s[:19], "%Y-%m-%dT%H:%M:%S"))
    except Exception:
        return None
before = ids()
log("watching workspace (%d existing devices); poll %.0fs, timeout %.0fs" % (len(before), poll, timeout))
t0, new = time.time(), None
while time.time() - t0 < timeout:
    time.sleep(poll)
    try:
        now = ids()
    except Exception as e:
        log("poll error (%s), retrying" % type(e).__name__)
        continue
    fresh = [i for i in now if i not in before]
    if fresh:
        new = fresh[0]
        if len(fresh) > 1:
            log("WARNING: %d new devices; aligning the first only" % len(fresh))
        break
if not new:
    log("no new device within the timeout"); sys.exit(1)
seen = time.time()
d = call("GET", "/devices/" + new)
d = d.get("device", d)
cfg = d.get("config") or {}
created = epoch(d.get("createdAt") or "")
log("new device %s (claim seen %.1fs after createdAt)" % (new, seen - created if created else -1))
if "cellular" in cfg:
    log("STOP (H10): the DEV document carries a cellular key: NOT patching"); print("stop=cellular"); sys.exit(1)
r = call("PATCH", "/devices/%s/config" % new, align)
r = r.get("device", r)
got = {k: (r.get("config") or {}).get(k) for k in align}
patched = time.time()
ok = got == align and "cellular" not in (r.get("config") or {})
print("device_id=%s" % new)
print("created_epoch=%s" % (created or ""))
print("patched_epoch=%d" % patched)
print("claim_to_patch_s=%s" % ("%.1f" % (patched - created) if created else "unknown"))
print("config_rev=%s" % r.get("configRev"))
print("aligned=%s" % ("yes" if ok else "NO"))
log("PATCH %s -> configRev %s, %s; claim->PATCH %s s" % (",".join(sorted(align)), r.get("configRev"), "aligned" if ok else "NOT ALIGNED",
    "%.1f" % (patched - created) if created else "?"))
sys.exit(0 if ok else 1)
PY
cat "$res"
if [ -n "$OUT" ]; then (umask 077; cp "$res" "$OUT"); fi
[ "$rc" = 0 ] || { say "NOT aligned (rc $rc)"; exit 1; }

# ---- the bench side (optional)
{ [ -n "$HOST" ] && [ -n "$W" ]; } || exit 0
rsh() { "$P8_SSH" -o BatchMode=yes -o ConnectTimeout=10 "$HOST" "$@"; }
created=$(sed -n 's/^created_epoch=//p' "$res")
for i in $(seq 60); do
  drc=0; dout=$(rsh "$P8_RSUDO bash $W/bin/p8-verify.sh dev-settings --window $W" 2>&1) || drc=$?
  case $drc in
    0) say "bench dev-settings PASS after ~$((i * 2)) s"; printf '%s\n' "$dout"; break ;;
    1) say "STOP (H10): the bench settings carry cellular"; printf '%s\n' "$dout"; exit 1 ;;
  esac
  [ "$i" = 60 ] && { say "bench dev-settings not PASS after 120 s"; printf '%s\n' "$dout"; exit 1; }
  sleep 2
done
[ -n "$LABEL" ] || LABEL=$(rsh "$P8_RSUDO sed -n 's/^label=//p' /root/p8-window.active" 2>/dev/null || true)
say "waiting ${F2WAIT}s for the daemon's 30 s re-read, then the F2 summary"
sleep "$F2WAIT"
rsh "$P8_RSUDO bash $W/bin/p8-modem-sampler.sh summary $W/modem-${LABEL:-w1}.tsv ${created:+--claim-epoch $created}" || { say "F2 summary NOT PASS"; exit 1; }
