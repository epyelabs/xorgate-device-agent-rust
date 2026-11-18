#!/usr/bin/env bash
# p8-cam0.sh: the "camera live plays" check of a Phase 8 bench window, Mac side.
# READ-ONLY and FAIL-CLOSED. PROD (DA15, the default; the P8_CAM0_HOOK of
# `p8-verify.sh cam0`/`da15`; DA21) or DEV (P8_CAM0_STAGE=dev; `p8-devcam0.sh`; W1
# DA12 / W2 DA13 against the freshly claimed device).
#
#   XG_USER=… XG_PASS=… P8_PLAYWRIGHT_NODE_PATH=<dir with node_modules/playwright> scripts/bench/p8-cam0.sh
#   … P8_PROD_DEVICE=<full PROD device id> P8_CAM_KEYS=cam0,cam1 scripts/bench/p8-cam0.sh
#   … P8_DEV_DEVICE=<full DEV device id> scripts/bench/p8-devcam0.sh
#
# Navigation (F-CAM0-NAV, P19 2026-11-18): console v0.4.3's router ignores a
# history.pushState + synthetic popstate, so the old tool waited on a page that
# never came. Now: sign in, install the org/workspace store with
# ctx.addInitScript (the store reads `xorgate.workspace` ONCE, at boot;
# devcam-investigation.md), then load `/devices/<full id>/streams` DIRECTLY
# (page.goto). Nothing is clicked after sign-in, and nothing depends on the
# Register dialog. The check FAILS FAST, naming the cause, unless the console's
# OWN `GET /v1/devices/<id>` returns 200 and its `GET …/video-channels` returns a
# non-empty list.
#
# Tiles: one per requested stream key (P8_CAM_KEYS, default cam0), selected by
# the CAPTION the console gives that key: the device model's
# ioCapabilities.media.video[].label, read from the console's own model GET, or
# the key itself when the model has none (the console's own fallback). A tile
# counts only if exactly one tile on the device's page carries that caption:
# never "the first tile".
# Pass, per tile: readyState >= 2, a non-zero size, and the <video>'s
# currentTime advancing >= P8_CAM0_MIN_ADVANCE_S (3) over a P8_CAM0_SPAN_S (10) s
# window. The mean luma is recorded, not judged. No screenshot.
#
# Read-only, enforced, per request:
#   PROD: on api.xorgate.io only GET/HEAD/OPTIONS + EXACTLY `POST /v1/live/access`
#         (the viewer grant); everything else, DELETE of it included, is ABORTED.
#   DEV:  every request to the PROD API is ABORTED; on the DEV API the same
#         allow-list. The DEV variant REFUSES to run against a PROD host.
#   Service workers are blocked; nothing is clicked after sign-in.
#
# Env: P8_CAM0_STAGE (prod|dev), P8_PROD_ORG (Alocate cb1e1710-…), P8_PROD_DEVICE (a
# full id; preferred), P8_PROD_DEVICE_PREFIX (58b51737; used only without
# P8_PROD_DEVICE, through the org listing), P8_DEV_DEVICE, P8_DEV_ORG/P8_DEV_WS
# (Acme / Crazy Project), P8_CAM_KEYS (cam0), P8_CAM0_WAIT_S (90, until every tile
# plays), P8_CAM0_SPAN_S (10), P8_CAM0_MIN_ADVANCE_S (3), P8_CAM0_FAST_S (25, the
# fail-fast budget for the console's device + channel GETs), P8_CHROME.
# P8_CAM0_LABEL is GONE (it allowed a first-tile fallback): set => exit 2.
# TESTS ONLY (P8_CAM0_TEST=1): P8_CAM0_CONSOLE / P8_CAM0_API_BASE point at a local fake;
#   BOTH must be 127.0.0.1/localhost (else REFUSED), and every *.xorgate.io request is aborted.
#   P8_CAM0_SELFTEST=1   runs the request policy, the tile picker and the verdict
#                        rule over fixed tables (no browser, no network).
# Output: one JSON verdict line, no token. Exit: 0 PASS; 1 FAIL (incl. any error); 2 config/refused.
set -euo pipefail
export P8_CAM0_STAGE=${P8_CAM0_STAGE:-prod}
case $P8_CAM0_STAGE in prod | dev) ;; *) echo '{"result":"FAIL","why":"P8_CAM0_STAGE must be prod or dev"}'; exit 2 ;; esac
[ -z "${P8_CAM0_LABEL:-}" ] || { echo '{"result":"FAIL","why":"P8_CAM0_LABEL is no longer supported: use P8_CAM_KEYS (tiles are selected by the model caption of each key)"}'; exit 2; }
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
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
// The request policy (pure; P8_CAM0_SELFTEST runs it over a table).
function decide(method, url, api = API, stage = STAGE, test = TEST) {
  const u = new URL(url);
  // Test mode never talks to a real xorgate host (review bench-tools-r3 SHOULD).
  if (test && (u.hostname === "xorgate.io" || u.hostname.endsWith(".xorgate.io"))) return "abort";
  if (stage === "dev" && u.host === PROD_API) return "abort";
  // The allow-list ALWAYS applies to the PROD API, not only to the configured one.
  if (u.host !== api && u.host !== PROD_API) return "allow";
  if (["GET", "HEAD", "OPTIONS"].includes(method)) return "allow";
  if (method === "POST" && u.pathname === "/v1/live/access") return "allow-live";
  return "abort";
}
// The tile picker (pure): tiles = [[caption texts found in tile 0], [… tile 1], …].
// Exactly one tile must carry the caption; never a positional fallback.
function pickTile(tiles, caption) {
  const hits = [];
  tiles.forEach((texts, i) => { if (texts.includes(caption)) hits.push(i); });
  if (hits.length === 1) return { index: hits[0] };
  return { index: null, why: hits.length ? "ambiguous: " + hits.length + " tiles captioned \"" + caption + "\"" : "no tile captioned \"" + caption + "\"" };
}
// The verdict rule (pure): a = the first playing sample, b = the sample >= span s later.
function judge(a, b, minAdvance) {
  if (!a) return { pass: false, why: "never playing (readyState >= 2 with a non-zero size)" };
  if (!b) return { pass: false, why: "no second sample" };
  if (!(b.rs >= 2) || !b.w) return { pass: false, why: "stopped playing (readyState " + b.rs + ")" };
  const d = b.ct - a.ct;
  if (!(d >= minAdvance)) return { pass: false, why: "currentTime advanced " + d.toFixed(2) + " s (< " + minAdvance + " s)" };
  return { pass: true };
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
  for (const [m, u, want] of T) { const got = decide(m, u, api, STAGE, false); if (got !== want) bad++; console.log((got === want ? "PASS" : "FAIL") + " " + STAGE + " " + m + " " + u + " -> " + got); }
  const passing = T.filter(([m, u]) => !["GET", "HEAD", "OPTIONS"].includes(m) && new URL(u).host === api && decide(m, u, api, STAGE, false) !== "abort");
  const only = passing.every(([m, u]) => m === "POST" && new URL(u).pathname === "/v1/live/access");
  console.log((only ? "PASS" : "FAIL") + " " + STAGE + ": the only non-GET that passes is POST /v1/live/access");
  if (!only) bad++;
  // The tile picker: never positional.
  const two = [["Left", "720p 15fps 1.9Mbps"], ["Right"]];
  for (const [tiles, cap, want, name] of [
    [two, "Left", 0, "the cam0 caption picks its own tile"], [two, "Right", 1, "the cam1 caption picks the SECOND tile"],
    [[["Left"]], "Right", null, "a caption no tile carries is a miss (no first-tile fallback)"],
    [[["Left"], ["Left"]], "Left", null, "a caption on two tiles is ambiguous"], [[], "cam0", null, "no tiles is a miss"],
    [[["Left 720p"]], "Left", null, "the caption must match a whole element text, not a substring"]]) {
    const got = pickTile(tiles, cap).index; const ok = got === want; if (!ok) bad++;
    console.log((ok ? "PASS" : "FAIL") + " tiles: " + name + " -> " + got);
  }
  // The verdict rule: currentTime must advance >= 3 s between the two samples.
  const s = (rs, w, ct) => ({ rs, w, ct });
  for (const [a, b, want, name] of [
    [s(4, 1280, 1.0), s(4, 1280, 11.0), true, "currentTime +10 s over 10 s"], [s(4, 1280, 1.0), s(4, 1280, 3.9), false, "+2.9 s is too little"],
    [s(4, 1280, 5.0), s(4, 1280, 5.0), false, "a frozen picture (currentTime still)"], [null, s(4, 1280, 9.0), false, "never playing"],
    [s(4, 1280, 1.0), s(1, 0, 9.0), false, "dropped to readyState 1"], [s(2, 640, 0.5), s(2, 640, 3.5), true, "exactly +3 s at readyState 2"]]) {
    const got = judge(a, b, 3).pass; const ok = got === want; if (!ok) bad++;
    console.log((ok ? "PASS" : "FAIL") + " verdict: " + name + " -> " + (got ? "pass" : "fail"));
  }
  process.exit(bad ? 1 : 0);
}
const out = { result: "FAIL", stage: STAGE, blocked: [], allowed: [], prodTouched: 0 };
const done = (code) => { console.log(JSON.stringify(out)); process.exit(code); };
// Host guards: the console/API must be the stage s own (the DEV variant never runs against PROD).
const ch = new URL(CONSOLE).host;
if (STAGE === "dev" && ([ch, API].includes(PROD_CONSOLE) || [ch, API].includes(PROD_API))) { out.why = "REFUSED: the DEV check against a PROD host"; done(2); }
// Test mode: BOTH the console and the API must be local (no partial override can
// leave a real host in play).
const local = (h) => ["127.0.0.1", "localhost"].includes(h.replace(/:[0-9]+$/, ""));
if (TEST && !(local(ch) && local(API))) { out.why = "REFUSED: test mode needs BOTH the console and the API on 127.0.0.1/localhost"; done(2); }
if (!TEST && !(STAGE === "dev" ? ch === DEV_CONSOLE && API === DEV_API : ch === PROD_CONSOLE && API === PROD_API)) { out.why = "REFUSED: unexpected console/API host"; done(2); }
const KEYS = (env.P8_CAM_KEYS || "cam0").split(",").map((k) => k.trim()).filter(Boolean);
if (!KEYS.length || KEYS.some((k) => !/^[A-Za-z0-9_-]{1,32}$/.test(k)) || new Set(KEYS).size !== KEYS.length) { out.why = "P8_CAM_KEYS must be distinct stream keys (e.g. cam0,cam1)"; done(2); }
const { chromium } = require("playwright");
const ORG = STAGE === "dev" ? (env.P8_DEV_ORG || "b1da47d8-c51e-4e4c-bb21-e3383765d0f3") : (env.P8_PROD_ORG || "cb1e1710-3ee5-49ad-b3d3-32b2290f6e19");
const WS_WANT = STAGE === "dev" ? (env.P8_DEV_WS || "f5cc23cb-47e3-4522-aad9-1608750db26c") : null;
const DEVICE = STAGE === "dev" ? (env.P8_DEV_DEVICE || "") : (env.P8_PROD_DEVICE || "");
const PREFIX = env.P8_PROD_DEVICE_PREFIX || "58b51737";
const num = (v, d) => (v === undefined || v === "" ? d : Number(v));
const WAIT = 1000 * num(env.P8_CAM0_WAIT_S, 90), FAST = 1000 * num(env.P8_CAM0_FAST_S, 25);
const SPAN = 1000 * num(env.P8_CAM0_SPAN_S, 10), MINADV = num(env.P8_CAM0_MIN_ADVANCE_S, 3);
(async () => {
  if (STAGE === "dev" && !UUID.test(DEVICE)) { out.why = "P8_DEV_DEVICE must be a full id"; return done(2); }
  if (STAGE === "prod" && DEVICE && !UUID.test(DEVICE)) { out.why = "P8_PROD_DEVICE must be a full id"; return done(2); }
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
    return d ? { id: d.id, ws: d.workspaceId, status: d.status, model: d.deviceModelId || null } : { err: "device " + PREFIX + " not in the org listing" };
  }, [API_BASE, ORG, DEVICE, PREFIX]);
  if (found.err) return fail(found.err);
  if (DEVICE && found.id !== DEVICE) return fail("the lookup returned another device");
  if (!UUID.test(String(found.id))) return fail("the device id is not a full id");
  if (WS_WANT && found.ws !== WS_WANT) return fail("the device is not in the expected workspace");
  out.device = String(found.id).slice(0, 8); out.status = found.status;
  // The org/workspace go into the store BEFORE the app boots (every navigation of this context).
  await ctx.addInitScript(([org, ws]) => {
    try { localStorage.setItem("xorgate.workspace", JSON.stringify({ state: { currentOrganizationId: org, currentWorkspaceId: ws }, version: 0 })); } catch (e) {}
  }, [ORG, found.ws]);
  const seen = { dev: null, devOrg: null, ch: null, chLen: null, model: null, caps: null };
  const devPath = "/v1/devices/" + found.id, chPath = devPath + "/video-channels";
  const modelPath = found.model ? "/v1/device-models/" + found.model : null;
  page.on("response", async (res) => {
    try {
      const req = res.request(), u = new URL(res.url());
      if (u.host !== API || req.method() !== "GET") return;
      if (u.pathname === devPath) { seen.dev = res.status(); seen.devOrg = (req.headers()["x-organization-id"] || "").slice(0, 8); }
      if (u.pathname === chPath) {
        // seen.ch only AFTER the body parsed (review NIT): never a premature "no channels"
        if (!res.ok()) { seen.ch = res.status(); return; }
        let b;
        try { b = await res.json(); } catch (e) { seen.ch = "an unparsable body"; return; }
        const l = Array.isArray(b) ? b : (b.channels || b.videoChannels || b.items || b.data || []);
        seen.chLen = Array.isArray(l) ? l.length : 0; seen.ch = res.status();
      }
      if (modelPath && u.pathname === modelPath) {
        if (!res.ok()) { seen.model = res.status(); return; }
        let b;
        try { b = await res.json(); } catch (e) { seen.model = "an unparsable body"; return; }
        const m = b.device_model || b.deviceModel || b.model || b;   // GET /v1/device-models/<id> = { device_model: row }
        const v = (((m || {}).ioCapabilities || {}).media || {}).video;
        seen.caps = {};
        if (Array.isArray(v)) for (const c of v) if (c && typeof c.key === "string") seen.caps[c.key] = typeof c.label === "string" && c.label ? c.label : c.key;
        seen.model = res.status();
      }
    } catch (e) {}
  });
  // F-CAM0-NAV: load the device s streams page DIRECTLY (no pushState/popstate routing).
  await page.goto(CONSOLE + "/devices/" + found.id + "/streams", { waitUntil: "domcontentloaded" });
  // Fail fast, naming the cause (org mismatch, no channels) instead of a picture timeout.
  const tf = Date.now();
  while (Date.now() - tf < FAST && (seen.dev === null || (seen.dev === 200 && seen.chLen === null && seen.ch === null) || (seen.dev === 200 && modelPath && seen.model === null))) await page.waitForTimeout(250);
  out.url = new URL(page.url()).pathname.replace(/[0-9a-f-]{36}/g, "<id>");
  out.consoleDeviceGet = seen.dev; out.consoleOrg = seen.devOrg; out.channelsGet = seen.ch; out.channels = seen.chLen; out.modelGet = seen.model;
  if (seen.dev === null) return fail("the console never requested GET /v1/devices/<id> within " + FAST / 1000 + " s (url " + out.url + ")");
  if (seen.dev !== 200) return fail("the console s own GET /v1/devices/<id> returned " + seen.dev + " (console org " + seen.devOrg + "…, device org " + ORG.slice(0, 8) + "…): wrong org/workspace in the console");
  if (seen.ch === null) return fail("the console never requested the video channels within " + FAST / 1000 + " s (no model video caps?)");
  if (seen.ch !== 200) return fail("the console s GET …/video-channels returned " + seen.ch);
  if (!seen.chLen) return fail("the device has NO video channels (empty list)");
  // The caption of each key: the model s label, else the key (the console s own fallback).
  const CAP = {}; for (const k of KEYS) CAP[k] = (seen.caps && seen.caps[k]) || k;
  out.captions = CAP;
  // One evaluate returns every <video> with its tile s texts; the PICK runs here,
  // in node, with the same pickTile the selftest covers.
  const raw = () => page.evaluate(() => [...document.querySelectorAll("video")].map((v) => {
    const t = v.closest(".group") || v.parentElement;
    const texts = t ? [...t.querySelectorAll("*")].map((e) => (e.textContent || "").trim()).filter(Boolean) : [];
    let mean = null;
    try {
      const c = document.createElement("canvas"); c.width = 64; c.height = 36;
      const g = c.getContext("2d"); g.drawImage(v, 0, 0, 64, 36);
      const px = g.getImageData(0, 0, 64, 36).data; let s = 0;
      for (let j = 0; j < px.length; j += 4) s += 0.299 * px[j] + 0.587 * px[j + 1] + 0.114 * px[j + 2];
      mean = Math.round(s / (px.length / 4));
    } catch (e) { mean = null; }
    return { texts, rs: v.readyState, w: v.videoWidth, h: v.videoHeight, ct: v.currentTime, paused: v.paused, mean };
  }));
  const sample = async () => {
    const vs = await raw(), r = { tiles: vs.length, per: {} };
    for (const k of KEYS) {
      const p = pickTile(vs.map((x) => x.texts), CAP[k]);
      if (p.index === null) { r.per[k] = { miss: p.why }; continue; }
      const { texts, ...rest } = vs[p.index]; r.per[k] = rest;
    }
    return r;
  };
  // 1. Wait (<= WAIT) until every requested tile plays; 2. sample again SPAN later.
  const t0 = Date.now(); let first = null, last = null, firstAt = null;
  const playing = (x) => x && !x.miss && x.rs >= 2 && x.w > 0;
  while (Date.now() - t0 < WAIT) {
    last = await sample();
    if (KEYS.every((k) => playing(last.per[k]))) { first = last; firstAt = Date.now(); break; }
    await page.waitForTimeout(1000);
  }
  out.tiles = last ? last.tiles : 0;
  let second = null, wall = 0;
  if (first) { const tw = Date.now(); await page.waitForTimeout(SPAN); second = await sample(); wall = (Date.now() - tw) / 1000; out.tiles = second.tiles; }
  out.perTile = {}; let allPass = true;
  for (const k of KEYS) {
    const a = first && first.per[k], b = second && second.per[k];
    const miss = (b && b.miss) || (last && last.per[k] && last.per[k].miss);
    const v = miss ? { pass: false, why: miss } : judge(playing(a) ? a : null, b && !b.miss ? b : null, MINADV);
    out.perTile[k] = { caption: CAP[k], result: v.pass ? "PASS" : "FAIL", why: v.why,
      size: b && b.w ? b.w + "x" + b.h : undefined, readyState: b ? b.rs : undefined,
      currentTimeAdvance_s: a && b && !b.miss ? Number((b.ct - a.ct).toFixed(2)) : undefined, overWall_s: second ? Number(wall.toFixed(1)) : undefined,
      luma: b && !b.miss ? b.mean : undefined, secsToPlay: firstAt ? Math.round((firstAt - t0) / 1000) : undefined };
    if (!v.pass) allPass = false;
  }
  if (allPass) { out.result = "PASS"; delete out.why; await browser.close(); return done(out.prodTouched ? 1 : 0); }
  return fail(first ? "a tile did not advance (see perTile)" : "no live picture on every requested tile (readyState >= 2 with a size) within " + WAIT / 1000 + " s (see perTile)");
})().catch((e) => { out.why = "error: " + String(e && e.message || e).split("\n")[0].slice(0, 160); done(1); });
'
