// Phase 3 WP5 — dump `reconcileHardware` (src/media/hardware-reconcile.ts:77-182)
// pass by pass on a tmp dir with a fake `listExec`: the 4 async it()s of
// test/hardware-block.test.ts › reconcileHardware (CF26), the bench's
// two-camera CM5 block with the bench listing (hardware-block.test.ts:80-86)
// for BOTH socket roots (the JS /var/lib/xorgate/shm and the Rust
// /var/lib/xorgate-rs/shm — D4), and the remaining branches (sensor mismatch,
// unmapped path, zero cameras, unrecognizable error, custom .gst.direct,
// stale .camera, trim-equal files, missing dir).
//
// Each pass records: log calls (level, msg), statuses (Map order),
// changedStreams, cameras, and the directory afterwards (every file's bytes;
// null = the directory does not exist).
//
//   tools/replay/js/run-tsx.sh dump-hardware-reconcile.mts > fixtures/hardware_reconcile/reconcile.json
import { mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { reconcileHardware } from "../src/media/hardware-reconcile.ts";
import { buildBridgedKvsPipeline, buildDirectKvsPipeline } from "../src/media/pipeline-builder.ts";

type Listing = { stdout: string; stderr: string; exitCode: number | null; failed: boolean; notFound: boolean };
const ok = (stdout: string): Listing => ({ stdout, stderr: "", exitCode: 0, failed: false, notFound: false });
const NOT_FOUND: Listing = { stdout: "", stderr: "", exitCode: null, failed: true, notFound: true };

const CM4_BLOCK = {
  v: 2, platform: "rpi-cm4", encoder: "hw-h264", rails: false, statusLed: null, imu: "bno085",
  video: [{ key: "cam0", connector: "csi0", sensor: "imx290", codec: "h264", label: "Front" }],
};
const CM4_TWO = { ...CM4_BLOCK, video: [...CM4_BLOCK.video, { key: "cam1", connector: "csi1", sensor: "imx290", codec: "h264" }] };
const CM4_LISTING = `Available cameras
-----------------
0 : imx290 [1920x1080 12-bit RGGB] (/base/soc/i2c0mux/i2c@0/imx290@1a)
    Modes: whatever
`;
// Argus CM5 Rev1 (research/backend-device-contract.md:119), the bench's block.
const CM5_BENCH = {
  v: 2, platform: "rpi-cm5", encoder: "sw-h264", rails: true, statusLed: { r: 12, g: 21, b: 16 }, imu: "bno085",
  video: [
    { key: "cam0", connector: "csi0", sensor: "imx290", codec: "h264" },
    { key: "cam1", connector: "csi1", sensor: "imx290", codec: "h264" },
  ],
};
const CM5_LISTING =
  "Available cameras\n-----------------\n" +
  "0 : imx290 [1920x1080 12-bit RGGB] (/base/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a)\n" +
  "    Modes: 'SRGGB10_CSI2P' : 1280x720 [30.00 fps]\n" +
  "1 : imx290 [1920x1080 12-bit RGGB] (/base/axi/pcie@1000120000/rp1/i2c@70000/imx290@1a)\n" +
  "    Modes: 'SRGGB10_CSI2P' : 1280x720 [60.00 fps]\n";
const CM5_THREE =
  CM5_LISTING + "2 : imx477 [4056x3040 12-bit RGGB] (/base/axi/pcie@1000120000/rp1/i2c@99999/imx477@1a)\n";

interface Pass { pre?: Record<string, string | null>; listing: Listing }
interface Scenario { name: string; block: unknown; sockRoot: string; subdir?: string; passes: Pass[] }

const JS_SOCK = "/var/lib/xorgate/shm";
const RS_SOCK = "/var/lib/xorgate-rs/shm";
const SCENARIOS: Scenario[] = [
  {
    name: "renders the trio for a matched camera and reports changed streams",
    block: CM4_BLOCK, sockRoot: JS_SOCK,
    passes: [{ listing: ok(CM4_LISTING) }, { listing: ok(CM4_LISTING) }],
  },
  {
    name: "re-renders a recognized render on encoder change, but never a custom pipeline",
    block: CM4_BLOCK, sockRoot: JS_SOCK,
    passes: [
      { pre: { "cam0.gst": `${buildBridgedKvsPipeline("sw-h264", `${JS_SOCK}/cam0.sock`)}\n` }, listing: ok(CM4_LISTING) },
      { pre: { "cam0.gst": "videotestsrc ! my custom thing ! appsink\n" }, listing: ok(CM4_LISTING) },
    ],
  },
  {
    name: "reports no-camera when the connector is empty, touching nothing",
    block: CM4_TWO, sockRoot: JS_SOCK,
    passes: [{ listing: ok(CM4_LISTING) }],
  },
  {
    name: "touches nothing when enumeration is unavailable",
    block: CM4_BLOCK, sockRoot: JS_SOCK,
    passes: [{ listing: NOT_FOUND }],
  },
  {
    name: "bench CM5 two-camera block, JS socket root (= /etc/xorgate/pipelines renders)",
    block: CM5_BENCH, sockRoot: JS_SOCK,
    passes: [{ listing: ok(CM5_LISTING) }, { listing: ok(CM5_LISTING) }],
  },
  {
    name: "bench CM5 two-camera block, Rust socket root (/var/lib/xorgate-rs/etc/pipelines, D4)",
    block: CM5_BENCH, sockRoot: RS_SOCK,
    passes: [
      { listing: ok(CM5_LISTING) },
      { listing: ok(CM5_LISTING) },
      // A JS-socket render under the Rust home is NOT recognized (bench recipe step 2): custom.
      { pre: { "cam0.gst": `${buildBridgedKvsPipeline("sw-h264", `${JS_SOCK}/cam0.sock`)}\n` }, listing: ok(CM5_LISTING) },
    ],
  },
  {
    name: "sensor mismatch is used anyway (warned); an unmapped enumerated path is warned",
    block: { ...CM5_BENCH, video: [CM5_BENCH.video[0], { ...CM5_BENCH.video[1], sensor: "imx477" }] },
    sockRoot: RS_SOCK,
    passes: [{ listing: ok(CM5_THREE) }],
  },
  {
    name: "zero cameras (No cameras available!, exit 255) is a real empty result: all no-camera, dir created",
    block: CM5_BENCH, sockRoot: RS_SOCK, subdir: "made/pipelines",
    passes: [{ listing: { stdout: "", stderr: "ERROR: *** No cameras available! ***\n", exitCode: 255, failed: true, notFound: false } }],
  },
  {
    name: "an error without a recognizable listing cannot enumerate: dir not created",
    block: CM5_BENCH, sockRoot: RS_SOCK, subdir: "never/pipelines",
    passes: [{ listing: { stdout: "", stderr: "ERROR: libcamera pipeline handler failed\n", exitCode: 1, failed: true, notFound: false } }],
  },
  {
    name: "custom .gst.direct left alone; stale .camera ground-truthed; trim-equal .gst not rewritten",
    block: CM5_BENCH, sockRoot: RS_SOCK,
    passes: [
      {
        pre: {
          "cam0.gst": buildBridgedKvsPipeline("sw-h264", `${RS_SOCK}/cam0.sock`) + "  \n\n",
          "cam0.camera": "/old/path/imx290@1a\n",
          "cam0.gst.direct": "libcamerasrc ! my direct ! appsink\n",
          "cam1.gst.direct": `${buildDirectKvsPipeline("sw-h264", "cam1", "/base/axi/pcie@1000120000/rp1/i2c@70000/imx290@1a")}`,
        },
        listing: ok(CM5_LISTING),
      },
      { pre: { "cam1.camera": "  /base/axi/pcie@1000120000/rp1/i2c@70000/imx290@1a  " }, listing: ok(CM5_LISTING) },
    ],
  },
];

function snapshotDir(dir: string): Record<string, string> | null {
  if (!existsSync(dir)) return null;
  const out: Record<string, string> = {};
  for (const n of readdirSync(dir).sort()) out[n] = readFileSync(join(dir, n), "utf8");
  return out;
}

const cases: unknown[] = [];
for (const [n, sc] of SCENARIOS.entries()) {
  const root = `/tmp/xg-reconcile/s${n}`;
  rmSync(root, { recursive: true, force: true });
  mkdirSync(root, { recursive: true });
  const dir = join(root, sc.subdir ?? "pipelines");
  const passes: unknown[] = [];
  for (const p of sc.passes) {
    if (p.pre) {
      mkdirSync(dir, { recursive: true });
      for (const [f, c] of Object.entries(p.pre)) {
        if (c === null) rmSync(join(dir, f), { force: true });
        else writeFileSync(join(dir, f), c);
      }
    }
    const logs: [string, string][] = [];
    const res = await reconcileHardware(sc.block as any, {
      pipelineDir: dir,
      shmSocketPathFor: (key: string) => `${sc.sockRoot}/${key}.sock`,
      log: (level, msg) => logs.push([level, msg]),
      listExec: async () => p.listing,
    });
    passes.push({
      pre: p.pre ?? null,
      listing: p.listing,
      expected: {
        logs,
        statuses: [...res.statuses.entries()],
        changedStreams: res.changedStreams,
        cameras: res.cameras,
        files: snapshotDir(dir),
      },
    });
  }
  cases.push({ name: sc.name, input: { block: sc.block, sockRoot: sc.sockRoot, dir: sc.subdir ?? "pipelines" }, passes });
}
process.stdout.write(
  JSON.stringify(
    {
      jsSourceFile: "src/media/hardware-reconcile.ts (reconcileHardware :77-182) + test/hardware-block.test.ts › reconcileHardware (4 async it()s, CF26)",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy: "plans/device-agent-js-to-rust/phase-3/evidence/wp5-tools/dump-hardware-reconcile.mts via tools/replay/js/run-tsx.sh",
      cases,
    },
    null,
    2,
  ) + "\n",
);
