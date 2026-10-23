// WP2 (phase 3) driver: run the REAL TelemetryRecorder + SegmentWriter of
// JS @ 89b5c64 over scripted tick sequences and dump, per case, which groups
// each tick wrote (core recorder_gate fixture) and every finalized segment's
// DECOMPRESSED bytes (agent recorder byte comparison).
//
//   npx tsx .rig-drivers/dump-recorder.mts <outdir>
//   → <outdir>/recorder_gate/telemetry-recorder.json
//
// Clocks are scripted per step: Date.now (names + ts + header startedAt), the
// recorder's monoMs seam (cadence + rotation age) and process.hrtime.bigint
// (the sample `mono` ns). The time source is a fake tracker set per step.
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";
import { createHash } from "node:crypto";
import { TelemetryRecorder } from "../src/recording/telemetry-recorder.js";

type Step = {
  wall: number;
  mono: number;
  monoNs: number;
  timeSource?: string;
  // group -> [updatedMono, fields]; omitted groups are absent from sampleGroups()
  groups: Record<string, [number, Record<string, { value: number; unit?: string }>]>;
};
type CaseIn = {
  name: string;
  rateHz: number;
  maxBytes?: number;
  maxAgeMs?: number;
  nativeCadenceHz?: Record<string, number>;
  initialTimeSource: string;
  steps: Step[];
};

const out = process.argv[2] ?? ".";
const realNow = Date.now;
const realHr = process.hrtime.bigint;

function leaf(value: number, unit?: string) {
  return unit === undefined ? { value } : { value, unit };
}

async function runCase(c: CaseIn) {
  const home = mkdtempSync(join(tmpdir(), "xg-wp2-rec-"));
  const dir = join(home, "recordings", "telemetry", "session-x");
  let wall = c.steps[0]?.wall ?? 0;
  let mono = c.steps[0]?.mono ?? 0;
  let monoNs = 0n;
  let ts = c.initialTimeSource;
  Date.now = () => wall;
  (process.hrtime as any).bigint = () => monoNs;
  let samples: Record<string, any> = {};
  const rec = new TelemetryRecorder({
    rateHz: c.rateHz,
    sampleGroups: async () => ({ ...samples }),
    timeSource: { current: () => ts as any, refresh: async () => ts as any, noteStep: () => {} },
    nativeCadenceHz: c.nativeCadenceHz,
    monoMs: () => mono,
    writer: {
      dir,
      maxBytes: c.maxBytes,
      maxAgeMs: c.maxAgeMs,
      header: () => ({
        type: "header",
        v: 1,
        deviceId: "dev-1",
        sessionId: "session-x",
        bootId: "boot-1",
        timeSource: ts as any,
        rateHz: c.rateHz,
        startedAt: Date.now(),
      }),
    },
  });
  const writer = (rec as any).writer;
  const origWrite = writer.writeLine.bind(writer);
  let tickWrote: string[] | null = null;
  writer.writeLine = (line: any, now: number) => {
    tickWrote = Object.keys(line.metrics);
    return origWrite(line, now);
  };
  const written: (string[] | null)[] = [];
  const active: (string | null)[] = [];
  for (const s of c.steps) {
    wall = s.wall;
    mono = s.mono;
    monoNs = BigInt(s.monoNs);
    if (s.timeSource) ts = s.timeSource;
    samples = {};
    for (const [g, [um, fields]] of Object.entries(s.groups)) {
      samples[g] = { fields, updatedMono: BigInt(um) };
    }
    tickWrote = null;
    await rec.tick();
    written.push(tickWrote);
    const ap = rec.activePath();
    active.push(ap === null ? null : ap.slice(dir.length + 1));
  }
  rec.stop();
  const files = readdirSync(dir)
    .sort()
    .map((name) => {
      const raw = readFileSync(join(dir, name));
      const text = name.endsWith(".gz") ? gunzipSync(raw).toString("utf8") : raw.toString("utf8");
      const lines = text.split("\n").filter((l) => l.length > 0).length;
      return {
        name,
        bytes: Buffer.byteLength(text),
        lines,
        sha256: createHash("sha256").update(text).digest("hex"),
        ...(text.length <= 6000 ? { text } : {}),
      };
    });
  rmSync(home, { recursive: true, force: true });
  Date.now = realNow;
  (process.hrtime as any).bigint = realHr;
  return { written, active, files };
}

// ------------------------------------------------------------------ cases
const cases: CaseIn[] = [];
const F = (v: number) => ({ a: leaf(v) });

// The five it()s of test/recording-recorder.test.ts (monoMs mirrors the wall
// clock there; here both are scripted explicitly with the same values).
cases.push({
  name: "writes a group only when its cache advanced, at its native cadence",
  rateHz: 10,
  maxAgeMs: 3_600_000,
  initialTimeSource: "ntp",
  steps: [
    { wall: 10_000, mono: 10_000, monoNs: 1, groups: { imu: [1, F(1)], gps: [1, { lat: leaf(9) }] } },
    { wall: 10_100, mono: 10_100, monoNs: 2, groups: { imu: [2, F(1)], gps: [1, { lat: leaf(9) }] } },
    { wall: 10_200, mono: 10_200, monoNs: 3, groups: { imu: [2, F(1)], gps: [1, { lat: leaf(9) }] } },
    { wall: 10_300, mono: 10_300, monoNs: 4, groups: { imu: [2, F(1)], gps: [2, { lat: leaf(9) }] } },
    { wall: 11_200, mono: 11_200, monoNs: 5, groups: { imu: [2, F(1)], gps: [2, { lat: leaf(9) }] } },
  ],
});
{
  const steps: Step[] = [];
  let m = 1;
  for (let t = 0; t < 10; t++) {
    steps.push({ wall: 10_000 + t * 50, mono: 10_000 + t * 50, monoNs: 100 + t, groups: { imu: [++m, F(t)] } });
  }
  cases.push({ name: "caps a fast group at the recorder rate", rateHz: 10, maxAgeMs: 3_600_000, initialTimeSource: "ntp", steps });
}
cases.push({
  name: "re-emits a header when the time source flips mid-segment",
  rateHz: 10,
  maxAgeMs: 3_600_000,
  initialTimeSource: "unsynced",
  steps: [
    { wall: 10_000, mono: 10_000, monoNs: 1, groups: { imu: [1, F(1)] } },
    { wall: 10_100, mono: 10_100, monoNs: 2, timeSource: "ntp", groups: { imu: [2, F(1)] } },
  ],
});
cases.push({
  name: "rotates an idle open segment by age via tick()",
  rateHz: 10,
  maxAgeMs: 500,
  initialTimeSource: "ntp",
  steps: [
    { wall: 10_000, mono: 10_000, monoNs: 1, groups: { imu: [1, F(1)] } },
    { wall: 10_600, mono: 10_600, monoNs: 2, groups: { imu: [1, F(1)] } },
  ],
});
cases.push({
  name: "keeps sampling and rolling across a backward clock step (Phase 8)",
  rateHz: 10,
  maxAgeMs: 1_000,
  initialTimeSource: "ntp",
  steps: [
    { wall: Date.UTC(2080, 0, 6, 0, 0, 52), mono: 0, monoNs: 1, groups: { imu: [1, F(1)] } },
    { wall: 10_000, mono: 2_000, monoNs: 2_000_000_000, groups: { imu: [2, F(2)] } },
    { wall: 10_000, mono: 4_000, monoNs: 4_000_000_000, groups: { imu: [3, F(3)] } },
  ],
});

// Rate sweep: every group updates every tick, ticks at the recorder period
// with a little jitter; "media" is an unknown group (capped at rateHz).
for (const rateHz of [1, 2, 5, 10, 20]) {
  const steps: Step[] = [];
  const period = Math.round(1000 / rateHz);
  let wall = 1_760_000_000_000;
  let mono = 5_000;
  let u = 10;
  for (let i = 0; i < 60; i++) {
    const jitter = [0, 3, -4, 7, -2, 11, -9, 1][i % 8];
    wall += period + jitter;
    mono += period + jitter;
    u += 1;
    const groups: Step["groups"] = {
      imu: [u, { quatR: leaf(0.5 + i / 1000), accelX: leaf(-0.000004 * i, "m/s2") }],
      gps: [i % 3 === 0 ? u : u - (i % 3), { lat: leaf(43.651070 + i * 1e-7), speed: leaf(i * 0.1, "km/h") }],
      lte: [u - (i % 2), { rssi: leaf(-71 - (i % 5), "dBm") }],
      system: [i % 4 === 0 ? u : u - (i % 4), { cpuTempC: leaf(58.1 + i / 7, "C"), big: leaf(1e21), neg0: leaf(-0) }],
      media: [u, { recording: leaf(i % 2) }],
    };
    steps.push({ wall, mono, monoNs: mono * 1_000_000 + 123, groups });
  }
  cases.push({ name: `rate sweep ${rateHz} Hz`, rateHz, initialTimeSource: "rtc", steps });
}

// Default 60 s monotonic age rotation at 1 Hz over 130 s (no overrides), with
// a forward wall step mid-segment (names/ts move, age does not).
{
  const steps: Step[] = [];
  let wall = 1_760_000_000_000;
  for (let i = 0; i < 130; i++) {
    if (i === 70) wall += 3_600_000; // wall step +1 h
    wall += 1000;
    steps.push({ wall, mono: 1000 * (i + 1), monoNs: 1e9 * (i + 1), groups: { system: [i + 1, { cpu: leaf(i) }] } });
  }
  cases.push({ name: "default 60 s monotonic rotation across a wall step", rateHz: 1, initialTimeSource: "ntp", steps });
}

// ~1 MB rotation: wide imu rows at 20 Hz.
{
  const fields: Record<string, { value: number; unit?: string }> = {};
  for (let k = 0; k < 60; k++) fields[`field_${k}_with_a_long_name`] = leaf(k + 0.123456789, "unit-string-x");
  const steps: Step[] = [];
  for (let i = 0; i < 900; i++) {
    const f = { ...fields, i: leaf(i) };
    steps.push({ wall: 1_700_000_000_000 + i * 50, mono: i * 50, monoNs: i * 50_000_000, groups: { imu: [i + 1, f] } });
  }
  // `gen` lets the Rust test rebuild the 900 steps instead of reading them.
  cases.push({ name: "rotates at 1 MB before 60 s", rateHz: 20, initialTimeSource: "gps", steps, gen: { wide: { n: 900, fields: 60 } } } as any);
}

// Time source changes while open, while closed (between segments), and back.
{
  const steps: Step[] = [];
  const seq = ["unsynced", "unsynced", "rtc", "rtc", "nitz", "gps", "gps", "ntp", "ntp", "ntp", "rtc", "ntp"];
  for (let i = 0; i < seq.length; i++) {
    steps.push({
      wall: 1_000_000 + i * 1000,
      mono: i * 1000,
      monoNs: i * 1e9,
      timeSource: seq[i],
      groups: i === 4 || i === 5 ? {} : { gps: [i + 1, { fix: leaf(i % 2) }] },
    });
  }
  cases.push({ name: "time source flips open, idle and across rotation", rateHz: 1, maxAgeMs: 3_500, initialTimeSource: "unsynced", steps });
}

// A cadence override that replaces the table (unknown groups → rateHz).
cases.push({
  name: "nativeCadenceHz override replaces the table",
  rateHz: 10,
  maxAgeMs: 3_600_000,
  nativeCadenceHz: { gps: 5 },
  initialTimeSource: "ntp",
  steps: Array.from({ length: 12 }, (_, i) => ({
    wall: 50_000 + i * 100,
    mono: 50_000 + i * 100,
    monoNs: i,
    groups: { gps: [i + 1, F(i)], imu: [i + 1, F(i)] } as Step["groups"],
  })),
});

const result = [];
for (const c of cases) {
  const r = await runCase(c);
  const input = (c as any).gen ? { ...c, steps: undefined } : c;
  result.push({ name: c.name, input, expected: r });
}
const doc = {
  jsSourceFile: "src/recording/telemetry-recorder.ts + src/recording/segments.ts (test/recording-recorder.test.ts ×5 + scenarios)",
  jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
  generatedBy:
    "npx tsx plans/device-agent-js-to-rust/phase-3/evidence/wp2-tools/dump-recorder.mts (scratch git-archive copy @ 89b5c64; handed to tools/replay/js/ via WP6)",
  cases: result,
};
mkdirSync(join(out, "recorder_gate"), { recursive: true });
writeFileSync(join(out, "recorder_gate", "telemetry-recorder.json"), JSON.stringify(doc) + "\n");
console.log(`wrote ${result.length} cases`);
