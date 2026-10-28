// Phase 5b CF78 (WP4): copied from plans/device-agent-js-to-rust/phase-2/evidence/wp2-tools/dump-imu-scenarios.mts so redump-all.sh
// re-runs it at any ref; only the JS root (cwd), the output dir ($OUT) and the jsCommit stamp
// ($JS_COMMIT_FULL) were changed.
// Phase 2 WP2 — dump the IMU collector scenarios (fixtures/collectors/imu-scenarios.json).
// Run from a `git archive 89b5c64` scratch copy (npm ci done):
//   npx tsx dump-imu-scenarios.mts > imu-scenarios.json
// Each scenario runs in its OWN node process (exec.ts caches commandExists per
// process) with XORGATE_MOCK=0, a PATH of shims only (i2cdetect / python3 /
// which / sh / cat — so "missing" really is missing), XORGATE_PYTHON_DIR and
// XORGATE_HOME in a scratch dir, Date.now / process.hrtime.bigint mocked to one
// clock (wall = BASE + mono) and console.log/warn/error captured per step.
// The python3 shim is the protocol the Rust test re-implements: it appends its
// argv to argv.log, bumps spawn.count, relays a FIFO line by line (lines
// starting "E:" go to stderr), then exits with the code in exit.code ("K" =
// SIGKILL itself).
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { open } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BASE = 1_781_000_000_000;

function grid(addrs: number[]): string {
  let out = "     0  1  2  3  4  5  6  7  8  9  a  b  c  d  e  f\n";
  for (let row = 0; row < 8; row++) {
    let line = `${(row * 16).toString(16).padStart(2, "0")}:`;
    for (let col = 0; col < 16; col++) {
      const a = row * 16 + col;
      if (a < 0x03 || a > 0x77) line += "   ";
      else line += addrs.includes(a) ? ` ${a.toString(16).padStart(2, "0")}` : " --";
    }
    out += line.replace(/\s+$/, "") + "\n";
  }
  return out;
}

const S1 = '{"type": "sample", "quat": {"r": 0.9238795, "i": 0.0, "j": 0.0, "k": 0.3826834}, "linaccel": {"x": 0.01234, "y": -0.5678, "z": 9.80665}}';
const S2 = '{"type": "sample", "quat": {"r": 0.7071068, "i": 0.7071068, "j": 0.0, "k": 0.0}, "linaccel": {"x": 1.0005, "y": 0.0, "z": -0.0004}}';
const S3 = '{"type": "sample", "quat": {"r": 0.5, "i": 0.5, "j": 0.5, "k": 0.5}, "linaccel": {"x": 0.12345, "y": 0.23456, "z": 0.34567}}';
const MOUNT = { imuMount: { enabled: true, rollDeg: 0, pitchDeg: 0, yawDeg: 90 } };

type Step =
  | { op: "read" }
  | { op: "sample" }
  | { op: "waitSpawn"; n: number }
  | { op: "line"; text: string }
  | { op: "exit"; code: string }
  | { op: "settings"; json: unknown }
  | { op: "advance"; ms: number }
  | { op: "settle" };

interface I2c { stdout: string; stderr?: string; code?: number }
interface Scenario {
  name: string;
  i2c: I2c | null; // null = no i2cdetect on PATH
  python: boolean; // false = no python3 on PATH
  steps: Step[];
}

const SCENARIOS: Scenario[] = [
  {
    name: "detect 0x4a → stream → group; unparsed line keeps latest + updatedMono (CF18); exit after samples → re-detect; exit before any → parked",
    i2c: { stdout: grid([0x4a, 0x6b]) },
    python: true,
    steps: [
      { op: "read" }, { op: "waitSpawn", n: 1 }, { op: "read" }, { op: "sample" },
      { op: "line", text: '{"type": "ready", "address": "0x4a"}' }, { op: "read" },
      { op: "advance", ms: 100 }, { op: "line", text: S1 }, { op: "read" }, { op: "sample" },
      { op: "advance", ms: 100 }, { op: "line", text: '{"type": "sample", "quat": {"r": 0.9' },
      { op: "line", text: "not json at all" }, { op: "line", text: '{"type":"other"}' }, { op: "read" }, { op: "sample" },
      { op: "line", text: '{"type": "error", "error": "I2C NACK at 0x4a"}' },
      { op: "line", text: "E:Traceback (most recent call last):" }, { op: "line", text: "E:   \t" },
      { op: "advance", ms: 100 }, { op: "line", text: S2 }, { op: "read" }, { op: "sample" },
      { op: "settings", json: MOUNT }, { op: "read" }, { op: "settings", json: null }, { op: "read" },
      { op: "exit", code: "0" }, { op: "read" }, { op: "sample" }, { op: "read" },
      { op: "waitSpawn", n: 2 }, { op: "read" },
      { op: "advance", ms: 100 }, { op: "line", text: S3 }, { op: "sample" }, { op: "read" },
      { op: "exit", code: "K" }, { op: "read" }, { op: "waitSpawn", n: 3 },
      { op: "line", text: "E:ModuleNotFoundError: No module named 'adafruit_bno08x'" },
      { op: "exit", code: "2" }, { op: "read" }, { op: "sample" }, { op: "settle" }, { op: "read" },
    ],
  },
  {
    name: "sample() alone kicks detection; exit before any sample by signal → (exit ?) parked",
    i2c: { stdout: grid([0x4b]) },
    python: true,
    steps: [{ op: "sample" }, { op: "waitSpawn", n: 1 }, { op: "sample" }, { op: "exit", code: "K" }, { op: "read" }, { op: "sample" }],
  },
  {
    name: "no BNO085 on the bus → one warning, inert, never re-detected",
    i2c: { stdout: grid([0x6b]) },
    python: true,
    steps: [{ op: "read" }, { op: "settle" }, { op: "read" }, { op: "settle" }, { op: "sample" }, { op: "settle" }],
  },
  {
    name: "i2cdetect missing → one warning, inert",
    i2c: null,
    python: true,
    steps: [{ op: "read" }, { op: "settle" }, { op: "read" }, { op: "settle" }],
  },
  {
    name: "i2cdetect fails with stderr → the trimmed stderr is the reason",
    i2c: { stdout: "", stderr: "Error: Could not open file `/dev/i2c-1' or `/dev/i2c/1': Permission denied\n", code: 1 },
    python: true,
    steps: [{ op: "read" }, { op: "settle" }, { op: "read" }],
  },
  {
    name: "i2cdetect fails silently → generic reason",
    i2c: { stdout: "", stderr: "  \n", code: 2 },
    python: true,
    steps: [{ op: "read" }, { op: "settle" }, { op: "read" }],
  },
  {
    name: "python3 missing → reader exited before any sample (exit ?), parked",
    i2c: { stdout: grid([0x4a]) },
    python: false,
    steps: [{ op: "read" }, { op: "settle" }, { op: "read" }, { op: "settle" }, { op: "read" }],
  },
];

async function runOne(idx: number): Promise<void> {
  const sc = SCENARIOS[idx];
  const dir = mkdtempSync(join(tmpdir(), "xg-imu-dump-"));
  const bin = join(dir, "bin");
  const py = join(dir, "python");
  const home = join(dir, "home");
  for (const d of [bin, py, home]) mkdirSync(d);
  for (const [name, target] of [["which", "/usr/bin/which"], ["sh", "/bin/sh"], ["cat", "/bin/cat"]]) {
    symlinkSync(target, join(bin, name));
  }
  const fifo = join(dir, "imu.fifo");
  spawnSync("/usr/bin/mkfifo", [fifo]);
  if (sc.i2c) {
    writeFileSync(join(dir, "i2c.out"), sc.i2c.stdout);
    writeFileSync(join(dir, "i2c.err"), sc.i2c.stderr ?? "");
    writeFileSync(join(dir, "i2c.code"), String(sc.i2c.code ?? 0));
    writeFileSync(
      join(bin, "i2cdetect"),
      `#!/bin/sh\n/bin/cat "${dir}/i2c.out"\n/bin/cat "${dir}/i2c.err" >&2\nexit $(/bin/cat "${dir}/i2c.code")\n`,
    );
    chmodSync(join(bin, "i2cdetect"), 0o755);
  }
  if (sc.python) {
    writeFileSync(
      join(bin, "python3"),
      [
        "#!/bin/sh",
        `D="${dir}"`,
        'printf "%s\\n" "$*" >> "$D/argv.log"',
        'n=$(/bin/cat "$D/spawn.count" 2>/dev/null || echo 0); echo $((n+1)) > "$D/spawn.count"',
        'while IFS= read -r l; do case "$l" in E:*) printf "%s\\n" "${l#E:}" >&2;; *) printf "%s\\n" "$l";; esac; done < "$D/imu.fifo"',
        'c=$(/bin/cat "$D/exit.code" 2>/dev/null || echo 0)',
        '[ "$c" = K ] && kill -KILL $$',
        'exit "$c"',
        "",
      ].join("\n"),
    );
    chmodSync(join(bin, "python3"), 0o755);
  }
  process.env.PATH = bin;
  process.env.XORGATE_MOCK = "0";
  process.env.XORGATE_PYTHON_DIR = py;
  process.env.XORGATE_HOME = home;
  let mono = 0;
  Date.now = () => BASE + mono;
  process.hrtime.bigint = () => BigInt(mono) * 1_000_000n;
  const logs: { level: string; msg: string }[] = [];
  for (const level of ["log", "warn", "error"] as const) {
    console[level] = (...a: unknown[]) => void logs.push({ level, msg: a.map(String).join(" ") });
  }
  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
  const { createImuCollector } = await import("./src/agent/collectors/imu.ts");
  const c = createImuCollector();
  let fh: Awaited<ReturnType<typeof open>> | null = null;
  const spawns = () => (existsSync(join(dir, "spawn.count")) ? Number(readFileSync(join(dir, "spawn.count"), "utf8")) : 0);
  const out: unknown[] = [];
  for (const step of sc.steps) {
    const before = logs.length;
    let result: unknown = undefined;
    switch (step.op) {
      case "read":
        result = await c.read();
        await sleep(30);
        break;
      case "sample": {
        const s = await c.sample!();
        result = s ? { fields: s.fields, updatedMonoMs: Number(s.updatedMono / 1_000_000n) } : null;
        await sleep(30);
        break;
      }
      case "waitSpawn":
        for (let i = 0; i < 500 && spawns() < step.n; i++) await sleep(10);
        if (spawns() !== step.n) throw new Error(`spawn ${step.n} never happened`);
        if (existsSync(join(dir, "exit.code"))) rmSync(join(dir, "exit.code"));
        fh = await open(fifo, "w");
        await sleep(50);
        break;
      case "line":
        await fh!.write(step.text + "\n");
        await sleep(120);
        break;
      case "exit":
        writeFileSync(join(dir, "exit.code"), step.code);
        await fh!.close();
        fh = null;
        await sleep(400);
        break;
      case "settings":
        if (step.json === null) rmSync(join(home, "settings.json"), { force: true });
        else writeFileSync(join(home, "settings.json"), JSON.stringify(step.json));
        break;
      case "advance":
        mono += step.ms;
        break;
      case "settle":
        await sleep(400);
        break;
    }
    out.push({ ...step, ...(result !== undefined ? { result } : {}), logs: logs.slice(before) });
  }
  await sleep(200);
  const argv = existsSync(join(dir, "argv.log")) ? readFileSync(join(dir, "argv.log"), "utf8").split("\n").filter(Boolean) : [];
  const trailing = logs.length;
  process.stdout.write(
    JSON.stringify({
      name: sc.name,
      input: {
        i2c: sc.i2c,
        python: sc.python,
        baseWallMs: BASE,
        // argv after the script path (the dir is per-run): "--address 0x4a"
        helperArgs: argv.map((a) => a.slice(a.indexOf(" ") + 1)),
        helperScript: argv.length ? argv[0].slice(0, argv[0].indexOf(" ")).replace(py, "<python>") : null,
        spawnCount: spawns(),
      },
      steps: out,
      logsAfterLastStep: logs.slice(trailing),
    }),
  );
  rmSync(dir, { recursive: true, force: true });
  process.exit(0);
}

if (process.argv[2] !== undefined) {
  await runOne(Number(process.argv[2]));
} else {
  const cases = SCENARIOS.map((_, i) => {
    const r = spawnSync(process.execPath, [...process.execArgv, process.argv[1], String(i)], { encoding: "utf8" });
    if (r.status !== 0) throw new Error(`scenario ${i}: ${r.stderr}`);
    return JSON.parse(r.stdout);
  });
  process.stdout.write(
    JSON.stringify(
      {
        jsSourceFile: "src/agent/collectors/imu.ts (createImuCollector) + src/hardware/imu.ts (detectImu, streamImuData) + src/lib/exec.ts (run, runStream)",
        jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
        generatedBy:
          "npx tsx dump-imu-scenarios.mts in a git-archive scratch copy of 89b5c64 (phase-2 WP2; PATH = i2cdetect/python3 shims, XORGATE_MOCK=0, Date.now/hrtime mocked to wall = baseWallMs + mono)",
        cases,
      },
      null,
      2,
    ) + "\n",
  );
}
