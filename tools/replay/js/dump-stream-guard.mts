// Phase 3 WP5 — dump the media spawn guard (worker.ts:209-220,
// `config.media.streamGuard`) over its whole input space.
//
// worker.ts is a top-level script and cannot be imported, so this driver
// slices the REAL closure text out of src/worker.ts (asserting the line
// numbers), strips types with esbuild and evaluates it with the worker's
// module-level collaborators injected: `replay` (null — real mode),
// `hardwareBlock`, `streamGuardArmed`, `streamHardware`, `config.media`, and
// the REAL `resolveChannelPipeline` (src/media/pipeline.ts) over a tmp dir.
//
//   tools/replay/js/run-tsx.sh dump-stream-guard.mts > fixtures/stream_guard/verdict.json
import { readFileSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { transformSync } from "esbuild";
import { resolveChannelPipeline } from "../src/media/pipeline.ts";

const ROOT = process.cwd();
const src = readFileSync(join(ROOT, "src/worker.ts"), "utf8").split("\n");
const start = src.findIndex((l) => l.startsWith("config.media.streamGuard = (streamKey) => {"));
const end = src.findIndex((l, i) => i > start && l === "};");
// The slice is pinned per JS ref (Phase 5b: 09da38d moved it to :224-235).
const SLICES: Record<string, [number, number]> = { "89b5c64": [208, 219], "09da38d": [223, 234] };
const [sIdx, eIdx] = SLICES[(process.env.JS_COMMIT_FULL ?? "89b5c64").slice(0, 7)] ?? [-1, -1];
if (start !== sIdx || end !== eIdx) throw new Error(`unexpected slice ${start + 1}-${end + 1}`);
const body = transformSync(src.slice(start, end + 1).join("\n"), { loader: "ts" }).code;
const make = new Function(
  "config",
  "replay",
  "hardwareBlock",
  "streamGuardArmed",
  "streamHardware",
  "resolveChannelPipeline",
  `${body}\nreturn config.media.streamGuard;`,
);

const dir = "/tmp/xg-stream-guard/pipelines";
rmSync("/tmp/xg-stream-guard", { recursive: true, force: true });
mkdirSync(dir, { recursive: true });
writeFileSync(join(dir, "cam0.gst"), "x\n");

const cases: unknown[] = [];
const STATUSES = [null, "rendered", "ok", "custom", "no-camera"] as const;
for (const hasBlock of [false, true]) {
  for (const armed of [false, true]) {
    for (const status of STATUSES) {
      for (const key of ["cam0", "cam1"]) {
        for (const globalPipeline of [null, "/etc/xorgate/global.gst"]) {
          const streamHardware = new Map<string, string>();
          if (status !== null) streamHardware.set(key, status);
          const config = { media: { pipelineDir: dir, pipelinePath: globalPipeline ?? undefined } as any };
          const guard = make(config, null, hasBlock ? { platform: "rpi-cm5" } : null, armed, streamHardware, resolveChannelPipeline);
          cases.push({
            name: `block=${hasBlock} armed=${armed} status=${status} key=${key}${key === "cam0" ? "(.gst present)" : "(.gst absent)"} global=${globalPipeline}`,
            input: { hasBlock, armed, status, pipelinePresent: key === "cam0", globalPipeline },
            expected: guard(key),
          });
        }
      }
    }
  }
}
process.stdout.write(
  JSON.stringify(
    {
      jsSourceFile: "src/worker.ts:209-220 (config.media.streamGuard, real mode)",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy: "plans/device-agent-js-to-rust/phase-3/evidence/wp5-tools/dump-stream-guard.mts via tools/replay/js/run-tsx.sh",
      cases,
    },
    null,
    2,
  ) + "\n",
);
