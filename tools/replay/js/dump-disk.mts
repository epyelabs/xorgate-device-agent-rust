// WP2 (phase 3) driver: run the REAL DiskManager of JS @ 89b5c64 over
// scripted recordings trees and dump, per step, the scan listing the enforce
// pass saw, every unlinkSync it made (in order), stats(), and at the end the
// remaining tree + counters.json bytes.
//
//   npx tsx .rig-drivers/dump-disk.mts <outdir>
//   → <outdir>/disk_plan/recording-disk.json
//
// fs.unlinkSync is wrapped (node:module syncBuiltinESMExports) only to LOG
// the call order; it still unlinks. mtimes are multiples of 250 ms so
// utimesSync round-trips exactly.
import fs from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { join, dirname, relative } from "node:path";

const unlinks: string[] = [];
let ROOT = "";
const origUnlink = fs.unlinkSync;
(fs as any).unlinkSync = (p: any) => {
  if (ROOT && String(p).startsWith(ROOT)) unlinks.push(relative(ROOT, String(p)));
  return origUnlink(p);
};
syncBuiltinESMExports();
const { DiskManager } = await import("../src/recording/disk.js");

type FileSpec = { rel: string; size: number; mtimeMs: number };
type Step =
  | { op: "enforce" }
  | { op: "stats" }
  | { op: "writeCounters"; text: string }
  | { op: "add"; files: FileSpec[] }
  | { op: "setActive"; owner: string; rel: string | null }
  | { op: "setFree"; value: number | null }
  | { op: "setRetention"; maxBytes: number; minFreeBytes: number }
  | { op: "newManager" };
type CaseIn = {
  name: string;
  files: FileSpec[];
  dirs?: string[];
  counters?: string;
  retention: { maxBytes: number; minFreeBytes: number };
  free: number | null;
  steps: Step[];
};

function put(root: string, f: FileSpec) {
  const p = join(root, f.rel);
  fs.mkdirSync(dirname(p), { recursive: true });
  fs.writeFileSync(p, Buffer.alloc(f.size, 1));
  fs.utimesSync(p, f.mtimeMs / 1000, f.mtimeMs / 1000);
}

function tree(root: string, dir = root, out: string[] = []): string[] {
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const e of entries) {
    const p = join(dir, e.name);
    if (e.isDirectory()) {
      out.push(relative(root, p) + "/");
      tree(root, p, out);
    } else out.push(relative(root, p));
  }
  return out;
}

async function runCase(c: CaseIn) {
  const home = fs.mkdtempSync(join(tmpdir(), "xg-wp2-disk-"));
  const root = join(home, "recordings");
  ROOT = root;
  for (const d of c.dirs ?? []) fs.mkdirSync(join(root, d), { recursive: true });
  for (const f of c.files) put(root, f);
  if (c.counters !== undefined) {
    fs.mkdirSync(root, { recursive: true });
    fs.writeFileSync(join(root, "counters.json"), c.counters);
  }
  let free: number | null = c.free;
  let retention = c.retention;
  const mk = () => new DiskManager({ root, retention, freeBytes: async () => free });
  let dm = mk();
  const out: unknown[] = [];
  for (const s of c.steps) {
    switch (s.op) {
      case "enforce": {
        const listing = (dm as any).scan().map((f: any) => ({
          path: relative(root, f.path),
          size: f.size,
          mtimeMs: f.mtimeMs,
          closed: f.closed,
        }));
        const active = [...(dm as any).activePaths].map((p: string) =>
          relative(root, p.slice(p.indexOf("\0") + 1)), // disk.ts separates owner/path with a NUL
        );
        unlinks.length = 0;
        await dm.enforce();
        out.push({ op: "enforce", listing, active, free, retention, unlinks: [...unlinks], stats: dm.stats() });
        break;
      }
      case "stats":
        out.push({ op: "stats", stats: dm.stats() });
        break;
      case "writeCounters":
        fs.writeFileSync(join(root, "counters.json"), s.text);
        out.push({ op: s.op });
        break;
      case "add":
        for (const f of s.files) put(root, f);
        out.push({ op: s.op });
        break;
      case "setActive":
        dm.setActivePath(s.owner, s.rel === null ? null : join(root, s.rel));
        out.push({ op: s.op });
        break;
      case "setFree":
        free = s.value;
        out.push({ op: s.op });
        break;
      case "setRetention":
        retention = { maxBytes: s.maxBytes, minFreeBytes: s.minFreeBytes };
        dm.setRetention(retention);
        out.push({ op: s.op });
        break;
      case "newManager":
        dm = mk();
        out.push({ op: s.op, stats: dm.stats() });
        break;
    }
  }
  const finalTree = tree(root);
  let counters: string | null = null;
  try {
    counters = fs.readFileSync(join(root, "counters.json"), "utf8");
  } catch {
    /* absent */
  }
  const rootExists = fs.existsSync(root);
  ROOT = "";
  fs.rmSync(home, { recursive: true, force: true });
  return { steps: out, tree: finalTree, counters, rootExists };
}

const T = (s: string, n: string, size: number, mtimeSec: number): FileSpec => ({
  rel: `telemetry/${s}/${n}`,
  size,
  mtimeMs: mtimeSec * 1000,
});
const cases: CaseIn[] = [];
const big = { maxBytes: 1_000_000, minFreeBytes: 0 };
const tiny = { maxBytes: 100, minFreeBytes: 0 };

// The eight it()s of test/recording-disk.test.ts.
cases.push({
  name: "evicts oldest closed segments first until under maxBytes",
  files: [T("s1", "seg-1-00000.jsonl.gz", 1000, 100), T("s1", "seg-2-00001.jsonl.gz", 1000, 200), T("s2", "seg-3-00000.jsonl.gz", 1000, 300)],
  retention: { maxBytes: 2000, minFreeBytes: 0 },
  free: 10_000_000,
  steps: [{ op: "enforce" }],
});
cases.push({
  name: "evicts for the min-free floor even under quota",
  files: [T("s1", "seg-1-00000.jsonl.gz", 1000, 100), T("s1", "seg-2-00001.jsonl.gz", 1000, 200)],
  retention: { maxBytes: 1_000_000, minFreeBytes: 1000 },
  free: 500,
  steps: [{ op: "enforce" }],
});
cases.push({
  name: "never touches the active plain segment or unknown files",
  files: [T("s1", "seg-9-00009.jsonl", 5000, 50), T("s1", "seg-1-00000.jsonl.gz", 1000, 100)],
  retention: tiny,
  free: 10_000_000,
  steps: [{ op: "enforce" }],
});
cases.push({
  name: "persists droppedSegments across restarts",
  files: [T("s1", "seg-1-00000.jsonl.gz", 1000, 100)],
  retention: tiny,
  free: 10_000_000,
  steps: [{ op: "enforce" }, { op: "stats" }, { op: "newManager" }],
});
cases.push({
  name: "reflects an external counter reset without a restart (Phase 6.5)",
  files: [T("s1", "seg-1-00000.jsonl.gz", 1000, 100)],
  retention: tiny,
  free: 10_000_000,
  steps: [
    { op: "enforce" },
    { op: "stats" },
    { op: "writeCounters", text: JSON.stringify({ v: 1, droppedSegments: 0 }) },
    { op: "stats" },
    { op: "add", files: [T("s2", "seg-2-00000.jsonl.gz", 1000, 200)] },
    { op: "enforce" },
  ],
});
cases.push({
  name: "respects setActivePath registration",
  files: [T("s1", "seg-1-00000.jsonl.gz", 1000, 100), T("s1", "seg-2-00001.jsonl.gz", 1000, 200)],
  retention: tiny,
  free: 10_000_000,
  steps: [{ op: "setActive", owner: "telemetry", rel: "telemetry/s1/seg-1-00000.jsonl.gz" }, { op: "enforce" }],
});
cases.push({
  name: "reports backlog without evicting when within limits",
  files: [T("s1", "seg-1-00000.jsonl.gz", 1000, 100), T("s1", "seg-2-00001.jsonl.gz", 2000, 200)],
  retention: big,
  free: 10_000_000,
  steps: [{ op: "enforce" }],
});
cases.push({
  name: "handles a missing tree gracefully",
  files: [],
  retention: tiny,
  free: 10_000_000,
  steps: [{ op: "enforce" }, { op: "stats" }],
});

// Mixed telemetry + video tree: mp4 evicted with its sidecar, session.json
// kept, unknown/plain/tmp files counted in total but never evicted, equal
// mtimes resolved by scan (byte) order across dirs, drained telemetry
// session dirs pruned, video cam dirs never (TS depth).
const V = (cam: string, sid: string, n: string, size: number, mtimeMs: number): FileSpec => ({
  rel: `video/${cam}/${sid}/${n}`,
  size,
  mtimeMs,
});
const mixed: FileSpec[] = [
  V("cam0", "019a0000-old", "session.json", 300, 1_000_000),
  V("cam0", "019a0000-old", "00000.mp4", 40_000, 1_000_250),
  V("cam0", "019a0000-old", "00000.json", 400, 1_000_250),
  V("cam0", "019a0000-old", "00001.mp4", 40_000, 1_060_250),
  V("cam0", "019a0000-old", "00001.json", 400, 1_060_250),
  V("cam1", "019a0001-old", "session.json", 300, 1_000_000),
  V("cam1", "019a0001-old", "00000.mp4", 30_000, 1_000_250),
  V("cam1", "019a0001-old", "00001.mp4", 30_000, 1_060_500),
  { rel: "telemetry/019a0002/seg-1-00000.jsonl.gz", size: 5_000, mtimeMs: 1_000_250 },
  { rel: "telemetry/019a0002/seg-2-00001.jsonl.gz", size: 5_000, mtimeMs: 1_060_000 },
  { rel: "telemetry/019a0003/seg-3-00000.jsonl.gz", size: 5_000, mtimeMs: 1_200_000 },
  { rel: "telemetry/019a0003/seg-4-00001.jsonl", size: 7_000, mtimeMs: 900_000 },
  { rel: "telemetry/019a0003/seg-4-00001.jsonl.gz.tmp.4242", size: 6_000, mtimeMs: 900_000 },
  { rel: "telemetry/notes.txt", size: 123, mtimeMs: 500_000 },
  { rel: "video/cam0/stray.json", size: 77, mtimeMs: 400_000 },
];
cases.push({
  name: "mixed tree: quota walk across telemetry and video",
  files: mixed,
  dirs: ["telemetry/019a00ff-empty"],
  counters: JSON.stringify({ v: 1, droppedSegments: 5 }),
  retention: { maxBytes: 120_000, minFreeBytes: 0 },
  free: null,
  steps: [
    { op: "setActive", owner: "video:cam1", rel: "video/cam1/019a0001-old/00001.mp4" },
    { op: "setActive", owner: "telemetry", rel: "telemetry/019a0003/seg-4-00001.jsonl" },
    { op: "enforce" },
    { op: "setActive", owner: "video:cam1", rel: null },
    { op: "setRetention", maxBytes: 20_000, minFreeBytes: 0 },
    { op: "enforce" },
    { op: "stats" },
  ],
});
cases.push({
  name: "mixed tree: free floor drives eviction, credited per file",
  files: mixed,
  retention: { maxBytes: 10_000_000, minFreeBytes: 100_000 },
  free: 31_000,
  steps: [{ op: "enforce" }, { op: "setFree", value: 99_999 }, { op: "enforce" }, { op: "setFree", value: null }, { op: "enforce" }],
});
cases.push({
  name: "corrupt / odd counters files",
  files: [T("s1", "seg-1-00000.jsonl.gz", 1000, 100), T("s1", "seg-2-00001.jsonl.gz", 1000, 200)],
  counters: "{not json",
  retention: { maxBytes: 1500, minFreeBytes: 0 },
  free: 10_000_000,
  steps: [
    { op: "stats" },
    { op: "enforce" },
    { op: "writeCounters", text: '{"v":1,"droppedSegments":-3}' },
    { op: "stats" },
    { op: "writeCounters", text: '{"droppedSegments":"7"}' },
    { op: "stats" },
    { op: "writeCounters", text: "null" },
    { op: "stats" },
    { op: "writeCounters", text: '{"v":2,"droppedSegments":41,"extra":true}' },
    { op: "add", files: [T("s3", "seg-9-00000.jsonl.gz", 1000, 300)] },
    { op: "enforce" },
  ],
});
cases.push({
  name: "equal mtimes keep scan order",
  files: [
    T("b", "seg-1-00000.jsonl.gz", 100, 10),
    T("a", "seg-2-00000.jsonl.gz", 100, 10),
    T("a", "seg-1-00000.jsonl.gz", 100, 10),
    { rel: "video/cam0/s/00000.mp4", size: 100, mtimeMs: 10_000 },
    T("B", "seg-0-00000.jsonl.gz", 100, 10),
  ],
  retention: { maxBytes: 250, minFreeBytes: 0 },
  free: 10_000_000,
  steps: [{ op: "enforce" }],
});

const result = [];
for (const c of cases) result.push({ name: c.name, input: c, expected: await runCase(c) });
const outDir = process.argv[2] ?? ".";
fs.mkdirSync(join(outDir, "disk_plan"), { recursive: true });
fs.writeFileSync(
  join(outDir, "disk_plan", "recording-disk.json"),
  JSON.stringify(
    {
      jsSourceFile: "src/recording/disk.ts (test/recording-disk.test.ts ×8 + tree scenarios)",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy:
        "npx tsx plans/device-agent-js-to-rust/phase-3/evidence/wp2-tools/dump-disk.mts (scratch git-archive copy @ 89b5c64, linux/arm64 node:20; handed to tools/replay/js/ via WP6)",
      cases: result,
    },
    null,
    1,
  ) + "\n",
);
console.log(`wrote ${result.length} cases`);
