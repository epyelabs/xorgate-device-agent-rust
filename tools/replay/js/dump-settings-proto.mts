// Phase 5b CF78 (WP4): copied from plans/device-agent-js-to-rust/phase-2/evidence/wp6-tools/dump-settings-proto.mts so redump-all.sh
// re-runs it at any ref; only the JS root (cwd), the output dir ($OUT) and the jsCommit stamp
// ($JS_COMMIT_FULL) were changed.
// review-wp6 row 5 (orchestrator decision: PARITY): what the REAL TS parsers do
// with a `__proto__` key in the two camera maps. JSON.parse keeps it as an own
// property; parseRecordingSettings' `c[key] = value` (settings.ts:481) and
// parseCameraMountSettings' `out[key] = { rotationDeg }` (:276) assign into a
// plain object, which runs the Object.prototype.__proto__ setter, so the key
// is dropped. `loadSettings` cases record the exact bytes `saveSettings`
// re-writes (load → re-save); the parse ops cover the cloud-apply path
// (cloud-config.ts calls the same parsers on JSON.parse'd payloads).
// Run from a scratch `git archive 89b5c64` copy (non-root):
//   cd <scratch>/js-89b5c64 && npx tsx <this file> > fixtures/settings/proto-keys.json
import { mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const { loadSettings, saveSettings, parseRecordingSettings, parseCameraMountSettings } =
  await import(join(process.cwd(), "src/daemon/settings.ts"));

type Case = { name: string; op: string; input: string };
const cases: Case[] = [
  { name: "load: __proto__ camera enable dropped, siblings kept in order", op: "loadSettings", input: '{"recording":{"video":{"cameras":{"cam0":true,"__proto__":false,"constructor":false,"7":true}}}}' },
  { name: "load: __proto__-only camera enables drop cameras, video and recording", op: "loadSettings", input: '{"mode":"real","recording":{"video":{"cameras":{"__proto__":true}}}}' },
  { name: "load: __proto__ with a non-boolean value is dropped as before", op: "loadSettings", input: '{"recording":{"video":{"enabled":true,"cameras":{"__proto__":{"x":1},"cam1":false}}}}' },
  { name: "load: __proto__ cameraMount entry dropped", op: "loadSettings", input: '{"cameraMount":{"cameras":{"__proto__":{"rotationDeg":180},"cam0":{"rotationDeg":0},"toString":{"rotationDeg":180}}}}' },
  { name: "load: __proto__-only cameraMount keeps {cameras:{}}", op: "loadSettings", input: '{"cameraMount":{"cameras":{"__proto__":{"rotationDeg":0}}}}' },
  { name: "load: invalid __proto__ cameraMount entry", op: "loadSettings", input: '{"cameraMount":{"cameras":{"__proto__":{"rotationDeg":90},"cam0":{"rotationDeg":180}}}}' },
  { name: "load: top-level and namespace-level __proto__ are just unknown keys", op: "loadSettings", input: '{"__proto__":{"mode":"sim"},"mode":"real","recording":{"__proto__":{"telemetry":{"enabled":true}},"video":{"__proto__":{"enabled":true},"enabled":false}},"cameraMount":{"__proto__":{"cameras":{}}}}' },
  { name: "parse (cloud): recording cameras __proto__", op: "parseRecordingSettings", input: '{"video":{"cameras":{"__proto__":true,"cam0":false}}}' },
  { name: "parse (cloud): recording cameras __proto__ only", op: "parseRecordingSettings", input: '{"video":{"cameras":{"__proto__":true}}}' },
  { name: "parse (cloud): cameraMount __proto__", op: "parseCameraMountSettings", input: '{"cameras":{"cam1":{"rotationDeg":180},"__proto__":{"rotationDeg":180}}}' },
];

const scratch = mkdtempSync(join(tmpdir(), "xg-wp6-proto-"));
const out: unknown[] = [];
for (const c of cases) {
  let expectedJson: string | null;
  if (c.op === "loadSettings") {
    const src = join(scratch, "in.json");
    const dst = join(scratch, "out.json");
    writeFileSync(src, c.input);
    saveSettings(loadSettings(src), dst);
    expectedJson = readFileSync(dst, "utf8");
  } else {
    const fn = c.op === "parseRecordingSettings" ? parseRecordingSettings : parseCameraMountSettings;
    const r = fn(JSON.parse(c.input));
    expectedJson = r === undefined ? null : JSON.stringify(r);
  }
  out.push({ name: c.name, op: c.op, input: c.input, expectedJson });
}
rmSync(scratch, { recursive: true, force: true });
process.stdout.write(JSON.stringify({
  jsSourceFile: "src/daemon/settings.ts (parseCameraMountSettings :266-279, parseRecordingSettings cameras :476-484, loadSettings/saveSettings :610-642)",
  jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
  generatedBy: "npx tsx dump-settings-proto.mts in a git-archive scratch copy of 89b5c64, non-root (phase-2 WP6 fix, review-wp6 row 5; script in phase-2/evidence/wp6-tools)",
  cases: out,
}, null, 2) + "\n");
