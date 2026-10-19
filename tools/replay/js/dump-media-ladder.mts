// Phase 3 WP5 — dump the KVS master supervisor's behaviour
// (`createMediaCollector`, src/agent/collectors/media.ts:83-380) as script
// traces, from the REAL module with a fake runtime.
//
// media.ts imports `detectRuntime`/`streamChannel` from
// ../../hardware/kvs-webrtc.js. Without editing the JS repo, this driver copies
// the REAL media.ts source text into .rig-drivers/ (inside the scratch copy),
// rewrites only its import specifiers (the kvs-webrtc import → a 2-line fake
// module that forwards to globalThis.__fake; manifest/pipeline → the real
// modules), and imports it. Timers and Date.now run on a virtual clock
// (setTimeout/clearTimeout replaced; one queue ordered by deadline, then
// creation); console.log/warn/error are captured; microtasks are flushed
// after every timer and every script step (setImmediate stays real).
//
//   tools/replay/js/run-tsx.sh dump-media-ladder.mts > fixtures/media_ladder/ladder.json
import { readFileSync, writeFileSync, mkdirSync, rmSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";

const ROOT = process.cwd();
const HERE = join(ROOT, ".rig-drivers");
mkdirSync(HERE, { recursive: true });

// ---- the module under test: the real source, import specifiers rewritten ----
const src = readFileSync(join(ROOT, "src/agent/collectors/media.ts"), "utf8");
const IMPORTS: [string, string][] = [
  [`from "../../media/manifest.js";`, `from "../src/media/manifest.ts";`],
  [`from "../../media/pipeline.js";`, `from "../src/media/pipeline.ts";`],
  [`from "../../hardware/kvs-webrtc.js";`, `from "./fake-kvs-webrtc.ts";`],
];
let mut = src;
for (const [a, b] of IMPORTS) {
  if (mut.split(a).length !== 2) throw new Error(`media.ts import not found exactly once: ${a}`);
  mut = mut.replace(a, b);
}
writeFileSync(join(HERE, "media-under-test.ts"), mut);
writeFileSync(
  join(HERE, "fake-kvs-webrtc.ts"),
  "export const detectRuntime = (...a: any[]) => (globalThis as any).__fake.detectRuntime(...a);\n" +
    "export const streamChannel = (...a: any[]) => (globalThis as any).__fake.streamChannel(...a);\n",
);

// ---- virtual time ----
const T0 = 1_750_000_000_000;
let now = 0;
let seq = 0;
type Timer = { id: number; at: number; seq: number; fn: () => void };
let timers: Timer[] = [];
(globalThis as any).setTimeout = (fn: () => void, ms: number) => {
  const t = { id: ++seq, at: now + (ms ?? 0), seq, fn };
  timers.push(t);
  return t;
};
(globalThis as any).clearTimeout = (t: Timer | null | undefined) => {
  if (t) timers = timers.filter((x) => x !== t);
};
const realNow = Date.now;
Date.now = () => T0 + now;
const flush = async () => {
  for (let i = 0; i < 8; i++) await new Promise((r) => setImmediate(r));
};
async function advanceTo(t: number) {
  for (;;) {
    timers.sort((a, b) => a.at - b.at || a.seq - b.seq);
    const next = timers[0];
    if (!next || next.at > t) break;
    timers.shift();
    now = next.at;
    next.fn();
    await flush();
  }
  now = t;
}

// ---- capture ----
type Ev = Record<string, unknown>;
let trace: Ev[] = [];
for (const level of ["log", "warn", "error"] as const) {
  (console as any)[level] = (line: string) => trace.push({ t: now, k: "log", level, line });
}
const out = (s: string) => process.stdout.write(s);

interface Fake {
  exit: (code: number | null) => void;
  exiting: boolean;
  done: boolean;
}

type Step =
  | { at: number; op: "read" }
  | { at: number; op: "exit"; channel: string; code: number | null }
  | { at: number; op: "shed"; on: boolean }
  | { at: number; op: "disabled"; keys: string[] }
  | { at: number; op: "restart"; key: string }
  | { at: number; op: "stop" }
  | { at: number; op: "guard"; key: string; verdict: "ok" | "no-camera" | "no-pipeline" }
  | { at: number; op: "pipeline"; key: string; present: boolean };

interface Scenario {
  name: string;
  detect: { available: boolean; gstreamer: boolean; reason?: string };
  /** Written to the manifest path as JSON; null = no file; a string = raw text. */
  manifest: unknown;
  guard?: Record<string, "ok" | "no-camera" | "no-pipeline">;
  pipelines?: string[];
  globalPipeline?: string;
  sigintExitMs?: number;
  steps: Step[];
  endAt: number;
}

const DEV = "a73f7a36-dev";
const manifest = (keys: string[]) => ({
  deviceId: DEV,
  region: "us-east-1",
  credentialEndpoint: "c1abcdefgh.credentials.iot.us-east-1.amazonaws.com",
  roleAlias: "xorgate-kvs-role-alias",
  channels: keys.map((k) => ({ streamKey: k, channelName: `${DEV}-${k}` })),
});
const ch = (k: string) => `${DEV}-${k}`;

const SCENARIOS: Scenario[] = [
  {
    name: "quick exits x3 -> failed -> parked 60 s -> retry; a stable master that drops restarts promptly",
    detect: { available: true, gstreamer: true },
    manifest: manifest(["cam0"]),
    steps: [
      { at: 0, op: "read" },
      { at: 100, op: "read" },
      { at: 1000, op: "exit", channel: ch("cam0"), code: 1 },
      { at: 1500, op: "read" },
      { at: 4000, op: "exit", channel: ch("cam0"), code: 1 },
      { at: 9000, op: "exit", channel: ch("cam0"), code: null },
      { at: 10000, op: "read" },
      { at: 70000, op: "read" },
      { at: 85000, op: "read" },
      { at: 100000, op: "exit", channel: ch("cam0"), code: 1 },
      { at: 101000, op: "read" },
      { at: 103000, op: "read" },
    ],
    endAt: 130000,
  },
  {
    name: "stable 15 s resets the backoff and the quick-failure count",
    detect: { available: true, gstreamer: true },
    manifest: manifest(["cam0"]),
    steps: [
      { at: 0, op: "read" },
      { at: 5000, op: "exit", channel: ch("cam0"), code: 1 },
      { at: 8000, op: "exit", channel: ch("cam0"), code: 127 },
      { at: 13000, op: "read" },
      { at: 30000, op: "read" },
      { at: 40000, op: "exit", channel: ch("cam0"), code: 3 },
      { at: 43000, op: "exit", channel: ch("cam0"), code: 1 },
      { at: 44000, op: "read" },
    ],
    endAt: 60000,
  },
  {
    name: "setShed parks every master (SIGINT, no respawn) and releases with a fresh ladder",
    detect: { available: true, gstreamer: true },
    manifest: manifest(["cam0", "cam1"]),
    sigintExitMs: 300,
    steps: [
      { at: 0, op: "read" },
      { at: 20000, op: "read" },
      { at: 21000, op: "shed", on: true },
      { at: 21100, op: "read" },
      { at: 22000, op: "read" },
      { at: 23000, op: "shed", on: true },
      { at: 24000, op: "restart", key: "cam0" },
      { at: 30000, op: "shed", on: false },
      { at: 31000, op: "read" },
      { at: 40000, op: "exit", channel: ch("cam1"), code: 1 },
      { at: 41000, op: "shed", on: true },
      { at: 41500, op: "read" },
      { at: 42000, op: "shed", on: false },
      { at: 50000, op: "shed", on: true },
      { at: 50100, op: "shed", on: false },
      { at: 50200, op: "read" },
      { at: 51000, op: "read" },
    ],
    endAt: 80000,
  },
  {
    name: "shed before the lazy start: channels start stopped, release spawns them",
    detect: { available: true, gstreamer: true },
    manifest: manifest(["cam0"]),
    steps: [
      { at: 0, op: "shed", on: true },
      { at: 0, op: "read" },
      { at: 500, op: "read" },
      { at: 1000, op: "shed", on: false },
      { at: 1500, op: "read" },
    ],
    endAt: 20000,
  },
  {
    name: "setDisabledStreams parks one camera's master; re-enable starts it; remembered before start",
    detect: { available: true, gstreamer: true },
    manifest: manifest(["cam0", "cam1"]),
    sigintExitMs: 300,
    steps: [
      { at: 0, op: "disabled", keys: ["cam1"] },
      { at: 0, op: "read" },
      { at: 1000, op: "read" },
      { at: 5000, op: "disabled", keys: [] },
      { at: 6000, op: "disabled", keys: ["cam0"] },
      { at: 7000, op: "read" },
      { at: 8000, op: "disabled", keys: ["cam0"] },
      { at: 9000, op: "disabled", keys: [] },
      { at: 9500, op: "read" },
      { at: 10000, op: "exit", channel: ch("cam1"), code: 1 },
      { at: 10500, op: "disabled", keys: ["cam1"] },
      { at: 11000, op: "shed", on: true },
      { at: 11500, op: "disabled", keys: [] },
      { at: 12000, op: "read" },
      { at: 13000, op: "shed", on: false },
      { at: 13500, op: "read" },
    ],
    endAt: 40000,
  },
  {
    name: "restartChannel bounces a live master (onExit respawns); no-op without a handle or while shed",
    detect: { available: true, gstreamer: true },
    manifest: manifest(["cam0"]),
    sigintExitMs: 300,
    steps: [
      { at: 0, op: "read" },
      { at: 20000, op: "restart", key: "cam0" },
      { at: 21000, op: "read" },
      { at: 23000, op: "restart", key: "cam0" },
      { at: 23200, op: "restart", key: "cam1" },
      { at: 24000, op: "restart", key: "cam0" },
      { at: 25000, op: "read" },
      { at: 30000, op: "shed", on: true },
      { at: 31000, op: "restart", key: "cam0" },
    ],
    endAt: 40000,
  },
  {
    name: "stream guard: no-camera / no-pipeline park on the 60 s retry, logged on verdict change only",
    detect: { available: true, gstreamer: true },
    manifest: manifest(["cam0", "cam1"]),
    guard: { cam0: "no-camera", cam1: "ok" },
    pipelines: ["cam1"],
    globalPipeline: "/etc/xorgate/global.gst",
    steps: [
      { at: 0, op: "read" },
      { at: 1000, op: "read" },
      { at: 61000, op: "guard", key: "cam0", verdict: "no-pipeline" },
      { at: 62000, op: "read" },
      { at: 121000, op: "read" },
      { at: 122000, op: "guard", key: "cam0", verdict: "ok" },
      { at: 182000, op: "read" },
      { at: 183000, op: "pipeline", key: "cam0", present: true },
      { at: 184000, op: "restart", key: "cam0" },
      { at: 190000, op: "read" },
    ],
    endAt: 200000,
  },
  {
    name: "stop SIGINTs every master and cancels restarts; reads are IDLE afterwards",
    detect: { available: true, gstreamer: true },
    manifest: manifest(["cam0", "cam1"]),
    sigintExitMs: 300,
    steps: [
      { at: 0, op: "read" },
      { at: 3000, op: "exit", channel: ch("cam1"), code: 1 },
      { at: 4000, op: "stop" },
      { at: 4100, op: "read" },
      { at: 4200, op: "stop" },
      { at: 4300, op: "shed", on: true },
      { at: 4400, op: "disabled", keys: ["cam0"] },
      { at: 4500, op: "restart", key: "cam0" },
      { at: 5000, op: "read" },
    ],
    endAt: 20000,
  },
  {
    name: "inert: runtime unavailable (reason) warns once",
    detect: { available: false, gstreamer: false, reason: "runtime binary not found at /opt/xorgate-rs/media/xorgate-kvs-webrtc" },
    manifest: manifest(["cam0"]),
    steps: [
      { at: 0, op: "read" },
      { at: 1000, op: "read" },
      { at: 2000, op: "shed", on: true },
      { at: 3000, op: "shed", on: false },
      { at: 4000, op: "read" },
    ],
    endAt: 5000,
  },
  {
    name: "inert: runtime unavailable without a reason",
    detect: { available: false, gstreamer: false },
    manifest: manifest(["cam0"]),
    steps: [{ at: 0, op: "read" }, { at: 1000, op: "read" }],
    endAt: 2000,
  },
  {
    name: "inert: no manifest file",
    detect: { available: true, gstreamer: true },
    manifest: null,
    steps: [{ at: 0, op: "read" }, { at: 1000, op: "read" }],
    endAt: 2000,
  },
  {
    name: "inert: corrupt manifest",
    detect: { available: true, gstreamer: false },
    manifest: "{not json",
    steps: [{ at: 0, op: "read" }, { at: 1000, op: "read" }],
    endAt: 2000,
  },
  {
    name: "inert: manifest with no channels (shared bench profile)",
    detect: { available: true, gstreamer: true },
    manifest: manifest([]),
    steps: [{ at: 0, op: "read" }, { at: 1000, op: "read" }, { at: 1500, op: "shed", on: true }],
    endAt: 2000,
  },
  {
    name: "gst-launch-1.0 missing is a warning; masters still start",
    detect: { available: true, gstreamer: false },
    manifest: manifest(["cam0"]),
    steps: [{ at: 0, op: "read" }, { at: 16000, op: "read" }],
    endAt: 17000,
  },
];

const { createMediaCollector } = await import(join(HERE, "media-under-test.ts"));

const results: unknown[] = [];
for (const [n, sc] of SCENARIOS.entries()) {
  now = 0;
  timers = [];
  trace = [];
  const base = `/tmp/xg-media-ladder/s${n}`;
  rmSync(base, { recursive: true, force: true });
  const pipelineDir = `${base}/pipelines`;
  mkdirSync(pipelineDir, { recursive: true });
  for (const k of sc.pipelines ?? []) writeFileSync(join(pipelineDir, `${k}.gst`), "x\n");
  const manifestPath = `${base}/kvs.json`;
  if (sc.manifest !== null) {
    writeFileSync(manifestPath, typeof sc.manifest === "string" ? sc.manifest : JSON.stringify(sc.manifest));
  }
  const guard: Record<string, string> = { ...(sc.guard ?? {}) };
  const sigintExitMs = sc.sigintExitMs ?? 50;
  const fakes = new Map<string, Fake>();
  (globalThis as any).__fake = {
    detectRuntime: async () => ({ ...sc.detect }),
    streamChannel: (channelName: string, _mt: string, _src: string, _bin: string, env: any) => {
      if (fakes.get(channelName) && !fakes.get(channelName)!.done) throw new Error(`${sc.name}: double spawn ${channelName}`);
      trace.push({ t: now, k: "spawn", channel: channelName, pipeline: env.pipelinePath ?? null });
      let resolve!: (r: unknown) => void;
      const done = new Promise((r) => (resolve = r));
      const f: Fake = {
        exiting: false,
        done: false,
        exit: (code) => {
          if (f.done) throw new Error(`${sc.name}: ${channelName} exited twice`);
          f.done = true;
          resolve({ stdout: "", stderr: "", exitCode: code, failed: code !== 0, notFound: false });
        },
      };
      fakes.set(channelName, f);
      return {
        done,
        stop: (sig: string) => {
          trace.push({ t: now, k: "signal", channel: channelName, sig });
          if (!f.exiting && !f.done) {
            f.exiting = true;
            setTimeout(() => f.exit(0), sigintExitMs);
          }
        },
      };
    },
  };
  const cfg = {
    enabled: true,
    binaryPath: "/opt/xorgate-rs/media/xorgate-kvs-webrtc",
    libDir: "/opt/xorgate-rs/media/lib",
    manifestPath,
    source: "testsrc",
    mediaType: "video-only",
    pipelineDir,
    pipelinePath: sc.globalPipeline,
    logLevel: "3",
    idleTeardownSecs: "30",
    certPath: "/etc/xorgate/cert.crt",
    keyPath: "/etc/xorgate/private.key",
    caCertPath: "/opt/xorgate-rs/media/cert.pem",
    streamGuard: sc.guard ? (k: string) => guard[k] ?? "ok" : undefined,
  };
  const sup = createMediaCollector(cfg);
  for (const step of sc.steps) {
    await advanceTo(step.at);
    switch (step.op) {
      case "read":
        trace.push({ t: now, k: "read", media: sup.read() });
        break;
      case "exit": {
        const f = fakes.get(step.channel);
        if (!f || f.done || f.exiting) throw new Error(`${sc.name}: no live ${step.channel} at ${step.at}`);
        f.exit(step.code);
        break;
      }
      case "shed":
        sup.setShed(step.on);
        break;
      case "disabled":
        sup.setDisabledStreams(step.keys);
        break;
      case "restart":
        sup.restartChannel(step.key);
        break;
      case "stop":
        sup.stop();
        break;
      case "guard":
        guard[step.key] = step.verdict;
        break;
      case "pipeline": {
        const p = join(pipelineDir, `${step.key}.gst`);
        if (step.present) writeFileSync(p, "x\n");
        else rmSync(p, { force: true });
        break;
      }
    }
    await flush();
  }
  await advanceTo(sc.endAt);
  results.push({
    name: sc.name,
    input: {
      detect: sc.detect,
      manifest: sc.manifest,
      manifestPath,
      pipelineDir,
      globalPipeline: sc.globalPipeline ?? null,
      guard: sc.guard ?? null,
      pipelines: sc.pipelines ?? [],
      sigintExitMs,
      libDir: cfg.libDir,
      source: cfg.source,
      steps: sc.steps,
      endAt: sc.endAt,
    },
    expected: trace,
  });
}
Date.now = realNow;

let commit = "89b5c6407ef88e20f7168bd767d22a7be04ebcb6";
out(
  JSON.stringify(
    {
      jsSourceFile: "src/agent/collectors/media.ts (createMediaCollector, :83-380)",
      jsCommit: commit,
      generatedBy:
        "plans/device-agent-js-to-rust/phase-3/evidence/wp5-tools/dump-media-ladder.mts via tools/replay/js/run-tsx.sh (real media.ts, fake kvs-webrtc runtime, virtual timers)",
      cases: results,
    },
    null,
    2,
  ) + "\n",
);
