#!/usr/bin/env bash
# p8-cam0.sh: the "cam0 live plays" check of a Phase 8 bench window, Mac side.
# READ-ONLY and FAIL-CLOSED. PROD (DA15, the default; the P8_CAM0_HOOK of
# `p8-verify.sh cam0`/`da15`) or DEV (P8_CAM0_STAGE=dev; `p8-devcam0.sh`; W1
# DA12 / W2 DA13 against the freshly claimed device).
#
#   XG_USER=… XG_PASS=… P8_PLAYWRIGHT_NODE_PATH=<dir with node_modules/playwright> scripts/bench/p8-cam0.sh
#   … P8_DEV_DEVICE=<full DEV device id> scripts/bench/p8-devcam0.sh
#
# Method (phase-7a K11), measured on the <video> element itself, never the dot,
# no screenshot: readyState >= 2, a non-zero size, decoded frames advancing over
# >= 3 s, and a picture that is not uniformly black or white.
#
# The console's org (devcam-investigation.md, 2026-11-15): its workspace store
# reads the `xorgate.workspace` localStorage key ONCE, at boot. Writing it after
# sign-in did nothing, so the console stayed in the user's FIRST org and 404'd a
# device in any other. Now the key is installed with ctx.addInitScript and the
# app is RELOADED before the Streams page, and the check FAILS FAST, naming the
# cause, unless the console's OWN `GET /v1/devices/<id>` returns 200 and its
# `GET /v1/devices/<id>/video-channels` returns a non-empty list.
#
# Read-only, enforced, per request:
#   PROD: on api.xorgate.io only GET/HEAD/OPTIONS + EXACTLY `POST /v1/live/access`
#         (the viewer grant); everything else, DELETE of it included, is ABORTED.
#   DEV:  every request to the PROD API is ABORTED; on the DEV API the same
#         allow-list. The DEV variant REFUSES to run against a PROD host.
#   Service workers are blocked; nothing is clicked after sign-in.
#
# Env: P8_CAM0_STAGE (prod|dev), P8_PROD_ORG (Alocate cb1e1710-…), P8_PROD_DEVICE_PREFIX
# (58b51737), P8_DEV_DEVICE, P8_DEV_ORG/P8_DEV_WS (Acme / Crazy Project), P8_CAM0_LABEL
# (cam0; the first tile if none carries it), P8_CAM0_WAIT_S (90), P8_CAM0_FAST_S (25,
# the fail-fast budget for the console's device + channel GETs), P8_CHROME.
# TESTS ONLY (P8_CAM0_TEST=1): P8_CAM0_CONSOLE / P8_CAM0_API_BASE point at a local fake.
#   P8_CAM0_SELFTEST=1   runs the request policy over a fixed table (no browser, no network).
# Output: one JSON verdict line, no token. Exit: 0 PASS; 1 FAIL (incl. any error); 2 config/refused.
set -euo pipefail
export P8_CAM0_STAGE=${P8_CAM0_STAGE:-prod}
case $P8_CAM0_STAGE in prod | dev) ;; *) echo '{"result":"FAIL","why":"P8_CAM0_STAGE must be prod or dev"}'; exit 2 ;; esac
if [ "${P8_CAM0_SELFTEST:-0}" = 1 ]; then P8_CAM0_MODE=selftest; else P8_CAM0_MODE=run
  { [ -n "${XG_USER:-}" ] && [ -n "${XG_PASS:-}" ]; } || { echo '{"result":"FAIL","why":"XG_USER/XG_PASS unset"}'; exit 2; }
  [ -d "${P8_PLAYWRIGHT_NODE_PATH:-/nonexistent}/playwright" ] || { echo '{"result":"FAIL","why":"P8_PLAYWRIGHT_NODE_PATH has no playwright"}'; exit 2; }
fi
export P8_CAM0_MODE
NODE_PATH=${P8_PLAYWRIGHT_NODE_PATH:-} exec node -e '
const env = process.env, STAGE = env.P8_CAM0_STAGE, TEST = env.P8_CAM0_TEST === "1";
const PROD_API = "api.xorgate.io", PROD_CONSOLE = "console.xorgate.io";
const DEV_API = "h357muxy3k.execute-api.us-east-1.amazonaws.com", DEV_CONSOLE = "dev.xorgate.io";
const CONSOLE = (TEST && env.P8_CAM0_CONSOLE) || (STAGE === "dev" ? "https://" + DEV_CONSOLE : "https://" + PROD_CONSOLE);
const API_BASE = (TEST && env.P8_CAM0_API_BASE) || "https://" + (STAGE === "dev" ? DEV_API : PROD_API);
const API = new URL(API_BASE).host;
// The request policy (pure; P8_CAM0_SELFTEST runs it over a table).
function decide(method, url, api = API, stage = STAGE) {
  const u = new URL(url);
  if (stage === "dev" && u.host === PROD_API) return "abort";
  if (u.host !== api) return "allow";
  if (["GET", "HEAD", "OPTIONS"].includes(method)) return "allow";
  if (method === "POST" && u.pathname === "/v1/live/access") return "allow-live";
  return "abort";
}
if (env.P8_CAM0_MODE === "selftest") {
  const P = [["POST", "https://api.xorgate.io/v1/live/access", "allow-live"], ["POST", "https://api.xorgate.io/v1/live/access?x=1", "allow-live"],
    ["DELETE", "https://api.xorgate.io/v1/live/access", "abort"], ["PUT", "https://api.xorgate.io/v1/live/access", "abort"],
    ["POST", "https://api.xorgate.io/v1/live/access/x", "abort"], ["POST", "https://api.xorgate.io/v1/live/accessx", "abort"],
    ["POST", "https://api.xorgate.io/v1/live/access/", "abort"], ["POST", "https://api.xorgate.io/v1/devices/58b51737-0000/commands", "abort"],
    ["PATCH", "https://api.xorgate.io/v1/devices/58b51737-0000/config", "abort"], ["PUT", "https://api.xorgate.io/v1/devices/58b51737-0000", "abort"],
    ["DELETE", "https://api.xorgate.io/v1/devices/58b51737-0000", "abort"], ["POST", "https://api.xorgate.io/v1/device-registrations", "abort"],
    ["GET", "https://api.xorgate.io/v1/devices", "allow"], ["OPTIONS", "https://api.xorgate.io/v1/live/access", "allow"],
    ["POST", "https://cognito-idp.us-east-1.amazonaws.com/", "allow"]];
  const D = [["GET", "https://api.xorgate.io/v1/devices", "abort"], ["POST", "https://api.xorgate.io/v1/live/access", "abort"],
    ["POST", "https://" + DEV_API + "/v1/live/access", "allow-live"], ["DELETE", "https://" + DEV_API + "/v1/live/access", "abort"],
    ["POST", "https://" + DEV_API + "/v1/live/access/x", "abort"], ["PATCH", "https://" + DEV_API + "/v1/devices/x/config", "abort"],
    ["POST", "https://" + DEV_API + "/v1/devices/x/commands", "abort"], ["POST", "https://" + DEV_API + "/v1/device-registrations", "abort"],
    ["DELETE", "https://" + DEV_API + "/v1/devices/x", "abort"], ["GET", "https://" + DEV_API + "/v1/devices", "allow"],
    ["POST", "https://cognito-idp.us-east-1.amazonaws.com/", "allow"]];
  const T = STAGE === "dev" ? D : P, api = STAGE === "dev" ? DEV_API : PROD_API;
  let bad = 0;
  for (const [m, u, want] of T) { const got = decide(m, u, api); if (got !== want) bad++; console.log((got === want ? "PASS" : "FAIL") + " " + STAGE + " " + m + " " + u + " -> " + got); }
  const passing = T.filter(([m, u]) => !["GET", "HEAD", "OPTIONS"].includes(m) && new URL(u).host === api && decide(m, u, api) !== "abort");
  const only = passing.every(([m, u]) => m === "POST" && new URL(u).pathname === "/v1/live/access");
  console.log((only ? "PASS" : "FAIL") + " " + STAGE + ": the only non-GET that passes is POST /v1/live/access");
  process.exit(bad || !only ? 1 : 0);
}
const out = { result: "FAIL", stage: STAGE, blocked: [], allowed: [], prodTouched: 0 };
const done = (code) => { console.log(JSON.stringify(out)); process.exit(code); };
// Host guards: the console/API must be the stage s own (the DEV variant never runs against PROD).
const ch = new URL(CONSOLE).host;
if (STAGE === "dev" && ([ch, API].includes(PROD_CONSOLE) || [ch, API].includes(PROD_API))) { out.why = "REFUSED: the DEV check against a PROD host"; done(2); }
if (!TEST && !(STAGE === "dev" ? ch === DEV_CONSOLE && API === DEV_API : ch === PROD_CONSOLE && API === PROD_API)) { out.why = "REFUSED: unexpected console/API host"; done(2); }
const { chromium } = require("playwright");
const ORG = STAGE === "dev" ? (env.P8_DEV_ORG || "b1da47d8-c51e-4e4c-bb21-e3383765d0f3") : (env.P8_PROD_ORG || "cb1e1710-3ee5-49ad-b3d3-32b2290f6e19");
const WS_WANT = STAGE === "dev" ? (env.P8_DEV_WS || "f5cc23cb-47e3-4522-aad9-1608750db26c") : null;
const DEVICE = STAGE === "dev" ? (env.P8_DEV_DEVICE || "") : null;
const PREFIX = env.P8_PROD_DEVICE_PREFIX || "58b51737";
const LABEL = env.P8_CAM0_LABEL || "cam0";
const WAIT = 1000 * Number(env.P8_CAM0_WAIT_S || 90), FAST = 1000 * Number(env.P8_CAM0_FAST_S || 25);
(async () => {
  if (STAGE === "dev" && !/^[0-9a-f-]{36}$/.test(DEVICE)) { out.why = "P8_DEV_DEVICE must be a full id"; return done(2); }
  const launch = { headless: true };
  if (env.P8_CHROME) launch.executablePath = env.P8_CHROME;
  const browser = await chromium.launch(launch);
  const fail = async (why, code = 1) => { out.why = why; await browser.close(); done(code); };
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 1000 }, serviceWorkers: "block" });
  await ctx.route("**/*", (route) => {
    const req = route.request(), u = new URL(req.url()), d = decide(req.method(), req.url());
    const what = req.method() + " " + u.pathname.replace(/[0-9a-f-]{36}/g, "<id>");
    if (STAGE === "dev" && u.host === PROD_API) out.prodTouched++;
    if (d === "abort") { out.blocked.push(what); return route.abort(); }
    if (d === "allow-live") out.allowed.push(what);
    return route.fallback();
  });
  const page = await ctx.newPage();
  await page.goto(CONSOLE + "/", { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#email", { timeout: 30000 });
  await page.fill("#email", env.XG_USER);
  await page.fill("#password", env.XG_PASS);
  await page.click("button[type=submit]");
  await page.waitForURL((u) => !u.pathname.startsWith("/login"), { timeout: 30000 });
  // Find the device and its workspace with an EXPLICIT org header (GET only).
  const found = await page.evaluate(async ([BASE, ORG, ID, PREFIX]) => {
    const k = Object.keys(localStorage).find((x) => x.endsWith(".idToken"));
    if (!k) return { err: "no id token in the console session" };
    const h = { Authorization: "Bearer " + localStorage.getItem(k), "X-Organization-Id": ORG };
    const r = await fetch(BASE + (ID ? "/v1/devices/" + ID : "/v1/devices"), { headers: h });
    if (!r.ok) return { err: "device lookup HTTP " + r.status };
    const b = await r.json();
    const d = ID ? (b.device || b) : (b.devices || b.items || b.data || []).find((x) => String(x.id).startsWith(PREFIX));
    return d ? { id: d.id, ws: d.workspaceId, status: d.status } : { err: "device " + PREFIX + " not in the org listing" };
  }, [API_BASE, ORG, DEVICE, PREFIX]);
  if (found.err) return fail(found.err);
  if (WS_WANT && found.ws !== WS_WANT) return fail("the device is not in the expected workspace");
  out.device = String(found.id).slice(0, 8); out.status = found.status;
  // The fix: the org/workspace go into the store BEFORE the app boots.
  await ctx.addInitScript(([org, ws]) => {
    try { localStorage.setItem("xorgate.workspace", JSON.stringify({ state: { currentOrganizationId: org, currentWorkspaceId: ws }, version: 0 })); } catch (e) {}
  }, [ORG, found.ws]);
  const seen = { dev: null, devOrg: null, ch: null, chLen: null };
  const devPath = "/v1/devices/" + found.id, chPath = devPath + "/video-channels";
  page.on("response", async (res) => {
    try {
      const req = res.request(), u = new URL(res.url());
      if (u.host !== API || req.method() !== "GET") return;
      if (u.pathname === devPath) { seen.dev = res.status(); seen.devOrg = (req.headers()["x-organization-id"] || "").slice(0, 8); }
      if (u.pathname === chPath) {
        seen.ch = res.status();
        if (res.ok()) { const b = await res.json(); const l = Array.isArray(b) ? b : (b.channels || b.videoChannels || b.items || b.data || []); seen.chLen = Array.isArray(l) ? l.length : 0; }
      }
    } catch (e) {}
  });
  await page.goto(CONSOLE + "/", { waitUntil: "domcontentloaded" });   // reload: the store re-hydrates with the device s org
  await page.evaluate((p) => { history.pushState({}, "", p); dispatchEvent(new PopStateEvent("popstate")); }, devPath.replace("/v1", "") + "/streams");
  // Fail fast, naming the cause (org mismatch, no channels) instead of a picture timeout.
  const tf = Date.now();
  while (Date.now() - tf < FAST && (seen.dev === null || (seen.dev === 200 && seen.chLen === null && seen.ch === null))) await page.waitForTimeout(250);
  out.consoleDeviceGet = seen.dev; out.consoleOrg = seen.devOrg; out.channelsGet = seen.ch; out.channels = seen.chLen;
  if (seen.dev === null) return fail("the console never requested GET /v1/devices/<id> within " + FAST / 1000 + " s");
  if (seen.dev !== 200) return fail("the console s own GET /v1/devices/<id> returned " + seen.dev + " (console org " + seen.devOrg + "…, device org " + ORG.slice(0, 8) + "…): wrong org/workspace in the console");
  if (seen.ch === null) return fail("the console never requested the video channels within " + FAST / 1000 + " s (no model video caps?)");
  if (seen.ch !== 200) return fail("the console s GET …/video-channels returned " + seen.ch);
  if (!seen.chLen) return fail("the device has NO video channels (empty list)");
  const sample = () => page.evaluate((LABEL) => {
    const vs = [...document.querySelectorAll("video")];
    if (!vs.length) return null;
    let i = vs.findIndex((v) => (v.parentElement && v.parentElement.innerText || "").includes(LABEL));
    const byLabel = i >= 0; if (i < 0) i = 0;
    const v = vs[i], q = v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality() : { totalVideoFrames: 0 };
    let mean = null, max = null;
    try {
      const c = document.createElement("canvas"); c.width = 64; c.height = 36;
      const g = c.getContext("2d"); g.drawImage(v, 0, 0, 64, 36);
      const px = g.getImageData(0, 0, 64, 36).data; let s = 0; max = 0;
      for (let j = 0; j < px.length; j += 4) { const y = 0.299 * px[j] + 0.587 * px[j + 1] + 0.114 * px[j + 2]; s += y; if (y > max) max = y; }
      mean = Math.round(s / (px.length / 4)); max = Math.round(max);
    } catch (e) { mean = null; }
    return { tiles: vs.length, byLabel, rs: v.readyState, w: v.videoWidth, h: v.videoHeight, frames: q.totalVideoFrames, mean, max };
  }, LABEL);
  const t0 = Date.now(); let first = null, last = null;
  while (Date.now() - t0 < WAIT) {
    await page.waitForTimeout(1000);
    const s = await sample();
    if (!s || s.rs < 2 || !s.w) { last = s; continue; }
    if (!first) { first = { ...s, t: Date.now() }; continue; }
    last = s;
    if (Date.now() - first.t >= 3000 && s.frames > first.frames && s.mean !== null && (s.mean >= 8 || s.max >= 32) && s.mean <= 247) {
      Object.assign(out, { result: "PASS", tile: s.byLabel ? LABEL : "first tile (no " + LABEL + " label)", tiles: s.tiles,
        size: s.w + "x" + s.h, readyState: s.rs, luma: s.mean + " (max " + s.max + ")", framesDelta: s.frames - first.frames,
        secs: Math.round((Date.now() - t0) / 1000) });
      await browser.close(); return done(out.prodTouched ? 1 : 0);
    }
  }
  out.last = last;
  return fail("no live picture (readyState >= 2 with frames advancing) within " + WAIT / 1000 + " s");
})().catch((e) => { out.why = "error: " + String(e && e.message || e).split("\n")[0].slice(0, 160); done(1); });
'
