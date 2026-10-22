// Phase 5b WP3 — golden outputs of the LED telemetry encoding and merges
// (`src/lib/led-state.ts:200-293`, `src/agent/led-telemetry.ts` @ 09da38d)
// for every input of `test/led-telemetry.test.ts` (27 its) + extras, from the
// REAL modules; plus the recorder it (`TelemetryRecorder` over 30 ticks with
// the LED wrapper) as the recorded `system` lines.
//
//   JS_COMMIT=<sha> run-tsx dump-led-telemetry.mts > fixtures/led_telemetry/led-telemetry.json
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";
import {
  HEALTH_FLAG_BITS,
  LED_LADDER,
  LED_STATE_CODES,
  encodeLedTelemetry,
  resolveLedState,
  rung,
  type LedSignals,
} from "../src/lib/led-state.js";
import {
  LED_TELEMETRY_SIM,
  LED_TELEMETRY_STARTING,
  withLedLeaves,
  withLedSampleGroups,
} from "../src/agent/led-telemetry.js";
import type { RecordingStatus } from "../src/agent/telemetry.js";
import { TelemetryRecorder } from "../src/recording/telemetry-recorder.js";

const RECORDING: RecordingStatus = {
  enabled: true,
  sessionId: "s1",
  backlogBytes: 0,
  backlogSegments: 0,
  droppedSegments: 0,
  uploading: false,
  lastUploadAt: null,
};
const healthy = (over: Partial<LedSignals> = {}): LedSignals => ({
  connection: "connected",
  recording: { ...RECORDING },
  lte: { status: "connected" },
  gnss: { fix: true, sky: "open" as never },
  starting: false,
  ...over,
});
const rec = (over: Partial<RecordingStatus>): RecordingStatus => ({ ...RECORDING, ...over });
const block = (reason: string): Partial<RecordingStatus> => ({ liveBlock: { reason, detail: "", since: 0 } as never });
const unfixed = { fix: false, sky: "open" as never };

// ---- encode: (name, signals[, explicit state]) ----
const enc: { name: string; signals: LedSignals; state?: { colour: string; pattern: string; reason: string } }[] = [
  { name: "healthy → 0 / 0", signals: healthy() },
  { name: "starting → 1", signals: healthy({ starting: true }) },
  { name: "starting, cloud connecting → 1 + mqtt_down", signals: healthy({ starting: true, connection: "connecting" }) },
  { name: "gps-wait from an unfixed receiver", signals: healthy({ gnss: unfixed }) },
  { name: "gps-wait from the GPS gate", signals: healthy({ recording: rec(block("gps-gate")) }) },
  { name: "network from LTE disconnected", signals: healthy({ lte: { status: "disconnected" } }) },
  { name: "network from LTE connecting", signals: healthy({ lte: { status: "connecting" } }) },
  ...(["connecting", "reconnecting", "closed", "error"] as const).map((connection) => ({
    name: `network from the cloud link alone (${connection})`,
    signals: healthy({ connection }),
  })),
  { name: "thermal", signals: healthy({ recording: rec(block("thermal")) }) },
  { name: "thermal with the shed it carries", signals: healthy({ recording: rec({ ...block("thermal"), powerShed: "no-live" }) }) },
  { name: "undervoltage from the verdict", signals: healthy({ recording: rec(block("undervoltage")) }) },
  { name: "undervoltage from a bare power shed", signals: healthy({ recording: rec({ powerShed: "no-live" }) }) },
  { name: "agent-down encodes as 6", signals: healthy(), state: { ...rung("agent-down") } },
  { name: "a network outage masks an unfixed GPS in the code, not in the flags", signals: healthy({ connection: "reconnecting", lte: { status: "disconnected" }, gnss: unfixed }) },
  { name: "a power shed on a GPS-gated device shows red; the gate stays visible", signals: healthy({ recording: rec({ ...block("gps-gate"), powerShed: "no-live" }), gnss: unfixed }) },
  { name: "a dead zone is green with the cloud down; both flagged", signals: healthy({ connection: "closed", lte: { status: "dead_zone" } }) },
  { name: "an unknown liveBlock reason sets only live_block", signals: healthy({ recording: rec(block("kvs-master-failed")) }) },
  { name: "no receiver and an unreadable LTE file are clear", signals: healthy({ gnss: null, lte: null }) },
  { name: "every flag at once", signals: healthy({ connection: "closed", lte: { status: "connecting" }, gnss: { fix: false, sky: "blocked" as never }, recording: rec({ ...block("thermal"), powerShed: "parked:no-live" }) }) },
  { name: "an unknown reason encodes as 0", signals: healthy(), state: { colour: "green", pattern: "heartbeat", reason: "nope" } },
  // extras
  { name: "extra: lte with no status is clear", signals: healthy({ lte: {} }) },
  { name: "extra: lte status unknown word", signals: healthy({ lte: { status: "registering" } }) },
  { name: "extra: dead zone with LTE fine and cloud fine", signals: healthy({ lte: { status: "dead_zone" } }) },
  { name: "extra: undervoltage verdict + shed + gps unfixed + cloud down", signals: healthy({ connection: "error", gnss: unfixed, recording: rec({ ...block("undervoltage"), powerShed: "parked" }) }) },
  { name: "extra: starting masks a thermal block", signals: healthy({ starting: true, recording: rec(block("thermal")) }) },
];
const encodeCases = enc.map((c) => {
  const state = c.state ?? resolveLedState(c.signals);
  return { name: c.name, input: { signals: c.signals, state }, expected: encodeLedTelemetry(state as never, c.signals) };
});

// ---- live merge ----
const metrics = {
  system: {
    cpu_usage: { value: 12.3, unit: "%" },
    cpu_temp: { value: 61.2, unit: "°C" },
    ram_usage: { value: 40, unit: "%" },
    disk_usage: { value: 50, unit: "%" },
    throttled: { value: 0 },
  },
  gps: { fix_quality: { value: 1 } },
};
const recorded = { system: { ...metrics.system, led_state: { value: 4 }, health_flags: { value: 2 } } };
const leavesCases = [
  { name: "adds both leaves to system, unit-less", metrics, t: { led_state: 3, health_flags: 12 } },
  { name: "creates the system group when the collector produced none", metrics: {}, t: LED_TELEMETRY_STARTING },
  { name: "replay (null) passes recorded system.* through untouched", metrics: recorded, t: null },
  { name: "the simulator is healthy", metrics: {}, t: LED_TELEMETRY_SIM },
  { name: "extra: system after another group keeps its position", metrics: { imu: { a: { value: 1 } }, system: { cpu_usage: { value: 1, unit: "%" } }, lte: { rssi: { value: -70 } } }, t: { led_state: 0, health_flags: 0 } },
  { name: "extra: no system among other groups → appended last", metrics: { imu: { a: { value: 1 } }, gps: { fix_quality: { value: 1 } } }, t: { led_state: 2, health_flags: 48 } },
  { name: "extra: existing leaves are overwritten in place", metrics: recorded, t: { led_state: 1, health_flags: 0 } },
].map((c) => {
  const before = JSON.stringify(c.metrics);
  const out = withLedLeaves(c.metrics as never, c.t);
  if (JSON.stringify(c.metrics) !== before) throw new Error("mutated");
  return { name: c.name, input: { metrics: c.metrics, t: c.t }, expected: { out, same: out === c.metrics } };
});

// ---- recorder merge (updatedMono as a number of ns) ----
const groupsCases = [
  { name: "keeps the system sample's freshness stamp and copies", groups: { system: { fields: { cpu_usage: { value: 1, unit: "%" } }, updatedMono: 7 } }, t: { led_state: 2, health_flags: 16 } },
  { name: "null passes through", groups: { system: { fields: { cpu_usage: { value: 1, unit: "%" } }, updatedMono: 7 } }, t: null },
  { name: "extra: no system group → synthesised at nowMono, appended last", groups: { imu: { fields: { a: { value: 1 } }, updatedMono: 3 } }, t: { led_state: 3, health_flags: 8 } },
  { name: "extra: system among others keeps position", groups: { system: { fields: {}, updatedMono: 5 }, imu: { fields: { a: { value: 1 } }, updatedMono: 6 } }, t: LED_TELEMETRY_STARTING },
].map((c) => {
  const g = Object.fromEntries(Object.entries(c.groups).map(([k, v]) => [k, { fields: v.fields, updatedMono: BigInt(v.updatedMono) }]));
  const out = withLedSampleGroups(g as never, c.t, () => 99n);
  const plain = Object.fromEntries(Object.entries(out).map(([k, v]) => [k, { fields: (v as { fields: unknown }).fields, updatedMono: Number((v as { updatedMono: bigint }).updatedMono) }]));
  return { name: c.name, input: { groups: c.groups, t: c.t, nowMono: 99 }, expected: { out: plain, same: out === g } };
});

// ---- the recorder it: 30 ticks at 10 Hz, system refreshes 1/s ----
const home = mkdtempSync(join(tmpdir(), "xg-ledtel-"));
let now = 10_000;
const realNow = Date.now;
Date.now = () => now;
const dir = join(home, "recordings", "telemetry", "session-x");
let mono = 0n;
let state = { led_state: 0, health_flags: 0 };
const recorder = new TelemetryRecorder({
  rateHz: 10,
  sampleGroups: async () => {
    mono += 1n;
    const sysMono = BigInt(Math.floor(Date.now() / 1000));
    return withLedSampleGroups(
      {
        imu: { fields: { a: { value: Number(mono) } }, updatedMono: mono },
        system: { fields: { cpu_usage: { value: 5, unit: "%" } }, updatedMono: sysMono },
      } as never,
      state,
    );
  },
  timeSource: { current: () => "ntp", refresh: async () => "ntp" } as never,
  monoMs: () => Date.now(),
  writer: {
    dir,
    header: () => ({ type: "header", v: 1, deviceId: "dev-1", sessionId: "session-x", bootId: "boot-1", timeSource: "ntp", rateHz: 10, startedAt: Date.now() }) as never,
    maxAgeMs: 3_600_000,
  },
} as never);
const ticks: { i: number; at: number; state: typeof state }[] = [];
for (let i = 0; i < 30; i++) {
  if (i === 5) state = { led_state: 3, health_flags: 8 };
  ticks.push({ i, at: now, state });
  await recorder.tick();
  now = 10_000 + (i + 1) * 100;
}
recorder.stop();
const gz = readdirSync(dir).filter((n) => n.endsWith(".jsonl.gz"));
const lines = gunzipSync(readFileSync(join(dir, gz[0]!))).toString("utf8").split("\n").filter((l) => l.length > 0).map((l) => JSON.parse(l));
const systemLines = lines.filter((l) => l.metrics?.system).map((l) => ({ ts: l.ts, system: l.metrics.system }));
Date.now = realNow;
rmSync(home, { recursive: true, force: true });

process.stdout.write(
  JSON.stringify(
    {
      jsSourceFile: "src/lib/led-state.ts (encodeLedTelemetry) + src/agent/led-telemetry.ts + src/recording/telemetry-recorder.ts",
      jsCommit: process.env.JS_COMMIT ?? "09da38d",
      generatedBy: "tools/replay/js/dump-led-telemetry.mts (phase-5b WP3): the inputs of test/led-telemetry.test.ts + extras through the REAL modules",
      LED_STATE_CODES,
      HEALTH_FLAG_BITS,
      ladderReasons: LED_LADDER.map((r) => r.reason),
      LED_TELEMETRY_STARTING,
      LED_TELEMETRY_SIM,
      cases: [
        ...encodeCases.map((c) => ({ op: "encode", ...c })),
        ...leavesCases.map((c) => ({ op: "withLedLeaves", ...c })),
        ...groupsCases.map((c) => ({ op: "withLedSampleGroups", ...c })),
      ],
      recorder: { ticks: ticks.length, systemLines },
    },
    null,
    1,
  ) + "\n",
);
