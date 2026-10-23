// Phase 5b CF78 (WP4): copied from plans/device-agent-js-to-rust/phase-1/evidence/dump-wp6.mts so redump-all.sh
// re-runs it at any ref; only the JS root (cwd), the output dir ($OUT) and the jsCommit stamp
// ($JS_COMMIT_FULL) were changed.
/**
 * WP6 fixture dumper (throwaway; run with `npx tsx` from the JS repo working
 * tree, never committed there). Replays the in-scope `it()` cases of
 * test/hardware-block.test.ts, test/recording-video-capture.test.ts,
 * test/pipeline.test.ts, test/recording-session.test.ts,
 * test/recording-segments.test.ts and test/recording-video-segments.test.ts
 * through the REAL TS functions and freezes their outputs as JSON.
 */
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";

const R = process.cwd();
const OUT = process.env.OUT ?? "fixtures";
const JS_COMMIT = process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d";
const GEN =
  "node script run in the JS repo working tree via npx tsx (plans/device-agent-js-to-rust/phase-1 WP6 dump-wp6.ts; not committed there)";

// Freeze the default clocks BEFORE anything reads them (Date.now / monotonicMs).
const FROZEN_WALL = 1_750_000_000_000 + 240_000;
const FROZEN_MONO_MS = 740_000;
Date.now = () => FROZEN_WALL;
process.hrtime.bigint = () => BigInt(FROZEN_MONO_MS) * 1_000_000n;

const pb = await import(`${R}/src/media/pipeline-builder.ts`);
const pipeline = await import(`${R}/src/media/pipeline.ts`);
const cameras = await import(`${R}/src/hardware/cameras.ts`);
const vc = await import(`${R}/src/recording/video-capture.ts`);
const presets = await import(`${R}/src/recording/video-presets.ts`);
const cfg = await import(`${R}/src/recording/config.ts`);
const session = await import(`${R}/src/recording/session.ts`);
const segments = await import(`${R}/src/recording/segments.ts`);
const vs = await import(`${R}/src/recording/video-segments.ts`);

type Case = Record<string, unknown>;
function write(rel: string, jsSourceFile: string, cases: Case[], extra: Record<string, unknown> = {}) {
  const path = join(OUT, rel);
  mkdirSync(join(path, ".."), { recursive: true });
  writeFileSync(
    path,
    JSON.stringify({ jsSourceFile, jsCommit: JS_COMMIT, generatedBy: GEN, ...extra, cases }, null, 2) + "\n",
  );
  console.log(`${rel}: ${cases.length} cases`);
}

// ---------------------------------------------------------------- A. pipeline templates
{
  const A: Case[] = [];
  const c = (name: string, op: string, input: unknown, expected: unknown) => A.push({ name, op, input, expected });
  const sock = "/var/lib/xorgate/shm/cam0.sock";
  c("hw-h264 bridged .gst reproduces README Appendix A exactly", "buildBridgedKvsPipeline",
    { encoder: "hw-h264", shmSocketPath: sock }, pb.buildBridgedKvsPipeline("hw-h264", sock));
  c("sw-h264 bridged .gst is byte-for-byte the historical pipeline", "buildBridgedKvsPipeline",
    { encoder: "sw-h264", shmSocketPath: "/shm/cam0.sock" }, pb.buildBridgedKvsPipeline("sw-h264", "/shm/cam0.sock"));
  c("sw-h264 bridged .gst is byte-for-byte the historical pipeline [legacy entry point]", "bridgedKvsPipeline",
    { shmSocketPath: "/shm/cam0.sock" }, vc.bridgedKvsPipeline("/shm/cam0.sock"));
  c("hw templates never name the encoder sampleVideoEncoder (TWCC skip, D5) [bridged]", "buildBridgedKvsPipeline",
    { encoder: "hw-h264", shmSocketPath: "/s" }, pb.buildBridgedKvsPipeline("hw-h264", "/s"));
  c("hw templates never name the encoder sampleVideoEncoder (TWCC skip, D5) [direct]", "buildDirectKvsPipeline",
    { encoder: "hw-h264", streamKey: "cam0", cameraName: "/cam/path" }, pb.buildDirectKvsPipeline("hw-h264", "cam0", "/cam/path"));
  c("direct sw template matches the install script's historical bytes (no TWCC name)", "buildDirectKvsPipeline",
    { encoder: "sw-h264", streamKey: "cam0", cameraName: "/base/soc/i2c0mux/i2c@0/imx290@1a" },
    pb.buildDirectKvsPipeline("sw-h264", "cam0", "/base/soc/i2c0mux/i2c@0/imx290@1a"));
  c("recording encode stage follows the encoder token (D6) [sw]", "buildRecordEncodeArgs",
    { encoder: "sw-h264", bitrateKbps: 2000, keyIntMax: 30 }, pb.buildRecordEncodeArgs("sw-h264", 2000, 30));
  c("recording encode stage follows the encoder token (D6) [hw]", "buildRecordEncodeArgs",
    { encoder: "hw-h264", bitrateKbps: 2000, keyIntMax: 30 }, pb.buildRecordEncodeArgs("hw-h264", 2000, 30));
  c("the hw recording stage carries the caps v4l2h264enc needs to negotiate [hw]", "buildRecordEncodeArgs",
    { encoder: "hw-h264", bitrateKbps: 1000, keyIntMax: 30 }, pb.buildRecordEncodeArgs("hw-h264", 1000, 30));
  c("the hw recording stage carries the caps v4l2h264enc needs to negotiate [sw]", "buildRecordEncodeArgs",
    { encoder: "sw-h264", bitrateKbps: 1000, keyIntMax: 30 }, pb.buildRecordEncodeArgs("sw-h264", 1000, 30));
  for (const [i, content] of [pb.buildBridgedKvsPipeline("sw-h264", sock), pb.buildBridgedKvsPipeline("hw-h264", sock), "something else"].entries()) {
    c(`recognizes both encoder renders as overwritable [${i}]`, "isRecognizedBridgedRender",
      { content, shmSocketPath: sock }, pb.isRecognizedBridgedRender(content, sock));
  }
  // Extras (no vitest case; the TS function is the oracle).
  c("extra: bridged render recognized with surrounding whitespace", "isRecognizedBridgedRender",
    { content: `\n  ${pb.buildBridgedKvsPipeline("hw-h264", sock)}\n`, shmSocketPath: sock },
    pb.isRecognizedBridgedRender(`\n  ${pb.buildBridgedKvsPipeline("hw-h264", sock)}\n`, sock));
  c("extra: JS trim strips a BOM and leaves U+0085", "isRecognizedBridgedRender",
    { content: `\uFEFF${pb.buildBridgedKvsPipeline("sw-h264", sock)}\u0085`, shmSocketPath: sock },
    pb.isRecognizedBridgedRender(`\uFEFF${pb.buildBridgedKvsPipeline("sw-h264", sock)}\u0085`, sock));
  c("extra: JS trim strips a BOM", "isRecognizedBridgedRender",
    { content: `\uFEFF${pb.buildBridgedKvsPipeline("sw-h264", sock)}\u3000`, shmSocketPath: sock },
    pb.isRecognizedBridgedRender(`\uFEFF${pb.buildBridgedKvsPipeline("sw-h264", sock)}\u3000`, sock));
  c("extra: bridged render for another socket is not recognized", "isRecognizedBridgedRender",
    { content: pb.buildBridgedKvsPipeline("sw-h264", "/other.sock"), shmSocketPath: sock },
    pb.isRecognizedBridgedRender(pb.buildBridgedKvsPipeline("sw-h264", "/other.sock"), sock));
  const directCases: [string, string, string][] = [
    [pb.buildDirectKvsPipeline("sw-h264", "cam0", "/c") + "\n", "cam0", "/c"],
    [pb.buildDirectKvsPipeline("hw-h264", "cam0", "/c"), "cam0", "/c"],
    ["libcamerasrc camera-name=/c ! video/x-raw ! x264enc ! appsink name=appsink-video", "cam0", "/c"],
    ["libcamerasrc camera-name=/d ! x264enc ! appsink", "cam0", "/c"],
    ["libcamerasrc camera-name=/c !\n x264enc ! appsink", "cam0", "/c"],
    ["videotestsrc ! x264enc ! appsink", "cam0", "/c"],
    ["libcamerasrc camera-name=/c ! v4l2h264enc ! appsink", "cam0", "/c"],
  ];
  for (const [i, [content, streamKey, cameraName]] of directCases.entries()) {
    c(`extra: isRecognizedDirectRender [${i}]`, "isRecognizedDirectRender",
      { content, streamKey, cameraName }, pb.isRecognizedDirectRender(content, streamKey, cameraName));
  }
  write("pipeline_builder/templates.json",
    "test/hardware-block.test.ts (pipeline templates; reconcileHardware › recognizes both encoder renders as overwritable)", A,
    { SHM_BRIDGE_CAPS: pb.SHM_BRIDGE_CAPS });
}

// ---------------------------------------------------------------- B. camera enumeration
{
  const B: Case[] = [];
  const c = (name: string, op: string, input: unknown, expected: unknown) => B.push({ name, op, input, expected });
  const CM5_LISTING = `Available cameras
-----------------
0 : imx290 [1920x1080 12-bit RGGB] (/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a)
    Modes: 'SRGGB10_CSI2P' : 1280x720 [30.00 fps]
1 : imx290 [1920x1080 12-bit RGGB] (/base/axi/pcie@1000120000/rp1/i2c@70000/imx290@1a)
    Modes: 'SRGGB10_CSI2P' : 1280x720 [60.00 fps]
`;
  const CM4_LISTING = `Available cameras
-----------------
0 : imx290 [1920x1080 12-bit RGGB] (/base/soc/i2c0mux/i2c@0/imx290@1a)
    Modes: whatever
`;
  c("parses the CM5 bench listing with RP1 connector order (88000=csi0, 70000=csi1)", "parseCameraList",
    { output: CM5_LISTING, platform: "rpi-cm5" }, cameras.parseCameraList(CM5_LISTING, "rpi-cm5"));
  for (const [i, p] of ["/base/soc/i2c0mux/i2c@0/imx290@1a", "/base/soc/i2c0mux/i2c@1/imx290@1a", "/weird/path"].entries()) {
    c(`maps CM4 Unicam paths [${i}]`, "connectorForPath", { platform: "rpi-cm4", cameraName: p }, cameras.connectorForPath("rpi-cm4", p));
  }
  c("extra: CM4 listing (reconcileHardware fixture) on rpi-cm4", "parseCameraList",
    { output: CM4_LISTING, platform: "rpi-cm4" }, cameras.parseCameraList(CM4_LISTING, "rpi-cm4"));
  c("extra: CM5 listing parsed for rpi-cm4 maps no connector", "parseCameraList",
    { output: CM5_LISTING, platform: "rpi-cm4" }, cameras.parseCameraList(CM5_LISTING, "rpi-cm4"));
  c("extra: no cameras", "parseCameraList", { output: "No cameras available!\n", platform: "rpi-cm5" },
    cameras.parseCameraList("No cameras available!\n", "rpi-cm5"));
  const odd = "  7:imx708 x (/p)\r\n1 : foo (x) (a(b))\n1 : foo(bar)\n1 : foo ()\n2 : ov5647 [2592x1944 10-bit GBRG] (/base/axi/pcie@1000120000/rp1/i2c@70000/ov5647@36) \n";
  c("extra: regex edge cases", "parseCameraList", { output: odd, platform: "rpi-cm5" }, cameras.parseCameraList(odd, "rpi-cm5"));
  const odd2 = "1 : foo (a()\n1 : foo \r (x)\n1 : foo (a\r(b)\n\uFEFF3 : bar baz (/q)\u2028\n";
  c("extra: regex backtracking edge cases", "parseCameraList", { output: odd2, platform: "rpi-cm4" }, cameras.parseCameraList(odd2, "rpi-cm4"));
  write("cameras/enumeration.json", "test/hardware-block.test.ts (camera enumeration parsing)", B);
}

// ---------------------------------------------------------------- C. capture argv builders + pipeline.ts helpers
{
  const C: Case[] = [];
  const c = (name: string, op: string, input: unknown, expected: unknown) => C.push({ name, op, input, expected });
  const specIn = (p: Record<string, unknown>) => p;
  const s1 = { cameraName: "/base/cam@1a", preset: "1080p30-4mbps", segmentSeconds: 60, outputDir: "/out", shmSocketPath: "/shm/cam0.sock" };
  c("record pipeline has both branches, preset caps, and segment length", "buildCapturePipelineArgs", specIn(s1),
    vc.buildCapturePipelineArgs({ ...s1, preset: presets.resolveVideoPreset(s1.preset) }));
  c("bridge-only pipeline has no encoder or splitmuxsink", "buildBridgeOnlyPipelineArgs",
    { cameraName: "/base/cam@1a", shmSocketPath: "/shm/cam0.sock" }, vc.buildBridgeOnlyPipelineArgs("/base/cam@1a", "/shm/cam0.sock"));
  c("bridged KVS pipeline keeps the TWCC encoder name and appsink contract", "bridgedKvsPipeline",
    { shmSocketPath: "/shm/cam0.sock" }, vc.bridgedKvsPipeline("/shm/cam0.sock"));

  // resolveBridgedCameras: real files in a tmp dir, recorded as a name → content map.
  const rb = (name: string, files: Record<string, string>, channels: { streamKey: string; channelName: string }[]) => {
    const dir = mkdtempSync(join(tmpdir(), "wp6-rb-"));
    for (const [n, t] of Object.entries(files)) writeFileSync(join(dir, n), t);
    // Review fix M1: also record the console.warn lines (the skip warnings).
    const warnings: string[] = [];
    const origWarn = console.warn;
    console.warn = (...a: unknown[]) => { warnings.push(a.map(String).join(" ")); };
    let out;
    try {
      out = vc.resolveBridgedCameras(dir, channels);
    } finally {
      console.warn = origWarn;
    }
    rmSync(dir, { recursive: true, force: true });
    C.push({ name, op: "resolveBridgedCameras", input: { pipelineDir: "/p", files, channels }, expected: out, warnings });
  };
  rb("returns only bridged streams with camera identity", {
    "cam0.gst": "shmsrc socket-path=/x ! appsink",
    "cam0.camera": "/base/cam0@1a\n",
    "cam1.gst": "libcamerasrc camera-name=/base/cam1@1a ! appsink",
    "cam2.gst": "shmsrc socket-path=/y ! appsink",
  }, [
    { streamKey: "cam0", channelName: "d-cam0" },
    { streamKey: "cam1", channelName: "d-cam1" },
    { streamKey: "cam2", channelName: "d-cam2" },
  ]);
  rb("falls back to the .gst.direct backup for camera identity", {
    "cam0.gst": "shmsrc socket-path=/x ! appsink",
    "cam0.gst.direct": "libcamerasrc camera-name=/base/cam0@1a ! appsink",
  }, [{ streamKey: "cam0", channelName: "d-cam0" }]);

  // Review fix M1: an EMPTY extracted name (`camera-name=""`) is falsy in the TS —
  // it falls through to the next file, and with nothing left the stream is warned+skipped.
  rb("extra: empty camera-name falls through to the next file, empty everywhere warns and skips", {
    "cam0.gst": "shmsrc socket-path=/x camera-name=/from-gst ! appsink",
    "cam0.gst.direct": 'libcamerasrc camera-name="" ! appsink',
    "cam1.gst": "shmsrc socket-path=/y ! appsink",
    "cam1.camera": "  \n",
    "cam1.gst.direct": 'libcamerasrc camera-name="" ! appsink',
  }, [
    { streamKey: "cam0", channelName: "d-cam0" },
    { streamKey: "cam1", channelName: "d-cam1" },
  ]);

  // Replay hooks: the it() drives the supervisor; the pure equivalent is the builder with `head`.
  const replay = { cameraName: "replay", head: ["filesrc", "location=/a/lap.h264"], preset: "1080p15-2mbps", segmentSeconds: 60, outputDir: "/home/video/cam0/sess", shmSocketPath: "/home/shm/cam0.sock" };
  c("sourceHead replaces the libcamerasrc head on every spawn and nothing after the tee [builder-level]", "buildCapturePipelineArgs", replay,
    vc.buildCapturePipelineArgs({ ...replay, preset: presets.resolveVideoPreset(replay.preset) }));
  // hardware-block.test.ts › pipeline templates › buildCapturePipelineArgs swaps only the encoder stage on hw-h264
  const base = { cameraName: "/cam", preset: "720p15-1mbps", segmentSeconds: 60, outputDir: "/out", shmSocketPath: "/shm/cam0.sock" };
  c("buildCapturePipelineArgs swaps only the encoder stage on hw-h264 [sw]", "buildCapturePipelineArgs", base,
    vc.buildCapturePipelineArgs({ ...base, preset: presets.resolveVideoPreset(base.preset) }));
  c("buildCapturePipelineArgs swaps only the encoder stage on hw-h264 [hw]", "buildCapturePipelineArgs", { ...base, encoder: "hw-h264" },
    vc.buildCapturePipelineArgs({ ...base, encoder: "hw-h264", preset: presets.resolveVideoPreset(base.preset) }));
  // Extras: every preset through both encoders (the strings are the contract).
  for (const name of presets.videoPresetNames()) {
    for (const encoder of ["sw-h264", "hw-h264"] as const) {
      const spec = { cameraName: "/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a", preset: name, segmentSeconds: 120, outputDir: "/var/lib/xorgate/recordings/video/cam0/0199c1f0-0000-7000-8000-000000000000", shmSocketPath: "/var/lib/xorgate/shm/cam0.sock", encoder };
      c(`extra: capture argv ${name} ${encoder}`, "buildCapturePipelineArgs", spec,
        vc.buildCapturePipelineArgs({ ...spec, preset: presets.resolveVideoPreset(name) }));
    }
  }
  // Extra: the env-var head seam.
  process.env.XORGATE_CAPTURE_SOURCE_HEAD = "  videotestsrc   is-live=true ";
  c("extra: XORGATE_CAPTURE_SOURCE_HEAD override (bridge-only)", "buildBridgeOnlyPipelineArgs",
    { cameraName: "/c", shmSocketPath: "/s", envOverride: "  videotestsrc   is-live=true " }, vc.buildBridgeOnlyPipelineArgs("/c", "/s"));
  c("extra: explicit head beats the env override", "buildBridgeOnlyPipelineArgs",
    { cameraName: "/c", shmSocketPath: "/s", head: ["filesrc", "location=/x"], envOverride: "  videotestsrc   is-live=true " },
    vc.buildBridgeOnlyPipelineArgs("/c", "/s", ["filesrc", "location=/x"]));
  process.env.XORGATE_CAPTURE_SOURCE_HEAD = "   ";
  c("extra: blank env override is ignored", "buildBridgeOnlyPipelineArgs",
    { cameraName: "/c", shmSocketPath: "/s", envOverride: "   " }, vc.buildBridgeOnlyPipelineArgs("/c", "/s"));
  delete process.env.XORGATE_CAPTURE_SOURCE_HEAD;

  // pipeline.test.ts › bridged mode helpers
  for (const [i, g] of ["libcamerasrc camera-name=/base/i2c@88000/imx290@1a ! caps", "videotestsrc ! appsink"].entries()) {
    c(`extractCameraName parses libcamerasrc pipelines [${i}]`, "extractCameraName", { gst: g }, pipeline.extractCameraName(g));
  }
  c("extra: extractCameraName strips one pair of quotes", "extractCameraName", { gst: 'libcamerasrc camera-name="/q x" ! x' }, pipeline.extractCameraName('libcamerasrc camera-name="/q x" ! x'));
  c("extra: extractCameraName first non-empty occurrence", "extractCameraName", { gst: "camera-name= camera-name=/z" }, pipeline.extractCameraName("camera-name= camera-name=/z"));
  {
    const dir = mkdtempSync(join(tmpdir(), "wp6-pl-"));
    writeFileSync(pipeline.pipelineFileFor(dir, "cam0"), "shmsrc socket-path=/x ! appsink");
    writeFileSync(pipeline.pipelineFileFor(dir, "cam1"), "libcamerasrc camera-name=/c ! appsink");
    for (const k of ["cam0", "cam1", "cam2"]) {
      const gst = existsSync(pipeline.pipelineFileFor(dir, k)) ? readFileSync(pipeline.pipelineFileFor(dir, k), "utf8") : null;
      c(`isBridgedPipeline keys off shmsrc in the .gst [${k}]`, "isBridgedPipeline", { gst }, pipeline.isBridgedPipeline(dir, k));
    }
    rmSync(dir, { recursive: true, force: true });
  }
  {
    const dir = mkdtempSync(join(tmpdir(), "wp6-rc-"));
    const snap = (k: string) => ({
      cameraFile: existsSync(pipeline.cameraFileFor(dir, k)) ? readFileSync(pipeline.cameraFileFor(dir, k), "utf8") : null,
      directBackup: existsSync(pipeline.directBackupFor(dir, k)) ? readFileSync(pipeline.directBackupFor(dir, k), "utf8") : null,
      gst: existsSync(pipeline.pipelineFileFor(dir, k)) ? readFileSync(pipeline.pipelineFileFor(dir, k), "utf8") : null,
    });
    writeFileSync(pipeline.pipelineFileFor(dir, "camA"), "libcamerasrc camera-name=/from-gst ! x");
    c("resolveCameraName prefers .camera, then backup, then .gst [gst]", "resolveCameraName", snap("camA"), pipeline.resolveCameraName(dir, "camA"));
    writeFileSync(pipeline.directBackupFor(dir, "camA"), "libcamerasrc camera-name=/from-backup ! x");
    c("resolveCameraName prefers .camera, then backup, then .gst [backup]", "resolveCameraName", snap("camA"), pipeline.resolveCameraName(dir, "camA"));
    writeFileSync(pipeline.cameraFileFor(dir, "camA"), "/from-camera-file\n");
    c("resolveCameraName prefers .camera, then backup, then .gst [camera file]", "resolveCameraName", snap("camA"), pipeline.resolveCameraName(dir, "camA"));
    c("resolveCameraName prefers .camera, then backup, then .gst [nope]", "resolveCameraName", snap("nope"), pipeline.resolveCameraName(dir, "nope"));
    // Review fix M1: an empty name in the backup falls through to the .gst.
    writeFileSync(pipeline.directBackupFor(dir, "camB"), 'libcamerasrc camera-name="" ! x');
    writeFileSync(pipeline.pipelineFileFor(dir, "camB"), "libcamerasrc camera-name=/from-gst ! x");
    c("extra: resolveCameraName skips an empty backup name (falsy) and takes the .gst", "resolveCameraName", snap("camB"), pipeline.resolveCameraName(dir, "camB"));
    rmSync(dir, { recursive: true, force: true });
  }
  write("video_capture/builders.json",
    "test/recording-video-capture.test.ts (pipeline builders, resolveBridgedCameras, replay hooks › sourceHead); test/hardware-block.test.ts (pipeline templates › buildCapturePipelineArgs swaps…); test/pipeline.test.ts (bridged mode helpers)", C);
}

// ---------------------------------------------------------------- D. session
{
  const D: Case[] = [];
  const c = (name: string, op: string, input: unknown, expected: unknown) => D.push({ name, op, input, expected });
  c("produces valid UUIDv7-shaped ids", "mintSessionId", { now: 1_800_000_000_000 }, session.mintSessionId(1_800_000_000_000));
  const a = session.mintSessionId(1_000_000), b = session.mintSessionId(2_000_000), cc = session.mintSessionId(1_800_000_000_000);
  c("sorts by mint time (oldest-first upload/eviction ordering)", "mintSessionIdSort",
    { nows: [1_000_000, 2_000_000, 1_800_000_000_000] }, { ids: [a, b, cc], sorted: [cc, a, b].sort() });
  c("is unique across calls at the same ms", "mintSessionIdMany", { now: 12345, count: 100 },
    Array.from({ length: 100 }, () => session.mintSessionId(12345)));
  for (const [i, now] of [0, -5, 1.9, 2 ** 48 - 1, 2 ** 48 + 1, 2 ** 53 - 1].entries()) {
    c(`extra: mintSessionId edge now [${i}]`, "mintSessionId", { now }, session.mintSessionId(now));
  }
  // Tracker scripts through the real tracker with an injected probe.
  async function tracker(name: string, script: { op: string; probe?: string; source?: string }[]) {
    let probed = "unsynced" as "ntp" | "rtc" | "unsynced";
    const t = session.createTimeSourceTracker(60_000, async () => probed);
    const steps: unknown[] = [];
    for (const s of script) {
      if (s.op === "refresh") { probed = s.probe as never; steps.push({ ...s, expect: await t.refresh() }); }
      else if (s.op === "noteStep") { t.noteStep(s.source as never); steps.push({ ...s }); }
      else if (s.op === "current") steps.push({ ...s, expect: t.current() });
    }
    D.push({ name, op: "timeSourceTracker", input: { refreshMs: 60_000, now: FROZEN_WALL }, expected: steps });
  }
  await tracker("reports the stepped source over a weaker probe value, immediately and across refreshes", [
    { op: "refresh", probe: "rtc" }, { op: "noteStep", source: "gps" }, { op: "current" }, { op: "refresh", probe: "rtc" },
  ]);
  await tracker("lets NTP supersede a step for good", [
    { op: "noteStep", source: "nitz" }, { op: "refresh", probe: "unsynced" }, { op: "refresh", probe: "ntp" }, { op: "refresh", probe: "rtc" },
  ]);
  await tracker("never downgrades a current ntp value on a late noteStep", [
    { op: "refresh", probe: "ntp" }, { op: "noteStep", source: "gps" }, { op: "current" },
  ]);
  await tracker("extra: initial value is unsynced; a step before any probe shows immediately", [
    { op: "current" }, { op: "noteStep", source: "nitz" }, { op: "current" }, { op: "refresh", probe: "rtc" },
  ]);
  write("session/session.json", "test/recording-session.test.ts (mintSessionId; timeSource tracker with offline clock steps)", D);
}

// ---------------------------------------------------------------- E. telemetry segments
{
  const E: Case[] = [];
  const HEADER = { type: "header", v: 1, deviceId: "dev-1", sessionId: "session-a", bootId: "boot-1", timeSource: "ntp", rateHz: 10, startedAt: 1000 };
  function gzLines(p: string): string {
    try {
      return gunzipSync(readFileSync(p)).toString("utf8");
    } catch {
      return "$raw:" + readFileSync(p, "utf8");
    }
  }
  function listTree(home: string): Record<string, string> {
    const out: Record<string, string> = {};
    const walk = (d: string) => {
      if (!existsSync(d)) return;
      for (const n of readdirSync(d).sort()) {
        const p = join(d, n);
        if (statSync(p).isDirectory()) walk(p);
        else out[p.slice(home.length)] = n.endsWith(".gz") ? gzLines(p) : readFileSync(p, "utf8");
      }
    };
    walk(home);
    return out;
  }
  type Op = { op: string; line?: unknown; now?: number; mono?: number; ts?: string };
  function writer(name: string, ctor: { maxBytes?: number | null; maxAgeMs?: number | null }, ops: Op[], monoInjected: boolean) {
    const home = mkdtempSync(join(tmpdir(), "wp6-seg-"));
    const dir = join(home, "recordings", "telemetry", "session-a");
    const closed: string[] = [];
    let mono = 0;
    const w = new segments.SegmentWriter({
      dir, header: () => ({ ...HEADER } as never),
      maxBytes: ctor.maxBytes === null ? undefined : ctor.maxBytes,
      maxAgeMs: ctor.maxAgeMs === null ? undefined : ctor.maxAgeMs,
      monoMs: () => mono,
      onSegmentClosed: (p) => closed.push(p.slice(home.length)),
    });
    const steps: unknown[] = [];
    for (const o of ops) {
      if (o.mono !== undefined) mono = o.mono;
      if (o.op === "writeLine") w.writeLine(o.line as never, o.now);
      else if (o.op === "noteTimeSource") w.noteTimeSource(o.ts as never);
      else if (o.op === "maybeRotate") w.maybeRotate();
      else if (o.op === "finalize") w.finalize();
      const ap = w.activePath();
      steps.push({ ...o, activePath: ap === null ? null : ap.slice(home.length), closed: [...closed] });
    }
    const files = listTree(home);
    rmSync(home, { recursive: true, force: true });
    E.push({ name, op: "segmentWriter", input: { dir: "/recordings/telemetry/session-a", header: HEADER, ctor, monoInjected }, expected: { steps, closed, files } });
  }
  const L = (ts: number, mono: number, seq: number, metrics: unknown) => ({ ts, mono, seq, metrics });
  writer("opens lazily, writes a header first, and rotates by age into a gz", { maxAgeMs: 1000 }, [
    { op: "writeLine", line: L(1000, 1, 0, { imu: { a: 1 } }), now: 1000, mono: 1000 },
    { op: "writeLine", line: L(1100, 2, 1, { imu: { a: 2 } }), now: 1100, mono: 1100 },
    { op: "writeLine", line: L(2500, 3, 2, { imu: { a: 3 } }), now: 2500, mono: 2500 },
  ], false);
  writer("applies default rotation limits when options are passed as undefined (manager path)", { maxBytes: null, maxAgeMs: null }, [
    { op: "writeLine", line: L(1000, 1, 0, { imu: { a: 1 } }), now: 1000, mono: 1000 },
    { op: "writeLine", line: L(62_000, 2, 1, { imu: { a: 2 } }), now: 62_000, mono: 62_000 },
  ], false);
  const future = Date.UTC(2080, 0, 6, 0, 0, 52);
  const rewound = Date.UTC(2026, 6, 29, 23, 24, 54);
  writer("keeps rolling after the wall clock jumps backwards (Phase 8)", { maxAgeMs: 60_000 }, [
    { op: "writeLine", line: L(future, 1, 0, { imu: { a: 1 } }), now: future, mono: 0 },
    { op: "writeLine", line: L(rewound, 2, 1, { imu: { a: 2 } }), now: rewound, mono: 60_000 },
    { op: "writeLine", line: L(rewound + 60_000, 3, 2, { imu: { a: 3 } }), now: rewound + 60_000, mono: 120_000 },
  ], false);
  writer("rotates by size", { maxBytes: 200, maxAgeMs: 60_000 },
    Array.from({ length: 10 }, (_, i) => ({ op: "writeLine", line: L(1000 + i, i, i, { imu: { a: i } }), now: 1000 + i, mono: 0 })), true);
  writer("finalize() closes the active segment and discards sample-less ones", {}, [
    { op: "writeLine", line: L(1, 1, 0, { gps: { lat: 1 } }), now: 1000, mono: 0 },
    { op: "finalize" }, { op: "finalize" },
  ], true);
  writer("re-emits a header line when the time source changes mid-segment", {}, [
    { op: "writeLine", line: L(1, 1, 0, { imu: { a: 1 } }), now: 1000, mono: 0 },
    { op: "noteTimeSource", ts: "ntp" }, { op: "noteTimeSource", ts: "unsynced" },
    { op: "writeLine", line: L(2, 2, 1, { imu: { a: 2 } }), now: 1001 },
    { op: "finalize" },
  ], true);
  writer("extra: maybeRotate closes an idle segment on monotonic age; noteTimeSource before open is remembered", { maxAgeMs: 500 }, [
    { op: "noteTimeSource", ts: "unsynced" },
    { op: "maybeRotate", mono: 10_000 },
    { op: "writeLine", line: L(5, 1, 0, { imu: { a: 1 } }), now: 5000, mono: 10_000 },
    { op: "noteTimeSource", ts: "ntp" },
    { op: "maybeRotate", mono: 10_499 },
    { op: "maybeRotate", mono: 10_500 },
    { op: "maybeRotate", mono: 20_000 },
    { op: "writeLine", line: L(6, 2, 1, { imu: { a: "ü€" } }), now: 6000, mono: 20_000 },
    { op: "finalize" },
  ], true);

  function orphan(name: string, before: Record<string, string>) {
    const home = mkdtempSync(join(tmpdir(), "wp6-orph-"));
    for (const [rel, text] of Object.entries(before)) {
      mkdirSync(join(home, rel, ".."), { recursive: true });
      writeFileSync(join(home, rel), text);
    }
    const n = segments.finalizeOrphanSegments(join(home, "recordings", "telemetry"));
    const files = listTree(home);
    rmSync(home, { recursive: true, force: true });
    E.push({ name, op: "finalizeOrphanSegments", input: { telemetryRoot: "/recordings/telemetry", files: before }, expected: { finalized: n, files } });
  }
  const head = JSON.stringify(HEADER);
  const good = JSON.stringify({ ts: 1, mono: 1, seq: 0, metrics: {} });
  orphan("gzips leftover plain segments, truncating a torn last line", {
    "/recordings/telemetry/old-session/seg-500-00000.jsonl": `${head}\n${good}\n{"ts":2,"mo`,
  });
  orphan("deletes segments with no complete sample and stray gzip temps", {
    "/recordings/telemetry/old-session/seg-1-00000.jsonl": `${head}\n`,
    "/recordings/telemetry/old-session/seg-2-00001.jsonl": `{"ts":1,"mo`,
    "/recordings/telemetry/old-session/seg-3-00002.jsonl.gz.tmp.123": "junk",
  });
  orphan("is a no-op when the tree does not exist", {});
  orphan("extra: already-closed gz and a session.json are left alone; blank lines do not count", {
    "/recordings/telemetry/s/seg-1-00000.jsonl.gz": "not really gz",
    "/recordings/telemetry/s/seg-2-00001.jsonl": `${head}\n\n\n${good}\n\n`,
    "/recordings/telemetry/s/seg-3-00002.jsonl": `${head}\n\n\n`,
  });
  write("segments/recording-segments.json", "test/recording-segments.test.ts", E);
}

// ---------------------------------------------------------------- F. video segments / sidecars
{
  const F: Case[] = [];
  const c = (name: string, op: string, input: unknown, expected: unknown) => F.push({ name, op, input, expected });
  const T0 = 1_750_000_000_000;
  const manifest = (overrides: Record<string, unknown> = {}) => ({
    v: 1, deviceId: "dev-1", streamKey: "cam0", sessionId: "sess-1", bootId: "boot-1", timeSource: "ntp",
    preset: "1080p15-2mbps", width: 1920, height: 1080, fps: 15, bitrateKbps: 2000, segmentSeconds: 60,
    anchorTs: T0, anchorMono: String(500n * 1_000_000_000n), ...overrides,
  });
  const FROZEN_PAIR = { wallMs: FROZEN_WALL, monoMs: FROZEN_MONO_MS };

  // presets (WP1 functions; the it() lives in this suite)
  c("resolves known presets and falls back to the default", "presets", {}, {
    find1080p30: presets.findVideoPreset("1080p30-4mbps"),
    resolveNope: presets.resolveVideoPreset("nope").name,
    names: presets.videoPresetNames(),
    DEFAULT_VIDEO_PRESET: presets.DEFAULT_VIDEO_PRESET,
  });
  c("settings default preset exists in the table", "presets", {}, {
    defaultPreset: cfg.RECORDING_DEFAULTS.video.preset,
    found: presets.findVideoPreset(cfg.RECORDING_DEFAULTS.video.preset),
  });
  for (const n of ["00007.mp4", "session.json", "00007.json", "notes.mp4", "0007.mp4", "000070.mp4", "99999999999999999999.mp4", "00007.mp4 "]) {
    c(`parses splitmuxsink fragment names only [segmentSeq ${JSON.stringify(n)}]`, "segmentSeq", { name: n }, vs.segmentSeq(n));
  }
  c("parses splitmuxsink fragment names only [sidecarPathFor]", "sidecarPathFor", { mp4Path: "/a/b/00007.mp4" }, vs.sidecarPathFor("/a/b/00007.mp4"));
  c("extra: sidecarPathFor without the suffix", "sidecarPathFor", { mp4Path: "/a/b/x.mp4.bak" }, vs.sidecarPathFor("/a/b/x.mp4.bak"));

  // buildSidecar (P). Where the it() passes no clockPair, the frozen defaults are recorded.
  const bs = (name: string, m: unknown, seq: number, stat: unknown, opts: Record<string, unknown>) => {
    const sc = vs.buildSidecar(m as never, seq, stat as never, opts as never);
    c(name, "buildSidecar", { manifest: m, seq, stat, opts: { ...opts, clockPair: opts.clockPair ?? FROZEN_PAIR }, clockPairInjected: opts.clockPair === undefined },
      { json: JSON.stringify(sc) });
  };
  bs("uses file birthtime when sane (anchor=fstat)", manifest(), 2, { sizeBytes: 123, birthtimeMs: T0 + 120_000, mtimeMs: T0 + 180_000 },
    { finalized: true, clockPair: { wallMs: T0 + 180_000, monoMs: 500_000 + 180_000 } });
  bs("falls back to anchor arithmetic when birthtime is bogus (anchor=computed)", manifest(), 3, { sizeBytes: 5, birthtimeMs: 0, mtimeMs: T0 + 240_000 }, { finalized: false });
  bs("anchors fragment 0 to the next fragment's birthtime (anchor=boundary, Phase 6.6)", manifest(), 0, { sizeBytes: 9, birthtimeMs: T0, mtimeMs: T0 + 62_000 },
    { finalized: true, boundaryEndMs: T0 + 62_000, clockPair: { wallMs: T0 + 62_000, monoMs: 500_000 + 62_000 } });
  bs("ignores a bogus boundary and falls back to fstat", manifest(), 0, { sizeBytes: 9, birthtimeMs: T0, mtimeMs: T0 + 60_000 }, { finalized: true, boundaryEndMs: 0 });
  bs("extra: fractional birthtime/mtime round like Math.round; mtime before birth clamps duration to 0", manifest(), 4,
    { sizeBytes: 77.5, birthtimeMs: T0 + 0.5, mtimeMs: T0 - 3.2 }, { finalized: true, clockPair: { wallMs: T0 + 100.4, monoMs: 1234.6 }, timeSource: "gps" });
  bs("extra: negative-tie rounding (Math.round ties toward +∞)", manifest({ anchorTs: 5 }), 1,
    { sizeBytes: 1, birthtimeMs: T0 + 1.5, mtimeMs: T0 + 2.5 }, { finalized: false, clockPair: { wallMs: T0 + 2.5, monoMs: -0.5 } });
  bs("extra: huge seq keeps float semantics", manifest(), 1e20, { sizeBytes: 1, birthtimeMs: 0, mtimeMs: 0 }, { finalized: true, clockPair: FROZEN_PAIR });

  // Review fix S1: TS-valid but untyped manifests (readSessionManifest checks only v / sessionId /
  // anchorTs). buildSidecar copies every other field through as-is (undefined → key omitted) and
  // coerces segmentSeconds with JS ToNumber in its arithmetic.
  const oddSeg: [string, unknown][] = [
    ["string", "60"], ["hex+ws", " 0x3C\n"], ["empty string", ""], ["null", null], ["true", true],
    ["[60]", [60]], ['["60"]', ["60"]], ["[]", []], ["[1,2]", [1, 2]], ["{}", {}], ["exp", "1e1"],
    [".5", ".5"], ["5.", "5."], ["Infinity", "Infinity"], ["-Infinity", "-Infinity"], ["abc", "abc"],
    ["1_0", "1_0"], ["missing", undefined], ["0b11", "0b11"], ["-0x10", "-0x10"], ["0o17", "0o17"],
    ["[[]]", [[]]], ["[null]", [null]], ["[true]", [true]], ["inf", "inf"], ["+.5e1", "+.5e1"], ["BOM 7", "\uFEFF7"],
  ];
  for (const [label, seg] of oddSeg) {
    bs(`extra: untyped manifest, segmentSeconds ${label} (computed)`, manifest({ segmentSeconds: seg }), 3,
      { sizeBytes: 10, birthtimeMs: 0, mtimeMs: 0 }, { finalized: true, clockPair: FROZEN_PAIR });
  }
  bs("extra: untyped manifest copy-through (no deviceId, string width, null fps, unknown timeSource, object bitrate) [boundary]",
    manifest({ deviceId: undefined, width: "1920", fps: null, timeSource: "gnss-bogus", bitrateKbps: { k: [1.5, 7] }, segmentSeconds: "60", extra: true }), 0,
    { sizeBytes: 9, birthtimeMs: T0, mtimeMs: T0 + 62_000 }, { finalized: true, boundaryEndMs: T0 + 62_000, clockPair: FROZEN_PAIR });
  bs("extra: untyped manifest, timeSource missing and no override [fstat]", manifest({ timeSource: undefined, height: [1080] }), 2,
    { sizeBytes: 9, birthtimeMs: T0 + 120_000, mtimeMs: T0 + 180_000 }, { finalized: false, clockPair: FROZEN_PAIR });
  bs("extra: untyped manifest, timeSource null and no override [fstat]", manifest({ timeSource: null }), 2,
    { sizeBytes: 9, birthtimeMs: T0 + 120_000, mtimeMs: T0 + 180_000 }, { finalized: true, clockPair: FROZEN_PAIR });
  bs("extra: untyped manifest, timeSource null, override wins [fstat]", manifest({ timeSource: null }), 2,
    { sizeBytes: 9, birthtimeMs: T0 + 120_000, mtimeMs: T0 + 180_000 }, { finalized: true, timeSource: "ntp", clockPair: FROZEN_PAIR });

  // mid-session clock steps (Phase 8): 3 loop cases + the negative-mono check
  const M0 = 500_000, SEG = 60_000;
  const steps: [string, number][] = [
    ["forward (fake-hwclock behind → NTP)", 7 * 60_000 + 52_000],
    ["backward (2080 → NTP rewind)", -(53 * 365 * 24 * 3600_000)],
    ["small forward", 3_000],
  ];
  for (const [label, delta] of steps) {
    const m = manifest({ anchorTs: T0 - delta, anchorMono: String(BigInt(M0) * 1_000_000n) });
    const pair = { wallMs: T0 + 4 * SEG, monoMs: M0 + 4 * SEG };
    const calls = [] as unknown[];
    for (let seq = 0; seq < 4; seq++) {
      const birth = T0 + seq * SEG;
      const stat = { sizeBytes: 1000, birthtimeMs: birth, mtimeMs: birth + SEG };
      const opts = { finalized: true, ...(seq === 0 ? { boundaryEndMs: T0 + SEG } : {}), clockPair: pair };
      calls.push({ seq, stat, opts, json: JSON.stringify(vs.buildSidecar(m as never, seq, stat, opts)) });
    }
    c(`loses no segment and stays contiguous after a ${label} step`, "buildSidecarSeries", { manifest: m }, calls);
  }
  c("keeps startMono positive where the old anchor-difference math went negative", "oldStyleMono",
    { M0, T0, delta: 53 * 365 * 24 * 3600_000 }, { oldStyle: M0 + (T0 - (T0 + 53 * 365 * 24 * 3600_000)) });

  // Filesystem model: a session dir listing with stats → sweep plan.
  function sessionDir(home: string, streamKey: string, sessionId: string, fragments: number): string {
    const dir = join(home, "recordings", "video", streamKey, sessionId);
    mkdirSync(dir, { recursive: true });
    for (let i = 0; i < fragments; i++) {
      const p = join(dir, `${String(i).padStart(5, "0")}.mp4`);
      writeFileSync(p, Buffer.alloc(1000 + i, 1));
      utimesSync(p, (T0 + i * 60_000) / 1000, (T0 + (i + 1) * 60_000) / 1000);
    }
    return dir;
  }
  const listing = (dir: string) => {
    if (!existsSync(dir)) return [];
    return readdirSync(dir).map((name) => {
      const st = statSync(join(dir, name));
      return st.isDirectory() ? { name } : { name, stat: { sizeBytes: st.size, birthtimeMs: st.birthtimeMs, mtimeMs: st.mtimeMs } };
    });
  };
  const jsons = (dir: string) => {
    const out: Record<string, string> = {};
    if (!existsSync(dir)) return out;
    for (const n of readdirSync(dir).sort()) if (n.endsWith(".json")) out[n] = readFileSync(join(dir, n), "utf8");
    return out;
  };
  type SweepStep = { opts: Record<string, unknown>; sweep?: boolean; manifest?: unknown };
  function sweepCase(name: string, setup: (home: string) => { dir: string; manifest: unknown }, stepOpts: Record<string, unknown>[], extraSetup?: (dir: string) => void) {
    const home = mkdtempSync(join(tmpdir(), "wp6-vs-"));
    const { dir, manifest: m } = setup(home);
    extraSetup?.(dir);
    const steps: unknown[] = [];
    for (const opts of stepOpts) {
      const entries = listing(dir);
      const before = jsons(dir);
      const beforeNames = new Set(existsSync(dir) ? readdirSync(dir) : []);
      const res = vs.sweepSessionDir(dir, m as never, opts as never);
      const after = jsons(dir);
      const afterNames = new Set(existsSync(dir) ? readdirSync(dir) : []);
      const written: Record<string, string> = {};
      for (const [n, t] of Object.entries(after)) if (before[n] !== t) written[n] = t;
      const deleted = [...beforeNames].filter((n) => !afterNames.has(n)).sort();
      steps.push({
        entries,
        opts: { ...opts, clockPair: opts.clockPair ?? FROZEN_PAIR },
        clockPairInjected: opts.clockPair === undefined,
        expected: { closed: res.closed.map((p) => p.replace(home, "/home")), activePath: res.activePath === null ? null : res.activePath.replace(home, "/home"), written, deleted },
      });
    }
    rmSync(home, { recursive: true, force: true });
    c(name, "sweepSessionDir", { dir: dir.replace(home, "/home"), manifest: m }, steps);
  }
  sweepCase("sidecars all but the newest fragment and reports it active",
    (h) => ({ dir: sessionDir(h, "cam0", "sess-1", 3), manifest: manifest() }), [{ includeNewest: false }]);
  sweepCase("gives fragment 0 a boundary anchor when fragment 1 exists (Phase 6.6)",
    (h) => ({ dir: sessionDir(h, "cam0", "sess-b", 3), manifest: manifest() }), [{ includeNewest: false }]);
  sweepCase("is idempotent — a second sweep writes nothing new",
    (h) => ({ dir: sessionDir(h, "cam0", "sess-1", 3), manifest: manifest() }), [{ includeNewest: false }, { includeNewest: false }]);
  sweepCase("includeNewest closes the last fragment (stop path)",
    (h) => ({ dir: sessionDir(h, "cam0", "sess-1", 2), manifest: manifest() }), [{ includeNewest: false }, { includeNewest: true, finalized: true }]);
  sweepCase("reaps 0-byte power-cut fragments at session close instead of sidecaring them",
    (h) => ({ dir: sessionDir(h, "cam0", "sess-1", 2), manifest: manifest() }), [{ includeNewest: false }, { includeNewest: true }],
    (dir) => writeFileSync(join(dir, "00002.mp4"), Buffer.alloc(0)));
  sweepCase("handles an empty/missing dir",
    (h) => ({ dir: join(h, "nope"), manifest: manifest() }), [{ includeNewest: true }]);
  sweepCase("sidecars every closed fragment even when the clock stepped since spawn",
    (h) => ({ dir: sessionDir(h, "cam0", "sess-step", 4), manifest: manifest({ anchorTs: Date.UTC(2080, 0, 6, 0, 0, 52) }) }),
    [{ includeNewest: true, clockPair: { wallMs: T0 + 240_000, monoMs: 500_000 + 240_000 } }]);
  // timeSource re-stamping (Phase 1)
  sweepCase("sidecars carry the sweep-time value; session.json keeps the spawn value",
    (h) => { const m = manifest({ timeSource: "unsynced" }); const dir = sessionDir(h, "cam0", "sess-1", 3); vs.writeSessionManifest(dir, m as never); return { dir, manifest: m }; },
    [{ includeNewest: false, timeSource: "ntp" }]);
  sweepCase("crash recovery without an override falls back to the manifest value",
    (h) => { const m = manifest({ timeSource: "rtc" }); const dir = sessionDir(h, "cam0", "sess-dead", 2); vs.writeSessionManifest(dir, m as never); return { dir, manifest: m }; },
    [{ includeNewest: true }]);
  sweepCase("extra: a stat race (next fragment vanished) and a bogus-birthtime listing",
    (h) => ({ dir: sessionDir(h, "cam0", "sess-x", 1), manifest: manifest() }), [{ includeNewest: true, finalized: false }]);

  // session manifest round-trip
  {
    const home = mkdtempSync(join(tmpdir(), "wp6-mf-"));
    const dir = join(home, "recordings", "video", "cam0", "sess-1");
    mkdirSync(dir, { recursive: true });
    vs.writeSessionManifest(dir, manifest() as never);
    const text = readFileSync(join(dir, "session.json"), "utf8");
    c("writes and reads back", "manifestRoundTrip", { manifest: manifest() }, { text, read: vs.readSessionManifest(dir) });
    const bad = join(home, "x");
    mkdirSync(bad, { recursive: true });
    c("returns null for missing or invalid manifests [missing]", "readSessionManifestText", { text: null }, vs.readSessionManifest(bad));
    writeFileSync(join(bad, "session.json"), "{broken");
    c("returns null for missing or invalid manifests [broken]", "readSessionManifestText", { text: "{broken" }, vs.readSessionManifest(bad));
    for (const [i, t] of ['{"v":2,"sessionId":"s","anchorTs":1}', '{"v":1,"sessionId":5,"anchorTs":1}', '{"v":1,"sessionId":"s","anchorTs":"1"}', "[1]", "null", '{"v":1.0,"sessionId":"s","anchorTs":1.5,"extra":true}'].entries()) {
      writeFileSync(join(bad, "session.json"), t);
      c(`extra: readSessionManifest validation [${i}]`, "readSessionManifestText", { text: t }, vs.readSessionManifest(bad));
    }
    rmSync(home, { recursive: true, force: true });
  }

  // recoverOrphanVideoSessions: tree model
  function recoverCase(name: string, setup: (home: string) => void, active: string[]) {
    const home = mkdtempSync(join(tmpdir(), "wp6-rec-"));
    setup(home);
    const root = join(home, "recordings", "video");
    const sessions: unknown[] = [];
    if (existsSync(root)) {
      for (const sk of readdirSync(root).sort()) {
        const sd = join(root, sk);
        if (!statSync(sd).isDirectory()) continue;
        for (const sid of readdirSync(sd).sort()) {
          const d = join(sd, sid);
          if (!statSync(d).isDirectory()) continue;
          const mp = join(d, "session.json");
          sessions.push({ streamKey: sk, sessionId: sid, manifestText: existsSync(mp) ? readFileSync(mp, "utf8") : null, entries: listing(d) });
        }
      }
    }
    const before: Record<string, string> = {};
    const walkJson = (d: string, into: Record<string, string>) => {
      if (!existsSync(d)) return;
      for (const n of readdirSync(d)) { const p = join(d, n); if (statSync(p).isDirectory()) walkJson(p, into); else if (n.endsWith(".json")) into[p.replace(home, "/home")] = readFileSync(p, "utf8"); }
    };
    walkJson(root, before);
    const n = vs.recoverOrphanVideoSessions(root, new Set(active));
    const after: Record<string, string> = {};
    walkJson(root, after);
    const written: Record<string, string> = {};
    for (const [p, t] of Object.entries(after)) if (before[p] !== t) written[p] = t;
    rmSync(home, { recursive: true, force: true });
    c(name, "recoverOrphanVideoSessions", { videoRoot: "/home/recordings/video", sessions, activeSessionIds: active, clockPair: FROZEN_PAIR }, { recovered: n, written });
  }
  recoverCase("backfills sidecars for dead sessions, marks the newest unfinalized", (h) => {
    const dir = sessionDir(h, "cam0", "sess-dead", 3);
    vs.writeSessionManifest(dir, manifest({ sessionId: "sess-dead" }) as never);
  }, []);
  recoverCase("skips active sessions and sessions without a manifest", (h) => {
    const active = sessionDir(h, "cam0", "sess-active", 2);
    vs.writeSessionManifest(active, manifest({ sessionId: "sess-active" }) as never);
    sessionDir(h, "cam1", "sess-nomanifest", 2);
  }, ["sess-active"]);
  recoverCase("no-ops without a video tree", () => {}, []);
  recoverCase("extra: a 0-byte newest fragment is reaped, an older one skipped", (h) => {
    const dir = sessionDir(h, "cam0", "sess-pc", 2);
    writeFileSync(join(dir, "00002.mp4"), Buffer.alloc(0));
    vs.writeSessionManifest(dir, manifest({ sessionId: "sess-pc", timeSource: "gps" }) as never);
  }, []);

  // Review fix S1: recovery gates on the TS rule only — a manifest that passes it but has
  // odd/missing fields is swept (sidecars copy the odd values through); one that fails it is skipped.
  recoverCase("extra: a TS-valid manifest with odd or missing fields is still recovered", (h) => {
    const d1 = sessionDir(h, "cam0", "sess-odd", 2);
    writeFileSync(join(d1, "session.json"), JSON.stringify(manifest({
      sessionId: "sess-odd", deviceId: undefined, width: "1920", fps: null, timeSource: "gnss-bogus",
      segmentSeconds: "60", extra: { a: [1, 2] },
    })));
    const d2 = sessionDir(h, "cam1", "sess-noseg", 2);
    writeFileSync(join(d2, "session.json"), JSON.stringify(manifest({ sessionId: "sess-noseg", segmentSeconds: undefined, bitrateKbps: 2000.5 })));
    const d3 = sessionDir(h, "cam1", "sess-invalid", 1);
    writeFileSync(join(d3, "session.json"), JSON.stringify(manifest({ sessionId: "sess-invalid", anchorTs: String(T0) })));
  }, []);

  write("sidecar/video-segments.json", "test/recording-video-segments.test.ts", F, { frozenClocks: FROZEN_PAIR });
}
