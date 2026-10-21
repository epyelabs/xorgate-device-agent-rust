// Phase 4 WP5: dump `xorgate pipeline …` / `xorgate recording reset-counters`
// goldens from the REAL TS commands @89b5c64 (src/commands/{pipeline,recording}.ts)
// → fixtures/cli_pipeline/cli-pipeline.json.
//
//   tools/replay/js/run-tsx.sh <plans>/phase-4/evidence/wp5-tools/dump-cli-pipeline.mts > fixtures/cli_pipeline/cli-pipeline.json
//
// Each case: a tmp ROOT (cwd = ROOT, XORGATE_HOME = ROOT/home, HARDWARE_MANIFEST
// = ROOT/hardware.json unless the case overrides it), seed files, then a
// sequence of steps. Per step: stdout / stderr text (console.log/warn/error +
// process.stdout.write, as the terminal sees them), the exit code
// (process.exitCode, or 1 + err.message on stderr for a throw — cli.tsx:164-167),
// and after the last step the whole ROOT tree (path → hex bytes with ROOT → "<ROOT>", dirs as null).
// Every occurrence of ROOT in output is replaced by "<ROOT>".
import { mkdtempSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

type Step = string[];
interface Case { name: string; files?: Record<string, string>; dirs?: string[]; env?: Record<string, string | null>; steps: Step[] }

const DIRECT = "libcamerasrc camera-name=/base/cam0@1a ! video/x-raw ! appsink name=appsink-video\n";
const MANIFEST = JSON.stringify({ deviceId: "d1", region: "us-east-1", credentialEndpoint: "c.example", roleAlias: "r",
  channels: [{ streamKey: "cam0", channelName: "dev-cam0" }, { streamKey: "cam1", channelName: "dev-cam1" }, { streamKey: "longstreamkey", channelName: "x" }] });
const CM4 = JSON.stringify({ v: 2, platform: "rpi-cm4", encoder: "hw-h264", rails: false, statusLed: null, imu: "bno085",
  video: [{ key: "cam0", connector: "csi0", sensor: "imx290", codec: "h264", label: "Front" }] });

const CASES: Case[] = [
  // test/pipeline.test.ts:51
  { name: "set then clear round-trips a stream pipeline", files: { "src.gst": "videotestsrc ! appsink name=appsink-video\n" },
    steps: [["pipeline", "set", "cam0", "src.gst", "--dir", "pipelines"], ["pipeline", "show", "cam0", "--dir", "pipelines"],
            ["pipeline", "clear", "cam0", "--dir", "pipelines"], ["pipeline", "clear", "cam0", "--dir", "pipelines"]] },
  // :62
  { name: "rejects an invalid streamKey (no path traversal)", files: { "src.gst": "videotestsrc ! appsink name=appsink-video\n" },
    steps: [["pipeline", "set", "../evil", "src.gst", "--dir", "p2"], ["pipeline", "show", "--dir", "p2"], ["pipeline", "clear", "a.b", "--dir", "p2"]] },
  // :69
  { name: "fails when the source file is missing", steps: [["pipeline", "set", "cam0", "nope.gst", "--dir", "p3"], ["pipeline", "set", "cam0", "--dir", "p3"]] },
  // :103
  { name: "bridge backs up the direct .gst, writes .camera, installs shmsrc pipeline", files: { "src.gst": DIRECT },
    steps: [["pipeline", "set", "cam0", "src.gst", "--dir", "pb"], ["pipeline", "bridge", "cam0", "--dir", "pb"],
            ["pipeline", "list", "--dir", "pb", "--manifest", "kvs.json"], ["pipeline", "bridge", "cam0", "--dir", "pb"],
            ["pipeline", "direct", "cam0", "--dir", "pb"]], },
  // :124
  { name: "bridge fails without a resolvable camera identity", files: { "src.gst": "videotestsrc ! appsink name=appsink-video\n" },
    steps: [["pipeline", "set", "cam0", "src.gst", "--dir", "pb2"], ["pipeline", "bridge", "cam0", "--dir", "pb2"]] },
  // :133
  { name: "direct fails without a backup", steps: [["pipeline", "direct", "cam9", "--dir", "pb3"]] },
  // beyond the suite
  { name: "bridge on a CM4 hardware block uses hw-h264; .camera file wins", files: { "hardware.json": CM4, "pb/cam1.camera": "  /from-camera-file \n", "pb/cam1.gst": DIRECT },
    steps: [["pipeline", "bridge", "cam1", "--dir", "pb"], ["pipeline", "show", "cam1", "--dir", "pb"]] },
  { name: "bridge without any .gst writes only .camera + the shmsrc pipeline", files: { "pb/cam2.camera": "/cam2\n" },
    steps: [["pipeline", "bridge", "cam2", "--dir", "pb"]] },
  { name: "set warns without appsink; list with a manifest", files: { "kvs.json": MANIFEST, "x.gst": "videotestsrc ! fakesink\n", "pl/zz.gst": "a appsink", "pl/10.gst": "b", "pl/B.gst": "c", "pl/notes.txt": "n" },
    steps: [["pipeline", "set", "cam1", "x.gst", "--dir", "pl"], ["pipeline", "list", "--dir", "pl", "--manifest", "kvs.json"], ["pipeline", "--dir", "pl", "--manifest", "kvs.json"]] },
  { name: "list without a manifest or dir", steps: [["pipeline", "list", "--dir", "nodir", "--manifest", "none.json"], ["pipeline", "list", "--dir", "", "--manifest", ""]] },
  { name: "list via env defaults", env: { KVS_PIPELINE_DIR: "envdir", KVS_MANIFEST: "kvs.json" }, files: { "kvs.json": MANIFEST, "envdir/cam0.gst": "shmsrc socket-path=/x ! appsink" },
    steps: [["pipeline", "list"], ["pipeline", "show", "cam0"]] },
  { name: "unknown subcommand prints usage", steps: [["pipeline", "frob"]] },
  { name: "show a missing pipeline and a directory (throws EISDIR)", dirs: ["ps/cam0.gst"], steps: [["pipeline", "show", "cam9", "--dir", "ps"], ["pipeline", "show", "cam0", "--dir", "ps"]] },
  { name: "set into a dir under a file throws ENOTDIR", files: { "src.gst": "appsink\n", "afile": "x" }, steps: [["pipeline", "set", "cam0", "src.gst", "--dir", "afile/sub"]] },
  { name: "clear a directory named like a pipeline throws", dirs: ["pc/cam0.gst"], steps: [["pipeline", "clear", "cam0", "--dir", "pc"]] },
  { name: "invalid utf-8 survives set as U+FFFD", files: { "bad.gst": "appsink \u0000" }, steps: [["pipeline", "set", "cam0", "bad.gst", "--dir", "pu"]] },
  // recording.ts
  { name: "recording reset-counters without a tree", steps: [["recording", "reset-counters"]] },
  { name: "recording reset-counters zeroes the counter", files: { "home/recordings/counters.json": "{\"v\":1,\"droppedSegments\":17}" },
    steps: [["recording", "--x", "reset-counters"]] },
  { name: "recording usage", steps: [["recording"], ["recording", "reset"], ["recording", "-h"]] },
];

const { pipelineCommand } = await import(join(process.cwd(), "src/commands/pipeline.ts"));
const { recordingCommand } = await import(join(process.cwd(), "src/commands/recording.ts"));

const ROOT_TOKEN = { root: "" };
function tree(root: string, rel = ""): Record<string, string | null> {
  const out: Record<string, string | null> = {};
  for (const n of readdirSync(join(root, rel))) {
    const r = rel ? `${rel}/${n}` : n;
    if (statSync(join(root, r)).isDirectory()) { out[r] = null; Object.assign(out, tree(root, r)); }
    else out[r] = Buffer.from(readFileSync(join(root, r)).toString("latin1").split(ROOT_TOKEN.root).join("<ROOT>"), "latin1").toString("hex");
  }
  return out;
}

const cases = [];
const origCwd = process.cwd();
for (const c of CASES) {
  const root = realpathSync(mkdtempSync(join(tmpdir(), "xg-wp5-")));
  for (const d of c.dirs ?? []) mkdirSync(join(root, d), { recursive: true });
  const seed: Record<string, string> = {};
  for (const [p, t] of Object.entries(c.files ?? {})) {
    mkdirSync(join(root, p, ".."), { recursive: true });
    const bytes = p === "bad.gst" ? Buffer.from([0x61, 0x70, 0x70, 0x73, 0x69, 0x6e, 0x6b, 0x20, 0xff, 0xfe, 0x0a]) : Buffer.from(t, "utf8");
    writeFileSync(join(root, p), bytes);
    seed[p] = bytes.toString("hex");
  }
  const env: Record<string, string | null> = { XORGATE_HOME: `${root}/home`, HARDWARE_MANIFEST: `${root}/hardware.json`, KVS_PIPELINE_DIR: null, KVS_MANIFEST: null, ...(c.env ?? {}) };
  for (const [k, v] of Object.entries(env)) { if (v === null) delete process.env[k]; else process.env[k] = v; }
  process.chdir(root);
  const steps = [];
  for (const argv of c.steps) {
    let stdout = "", stderr = "";
    const ol = console.log, ow = console.warn, oe = console.error, sw = process.stdout.write.bind(process.stdout);
    console.log = (s: unknown) => { stdout += `${s}\n`; };
    console.warn = (s: unknown) => { stderr += `${s}\n`; };
    console.error = (s: unknown) => { stderr += `${s}\n`; };
    (process.stdout as unknown as { write: (s: string) => boolean }).write = (s: string) => { stdout += s; return true; };
    process.exitCode = undefined;
    try {
      if (argv[0] === "pipeline") await pipelineCommand(argv.slice(1));
      else await recordingCommand(argv.slice(1));
    } catch (err) {
      stderr += `${err instanceof Error ? err.message : String(err)}\n`;
      process.exitCode = 1;
    }
    console.log = ol; console.warn = ow; console.error = oe;
    (process.stdout as unknown as { write: typeof sw }).write = sw;
    const sub = (s: string) => s.split(root).join("<ROOT>");
    steps.push({ argv, stdout: sub(stdout), stderr: sub(stderr), exitCode: process.exitCode ?? 0 });
    process.exitCode = undefined;
  }
  process.chdir(origCwd);
  ROOT_TOKEN.root = root;
  const files = tree(root);
  cases.push({ name: c.name, seedHex: seed, dirs: c.dirs ?? [], env: c.env ?? {}, steps, treeAfterHex: files });
  rmSync(root, { recursive: true, force: true });
}
console.log(JSON.stringify({
  jsSourceFile: "src/commands/pipeline.ts, src/commands/recording.ts",
  jsCommit: "89b5c64",
  generatedBy: "phase-4/evidence/wp5-tools/dump-cli-pipeline.mts → tools/replay/js/ via WP6 (Phase 4 WP5)",
  note: "seedHex = the exact seed bytes. Rust RESTART_HINT differs by Q9 (D-row): tests map the TS hint to the Rust one.",
  cases,
}, null, 1));
