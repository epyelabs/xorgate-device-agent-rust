// Phase 4 WP3 — dump `reconcileCameraMount` (src/hardware/boot-config.ts:329-457)
// and the privileged writer (system/xorgate-camrotate) scenario by scenario,
// following test/boot-config.test.ts @89b5c64 (:189-494) exactly: the same
// CONFIG_TXT, HARDWARE block, device() fixture (pipeline dir + fake device
// tree) and helperWriter (the REAL helper with XORGATE_BOOT_CONFIG), plus extra
// branches for every cloud-visible `detail` string.
//
// Each scenario records, per pass: the returned state (now fixed), every
// log(level, msg) call, every probe(sensor) call, and config.txt afterwards.
// Writer scenarios record each helper call's {ok, changed, detail}, the file
// and the .bak-camrotate file.
//
//   (cwd = a scratch copy of the JS repo @89b5c64 with node_modules; run in
//   docker node:20-bookworm --platform linux/arm64 -e TZ=UTC, as tools/replay/js/run-tsx.sh)
//   npx tsx .rig-drivers/dump-camera-mount.mts > fixtures/camera_mount/camera-mount.json
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { reconcileCameraMount, CAMROTATE_HELPER } from "../src/hardware/boot-config.ts";

const exec = promisify(execFile);
const HELPER = join(process.cwd(), "system", "xorgate-camrotate");
const NOW = 1785326400000;

const CONFIG_TXT = `# Raspberry Pi config
camera_auto_detect=0
dtoverlay=vc4-kms-v3d
[cm4]
otg_mode=1
[cm5]
dtoverlay=dwc2,dr_mode=host
[all]
dtoverlay=imx290,cam0,4lane
dtoverlay=imx290,cam1
#dtoverlay=googlevoicehat-soundcard
`;

const HARDWARE = {
  v: 2, platform: "rpi-cm4", encoder: "hw-h264", rails: false, statusLed: null, imu: "bno085",
  video: [
    { key: "cam0", connector: "csi0", sensor: "imx290", codec: "h264" },
    { key: "cam1", connector: "csi1", sensor: "imx290", codec: "h264" },
  ],
};

function device(opts: { config?: string; inForce?: Record<string, number> } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "xg-bootcfg-"));
  const bootConfigPath = join(dir, "config.txt");
  writeFileSync(bootConfigPath, opts.config ?? CONFIG_TXT);
  const pipelineDir = join(dir, "pipelines");
  mkdirSync(pipelineDir, { recursive: true });
  const dtRoot = join(dir, "device-tree");
  for (const [key, deg] of Object.entries(opts.inForce ?? {})) {
    const node = `/base/soc/i2c0mux/i2c@${key.slice(3)}/imx290@1a`;
    writeFileSync(join(pipelineDir, `${key}.camera`), `${node}\n`);
    const nodeDir = join(dtRoot, node.replace("/base/", ""));
    mkdirSync(nodeDir, { recursive: true });
    const buf = Buffer.alloc(4);
    buf.writeUInt32BE(deg, 0);
    writeFileSync(join(nodeDir, "rotation"), buf);
  }
  return { dir, bootConfigPath, pipelineDir, dtRoot, read: () => readFileSync(bootConfigPath, "utf8") };
}

function helperWriter(bootConfigPath: string) {
  return async (sensor: string, camToken: string, deg: 0 | 180) => {
    try {
      await exec(HELPER, [sensor, camToken, String(deg)], {
        env: { ...process.env, XORGATE_BOOT_CONFIG: bootConfigPath },
      });
      return { ok: true, changed: true, detail: "" };
    } catch (err) {
      const e = err as { code?: number; stderr?: string };
      if (e.code === 3) return { ok: true, changed: false, detail: "" };
      return { ok: false, changed: false, detail: e.stderr?.trim() ?? `exit ${e.code}` };
    }
  };
}

const mount = (cameras: Record<string, number>) => ({
  cameras: Object.fromEntries(Object.entries(cameras).map(([k, v]) => [k, { rotationDeg: v }])),
});

type Scenario = {
  name: string;
  config?: string;
  inForce?: Record<string, number>;
  settings: unknown; // undefined → absent namespace
  hardware: "HARDWARE" | null;
  bootConfigPath?: "fixture" | null | "missing" | "dir";
  probe: { ok: boolean; supports: boolean };
  apply: "helper" | { ok: boolean; changed: boolean; detail: string };
  passes: number;
  chmod444?: boolean;
};

const S = { ok: true, supports: true };
const scenarios: Scenario[] = [
  { name: "does nothing at all when the namespace is absent", inForce: { cam0: 0 }, settings: undefined, hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "edits the overlay, reports pending-reboot, and does not touch the file again", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 4 },
  { name: "reports applied once the kernel has actually booted with it", config: CONFIG_TXT.replace("imx290,cam0,4lane", "imx290,cam0,4lane,rotation=180"), inForce: { cam0: 180 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "re-edits after a re-image dropped the token, and asks for one more reboot", inForce: { cam0: 180 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "leaves an unmanaged camera's overlay strictly alone", inForce: { cam0: 0, cam1: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "refuses to edit a sensor whose overlay has no rotation parameter", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: { ok: true, supports: false }, apply: "helper", passes: 1 },
  { name: "refuses when the probe itself could not run", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: { ok: false, supports: false }, apply: "helper", passes: 1 },
  { name: "says so when there is no overlay line, or no camera by that name", config: "camera_auto_detect=1\n", inForce: { cam0: 0 }, settings: mount({ cam0: 180, cam7: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "reports a failed privileged write instead of pretending it landed", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: { ok: false, changed: false, detail: "sudo: a password is required" }, passes: 1 },
  { name: "says it could not confirm when the device tree is unreadable", settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "works on a v1 device with no hardware block", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: null, probe: S, apply: "helper", passes: 1 },
  { name: "handles an empty camera map (managing nothing) without reading the boot config", inForce: { cam0: 0 }, settings: { cameras: {} }, hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "rewrites duplicate lines in different sections to agree", config: "[cm4]\ndtoverlay=imx290,cam0\n[cm5]\ndtoverlay=imx290,cam0,4lane\n", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "survives a read-only boot config by reporting, not throwing", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1, chmod444: true },
  // extras — every remaining cloud-visible detail / log branch
  { name: "[extra] no config.txt on this device", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", bootConfigPath: null, probe: S, apply: "helper", passes: 1 },
  { name: "[extra] config.txt cannot be read (ENOENT)", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", bootConfigPath: "missing", probe: S, apply: "helper", passes: 1 },
  { name: "[extra] config.txt cannot be read (EISDIR)", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", bootConfigPath: "dir", probe: S, apply: "helper", passes: 1 },
  { name: "[extra] 180 back to 0 removes the token, pending until reboot", config: CONFIG_TXT.replace("imx290,cam0,4lane", "imx290,cam0,4lane,rotation=180"), inForce: { cam0: 180 }, settings: mount({ cam0: 0 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 2 },
  { name: "[extra] the helper refuses an uppercase overlay name (usage)", config: "dtoverlay=IMX290,cam0\n", inForce: { cam0: 0 }, settings: mount({ cam0: 180 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "[extra] the helper refuses a two-digit camera token (usage)", config: "dtoverlay=imx290,cam10\n", settings: mount({ cam10: 180 }), hardware: null, probe: S, apply: "helper", passes: 1 },
  { name: "[extra] both cameras managed, sorted keys, one unconfirmed", inForce: { cam1: 0 }, settings: mount({ cam1: 180, cam0: 0 }), hardware: "HARDWARE", probe: S, apply: "helper", passes: 1 },
  { name: "[extra] a stream key the hardware block does not declare, v1 fallback refuses a non-camN key", inForce: { cam0: 0 }, settings: mount({ front: 180 }), hardware: null, probe: S, apply: "helper", passes: 1 },
];

async function runScenario(s: Scenario) {
  const d = device({ config: s.config, inForce: s.inForce });
  if (s.chmod444) chmodSync(d.bootConfigPath, 0o444);
  let bootConfigPath: string | null = d.bootConfigPath;
  if (s.bootConfigPath === null) bootConfigPath = null;
  if (s.bootConfigPath === "missing") bootConfigPath = join(d.dir, "nope", "config.txt");
  if (s.bootConfigPath === "dir") bootConfigPath = d.pipelineDir;
  const passes = [];
  for (let i = 0; i < s.passes; i += 1) {
    const logs: unknown[] = [];
    const probes: string[] = [];
    const applies: unknown[] = [];
    const writer = helperWriter(d.bootConfigPath);
    const state = await reconcileCameraMount({
      settings: s.settings as never,
      hardware: s.hardware === "HARDWARE" ? (HARDWARE as never) : null,
      pipelineDir: d.pipelineDir,
      bootConfigPath,
      dtRoot: d.dtRoot,
      now: () => NOW,
      log: (level, msg) => logs.push({ level, msg: msg.split(d.dir).join("<dir>") }),
      probe: async (sensor) => {
        probes.push(sensor);
        return s.probe;
      },
      applyRotation: async (sensor, cam, deg) => {
        applies.push([sensor, cam, deg]);
        if (s.apply === "helper") return writer(sensor, cam, deg);
        return s.apply;
      },
    });
    const json = JSON.stringify(state);
    passes.push({
      state: state === null ? null : JSON.parse(json.split(d.dir).join("<dir>")),
      logs,
      probes,
      applies,
      file: d.read(),
    });
  }
  return passes;
}

async function writerCases() {
  const out = [];
  {
    const d = device();
    const r = await helperWriter(d.bootConfigPath)("imx290", "cam0", 180);
    out.push({ name: "edits one token and leaves every other line and parameter alone", calls: [["imx290", "cam0", 180]], results: [r], file: d.read(), bak: existsSync(`${d.bootConfigPath}.bak-camrotate`) ? readFileSync(`${d.bootConfigPath}.bak-camrotate`, "utf8") : null });
  }
  {
    const d = device();
    const w = helperWriter(d.bootConfigPath);
    const r1 = await w("imx290", "cam0", 180);
    const r2 = await w("imx290", "cam0", 180);
    out.push({ name: "is idempotent: a second identical call writes nothing (exit 3)", calls: [["imx290", "cam0", 180], ["imx290", "cam0", 180]], results: [r1, r2], file: d.read(), bak: readFileSync(`${d.bootConfigPath}.bak-camrotate`, "utf8") });
  }
  {
    const d = device();
    const w = helperWriter(d.bootConfigPath);
    const r1 = await w("imx290", "cam0", 180);
    const r2 = await w("imx290", "cam0", 0);
    out.push({ name: "removes the token again for 0 rather than writing rotation=0", calls: [["imx290", "cam0", 180], ["imx290", "cam0", 0]], results: [r1, r2], file: d.read(), bak: readFileSync(`${d.bootConfigPath}.bak-camrotate`, "utf8") });
  }
  {
    const d = device();
    const r1 = await helperWriter(d.bootConfigPath)("imx290", "cam0", 180);
    const r2 = await helperWriter(d.bootConfigPath)("imx290", "cam1", 180);
    out.push({ name: "keeps a backup of the file as it was before the first edit", calls: [["imx290", "cam0", 180], ["imx290", "cam1", 180]], results: [r1, r2], file: d.read(), bak: readFileSync(`${d.bootConfigPath}.bak-camrotate`, "utf8") });
  }
  {
    const d = device();
    const w = helperWriter(d.bootConfigPath);
    const calls: [string, string, number][] = [["imx477", "cam0", 180], ["imx290", "cam0", 90], ["../../etc/passwd", "cam0", 180]];
    const results = [];
    for (const [a, b, c] of calls) {
      const r = await w(a, b, c as 0);
      results.push({ ...r, detail: r.detail.split(d.dir).join("<dir>") });
    }
    out.push({ name: "refuses a line that belongs to another overlay, and bad arguments", calls, results, file: d.read(), bak: existsSync(`${d.bootConfigPath}.bak-camrotate`) ? readFileSync(`${d.bootConfigPath}.bak-camrotate`, "utf8") : null });
  }
  return out;
}

const cases = [];
for (const s of scenarios) cases.push({ ...s, passes: await runScenario(s), passCount: s.passes });
const body = readFileSync(HELPER, "utf8");
console.log(JSON.stringify({
  jsSourceFile: "test/boot-config.test.ts",
  jsCommit: "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  generatedBy: "plans/device-agent-js-to-rust/phase-4/evidence/wp3-tools/dump-camera-mount.mts via npx tsx in a scratch copy of the JS repo @89b5c64 (docker node:20-bookworm linux/arm64, TZ=UTC, mawk; node " + process.version + "; the REAL system/xorgate-camrotate run directly with XORGATE_BOOT_CONFIG as the TS test does); paths under the scratch dir are rewritten to <dir>",
  constants: { CAMROTATE_HELPER, CONFIG_TXT, HARDWARE, NOW, helperNeverReboots: !/^\s*(sudo\s+)?(systemctl\s+reboot|reboot|shutdown)\b/m.test(body) },
  cases: [],
  reconcile: cases,
  writer: await writerCases(),
}, null, 2));
