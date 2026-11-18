#!/usr/bin/env bash
# shellcheck disable=SC2015  # test assertions: `a && ok || bad`
# p8-cam0.sh / p8-devcam0.sh against a LOCAL fake console + API (mocked routes;
# devcam-investigation.md, F-CAM0-NAV). Mac side: needs node + Playwright + a Chromium
# (it is under tests/mac/, so the homelab plain suites do not run it):
#   P8_PLAYWRIGHT_NODE_PATH=<…/node_modules> [P8_CHROME=<chrome>] bash scripts/bench/tests/mac/p8-cam0-browser.sh
# The fake console mimics the real one's two traps:
#   - its workspace store reads the `xorgate.workspace` key ONCE at boot, else takes the
#     user's FIRST org from /v1/me (devcam-investigation.md);
#   - like console v0.4.3, its router does NOT act on a synthetic popstate (F-CAM0-NAV):
#     only a real page load of /devices/<id>/streams renders the device.
# Tiles carry the model captions (cam0 "Left", cam1 "Right"), as LiveVideoGrid does.
#   S1  a device in the user's SECOND org, cam0: PASS (its "Left" tile, currentTime advancing)
#   S1b cam0 + cam1: both PASS, cam1 by its "Right" caption (the SECOND tile)
#   S1c a key no tile carries (cam2): FAIL, no first-tile fallback
#   S1M MUTANT: the addInitScript removed: FAIL (the console 404s in its first org)
#   S1N MUTANT: the v0.1.0 pushState/popstate navigation: FAIL (the console never loads the device)
#   S2  a device whose console GET is 404: FAIL FAST with the reason
#   S3  a device with no video channels: FAIL FAST with the reason
#   S4  DEV: an unknown device: FAIL FAST; S4b DEV: a device in the second org passes
#   S5  DEV: a PROD console host: REFUSED (exit 2)
#   S6  PROD: every write is aborted, only POST /v1/live/access passes
#   S7  test mode with only the API overridden: REFUSED before any navigation
#   S8  P8_CAM0_LABEL (the old first-tile knob) set: REFUSED (exit 2)
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)
B=$REPO/scripts/bench
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
[ -d "${P8_PLAYWRIGHT_NODE_PATH:-/nonexistent}/playwright" ] || { echo "FAIL: set P8_PLAYWRIGHT_NODE_PATH"; exit 2; }
T=$(mktemp -d)
cat > "$T/fake.py" <<'PY'
import http.server, json, sys, re
port, log = int(sys.argv[1]), sys.argv[2]
A, Bo = "aaaaaaaa-0000-0000-0000-00000000000a", "bbbbbbbb-0000-0000-0000-00000000000b"
MODEL = "dddddddd-0000-0000-0000-00000000000d"
DEV = {  # id: (org, workspace, channels, console-GET-ok)
  "58b51737-0000-0000-0000-000000000001": (Bo, "wsb", [{"streamKey": "cam0"}, {"streamKey": "cam1"}], True),  # S1: the user's SECOND org
  "58b51737-0000-0000-0000-000000000002": (Bo, "wsb", [{"streamKey": "cam0"}], False),    # S2: listed, but its GET 404s
  "58b51737-0000-0000-0000-000000000003": (Bo, "wsb", [], True),                          # S3: no channels
}
APP = open(sys.argv[3]).read()
class H(http.server.BaseHTTPRequestHandler):
    def j(self, code, body):
        b = json.dumps(body).encode(); self.send_response(code); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(b)
    def rec(self):
        open(log, "a").write("%s %s %s\n" % (self.command, self.path, self.headers.get("X-Organization-Id", "-")))
    def authed(self):
        return self.headers.get("Authorization") == "Bearer tok"
    def do_GET(self):
        self.rec()
        p = self.path.split("?")[0]
        if not p.startswith("/v1/"):
            b = APP.encode(); self.send_response(200); self.send_header("Content-Type", "text/html"); self.end_headers(); self.wfile.write(b); return
        if not self.authed(): return self.j(401, {})
        org = self.headers.get("X-Organization-Id")
        if p == "/v1/me": return self.j(200, {"organizations": [{"id": A}, {"id": Bo}]})
        if p == "/v1/devices": return self.j(200, {"devices": [{"id": i, "workspaceId": v[1], "status": "online", "deviceModelId": MODEL} for i, v in DEV.items() if v[0] == org]})
        if p == "/v1/device-models/" + MODEL:
            return self.j(200, {"device_model": {"id": MODEL, "ioCapabilities": {"media": {"video": [{"key": "cam0", "label": "Left"}, {"key": "cam1", "label": "Right"}]}}}})  # the real wrapper
        m = re.match(r"^/v1/devices/([^/]+)(/video-channels)?$", p)
        if m and m.group(1) in DEV:
            o, ws, chans, okget = DEV[m.group(1)]
            if org != o or not okget: return self.j(404, {"error": "not found"})
            if m.group(2): return self.j(200, {"channels": chans})
            return self.j(200, {"device": {"id": m.group(1), "workspaceId": ws, "status": "online", "deviceModelId": MODEL}})
        return self.j(404, {"error": "not found"})
    def do_POST(self):
        self.rec()
        if self.path.startswith("/v1/live/access"): return self.j(200, {"attached": True})
        self.j(500, {"error": "a write reached the fake API"})
    do_PUT = do_PATCH = do_DELETE = do_POST
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY
cat > "$T/app.html" <<'HTML'
<!doctype html><html><body><script>
const KEY = "xorgate.workspace", TK = "CognitoIdentityServiceProvider.c.u.idToken";
let st = null; try { st = JSON.parse(localStorage.getItem(KEY) || "null"); } catch (e) {}
let org = st && st.state && st.state.currentOrganizationId;   // read ONCE, at boot (zustand persist)
const tok = () => localStorage.getItem(TK);
async function ensureOrg() { if (!org) { const b = await (await fetch("/v1/me", { headers: { Authorization: "Bearer " + tok() } })).json(); org = b.organizations[0].id;
  localStorage.setItem(KEY, JSON.stringify({ state: { currentOrganizationId: org, currentWorkspaceId: null }, version: 0 })); } }
function login() { document.body.innerHTML = '<input id="email"><input id="password" type="password"><button type="submit" id="go">Sign in</button>';
  document.getElementById("go").onclick = async () => { localStorage.setItem(TK, "tok"); history.pushState({}, "", "/"); await ensureOrg(); route(); }; }
function tile(label, hue) {
  const d = document.createElement("div"); d.className = "group";
  const v = document.createElement("video"); v.autoplay = true; v.muted = true; v.playsInline = true; d.appendChild(v);
  const l = document.createElement("div"); l.textContent = label; d.appendChild(l);
  const s = document.createElement("span"); s.textContent = "720p 15fps 1Mbps"; d.appendChild(s);
  const cv = document.createElement("canvas"); cv.width = 320; cv.height = 180; const g = cv.getContext("2d"); let x = 0;
  setInterval(() => { g.fillStyle = hue; g.fillRect(0, 0, 320, 180); g.fillStyle = "#fc0"; g.fillRect(x % 320, 60, 40, 40); x += 8; }, 50);
  v.srcObject = cv.captureStream(20); v.play(); return d;
}
async function route() {
  const m = location.pathname.match(/^\/devices\/([^/]+)\/streams$/);
  if (!m) { document.body.innerHTML = "<h1>home</h1>"; return; }
  const h = { Authorization: "Bearer " + tok(), "X-Organization-Id": org };
  const r = await fetch("/v1/devices/" + m[1], { headers: h });
  if (!r.ok) { document.body.innerHTML = "<p>not found</p>"; return; }
  const dev = (await r.json()).device;
  const caps = ((((await (await fetch("/v1/device-models/" + dev.deviceModelId, { headers: h })).json()).device_model || {}).ioCapabilities || {}).media || {}).video || [];
  await fetch("/v1/live/access", { method: "POST", headers: h });
  await fetch("/v1/devices/" + m[1] + "/config", { method: "PATCH", headers: h, body: "{}" }).catch(() => {});   // a write the guard must abort
  const cl = (await (await fetch("/v1/devices/" + m[1] + "/video-channels", { headers: h })).json()).channels || [];
  if (!cl.length) { document.body.innerHTML = "<p>no channels</p>"; return; }
  document.body.innerHTML = "";
  cl.forEach((c, i) => { const cap = caps.find((x) => x.key === c.streamKey); document.body.appendChild(tile(cap ? cap.label : c.streamKey, i ? "#642" : "#246")); });
}
// Like console v0.4.3 (F-CAM0-NAV): NO popstate listener; only a real page load routes.
(async () => { if (!tok()) { history.replaceState({}, "", "/login"); login(); return; } await ensureOrg(); route(); })();
</script></body></html>
HTML
PORT=18961
python3 "$T/fake.py" "$PORT" "$T/req.log" "$T/app.html" & SP=$!
sleep 1
BASE=http://127.0.0.1:$PORT
run() { # <tool file> <out> [extra env…]: the tool against the fake (a hard 120 s cap)
  local f=$1 o=$2; shift 2
  env XG_USER=u XG_PASS=p P8_CAM0_TEST=1 P8_CAM0_CONSOLE="$BASE" P8_CAM0_API_BASE="$BASE" P8_CAM0_WAIT_S=20 P8_CAM0_FAST_S=8 P8_CAM0_SPAN_S=5 "$@" \
    perl -e 'alarm 120; exec @ARGV' bash "$f" > "$o" 2>&1
}
ORGB=bbbbbbbb-0000-0000-0000-00000000000b
D1=58b51737-0000-0000-0000-000000000001
: > "$T/req.log"
t0=$(date +%s); run "$B/p8-cam0.sh" "$T/s1.json" P8_PROD_ORG=$ORGB P8_PROD_DEVICE=$D1; rc=$?
grep -q '"result":"PASS"' "$T/s1.json" && [ $rc = 0 ] && grep -q '"consoleDeviceGet":200' "$T/s1.json" && grep -q '"channels":2' "$T/s1.json" \
  && grep -q '"cam0":{"caption":"Left","result":"PASS"' "$T/s1.json" && grep -q '"url":"/devices/<id>/streams"' "$T/s1.json" \
  && ok "S1: a device in the user's SECOND org passes by its \"Left\" tile, loaded directly ($(grep -o '"currentTimeAdvance_s":[0-9.]*' "$T/s1.json" | head -n1), $(( $(date +%s) - t0 )) s)" || { bad "S1"; cat "$T/s1.json"; }
grep -q "GET /v1/devices/$D1 $ORGB" "$T/req.log" && ok "S1: the console's own device GET carried the device's org" || bad "S1: the console's org header"
grep -q '"allowed":\["POST /v1/live/access"\]' "$T/s1.json" && grep -q 'PATCH /v1/devices/<id>/config' "$T/s1.json" && ! grep -q '^PATCH' "$T/req.log" \
  && ok "S6: only POST /v1/live/access passed; the console's PATCH was aborted and never reached the API" || { bad "S6"; cat "$T/s1.json"; }
run "$B/p8-cam0.sh" "$T/s1b.json" P8_PROD_ORG=$ORGB P8_PROD_DEVICE=$D1 P8_CAM_KEYS=cam0,cam1; rc=$?
grep -q '"result":"PASS"' "$T/s1b.json" && [ $rc = 0 ] && grep -q '"cam0":{"caption":"Left","result":"PASS"' "$T/s1b.json" && grep -q '"cam1":{"caption":"Right","result":"PASS"' "$T/s1b.json" && grep -q '"tiles":2' "$T/s1b.json" \
  && ok "S1b: cam0 + cam1 both pass, each by its own caption" || { bad "S1b"; cat "$T/s1b.json"; }
run "$B/p8-cam0.sh" "$T/s1c.json" P8_PROD_ORG=$ORGB P8_PROD_DEVICE=$D1 P8_CAM_KEYS=cam2; rc=$?
[ $rc = 1 ] && grep -q '"cam2":{"caption":"cam2","result":"FAIL","why":"no tile captioned \\"cam2\\""' "$T/s1c.json" \
  && ok "S1c: a key no tile carries FAILs (no first-tile fallback)" || { bad "S1c"; cat "$T/s1c.json"; }
perl -ne 'print unless /^  await ctx\.addInitScript\(/ .. /^  \}, \[ORG, found\.ws\]\);/' "$B/p8-cam0.sh" > "$T/p8-cam0-mutM.sh"
grep -q 'await ctx.addInitScript(' "$T/p8-cam0-mutM.sh" && bad "S1M: mutant not applied"
run "$T/p8-cam0-mutM.sh" "$T/s1m.json" P8_PROD_ORG=$ORGB P8_PROD_DEVICE=$D1
grep -q '"result":"FAIL"' "$T/s1m.json" && grep -q 'returned 404' "$T/s1m.json" && ok "S1M: without the addInitScript the console stays in its first org and 404s: CAUGHT" || { bad "S1M: mutant not caught"; cat "$T/s1m.json"; }
perl -pe 's#^  await page\.goto\(CONSOLE \+ "/devices/" \+ found\.id \+ "/streams", \{ waitUntil: "domcontentloaded" \}\);#  await page.goto(CONSOLE + "/", { waitUntil: "domcontentloaded" }); await page.evaluate((p) => { history.pushState({}, "", p); dispatchEvent(new PopStateEvent("popstate")); }, "/devices/" + found.id + "/streams");#' "$B/p8-cam0.sh" > "$T/p8-cam0-mutN.sh"
grep -q 'PopStateEvent' "$T/p8-cam0-mutN.sh" || bad "S1N: mutant not applied"
run "$T/p8-cam0-mutN.sh" "$T/s1n.json" P8_PROD_ORG=$ORGB P8_PROD_DEVICE=$D1
grep -q '"result":"FAIL"' "$T/s1n.json" && grep -q 'never requested GET /v1/devices/<id>' "$T/s1n.json" && ok "S1N: the v0.1.0 pushState/popstate navigation is CAUGHT (F-CAM0-NAV, as on console v0.4.3)" || { bad "S1N: mutant not caught"; cat "$T/s1n.json"; }
t0=$(date +%s); run "$B/p8-cam0.sh" "$T/s2.json" P8_PROD_ORG=$ORGB P8_PROD_DEVICE_PREFIX=58b51737-0000-0000-0000-000000000002; rc=$?; el=$(( $(date +%s) - t0 ))
[ $rc = 1 ] && grep -q "GET /v1/devices/<id> returned 404" "$T/s2.json" && [ "$el" -lt 40 ] && ok "S2: a 404 device (found by prefix) fails FAST (${el} s) with the reason" || { bad "S2 (${el} s)"; cat "$T/s2.json"; }
t0=$(date +%s); run "$B/p8-cam0.sh" "$T/s3.json" P8_PROD_ORG=$ORGB P8_PROD_DEVICE=58b51737-0000-0000-0000-000000000003; rc=$?; el=$(( $(date +%s) - t0 ))
[ $rc = 1 ] && grep -q 'NO video channels' "$T/s3.json" && [ "$el" -lt 40 ] && ok "S3: a device with no video channels fails FAST (${el} s) with the reason" || { bad "S3 (${el} s)"; cat "$T/s3.json"; }
t0=$(date +%s); run "$B/p8-devcam0.sh" "$T/s4.json" P8_DEV_ORG=$ORGB P8_DEV_WS=wsb P8_DEV_DEVICE=99999999-0000-0000-0000-000000000009; rc=$?; el=$(( $(date +%s) - t0 ))
[ $rc = 1 ] && grep -q 'device lookup HTTP 404' "$T/s4.json" && [ "$el" -lt 40 ] && ok "S4: DEV, an unknown device fails FAST (${el} s)" || { bad "S4"; cat "$T/s4.json"; }
run "$B/p8-devcam0.sh" "$T/s4b.json" P8_DEV_ORG=$ORGB P8_DEV_WS=wsb P8_DEV_DEVICE=$D1; rc=$?
grep -q '"result":"PASS"' "$T/s4b.json" && [ $rc = 0 ] && ok "S4b: DEV, a device in the second org passes" || { bad "S4b"; cat "$T/s4b.json"; }
env XG_USER=u XG_PASS=p P8_CAM0_TEST=1 P8_CAM0_CONSOLE=https://console.xorgate.io P8_DEV_DEVICE=$D1 bash "$B/p8-devcam0.sh" > "$T/s5.json" 2>&1; rc=$?
[ $rc = 2 ] && grep -q 'REFUSED: the DEV check against a PROD host' "$T/s5.json" && ok "S5: the DEV variant REFUSES a PROD console host" || { bad "S5"; cat "$T/s5.json"; }
env XG_USER=u XG_PASS=p P8_CAM0_TEST=1 P8_CAM0_API_BASE="$BASE" P8_PROD_DEVICE=$D1 P8_PLAYWRIGHT_NODE_PATH="$P8_PLAYWRIGHT_NODE_PATH" bash "$B/p8-cam0.sh" > "$T/s7.json" 2>&1; rc=$?
[ $rc = 2 ] && grep -q 'REFUSED: test mode needs BOTH' "$T/s7.json" && ! grep -q 'consoleDeviceGet' "$T/s7.json" && ok "S7: test mode with only the API overridden is REFUSED before any navigation (the real console never loads)" || { bad "S7"; cat "$T/s7.json"; }
env XG_USER=u XG_PASS=p P8_CAM0_LABEL=Right P8_PLAYWRIGHT_NODE_PATH="$P8_PLAYWRIGHT_NODE_PATH" bash "$B/p8-cam0.sh" > "$T/s8.json" 2>&1; rc=$?
[ $rc = 2 ] && grep -q 'P8_CAM0_LABEL is no longer supported' "$T/s8.json" && ok "S8: the old P8_CAM0_LABEL knob is REFUSED" || { bad "S8"; cat "$T/s8.json"; }
# (No "override outside test mode" case: without P8_CAM0_TEST=1 the overrides are
# simply ignored and the real host would be used; a test must never reach it.)
kill "$SP" 2>/dev/null; wait "$SP" 2>/dev/null; rm -rf "$T"
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
