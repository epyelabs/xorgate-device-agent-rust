#!/usr/bin/env bash
# p8-cam0.sh: the DA15 "PROD cam0 live plays" hook (P8_CAM0_HOOK for
# `p8-verify.sh cam0`/`da15`). Mac side. READ-ONLY and FAIL-CLOSED.
#
#   XG_USER=… XG_PASS=… P8_PLAYWRIGHT_NODE_PATH=<dir with node_modules/playwright> scripts/bench/p8-cam0.sh
#
# The phase-7a K11 method: Playwright (headless Chrome) signs in to the PROD
# console as the test user, opens 58b51737's Streams page, and measures the
# <video> element itself (never the green dot, no screenshot): readyState >= 2,
# a non-zero size, decoded frames advancing between two samples >= 3 s apart,
# and a picture that is not uniformly black or white: (mean luma >= 8 or max
# luma >= 32) and mean <= 247. (A dark room gives mean ~1 with real detail,
# max ~100: measured 2026-11-15 06:15 local.)
#
# Read-only, enforced: every request to the PROD API that is not GET/HEAD/
# OPTIONS is ABORTED (and counted; method + path only). The page is only
# viewed: nothing is clicked after sign-in. Viewing starts the device's KVS
# master for the session (idle teardown ends it), as any console viewer does.
#
# Env: P8_PROD_ORG (default Alocate cb1e1710-…), P8_PROD_DEVICE_PREFIX
# (58b51737), P8_CAM0_LABEL (cam0; the first tile if no tile carries it),
# P8_CAM0_WAIT_S (90), P8_CHROME (a Chrome for Testing binary; else Playwright's).
# The device and its workspace are found through the API (GET) at run time.
# Output: one JSON verdict line, no ids beyond the prefix, no token.
# Exit: 0 PASS; 1 FAIL (incl. any error); 2 configuration.
set -euo pipefail
{ [ -n "${XG_USER:-}" ] && [ -n "${XG_PASS:-}" ]; } || { echo '{"result":"FAIL","why":"XG_USER/XG_PASS unset"}'; exit 2; }
[ -d "${P8_PLAYWRIGHT_NODE_PATH:-/nonexistent}/playwright" ] || { echo '{"result":"FAIL","why":"P8_PLAYWRIGHT_NODE_PATH has no playwright"}'; exit 2; }
NODE_PATH=$P8_PLAYWRIGHT_NODE_PATH exec node -e '
const { chromium } = require("playwright");
const CONSOLE = "https://console.xorgate.io", API = "api.xorgate.io";
const ORG = process.env.P8_PROD_ORG || "cb1e1710-3ee5-49ad-b3d3-32b2290f6e19";
const PREFIX = process.env.P8_PROD_DEVICE_PREFIX || "58b51737";
const LABEL = process.env.P8_CAM0_LABEL || "cam0";
const WAIT = 1000 * Number(process.env.P8_CAM0_WAIT_S || 90);
const out = { result: "FAIL", device: PREFIX, blocked: [] };
const done = (code) => { console.log(JSON.stringify(out)); process.exit(code); };
(async () => {
  const launch = { headless: true };
  if (process.env.P8_CHROME) launch.executablePath = process.env.P8_CHROME;
  const browser = await chromium.launch(launch);
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  await ctx.route("**/*", (route) => {
    const req = route.request(), u = new URL(req.url());
    if (u.host === API && !["GET", "HEAD", "OPTIONS"].includes(req.method())) {
      out.blocked.push(req.method() + " " + u.pathname.replace(/[0-9a-f-]{36}/g, "<id>"));
      return route.abort();
    }
    return route.fallback();
  });
  const page = await ctx.newPage();
  await page.goto(CONSOLE + "/", { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#email", { timeout: 30000 });
  await page.fill("#email", process.env.XG_USER);
  await page.fill("#password", process.env.XG_PASS);
  await page.click("button[type=submit]");
  await page.waitForURL((u) => !u.pathname.startsWith("/login"), { timeout: 30000 });
  const found = await page.evaluate(async ([API, ORG, PREFIX]) => {
    const k = Object.keys(localStorage).find((x) => x.endsWith(".idToken"));
    if (!k) return { err: "no id token in the console session" };
    const r = await fetch("https://" + API + "/v1/devices", { headers: { Authorization: "Bearer " + localStorage.getItem(k), "X-Organization-Id": ORG } });
    if (!r.ok) return { err: "device list HTTP " + r.status };
    const b = await r.json();
    const rows = b.devices || b.items || b.data || [];
    const d = rows.find((x) => String(x.id).startsWith(PREFIX));
    return d ? { id: d.id, ws: d.workspaceId, status: d.status } : { err: "device " + PREFIX + " not in the org listing" };
  }, [API, ORG, PREFIX]);
  if (found.err) { out.why = found.err; return done(1); }
  out.status = found.status;
  await page.evaluate(([org, ws]) => localStorage.setItem("xorgate.workspace",
    JSON.stringify({ state: { currentOrganizationId: org, currentWorkspaceId: ws }, version: 0 })), [ORG, found.ws]);
  // Deep links 404 on the static host: navigate inside the SPA.
  await page.evaluate((p) => { history.pushState({}, "", p); dispatchEvent(new PopStateEvent("popstate")); }, "/devices/" + found.id + "/streams");
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
      await browser.close(); return done(0);
    }
  }
  out.why = "no live picture within " + WAIT / 1000 + " s"; out.last = last;
  await browser.close(); done(1);
})().catch((e) => { out.why = "error: " + String(e && e.message || e).split("\n")[0].slice(0, 160); done(1); });
'
