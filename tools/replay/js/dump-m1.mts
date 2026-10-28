// Phase 5b CF78 (WP4): copied from plans/device-agent-js-to-rust/phase-1/evidence/dump-m1.mts so redump-all.sh
// re-runs it at any ref; only the JS root (cwd), the output dir ($OUT) and the jsCommit stamp
// ($JS_COMMIT_FULL) were changed.
/**
 * Integration M1 fixture dumper (throwaway; run with `npx tsx` from the JS
 * repo working tree, never committed there): JS-exact number printing on the
 * `Value` path — imuMount 5e-6 through the cloud merge, the config report and
 * config-sync.json; a segment SampleLine with lat 4e-6 and an integer 1e20.
 */
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync, readdirSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";

const R = process.cwd();
const cc = await import(`${R}/src/agent/cloud-config.ts`);
const cs = await import(`${R}/src/agent/config-sync.ts`);
const st = await import(`${R}/src/daemon/settings.ts`);
const segments = await import(`${R}/src/recording/segments.ts`);

const out: Record<string, unknown> = {};
const BUILD = { version: "0.2.0", channel: "local", builtAt: null, packageVersion: "0.2.0" };
const IMU = { enabled: true, rollDeg: 0.000005, pitchDeg: 0, yawDeg: -0.0000015 };
const PAYLOAD = JSON.stringify({ v: 1, rev: 3, imuMount: IMU });

// ---- cloud-config: parse + apply over a tmp settings.json
{
  const home = mkdtempSync(join(tmpdir(), "m1-"));
  const file = join(home, "settings.json");
  const steps: unknown[] = [];
  steps.push({ op: "parseCloudConfig", payload: PAYLOAD, expectedJson: JSON.stringify(cc.parseCloudConfig(PAYLOAD)) });
  steps.push({ op: "parseCloudMessage", payload: PAYLOAD, expectedJson: JSON.stringify(cc.parseCloudMessage(PAYLOAD)) });
  const doc = { mode: "real", gnssAntBias: true };
  st.saveSettings(doc as never, file);
  steps.push({ op: "save", doc, fileAfter: readFileSync(file, "utf8") });
  const config = { imuMount: IMU };
  const r = cc.applyCloudConfig(config as never, file);
  const fileAfter = readFileSync(file, "utf8");
  steps.push({ op: "apply", config, expect: { changed: r.changed, fileAfter, loadedJson: JSON.stringify(st.loadSettings(file)) } });
  out.cloud = { name: "[integration M1] imuMount 5e-6 survives the merge printed like JS", source: "src/agent/cloud-config.ts:293-411 (integration review M1)", steps };

  // ---- config report over the merged file
  const loaded = st.loadSettings(file);
  const base = { gnssAntBias: true };
  const changed = cs.diffNamespaces(base, cs.namespacesOf(loaded));
  const desired = cc.parseCloudMessage(PAYLOAD)!;
  const input = { deviceId: "dev-1", ts: 1234, origin: "local", lastAppliedRev: 3, settings: loaded,
    modem: { value: true, fresh: true }, desired: { rev: 3, raw: desired.raw, parsed: desired.config }, changed };
  const report = cs.buildConfigReport(input as never);
  const cr: unknown[] = [];
  cr.push({ name: "[integration M1] imuMount 5e-6 in effective + changed prints like JS", source: "src/agent/config-sync.ts:1059 (integration review M1)", op: "buildConfigReport",
    input: { deviceId: "dev-1", ts: 1234, origin: "local", lastAppliedRev: 3, settings: loaded, modem: { value: true, fresh: true }, changed, desiredPayload: PAYLOAD, build: BUILD },
    expectedJson: JSON.stringify(report) });
  cr.push({ name: "[integration M1] namespacesOf keeps imuMount 5e-6 JS-printed", source: "src/agent/config-sync.ts:287 (integration review M1)", op: "namespacesOf",
    input: { settings: loaded }, expectedJson: JSON.stringify(cs.namespacesOf(loaded)) });
  cr.push({ name: "[integration M1] diffNamespaces keeps imuMount 5e-6 JS-printed", source: "src/agent/config-sync.ts:301 (integration review M1)", op: "diffNamespaces",
    input: { base, next: cs.namespacesOf(loaded) }, expectedJson: JSON.stringify(changed) });
  cr.push({ name: "[integration M1] buildEffective keeps imuMount 5e-6 JS-printed", source: "src/agent/config-sync.ts:597 (integration review M1)", op: "buildEffective",
    input: { settings: loaded, modem: { value: true, fresh: true }, mode: { $undefined: true }, cameraMount: { $undefined: true }, telemetryScope: { $undefined: true } },
    expectedJson: JSON.stringify(cs.buildEffective(loaded, { value: true, fresh: true } as never)) });
  // config-sync.json baseline written from the merged file
  const statePath = join(home, "config-sync.json");
  const hash = cs.readSettingsHash(file);
  cs.ensureConfigSyncBaseline(statePath, file);
  const stateText = readFileSync(statePath, "utf8");
  cr.push({ name: "[integration M1] config-sync.json baseline keeps imuMount 5e-6 JS-printed", source: "src/agent/config-sync.ts:328 (integration review M1)", op: "ensureConfigSyncBaseline",
    input: { state: { lastAppliedRev: 0, lastReportTs: 0 }, settings: loaded, settingsHash: hash }, expectedJson: `{"state":${stateText},"save":true}` });
  cr.push({ name: "[integration M1] loadConfigSyncState round-trips imuMount 5e-6 and an integer 1e20", source: "src/agent/config-sync.ts:201 (integration review M1)", op: "loadConfigSyncState",
    input: { text: `{"lastAppliedRev":3,"lastReportTs":1234,"namespaces":{"imuMount":{"enabled":true,"rollDeg":0.000005,"pitchDeg":0,"yawDeg":-0.0000015},"x":100000000000000000000,"y":9007199254740993},"settingsHash":"h"}` },
    expectedJson: (() => { const p = join(home, "cs2.json"); writeFileSync(p, `{"lastAppliedRev":3,"lastReportTs":1234,"namespaces":{"imuMount":{"enabled":true,"rollDeg":0.000005,"pitchDeg":0,"yawDeg":-0.0000015},"x":100000000000000000000,"y":9007199254740993},"settingsHash":"h"}`); return JSON.stringify(cs.loadConfigSyncState(p)); })() });
  out.report = cr;
  rmSync(home, { recursive: true, force: true });
}

// ---- segment writer: a sample with lat 4e-6 and an integer 1e20
{
  const HEADER = { type: "header", v: 1, deviceId: "dev-1", sessionId: "session-a", bootId: "boot-1", timeSource: "ntp", rateHz: 10, startedAt: 1000 };
  const home = mkdtempSync(join(tmpdir(), "m1-seg-"));
  const dir = join(home, "recordings", "telemetry", "session-a");
  const closed: string[] = [];
  let mono = 0;
  const w = new segments.SegmentWriter({ dir, header: () => ({ ...HEADER }) as never, monoMs: () => mono, onSegmentClosed: (p: string) => closed.push(p.slice(home.length)) });
  const ops = [
    { op: "writeLine", line: { ts: 1000, mono: 1, seq: 0, metrics: { gps: { lat: { value: 0.000004, unit: "deg" }, lon: { value: -0.0000099 } }, odo: { m: { value: 1e20 } } } }, now: 1000, mono: 0 },
    { op: "finalize" },
  ];
  const steps: unknown[] = [];
  for (const o of ops) {
    if ((o as { mono?: number }).mono !== undefined) mono = (o as { mono: number }).mono;
    if (o.op === "writeLine") w.writeLine((o as { line: never }).line, (o as { now: number }).now);
    else if (o.op === "finalize") w.finalize();
    const ap = w.activePath();
    steps.push({ ...o, activePath: ap === null ? null : ap.slice(home.length), closed: [...closed] });
  }
  const files: Record<string, string> = {};
  const walk = (d: string) => {
    if (!existsSync(d)) return;
    for (const n of readdirSync(d).sort()) {
      const p = join(d, n);
      if (statSync(p).isDirectory()) walk(p);
      else files[p.slice(home.length)] = n.endsWith(".gz") ? gunzipSync(readFileSync(p)).toString("utf8") : readFileSync(p, "utf8");
    }
  };
  walk(home);
  rmSync(home, { recursive: true, force: true });
  out.segment = { name: "[integration M1] a sample's gps lat 4e-6 and an integer 1e20 print like JS", op: "segmentWriter",
    input: { dir: "/recordings/telemetry/session-a", header: HEADER, ctor: {}, monoInjected: true }, expected: { steps, closed, files } };
}

console.log(JSON.stringify(out, null, 2));
