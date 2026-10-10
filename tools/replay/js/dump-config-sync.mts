// WP3 tsx driver (Phase 2): drives the REAL `ConfigSync` class, `applyCloudConfig`,
// `parseCloudMessage`, `saveConfigSyncState`/`ensureConfigSyncBaseline` and the
// helpers of `src/agent/config-sync.ts` @ 89b5c64, and dumps two fixtures:
//
//   fixtures/config_sync_rules/class-scenarios.json — the 26 class/timer it()s of
//     test/config-sync.test.ts + the 2 of test/config-sync-cellular.test.ts, each
//     run with the SAME steps and option values as the it(), plus `now: () => NOW`
//     so the report bytes are deterministic. Captured: every published payload
//     (exact JSON.stringify bytes), the topics, the onEvent lines, the modem read
//     count and the final config-sync.json bytes.
//   fixtures/config_sync_rules/rules.json — cases for the pure kernels the Rust
//     port lifts out of the class (request coalescing, the settle predicate, the
//     modem-arrival trigger, the APN fingerprint, the publish outcome =
//     empty-local-diff downgrade + persist-after-publish), every expected value
//     OBSERVED on the class, plus the `cloud config applied:` summaries
//     (`JSON.stringify(parsed)`) and the config-sync.json byte shapes.
//
// Run from a `git archive 89b5c64` scratch copy: `npx tsx dump-config-sync.mts <outdir>`.
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const cs = await import(join(process.cwd(), "src/agent/config-sync.ts"));
const cc = await import(join(process.cwd(), "src/agent/cloud-config.ts"));
const st = await import(join(process.cwd(), "src/daemon/settings.ts"));
const { ConfigSync, configSyncPath, loadConfigSyncState, saveConfigSyncState, ensureConfigSyncBaseline,
  readSettingsHash, buildConfigReport, parseApnTelemetry } = cs;
const { applyCloudConfig, parseCloudMessage } = cc;
const { saveSettings, settingsPath } = st;

const OUT = process.argv[2] ?? ".";
const NOW = 1_786_017_600_000; // 2026-08-06T12:00:00Z
const delay = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));
const FRESH = { value: true, fresh: true };
const NO_MODEM = { value: null, fresh: false, detail: "modem telemetry unavailable" };

function home(): string {
  const d = mkdtempSync(join(tmpdir(), "xg-wp3-"));
  process.env.XORGATE_HOME = d;
  return d;
}
function desiredFrom(message: Record<string, unknown>) {
  const parsed = parseCloudMessage(JSON.stringify({ v: 1, ...message }));
  if (parsed === null) throw new Error("test message did not parse");
  return { rev: parsed.config.rev ?? 0, raw: parsed.raw, parsed: parsed.config };
}
function fileText(p: string): string | null {
  return existsSync(p) ? readFileSync(p, "utf8") : null;
}

interface Capture {
  sent: string[];
  topics: string[];
  events: string[];
  reads: number;
}
function capture(): Capture {
  return { sent: [], topics: [], events: [], reads: 0 };
}
function base(c: Capture, over: Record<string, unknown> = {}) {
  return {
    deviceId: "dev-1",
    topic: "t",
    publish: (topic: string, p: string) => {
      c.topics.push(topic);
      c.sent.push(p);
    },
    onEvent: (level: string, msg: string) => c.events.push(`${level}: ${msg}`),
    now: () => NOW,
    ...over,
  };
}

const scenarios: Record<string, unknown>[] = [];
async function scenario(name: string, it: string, fn: (h: string, c: Capture) => Promise<Record<string, unknown> | void>) {
  const h = home();
  const c = capture();
  const extra = (await fn(h, c)) ?? {};
  scenarios.push({
    name,
    it,
    expected: {
      sent: c.sent,
      topics: c.topics,
      // onEvent lines name the settings path: home-relative for the fixture.
      events: c.events.map((e) => e.split(h).join("<home>")),
      reads: c.reads,
      configSyncFile: fileText(configSyncPath()),
      ...extra,
    },
  });
  rmSync(h, { recursive: true, force: true });
}

// --- config-sync.json state file ------------------------------------------------
await scenario("state file: lives beside settings.json", "config-sync.test.ts:60", async (h) => ({
  configSyncPath: configSyncPath().slice(h.length),
  settingsPath: settingsPath().slice(h.length),
}));
await scenario("state file: a failed persist returns false", "config-sync.test.ts:67", async () => ({
  saved: saveConfigSyncState({ lastAppliedRev: 1, lastReportTs: 0 }, "/nope/does/not/exist.json"),
}));

// --- the boot race (modemArrivalMs 10, debounceMs 1) ----------------------------
function arrival(c: Capture, modem: () => Promise<unknown>) {
  return new ConfigSync(base(c, {
    settings: () => ({}),
    modem: async () => { c.reads += 1; return modem(); },
    debounceMs: 1,
    modemArrivalMs: 10,
  }));
}
await scenario("boot race: re-reports once the daemon speaks", "config-sync.test.ts:199", async (_h, c) => {
  const sync = arrival(c, async () =>
    c.reads <= 2 ? NO_MODEM : { value: true, fresh: true, sim: { state: "ready", present: 1 } });
  await sync.publishReport("boot");
  await delay(120);
  sync.stop();
});
await scenario("boot race: no watch when the modem already spoke", "config-sync.test.ts:221", async (_h, c) => {
  const sync = arrival(c, async () => ({ value: true, fresh: true, sim: { state: "ready", present: 1 } }));
  await sync.publishReport("boot");
  await delay(120);
  sync.stop();
});
await scenario("boot race: gives up on a device with no modem", "config-sync.test.ts:235", async (_h, c) => {
  const sync = arrival(c, async () => NO_MODEM);
  await sync.publishReport("boot");
  await delay(300);
  sync.stop();
});
await scenario("boot race: survives a probe that throws", "config-sync.test.ts:249", async (_h, c) => {
  const sync = arrival(c, async () => { throw new Error("telemetry exploded"); });
  await sync.publishReport("boot");
  await delay(60);
  sync.stop();
});

// --- ConfigSync -------------------------------------------------------------------
function harness(c: Capture, over: Record<string, unknown> = {}) {
  return new ConfigSync(base(c, {
    topic: "xorgate/devices/dev-1/config/reported",
    settings: () => ({ gnssAntBias: true }),
    modem: async () => { c.reads += 1; return FRESH; },
    debounceMs: 1,
    ...over,
  }));
}
await scenario("coalesces boot+cloud+boot into one cloud report", "config-sync.test.ts:620", async (_h, c) => {
  const sync = harness(c);
  sync.request("boot");
  sync.request("cloud");
  sync.request("boot");
  await delay(30);
  sync.stop();
});
await scenario("publishes to the reported topic", "config-sync.test.ts:632", async (_h, c) => {
  const sync = harness(c, { settings: () => ({}), modem: async () => { c.reads += 1; return NO_MODEM; } });
  sync.request("boot");
  await delay(30);
  sync.stop();
});
await scenario("advances lastAppliedRev only when applied", "config-sync.test.ts:648", async (_h, c) => {
  const sync = harness(c);
  const d = desiredFrom({ rev: 5, gnssAntBias: true });
  const steps: unknown[] = [sync.shouldApply(5)];
  sync.noteCloudConfig(5, d.raw, d.parsed, false);
  steps.push(sync.lastAppliedRev);
  const afterSkip = fileText(configSyncPath());
  sync.noteCloudConfig(5, d.raw, d.parsed, true);
  steps.push(sync.lastAppliedRev, sync.shouldApply(5), sync.shouldApply(6), loadConfigSyncState().lastAppliedRev);
  sync.stop();
  return { steps, afterSkip };
});
await scenario("carries the last desired doc on a skipped redelivery", "config-sync.test.ts:666", async (_h, c) => {
  const sync = harness(c);
  const d = desiredFrom({ rev: 3, gnssAntBias: true });
  sync.noteCloudConfig(3, d.raw, d.parsed, true);
  sync.noteCloudConfig(3, d.raw, d.parsed, false);
  sync.request("cloud");
  await delay(30);
  sync.stop();
});
await scenario("settle re-report of a modem-poll namespace", "config-sync.test.ts:680", async (_h, c) => {
  let modemReads = 0;
  const sync = harness(c, {
    topic: "t",
    modem: async () => { c.reads += 1; return { value: modemReads++ > 0, fresh: true }; },
    settleMs: 5,
  });
  const d = desiredFrom({ rev: 4, gnssAntBias: true });
  sync.noteCloudConfig(4, d.raw, d.parsed, true);
  sync.request("cloud");
  for (let i = 0; i < 200 && c.sent.length < 2; i++) await delay(10);
  sync.stop();
});
await scenario("settle re-reports are bounded", "config-sync.test.ts:718", async (_h, c) => {
  const sync = harness(c, {
    topic: "t",
    modem: async () => { c.reads += 1; return { value: false, fresh: true }; },
    settleMs: 5,
    settleReports: 2,
  });
  const d = desiredFrom({ rev: 4, gnssAntBias: true });
  sync.noteCloudConfig(4, d.raw, d.parsed, true);
  sync.request("cloud");
  await delay(120);
  sync.stop();
});
await scenario("no settle for a divergent agent-poll namespace", "config-sync.test.ts:741", async (_h, c) => {
  const sync = harness(c, {
    topic: "t",
    settings: () => ({ timeSync: { offlineSources: false } }),
    modem: async () => { c.reads += 1; return NO_MODEM; },
    settleMs: 5,
  });
  const d = desiredFrom({ rev: 4, timeSync: { offlineSources: true } });
  sync.noteCloudConfig(4, d.raw, d.parsed, true);
  sync.request("cloud");
  await delay(120);
  sync.stop();
});
await scenario("swallows a publish failure", "config-sync.test.ts:763", async (_h, c) => {
  const sync = new ConfigSync(base(c, {
    topic: "t",
    publish: () => { throw new Error("broker gone"); },
    settings: () => ({}),
    modem: async () => { c.reads += 1; return NO_MODEM; },
    debounceMs: 1,
  }));
  await sync.publishReport("boot");
  sync.stop();
});
await scenario("stops queueing once stopped", "config-sync.test.ts:778", async (_h, c) => {
  const sync = harness(c);
  sync.stop();
  sync.request("cloud");
  await delay(30);
});

// --- the settings.json fingerprint watcher (watchMs 5, debounceMs 1, NO_MODEM) ----
function watcher(c: Capture, over: Record<string, unknown> = {}) {
  return new ConfigSync(base(c, {
    topic: "xorgate/devices/dev-1/config/reported",
    modem: async () => { c.reads += 1; return NO_MODEM; },
    debounceMs: 1,
    watchMs: 5,
    ...over,
  }));
}
const syncState = (s: InstanceType<typeof ConfigSync>) => JSON.stringify(s.syncState);
await scenario("watcher: out-of-band edit → local", "config-sync.test.ts:857", async (_h, c) => {
  saveSettings({ mode: "real", gnssAntBias: true });
  ensureConfigSyncBaseline();
  const sync = watcher(c);
  sync.startWatcher();
  saveSettings({ mode: "real", gnssAntBias: false });
  await delay(40);
  sync.stop();
  return { syncState: syncState(sync) };
});
await scenario("watcher: edit made while stopped", "config-sync.test.ts:876", async (_h, c) => {
  saveSettings({ gnssAntBias: true });
  ensureConfigSyncBaseline();
  saveSettings({ gnssAntBias: false });
  const sync = watcher(c);
  sync.startWatcher();
  await delay(30);
  sync.stop();
  return { syncState: syncState(sync) };
});
await scenario("watcher: a write that moved no synced namespace", "config-sync.test.ts:888", async (_h, c) => {
  saveSettings({ mode: "real", gnssAntBias: true });
  ensureConfigSyncBaseline();
  const sync = watcher(c);
  sync.startWatcher();
  saveSettings({ mode: "sim", gnssAntBias: true });
  await delay(40);
  sync.stop();
  return { syncState: syncState(sync), hashMatches: sync.syncState.settingsHash === readSettingsHash() };
});
await scenario("watcher: the worker's own cloud apply is not local", "config-sync.test.ts:901", async (_h, c) => {
  saveSettings({ gnssAntBias: true });
  ensureConfigSyncBaseline();
  const sync = watcher(c);
  sync.startWatcher();
  const m = parseCloudMessage(JSON.stringify({ v: 1, rev: 4, gnssAntBias: false }));
  applyCloudConfig(m.config);
  sync.noteCloudConfig(4, m.raw, m.config, true);
  sync.request("cloud");
  await delay(40);
  sync.stop();
  return { syncState: syncState(sync), settingsFile: fileText(settingsPath()) };
});
await scenario("watcher: pending local change survives a cloud doc", "config-sync.test.ts:921", async (_h, c) => {
  saveSettings({ gnssAntBias: true, timeSync: { offlineSources: true } });
  ensureConfigSyncBaseline();
  const sync = watcher(c, { online: () => false });
  sync.startWatcher();
  saveSettings({ gnssAntBias: true, timeSync: { offlineSources: false } });
  await delay(30);
  const pendingMid = sync.syncState.localPending;
  const m = parseCloudMessage(JSON.stringify({ v: 1, rev: 7, gnssAntBias: false }));
  applyCloudConfig(m.config);
  sync.noteCloudConfig(7, m.raw, m.config, true);
  await delay(30);
  sync.stop();
  return { syncState: syncState(sync), pendingMid };
});
await scenario("watcher: re-sends a local report on the next connect", "config-sync.test.ts:944", async (_h, c) => {
  saveSettings({ gnssAntBias: true });
  ensureConfigSyncBaseline();
  let online = false;
  const sync = watcher(c, { online: () => online });
  sync.startWatcher();
  saveSettings({ gnssAntBias: false });
  await delay(30);
  const pendingMid = sync.syncState.localPending;
  const sentMid = c.sent.length;
  online = true;
  sync.request("boot");
  await delay(30);
  sync.stop();
  return { syncState: syncState(sync), pendingMid, sentMid };
});
await scenario("watcher: reverted edit downgrades to boot", "config-sync.test.ts:965", async (_h, c) => {
  saveSettings({ gnssAntBias: true });
  ensureConfigSyncBaseline();
  const sync = watcher(c, { debounceMs: 40 });
  sync.startWatcher();
  saveSettings({ gnssAntBias: false });
  await delay(10);
  saveSettings({ gnssAntBias: true });
  await delay(80);
  sync.stop();
  return { syncState: syncState(sync) };
});
await scenario("watcher: unreadable settings.json never reports", "config-sync.test.ts:985", async (_h, c) => {
  saveSettings({
    mode: "real",
    gnssAntBias: true,
    timeSync: { offlineSources: true },
    imuMount: { enabled: true, rollDeg: 90, pitchDeg: 0, yawDeg: 90 },
  });
  ensureConfigSyncBaseline();
  const baselineFile = fileText(configSyncPath());
  const sync = watcher(c, { readSettings: () => ({ ok: false, reason: "unreadable" }) });
  sync.startWatcher();
  writeFileSync(settingsPath(), '{"gnssAntBias": true, "mode": "real"}');
  await delay(40);
  sync.stop();
  return { syncState: syncState(sync), baselineFile };
});
await scenario("watcher: warns once while unreadable", "config-sync.test.ts:1010", async (_h, c) => {
  saveSettings({ gnssAntBias: true });
  ensureConfigSyncBaseline();
  const sync = watcher(c, { readSettings: () => ({ ok: false, reason: "invalid" }) });
  sync.startWatcher();
  writeFileSync(settingsPath(), "{ this is not json");
  await delay(40);
  sync.stop();
});
await scenario("watcher: an ABSENT settings.json is an erase", "config-sync.test.ts:1028", async (_h, c) => {
  saveSettings({ gnssAntBias: true, timeSync: { offlineSources: true } });
  ensureConfigSyncBaseline();
  const sync = watcher(c);
  sync.startWatcher();
  rmSync(settingsPath());
  await delay(40);
  sync.stop();
  return { syncState: syncState(sync) };
});
await scenario("noteSettingsWritten refuses an unreadable file", "config-sync.test.ts:1040", async (_h, c) => {
  saveSettings({ gnssAntBias: true, timeSync: { offlineSources: true } });
  ensureConfigSyncBaseline();
  const before = fileText(configSyncPath());
  const sync = watcher(c, { readSettings: () => ({ ok: false, reason: "unreadable" }) });
  sync.noteSettingsWritten(["gnssAntBias", "timeSync"]);
  sync.stop();
  return { syncState: syncState(sync), before };
});

// --- checkApnChange (config-sync-cellular.test.ts) ----------------------------------
await scenario("apn watch: reports when the machinery moves", "config-sync-cellular.test.ts:263", async (_h, c) => {
  let block: Record<string, unknown> = { state: "applying", apn_requested: "new.apn" };
  const sync = new ConfigSync(base(c, {
    settings: () => ({}),
    modem: async () => { c.reads += 1; return { value: true, fresh: true, apn: block }; },
    debounceMs: 1,
  }));
  await sync.publishReport("boot");
  await sync.checkApnChange();
  await delay(30);
  const sentMid = c.sent.length;
  block = { state: "in-force", apn_applied: "new.apn", fails: 0 };
  await sync.checkApnChange();
  await delay(30);
  sync.stop();
  return { sentMid };
});
await scenario("apn watch: silent without APN machinery", "config-sync-cellular.test.ts:284", async (_h, c) => {
  const sync = new ConfigSync(base(c, {
    settings: () => ({}),
    modem: async () => { c.reads += 1; return { value: true, fresh: true }; },
    debounceMs: 1,
  }));
  await sync.publishReport("boot");
  await sync.checkApnChange();
  await delay(30);
  sync.stop();
});

writeFileSync(join(OUT, "class-scenarios.json"), JSON.stringify({
  jsSourceFile: "src/agent/config-sync.ts (class ConfigSync :1211-1651) via test/config-sync.test.ts + test/config-sync-cellular.test.ts scenarios",
  jsCommit: "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  generatedBy: "plans/device-agent-js-to-rust/phase-2/evidence/wp3-tools/dump-config-sync.mts (npx tsx, real timers; now() pinned to 1786017600000)",
  cases: scenarios,
}, null, 2) + "\n");

// =================================================================================
// rules.json — pure kernels, each expectation OBSERVED on the real class.
// =================================================================================
const rules: Record<string, unknown>[] = [];

// 1. Request coalescing (`request`, :1435-1449): the origin of the ONE report a
//    burst of requests produces, with and without a pending local change. The
//    sync state carries a namespace baseline `{}` and settings hold gnssAntBias,
//    so a `local` report keeps its origin (non-empty diff) and the observed
//    origin is exactly the coalesced one.
const SEQS = [["boot"], ["cloud"], ["local"], ["boot", "cloud", "boot"], ["cloud", "boot"],
  ["local", "cloud"], ["boot", "local", "boot"], ["cloud", "local"], ["boot", "boot"]];
for (const localPending of [false, true]) {
  for (const seq of SEQS) {
    const h = home();
    saveConfigSyncState({ lastAppliedRev: 0, lastReportTs: 0, namespaces: {}, ...(localPending ? { localPending: true } : {}) });
    const c = capture();
    const sync = new ConfigSync(base(c, { settings: () => ({ gnssAntBias: true }), modem: async () => NO_MODEM, debounceMs: 1 }));
    for (const o of seq) sync.request(o);
    await delay(20);
    sync.stop();
    rules.push({
      name: `coalesce ${seq.join("+")} localPending=${localPending}`,
      op: "nextOrigin",
      input: { seq, localPending },
      expected: { origin: JSON.parse(c.sent[0]).origin, reports: c.sent.length },
    });
    rmSync(h, { recursive: true, force: true });
  }
}

// 2. Settle predicate (`scheduleSettleReport`, :1638-1650): does a report built
//    from (settings, modem, desired) schedule ONE re-report, and with which origin?
const SETTLE: { name: string; settings: Record<string, unknown>; modem: Record<string, unknown>; msg: Record<string, unknown>; origin: string }[] = [
  { name: "gnssAntBias adjusted (modem behind)", settings: { gnssAntBias: true }, modem: { value: false, fresh: true }, msg: { rev: 4, gnssAntBias: true }, origin: "cloud" },
  { name: "gnssAntBias applied", settings: { gnssAntBias: true }, modem: { value: true, fresh: true }, msg: { rev: 4, gnssAntBias: true }, origin: "cloud" },
  { name: "gnssAntBias, modem unavailable", settings: { gnssAntBias: true }, modem: NO_MODEM, msg: { rev: 4, gnssAntBias: true }, origin: "boot" },
  { name: "timeSync divergent (agent-poll)", settings: { timeSync: { offlineSources: false } }, modem: NO_MODEM, msg: { rev: 4, timeSync: { offlineSources: true } }, origin: "cloud" },
  { name: "lteRecovery divergent (modem-poll)", settings: { lteRecovery: { gpsFixResetPolicy: "never" } }, modem: FRESH, msg: { rev: 4, lteRecovery: { gpsFixResetPolicy: "aggressive" } }, origin: "cloud" },
  { name: "recording clamped (agent-poll adjusted)", settings: { recording: { telemetry: { rateHz: 20 } } }, modem: FRESH, msg: { rev: 4, recording: { telemetry: { rateHz: 999 } } }, origin: "cloud" },
  { name: "gnssAntBias adjusted, local origin", settings: { gnssAntBias: true }, modem: { value: false, fresh: true }, msg: { rev: 4, gnssAntBias: true }, origin: "local" },
  { name: "no desired doc", settings: { gnssAntBias: true }, modem: { value: false, fresh: true }, msg: {}, origin: "boot" },
];
for (const s of SETTLE) {
  const h = home();
  saveConfigSyncState({ lastAppliedRev: 0, lastReportTs: 0, namespaces: {} });
  const c = capture();
  const sync = new ConfigSync(base(c, {
    settings: () => s.settings,
    modem: async () => s.modem,
    debounceMs: 1,
    settleMs: 5,
    settleReports: 1,
  }));
  if (Object.keys(s.msg).length > 0) {
    const d = desiredFrom(s.msg);
    sync.noteCloudConfig(d.rev, d.raw, d.parsed, false); // never touches settings
  }
  await sync.publishReport(s.origin);
  await delay(40);
  sync.stop();
  const first = JSON.parse(c.sent[0]);
  rules.push({
    name: `settle: ${s.name}`,
    op: "needsSettleReport",
    input: { status: first.status, origin: first.origin },
    expected: { settle: c.sent.length > 1, settleOrigin: c.sent.length > 1 ? JSON.parse(c.sent[1]).origin : null },
  });
  rmSync(h, { recursive: true, force: true });
}

// 3. Modem-arrival trigger (`armModemArrivalWatch`, :1593-1631): the first
//    report sees no modem; does the 1st arrival probe returning `sample` fire one
//    extra report?
const ARRIVAL = [
  { name: "sim only", sample: { value: null, fresh: false, detail: "modem has not applied a bias state yet", sim: { state: "ready", present: 1 } } },
  { name: "uplink only", sample: { value: true, fresh: true, uplink: { primary: "wlan0", modem_only: 0 } } },
  { name: "apn only", sample: { value: true, fresh: true, apn: { state: "in-force", apn_applied: "x" } } },
  { name: "bias only", sample: { value: true, fresh: true } },
  { name: "unavailable", sample: NO_MODEM },
];
for (const a of ARRIVAL) {
  const h = home();
  const c = capture();
  let n = 0;
  const sync = new ConfigSync(base(c, {
    settings: () => ({}),
    modem: async () => (n++ === 0 ? NO_MODEM : a.sample),
    debounceMs: 1,
    modemArrivalMs: 10,
  }));
  await sync.publishReport("boot");
  await delay(15); // exactly one probe
  sync.stop();
  rules.push({
    name: `arrival: ${a.name}`,
    op: "modemArrivalTrigger",
    input: { sample: a.sample },
    expected: { trigger: c.events.some((e) => e.includes("modem telemetry arrived")) },
  });
  rmSync(h, { recursive: true, force: true });
}

// 4. APN fingerprint (`JSON.stringify(sample.apn)` vs `JSON.stringify(report.effective.apn)`).
const APN_RAW: unknown[] = [
  { state: "in-force", apn_applied: "sp.mb.com", fails: 0 },
  { state: "applying", apn_requested: "new.apn", changed_at: "2026-08-06T11:59:00Z" },
  { state: "rolled-back", apn_applied: "old", apn_requested: "bad", last_error: "no attach", fails: 3 },
  { state: "bogus" },
  undefined,
];
for (const raw of APN_RAW) {
  const apn = parseApnTelemetry(raw);
  const report = buildConfigReport({
    deviceId: "d", ts: NOW, origin: "boot", lastAppliedRev: 0, settings: {},
    modem: { value: true, fresh: true, ...(apn === undefined ? {} : { apn }) }, desired: null,
  });
  rules.push({
    name: `apn fingerprint ${JSON.stringify(raw) ?? "undefined"}`,
    op: "apnFingerprint",
    input: { apn: apn ?? null },
    expected: {
      sample: apn === undefined ? null : JSON.stringify(apn),
      effective: report.effective.apn === undefined ? null : JSON.stringify(report.effective.apn),
    },
  });
}

// 5. Publish outcome (`publishReport`, :1513-1569): empty-local-diff downgrade +
//    persist-after-publish, observed as (sent origin, changed, config-sync.json).
const OUTCOME = [
  { name: "local with a diff, online", state: { namespaces: { gnssAntBias: true }, localPending: true }, settings: { gnssAntBias: false }, origin: "local", online: true },
  { name: "local with a diff, offline", state: { namespaces: { gnssAntBias: true }, localPending: true }, settings: { gnssAntBias: false }, origin: "local", online: false },
  { name: "local reverted (empty diff)", state: { namespaces: { gnssAntBias: true }, localPending: true }, settings: { gnssAntBias: true }, origin: "local", online: true },
  { name: "local, never baselined", state: {}, settings: { gnssAntBias: true }, origin: "local", online: true },
  { name: "local removal → null", state: { namespaces: { gnssAntBias: true, timeSync: { offlineSources: true } } }, settings: { gnssAntBias: true }, origin: "local", online: true },
  { name: "boot keeps the baseline", state: { namespaces: { gnssAntBias: true }, localPending: true }, settings: { gnssAntBias: false }, origin: "boot", online: true },
  { name: "cloud, offline", state: { settingsHash: "abc", namespaces: {} }, settings: {}, origin: "cloud", online: false },
];
for (const o of OUTCOME) {
  const h = home();
  writeFileSync(configSyncPath(), JSON.stringify({ lastAppliedRev: 2, lastReportTs: 1, ...o.state }));
  const c = capture();
  const sync = new ConfigSync(base(c, { settings: () => o.settings, modem: async () => NO_MODEM, online: () => o.online }));
  await sync.publishReport(o.origin);
  sync.stop();
  const r = JSON.parse(c.sent[0]);
  rules.push({
    name: `publish outcome: ${o.name}`,
    op: "publishOutcome",
    input: { stateFile: fileText(configSyncPath()) === null ? null : JSON.stringify({ lastAppliedRev: 2, lastReportTs: 1, ...o.state }), settings: o.settings, origin: o.origin, online: o.online, now: NOW },
    expected: { origin: r.origin, changed: r.changed ?? null, stateFile: fileText(configSyncPath()) },
  });
  rmSync(h, { recursive: true, force: true });
}

// 6. `cloud config applied: ${JSON.stringify(parsed)}` summaries (worker.ts:863).
const SUMMARIES: Record<string, unknown>[] = [
  { v: 1, rev: 9, gnssAntBias: true },
  { v: 1, rev: 3, ts: 5, gnssAntBias: false, imuMount: { enabled: true, rollDeg: 90.5, pitchDeg: 0, yawDeg: -90 }, recording: { telemetry: { rateHz: 999 } }, timeSync: { stepThresholdSecs: 12 } },
  { v: 1, rev: 11, uploadBucket: "xorgate-dev-media", scope: { organizationId: "org-1", workspaceId: "ws-a" }, hardware: { v: 2, platform: "rpi-cm4", encoder: "hw-h264", rails: false, statusLed: null, imu: "bno085", video: [{ key: "cam0", connector: "csi0", sensor: "imx290" }] }, cellular: { apn: "sp.mb.com" }, cameraMount: { cameras: { cam0: { rotationDeg: 180 } } } },
  { v: 1, imuMount: {}, recording: null, lteRecovery: { gpsFixResetPolicy: "never", deadZoneProfile: "x" }, hardware: { v: 1 } },
  { v: 1, rev: 2, recording: { retention: { maxBytes: 0.000005, minFreeBytes: 1e21 } } },
];
for (const m of SUMMARIES) {
  const parsed = parseCloudMessage(JSON.stringify(m));
  rules.push({
    name: `summary ${JSON.stringify(m).slice(0, 60)}`,
    op: "appliedSummary",
    input: { payload: JSON.stringify(m) },
    expected: { summary: JSON.stringify(parsed.config) },
  });
}

// 7. config-sync.json byte shapes (saveConfigSyncState / ensureConfigSyncBaseline).
{
  const h = home();
  saveConfigSyncState({ lastAppliedRev: 42, lastReportTs: 1700 });
  rules.push({ name: "save: zeros + rev", op: "stateFile", input: { step: "save42" }, expected: { file: fileText(configSyncPath()) } });
  saveSettings({ mode: "real", gnssAntBias: true, recording: { shed: { railLowVolts: 4.75, railRestoreVolts: 5 } }, imuMount: { enabled: true, rollDeg: 0.1, pitchDeg: 0, yawDeg: 90 } });
  ensureConfigSyncBaseline();
  rules.push({ name: "baseline over a Phase-3 file", op: "stateFile", input: { step: "baseline", settings: fileText(settingsPath()) }, expected: { file: fileText(configSyncPath()), hash: readSettingsHash() } });
  rmSync(h, { recursive: true, force: true });
}

// 8. The boot upgrades (worker.ts:92-105): ensureConfigSyncBaseline FIRST, then
//    migrateMediaEnabled over the manifest's stream keys.
{
  const sm = await import(join(process.cwd(), "src/daemon/settings-migrate.ts"));
  const mf = await import(join(process.cwd(), "src/media/manifest.ts"));
  for (const [name, settingsText, channels] of [
    ["mediaEnabled false, two cameras", '{"mode":"real","gnssAntBias":true,"mediaEnabled":false,"timeSync":{"offlineSources":true}}', ["cam0", "cam1"]],
    ["mediaEnabled false, no manifest", '{"mediaEnabled":false}', null],
    ["mediaEnabled true", '{"mode":"real","mediaEnabled":true}', ["cam0"]],
    ["no legacy key", '{"mode":"real","gnssAntBias":false}', ["cam0"]],
  ] as [string, string, string[] | null][]) {
    const h = home();
    writeFileSync(settingsPath(), settingsText);
    const kvs = join(h, "kvs.json");
    if (channels) {
      writeFileSync(kvs, JSON.stringify({ deviceId: "dev-1", region: "us-east-1", credentialEndpoint: "c.example", roleAlias: "ra",
        channels: channels.map((k) => ({ streamKey: k, channelName: `dev-1-${k}` })) }));
    }
    ensureConfigSyncBaseline();
    const streamKeys = (mf.loadManifest(kvs)?.channels ?? []).map((c: { streamKey: string }) => c.streamKey);
    const migration = sm.migrateMediaEnabled(streamKeys);
    rules.push({
      name: `boot: ${name}`,
      op: "bootUpgrade",
      input: { settings: settingsText, channels },
      expected: { migration, configSyncFile: fileText(configSyncPath()), settingsFile: fileText(settingsPath()) },
    });
    rmSync(h, { recursive: true, force: true });
  }
}

writeFileSync(join(OUT, "rules.json"), JSON.stringify({
  jsSourceFile: "src/agent/config-sync.ts (class ConfigSync :1211-1651, observed) + src/agent/cloud-config.ts parseCloudMessage",
  jsCommit: "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  generatedBy: "plans/device-agent-js-to-rust/phase-2/evidence/wp3-tools/dump-config-sync.mts (npx tsx)",
  cases: rules,
}, null, 2) + "\n");
console.log(`scenarios=${scenarios.length} rules=${rules.length}`);
