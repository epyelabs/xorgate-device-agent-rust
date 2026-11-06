// Phase 5b CF78 (WP4): copied from plans/device-agent-js-to-rust/phase-2/evidence/wp2-tools/dump-lte-scenarios.mts so redump-all.sh
// re-runs it at any ref; only the JS root (cwd), the output dir ($OUT) and the jsCommit stamp
// ($JS_COMMIT_FULL) were changed.
// Phase 2 WP2 — dump the lte collector scenarios (fixtures/collectors/lte-scenarios.json).
// Run from a `git archive 89b5c64` scratch copy (npm ci done):
//   npx tsx dump-lte-scenarios.mts > lte-scenarios.json
// One process: LTE_TELEMETRY_FILE (a module const) points at a scratch file the
// steps rewrite/remove; Date.now / process.hrtime.bigint are mocked to one clock
// (wall = BASE + mono); console.warn is captured per step. Each scenario gets a
// fresh createLteCollector(). The Rust test replays the steps on tokio paused time.
import { mkdtempSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BASE = 1_781_000_000_000;
const SAMPLE = {
  timestamp: "2026-06-13T08:21:45Z",
  csq: 18,
  rssi_dbm: -77,
  csq_interval: 30,
  status: "connected",
  iface: "usb0",
  ip: "10.164.20.3",
};

type Step =
  | { op: "file"; text: string }
  | { op: "absent" }
  | { op: "advance"; ms: number }
  | { op: "read" }
  | { op: "sample" };

const j = (v: unknown) => JSON.stringify(v);
const SCENARIOS: { name: string; steps: Step[] }[] = [
  { name: "maps daemon telemetry to rssi (dBm) + quality (asu) leaves (modem.test.ts:75)", steps: [{ op: "file", text: j(SAMPLE) }, { op: "read" }] },
  { name: "emits modem_temp (°C) when the daemon reports one (modem.test.ts:84)", steps: [{ op: "file", text: j({ ...SAMPLE, modem_temp: 39 }) }, { op: "read" }] },
  { name: "omits modem_temp when the daemon reports null (modem.test.ts:91)", steps: [{ op: "file", text: j({ ...SAMPLE, modem_temp: null }) }, { op: "read" }] },
  {
    name: "maps gnss.health + recovery.resets_24h to numeric leaves (modem.test.ts:99)",
    steps: [{ op: "file", text: j({ ...SAMPLE, gnss: { engine_on: 1, has_fix: 0, health: "degraded" }, recovery: { connect_fails: 0, resets_24h: 2 } }) }, { op: "read" }],
  },
  ...(["ok", "acquiring", "degraded", "unhealthy"] as const).map((health) => ({
    name: `codes gnss health ${health} by severity (modem.test.ts:111)`,
    steps: [{ op: "file", text: j({ ...SAMPLE, gnss: { health } }) }, { op: "read" }] as Step[],
  })),
  {
    name: "omits gnss_health/resets_24h before daemon knowledge or on old daemons (modem.test.ts:126)",
    steps: [{ op: "file", text: j({ ...SAMPLE, gnss: { health: null }, recovery: {} }) }, { op: "read" }, { op: "file", text: j(SAMPLE) }, { op: "read" }],
  },
  { name: "omits the group when the signal is unknown (csq 99) (modem.test.ts:143)", steps: [{ op: "file", text: j({ ...SAMPLE, csq: 99, rssi_dbm: null }) }, { op: "read" }] },
  { name: "omits the group when the daemon hasn't published (modem.test.ts:149)", steps: [{ op: "absent" }, { op: "read" }] },
  {
    name: "warn once while absent/unparseable, re-armed on recovery",
    steps: [
      { op: "absent" }, { op: "read" }, { op: "read" },
      { op: "file", text: "{ not json" }, { op: "read" },
      { op: "file", text: j(SAMPLE) }, { op: "read" },
      { op: "absent" }, { op: "read" }, { op: "read" },
      { op: "file", text: j({ ...SAMPLE, csq: 99 }) }, { op: "read" },
      { op: "absent" }, { op: "read" },
    ],
  },
  {
    name: "non-object JSON documents",
    steps: [
      { op: "file", text: "null" }, { op: "read" }, { op: "file", text: j(SAMPLE) }, { op: "read" },
      { op: "file", text: "5" }, { op: "read" }, { op: "file", text: "\"str\"" }, { op: "read" },
      { op: "file", text: "[]" }, { op: "read" }, { op: "file", text: "{}" }, { op: "read" },
      { op: "file", text: "false" }, { op: "read" }, { op: "file", text: "0" }, { op: "read" },
      { op: "file", text: j({ ...SAMPLE, csq: "18" }) }, { op: "read" },
      { op: "file", text: j({ ...SAMPLE, modem_temp: "39", rssi_dbm: "x", recovery: { resets_24h: "2" }, gnss: { health: "bogus" } }) }, { op: "read" },
    ],
  },
  {
    name: "sample(): ≤1/s re-read, updatedMono only when timestamp|csq changes",
    steps: [
      { op: "file", text: j(SAMPLE) }, { op: "sample" },
      { op: "advance", ms: 500 }, { op: "file", text: j({ ...SAMPLE, csq: 20, rssi_dbm: -73 }) }, { op: "sample" },
      { op: "advance", ms: 500 }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ ...SAMPLE, csq: 20, rssi_dbm: -60, modem_temp: 41 }) }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ ...SAMPLE, timestamp: "2026-06-13T08:22:15Z", csq: 20, rssi_dbm: -60 }) }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ ...SAMPLE, csq: 99 }) }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ ...SAMPLE, csq: 99 }) }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ ...SAMPLE, timestamp: "2026-06-13T08:22:15Z", csq: 20, rssi_dbm: -60 }) }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "absent" }, { op: "sample" },
      { op: "advance", ms: 999 }, { op: "file", text: j(SAMPLE) }, { op: "sample" },
      { op: "advance", ms: 1 }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ timestamp: 5, csq: 18 }) }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ csq: 18 }) }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ timestamp: null, csq: 18 }) }, { op: "sample" },
      { op: "advance", ms: 1000 }, { op: "file", text: j({ timestamp: [1, null, "a"], csq: 18 }) }, { op: "sample" },
      { op: "read" },
    ],
  },
];

const dir = mkdtempSync(join(tmpdir(), "xg-lte-dump-"));
const file = join(dir, "telemetry.json");
process.env.LTE_TELEMETRY_FILE = file;
let mono = 0;
Date.now = () => BASE + mono;
process.hrtime.bigint = () => BigInt(mono) * 1_000_000n;
const warns: string[] = [];
console.warn = (...a: unknown[]) => void warns.push(a.map(String).join(" "));
const { createLteCollector } = await import("./src/agent/collectors/lte.ts");

const cases: unknown[] = [];
for (const sc of SCENARIOS) {
  mono = 0;
  if (existsSync(file)) rmSync(file);
  const c = createLteCollector();
  const steps: unknown[] = [];
  for (const step of sc.steps) {
    const before = warns.length;
    let result: unknown = undefined;
    switch (step.op) {
      case "file":
        writeFileSync(file, step.text);
        break;
      case "absent":
        if (existsSync(file)) rmSync(file);
        break;
      case "advance":
        mono += step.ms;
        break;
      case "read":
        result = await c.read();
        break;
      case "sample": {
        const s = await c.sample!();
        result = s ? { fields: s.fields, updatedMonoMs: Number(s.updatedMono / 1_000_000n) } : null;
        break;
      }
    }
    steps.push({ ...step, ...(result !== undefined ? { result } : {}), warns: warns.slice(before) });
  }
  cases.push({ name: sc.name, input: { baseWallMs: BASE }, steps });
}
rmSync(dir, { recursive: true, force: true });
process.stdout.write(
  JSON.stringify(
    {
      jsSourceFile: "src/agent/collectors/lte.ts (createLteCollector) + src/hardware/modem.ts (readModemTelemetry)",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy:
        "npx tsx tools/replay/js/dump-lte-scenarios.mts from the root of the shared JS build at jsCommit (phase-2 WP2 driver, written at 89b5c64; re-run per CF78 / redump-all.sh; LTE_TELEMETRY_FILE scratch file, Date.now/hrtime mocked to wall = baseWallMs + mono)",
      cases,
    },
    null,
    2,
  ) + "\n",
);
