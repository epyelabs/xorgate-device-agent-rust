// Phase 3 WP3 — drive the REAL `Uploader` (`src/recording/uploader.ts` @ 89b5c64)
// through scripted PUT outcomes and dump every observable: events (level+msg),
// retry-timer delays (setTimeout patched to fire on demand), PUT keys + content
// types, onStatus `uploading` edges, onUploaded calls and the files left.
// Run via tools/replay/js/run-tsx.sh <this> > fixtures/upload_step/uploader-drain.json
import { mkdtempSync, mkdirSync, writeFileSync, existsSync, readdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { Uploader } from "../src/recording/uploader.ts";
import { createTelemetrySource } from "../src/recording/telemetry-source.ts";

type Outcome =
  | "ok"
  | { fail: string }
  | { failOffline: string } // isOnline() flips false inside the PUT, then it rejects
  | { failStop: string } // stop() inside the PUT, then it rejects
  | { failModeOff: string }; // mode() flips "off" inside the PUT (still online), then it rejects

interface Scenario {
  name: string;
  backoffMinMs?: number;
  backoffMaxMs?: number;
  /** Segment file names under one session dir (written in this order). */
  segments: string[];
  script: Outcome[];
  /** After this many failures (once), call pause() then kick() instead of firing the timer. */
  pauseAfterFailures?: number;
}

const SESSION = "0198aaaa-0000-7000-8000-000000000001";
const seg = (ts: number, seq: number) => `seg-${ts}-${String(seq).padStart(5, "0")}.jsonl.gz`;
const fails = (n: number, msg = "boom"): Outcome[] => Array.from({ length: n }, () => ({ fail: msg }));

const scenarios: Scenario[] = [
  { name: "default backoff 2 s doubling to the 5 min cap, then success", segments: [seg(1000, 0)], script: [...fails(12), "ok"] },
  { name: "retry seconds rounding: min 1500 max 10000", backoffMinMs: 1500, backoffMaxMs: 10000, segments: [seg(1000, 0)], script: [...fails(5), "ok"] },
  { name: "retry seconds rounding: min 500 (0.5 s rounds up)", backoffMinMs: 500, backoffMaxMs: 100000, segments: [seg(1000, 0)], script: [...fails(3), "ok"] },
  { name: "retry seconds rounding: min 2500", backoffMinMs: 2500, backoffMaxMs: 100000, segments: [seg(1000, 0)], script: [...fails(3), "ok"] },
  { name: "test knobs 20/100 ms round to 0 s", backoffMinMs: 20, backoffMaxMs: 100, segments: [seg(1000, 0)], script: [...fails(4), "ok"] },
  { name: "success resets the backoff", segments: [seg(1000, 0), seg(1000, 1)], script: [{ fail: "boom" }, { fail: "boom" }, "ok", { fail: "boom" }, "ok"] },
  { name: "failure after going offline is a pause, no retry timer", segments: [seg(1000, 0)], script: [{ failOffline: "The operation was aborted" }] },
  { name: "failure after stop() is a pause, no retry timer", segments: [seg(1000, 0)], script: [{ failStop: "The operation was aborted" }] },
  { name: "mode flipped off mid-PUT while online still logs a retry", segments: [seg(1000, 0)], script: [{ failModeOff: "boom" }] },
  { name: "empty error message", segments: [seg(1000, 0)], script: [{ fail: "" }, "ok"] },
  { name: "unrecognized name is skipped with one warning, queue continues", segments: ["weird.jsonl.gz", seg(1000, 0)], script: ["ok"] },
  { name: "pause() resets the backoff for the reconnect", segments: [seg(1000, 0)], script: [...fails(3), "ok"], pauseAfterFailures: 2 },
  { name: "three segments drain oldest-first", segments: [seg(2000, 0), seg(1000, 1), seg(1000, 0)], script: ["ok", "ok", "ok"] },
];

interface Timer { fn: () => void; ms: number; state: "armed" | "fired" | "cleared" }

async function run(sc: Scenario) {
  const home = mkdtempSync(join(tmpdir(), "wp3-"));
  const root = join(home, "recordings", "telemetry");
  const dir = join(root, SESSION);
  mkdirSync(dir, { recursive: true });
  for (const n of sc.segments) writeFileSync(join(dir, n), `payload-${n}`);

  const timers: Timer[] = [];
  const realSet = globalThis.setTimeout;
  const realClear = globalThis.clearTimeout;
  (globalThis as any).setTimeout = (fn: () => void, ms: number) => {
    const t: Timer = { fn, ms, state: "armed" };
    timers.push(t);
    return { unref() {}, __t: t } as any;
  };
  (globalThis as any).clearTimeout = (h: any) => {
    if (h && h.__t && h.__t.state === "armed") h.__t.state = "cleared";
  };

  let online = true;
  let mode: "auto" | "off" = "auto";
  const events: { level: string; msg: string }[] = [];
  const puts: { key: string; contentType: string; outcome: string }[] = [];
  const statuses: boolean[] = [];
  const uploaded: { name: string; key: string }[] = [];
  let i = 0;
  let failures = 0;
  // eslint-disable-next-line prefer-const
  let up: Uploader;
  up = new Uploader({
    sources: [createTelemetrySource("d", root)],
    isOnline: () => online,
    mode: () => mode,
    ...(sc.backoffMinMs !== undefined ? { backoffMinMs: sc.backoffMinMs } : {}),
    ...(sc.backoffMaxMs !== undefined ? { backoffMaxMs: sc.backoffMaxMs } : {}),
    onEvent: (level, msg) => events.push({ level, msg }),
    onStatus: (s) => statuses.push(s.uploading),
    onUploaded: (s, key) => uploaded.push({ name: s.name, key }),
    put: async (key, _body, contentType) => {
      const o = sc.script[i++] ?? "ok";
      puts.push({ key, contentType, outcome: typeof o === "string" ? o : JSON.stringify(o) });
      if (o === "ok") return;
      failures++;
      if ("failOffline" in o) { online = false; throw new Error(o.failOffline); }
      if ("failStop" in o) { up.stop(); throw new Error(o.failStop); }
      if ("failModeOff" in o) { mode = "off"; throw new Error(o.failModeOff); }
      throw new Error(o.fail);
    },
  });
  up.kick();
  await up.settled();
  let paused = false;
  for (let guard = 0; guard < 100; guard++) {
    if (sc.pauseAfterFailures !== undefined && !paused && failures === sc.pauseAfterFailures) {
      paused = true;
      up.pause();
      up.kick();
      await up.settled();
      continue;
    }
    const t = timers.find((x) => x.state === "armed");
    if (!t) break;
    t.state = "fired";
    t.fn();
    await up.settled();
  }
  globalThis.setTimeout = realSet;
  globalThis.clearTimeout = realClear;
  const left = existsSync(dir) ? readdirSync(dir).sort() : [];
  up.stop();
  rmSync(home, { recursive: true, force: true });
  return {
    events,
    timers: timers.map((t) => ({ ms: t.ms, state: t.state })),
    puts,
    statuses,
    uploaded,
    left,
    lastUploadAtSet: up.status().lastUploadAt !== null,
  };
}

const cases = [];
for (const sc of scenarios) {
  cases.push({ name: sc.name, op: "drain", input: sc, expected: await run(sc) });
}
console.log(
  JSON.stringify(
    {
      jsSourceFile: "src/recording/uploader.ts (Uploader drain/scheduleRetry/pause/stop) + src/recording/telemetry-source.ts",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy:
        "plans/device-agent-js-to-rust/phase-3/evidence/wp3-tools/dump-upload-step.mts via tools/replay/js/run-tsx.sh (scratch git-archive copy @ 89b5c64; setTimeout patched to fire on demand)",
      sessionId: SESSION,
      cases,
    },
    null,
    2,
  ),
);
