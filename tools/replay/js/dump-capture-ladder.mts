// WP4 dumper: drives the REAL VideoCaptureSupervisor (89b5c64) private
// onExit / checkProgress / isRecording / reapShmSegments / spawn event text
// over an input grid and prints the fixture JSON (fixtures/capture_ladder/).
// Run from the scratch root:  npx tsx <this> > ladder.json
import { mkdtempSync, mkdirSync, writeFileSync, readdirSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { VideoCaptureSupervisor } from "./src/recording/video-capture.ts";
import { resolveVideoPreset } from "./src/recording/video-presets.ts";

const home = mkdtempSync(join(tmpdir(), "wp4-ladder-"));
const NOW = 1_760_000_000_000;
const realNow = Date.now;
const realSetTimeout = globalThis.setTimeout;
const realSetInterval = globalThis.setInterval;

function never(): Promise<any> { return new Promise(() => {}); }

function makeSup(o: { record: boolean; stableMs?: number; stallMs?: number; events: { level: string; msg: string }[]; spawn?: any }) {
  return new VideoCaptureSupervisor({
    deviceId: "dev-1",
    cameras: [],
    record: o.record,
    preset: resolveVideoPreset("1080p15-2mbps"),
    segmentSeconds: 60,
    videoRoot: join(home, "video"),
    shmDir: join(home, "shm"),
    timeSource: () => "ntp",
    onEvent: (level, msg) => o.events.push({ level, msg }),
    spawn: o.spawn ?? (() => ({ done: never(), stop: () => {} })),
    ...(o.stableMs !== undefined ? { stableMs: o.stableMs } : {}),
    ...(o.stallMs !== undefined ? { stallMs: o.stallMs } : {}),
    sweepPollMs: 1e9,
  });
}

function bareRun(streamKey: string): any {
  return {
    cam: { streamKey, cameraName: "/c" }, state: "running", retiring: false, handle: null,
    sessionId: null, sessionDir: null, manifest: null, startedAt: 0, backoffMs: 2000,
    quickFailures: 0, stableTimer: null, restartTimer: null, watcher: null, sweepTimer: null,
    sweeping: false, progressPath: null, progressBytes: 0, progressAt: 0, stalled: false,
    stallFailures: 0, eosSeen: false, cycling: false, playingReported: false,
  };
}

const exitCases: any[] = [];
const sessions = [null, "019a2b3c-4d5e-7f60-8123-456789abcdef"];
let exitIdx = 0;
const exitGrid: any[] = [];
for (const stalled of [false, true])
for (const notFound of [false, true])
for (const ranMs of [0, 14_999, 15_000, 60_000])
for (const quickFailures of [0, 1, 2])
for (const stallFailures of [0, 1, 2])
for (const backoffMs of [2_000, 16_000, 30_000]) exitGrid.push({ stableMs: undefined, stalled, notFound, ranMs, quickFailures, stallFailures, backoffMs });
for (const ranMs of [9_999, 10_000, 14_999])
for (const quickFailures of [0, 2]) exitGrid.push({ stableMs: 10_000, stalled: false, notFound: false, ranMs, quickFailures, stallFailures: 0, backoffMs: 4_000 });
for (const g of exitGrid) {
  const { stableMs, stalled, notFound, ranMs, quickFailures, stallFailures, backoffMs } = g;
  const exitCode = [1, null, 0, 255][exitIdx % 4];
  const sessionId = sessions[(exitIdx >> 2) % 2];
  exitIdx += 1;
  const events: { level: string; msg: string }[] = [];
  const sup = makeSup({ record: false, stableMs, events });
  const run = bareRun("cam0");
  run.handle = { done: never(), stop: () => {} };
  run.stalled = stalled; run.quickFailures = quickFailures; run.stallFailures = stallFailures;
  run.backoffMs = backoffMs; run.sessionId = sessionId; run.startedAt = NOW - ranMs;
  (sup as any).runs.set("cam0", run);
  const delays: number[] = [];
  Date.now = () => NOW;
  (globalThis as any).setTimeout = (_f: any, d: number) => { delays.push(d); return { unref() {} } as any; };
  try { (sup as any).onExit("cam0", exitCode, notFound); } finally {
    Date.now = realNow; (globalThis as any).setTimeout = realSetTimeout;
  }
  exitCases.push({
    input: { streamKey: "cam0", ...(stableMs !== undefined ? { stableMs } : {}), stalled, notFound, ranMs, quickFailures, stallFailures, backoffMs, exitCode, endedSession: sessionId },
    expected: { state: run.state, quickFailures: run.quickFailures, stallFailures: run.stallFailures, backoffMs: run.backoffMs, restartMs: delays, events },
  });
}

// checkProgress
const progressCases: any[] = [];
const dir = join(home, "frag"); mkdirSync(dir, { recursive: true });
const pA = join(dir, "00000.mp4"), pB = join(dir, "00001.mp4"), pMissing = join(dir, "00009.mp4");
for (const stallMs of [undefined, 120])
for (const active of ["none", "A0", "A100", "A200", "missing"])
for (const progressPath of [null, "A", "B"])
for (const progressBytes of [0, 100, 150])
for (const elapsed of (stallMs === 120 ? [119, 120] : [0, 44_999, 45_000]))
for (const pre of ["live", "bridge", "stopped", "retiring", "stalled", "nohandle"]) {
  if (pre !== "live" && (progressBytes !== 0 || elapsed !== 45_000 || progressPath !== null)) continue;
  const events: { level: string; msg: string }[] = [];
  const sup = makeSup({ record: pre !== "bridge", stallMs, events });
  const run = bareRun("cam1");
  let stopped: string | null = null;
  run.handle = pre === "nohandle" ? null : { done: never(), stop: (s: string) => { stopped = s; } };
  run.sessionId = "019a2b3c-4d5e-7f60-8123-456789abcdef";
  run.retiring = pre === "retiring"; run.stalled = pre === "stalled";
  if (pre === "stopped") (sup as any).stopped = true;
  run.progressPath = progressPath === "A" ? pA : progressPath === "B" ? pB : null;
  run.progressBytes = progressBytes; run.progressAt = NOW - elapsed; run.stallFailures = 2;
  rmSync(pA, { force: true });
  let activePath: string | null = null;
  if (active.startsWith("A")) { writeFileSync(pA, Buffer.alloc(Number(active.slice(1)), 1)); activePath = pA; }
  if (active === "missing") activePath = pMissing;
  (sup as any).runs.set("cam1", run);
  Date.now = () => NOW;
  try { (sup as any).checkProgress(run, activePath); } finally { Date.now = realNow; }
  const rel = (p: string | null) => (p === pA ? "A" : p === pB ? "B" : p === null ? null : "?");
  progressCases.push({
    input: { streamKey: "cam1", sessionId: run.sessionId, ...(stallMs !== undefined ? { stallMs } : {}), pre, active, activeBytes: active.startsWith("A") ? Number(active.slice(1)) : 0, progressPath, progressBytes, elapsed, stallFailures: 2 },
    expected: { progressPath: rel(run.progressPath), progressBytes: run.progressBytes, progressAtDelta: run.progressAt - NOW, stallFailures: run.stallFailures, stalled: run.stalled, state: run.state, signal: stopped, events },
  });
}

// isRecording / status().recording / anyRecording
const recordingCases: any[] = [];
for (const record of [true, false])
for (const state of ["starting", "running", "retrying", "failed", "stopped"])
for (const stallFailures of [0, 1])
for (const progressBytes of [0, 10]) {
  const sup = makeSup({ record, events: [] });
  const run = bareRun("cam0"); run.state = state; run.stallFailures = stallFailures; run.progressBytes = progressBytes;
  (sup as any).runs.set("cam0", run);
  recordingCases.push({ input: { record, state, stallFailures, progressBytes }, expected: { isRecording: (sup as any).isRecording(run), statusRecording: sup.status()[0].recording, anyRecording: sup.anyRecording() } });
}

// reapShmSegments
const shmCases: any[] = [];
const shmSets: [string[], number | undefined][] = [
  [["shmpipe.4242.    0", "shmpipe.4242.    1", "shmpipe.9999.    0", "lttng-ust-wait-8"], 4242],
  [["shmpipe.42.    0", "shmpipe.4242.    0", "shmpipe.424.  0"], 42],
  [["shmpipe.4242.    0"], undefined],
  [["shmpipe.4242", "shmpipe.4242.", "xshmpipe.4242.0", "shmpipe.4242.x"], 4242],
  [[], 7],
];
for (const [names, pid] of shmSets) {
  const d = join(home, `shm-${shmCases.length}`); mkdirSync(d, { recursive: true });
  for (const n of names) writeFileSync(join(d, n), "x");
  const sup = new VideoCaptureSupervisor({ deviceId: "d", cameras: [], record: true, preset: resolveVideoPreset("720p15-1mbps"), segmentSeconds: 60, videoRoot: join(home, "v"), shmDir: join(home, "s"), timeSource: () => "ntp", shmSegmentDir: d });
  (sup as any).reapShmSegments(pid);
  shmCases.push({ input: { names, pid: pid ?? null }, expected: { remaining: readdirSync(d).sort() } });
}

// spawn event text (record + bridge), stderr line prefix
const spawnCases: any[] = [];
for (const record of [true, false])
for (const presetName of ["720p15-1mbps", "1080p15-2mbps", "1080p30-4mbps"]) {
  const events: { level: string; msg: string }[] = [];
  const sup = new VideoCaptureSupervisor({ deviceId: "dev-1", cameras: [{ streamKey: "cam1", cameraName: "/base/cam1@1a" }], record, preset: resolveVideoPreset(presetName), segmentSeconds: 60, videoRoot: join(home, "video"), shmDir: join(home, "shm"), timeSource: () => "ntp", onEvent: (level, msg) => events.push({ level, msg }), spawn: () => ({ done: never(), stop: () => {} }), stableMs: 1e9, sweepPollMs: 1e9 });
  sup.start();
  const sid = sup.status()[0].sessionId;
  (sup as any).stopped = true;
  spawnCases.push({ input: { record, streamKey: "cam1", preset: presetName, sessionId: sid }, expected: { events } });
}
const disabled: any[] = [];
for (const k of ["cam0", "cam1"]) {
  const events: { level: string; msg: string }[] = [];
  const sup = makeSup({ record: false, events });
  const run = bareRun(k); (sup as any).runs.set(k, run);
  (globalThis as any).setInterval = () => ({ unref() {} });
  await sup.setCameras([]);
  (globalThis as any).setInterval = realSetInterval;
  disabled.push({ input: { streamKey: k }, expected: { events } });
}

// The ladder constants are module-private in video-capture.ts, so they are
// read from the source text of THIS checkout (exactly one `const NAME = <int>;`
// each, `_` separators allowed) instead of being typed in here (review-wp4 N2).
const vcSrc = readFileSync("./src/recording/video-capture.ts", "utf8");
const CONSTANT_NAMES = ["STABLE_MS", "BASE_BACKOFF_MS", "MAX_BACKOFF_MS", "MAX_QUICK_FAILURES", "PARKED_RETRY_MS", "SWEEP_POLL_MS", "STALL_MS", "MAX_STALL_FAILURES"];
const constants: Record<string, number> = {};
for (const n of CONSTANT_NAMES) {
  const m = [...vcSrc.matchAll(new RegExp(`^const ${n} = ([0-9][0-9_]*);$`, "gm"))];
  if (m.length !== 1) throw new Error(`${n}: expected one numeric declaration, found ${m.length}`);
  constants[n] = Number(m[0][1].replaceAll("_", ""));
}
// SWEEP_POLL_MS is the one constant no case exercises: also observe it on the
// real class — the setInterval period watchSession arms when sweepPollMs is
// unset (`video-capture.ts:909`; a missing dir makes fs.watch throw → caught).
{
  const sup = new VideoCaptureSupervisor({
    deviceId: "dev-1", cameras: [], record: true, preset: resolveVideoPreset("1080p15-2mbps"),
    segmentSeconds: 60, videoRoot: join(home, "video"), shmDir: join(home, "shm"),
    timeSource: () => "ntp", spawn: () => ({ done: never(), stop: () => {} }),
  } as any);
  const run = bareRun("cam0");
  run.sessionDir = join(home, "no-such-session-dir");
  const periods: number[] = [];
  (globalThis as any).setInterval = (_f: any, ms: number) => { periods.push(ms); return { unref() {} }; };
  try { (sup as any).watchSession(run); } finally { (globalThis as any).setInterval = realSetInterval; }
  if (periods.length !== 1 || periods[0] !== constants.SWEEP_POLL_MS)
    throw new Error(`watchSession period ${JSON.stringify(periods)} != SWEEP_POLL_MS ${constants.SWEEP_POLL_MS}`);
}

console.log(JSON.stringify({
  jsSourceFile: "src/recording/video-capture.ts (VideoCaptureSupervisor onExit :722-856, checkProgress :963-1007, isRecording/status/anyRecording :568-594, reapShmSegments :873-891, spawn events :710-715, setCameras event :439)",
  jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
  generatedBy: "plans/device-agent-js-to-rust/phase-3/evidence/wp4-tools/dump-capture-ladder.mts via npx tsx in a git-archive 89b5c64 scratch copy (Date.now/setTimeout stubbed; private methods driven on the real class; constants read from the archive's src/recording/video-capture.ts, SWEEP_POLL_MS also observed as watchSession's setInterval period)",
  constants,
  cases: [
    ...exitCases.map((c, i) => ({ name: `onExit #${i}`, op: "onExit", ...c })),
    ...progressCases.map((c, i) => ({ name: `checkProgress #${i}`, op: "checkProgress", ...c })),
    ...recordingCases.map((c, i) => ({ name: `isRecording #${i}`, op: "isRecording", ...c })),
    ...shmCases.map((c, i) => ({ name: `reapShmSegments #${i}`, op: "reapShmSegments", ...c })),
    ...spawnCases.map((c, i) => ({ name: `spawn event #${i}`, op: "spawnEvent", ...c })),
    ...disabled.map((c, i) => ({ name: `setCameras disabled event #${i}`, op: "disabledEvent", ...c })),
  ],
}));
rmSync(home, { recursive: true, force: true });
process.exit(0);
