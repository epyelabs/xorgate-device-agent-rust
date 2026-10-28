// Phase 2 WP2 — dump the gps collector scenarios (fixtures/collectors/gps-scenarios.json).
// Run from a `git archive 89b5c64` scratch copy (npm ci done):
//   npx tsx dump-gps-scenarios.mts > gps-scenarios.json
// Each scenario runs in its OWN node process (GPS_NMEA_PORT is a module const)
// with Date.now / process.hrtime.bigint mocked to one clock (wall = BASE + mono),
// a FIFO as the NMEA port (a chunk per write, like a 1 Hz tty) and console.warn
// captured. The Rust test replays the same steps on tokio paused time.
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, existsSync, rmSync } from "node:fs";
import { open } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BASE = 1_781_000_000_000;

const NMEA =
  [
    "$GPGGA,083745.000,1430.1234,N,12101.5678,E,1,08,0.9,545.4,M,46.9,M,,*56",
    "$GPRMC,083745.000,A,1430.1234,N,12101.5678,E,0.06,31.66,130626,,,A*5A",
    "$GPGSA,A,3,04,05,,09,12,,,24,,,,,2.5,1.3,2.1*39",
    "$GPGSV,3,1,11,03,03,111,00,04,15,270,00*7F",
    "$GLGSV,2,1,08,65,50,100,40,66,30,200,35*6A",
  ].join("\n") + "\n";
const ACQ = "$GPGGA,083740.000,,,,,0,00,,,M,,M,,\n";
const LOST = "$GPGGA,083750.000,,,,,0,00,,,M,,M,,*71\n";
const SKY4 =
  [
    "$GPGGA,083745.000,1430.1234,N,12101.5678,E,1,08,0.9,545.4,M,46.9,M,,*56",
    "$GPGSV,2,1,08,05,31,077,29,15,79,067,20,20,46,052,30,21,,,21,1",
    "$GPGSV,2,2,08,10,11,274,,18,75,292,,23,41,286,,27,09,322,,1",
  ].join("\n") + "\n";
const GSV_ONLY =
  [
    "$GPGSV,2,1,08,05,31,077,29,15,79,067,20,20,46,052,30,21,,,21,1",
    "$GPGSV,2,2,08,10,11,274,,18,75,292,,23,41,286,,27,09,322,,1",
  ].join("\n") + "\n";

type Step =
  | { op: "write"; data: string }
  | { op: "advance"; ms: number }
  | { op: "read" }
  | { op: "sample" }
  | { op: "clock" }
  | { op: "sky" }
  | { op: "hasFix" }
  | { op: "skyFile" }
  | { op: "openWriter" }
  | { op: "settle" };

interface Scenario {
  name: string;
  port: "fifo" | "fifo-silent" | "absent";
  reopenSilentMs?: number;
  steps: Step[];
}

const SCENARIOS: Scenario[] = [
  {
    name: "mapped fix incl. satellites + fix_quality (gps.test.ts:32)",
    port: "fifo",
    steps: [
      { op: "read" }, { op: "openWriter" }, { op: "write", data: NMEA },
      { op: "read" }, { op: "skyFile" }, { op: "hasFix" }, { op: "sample" },
    ],
  },
  {
    name: "ttff_s across the 0→nonzero transition (gps.test.ts:58)",
    port: "fifo",
    steps: [
      { op: "read" }, { op: "openWriter" }, { op: "advance", ms: 700 },
      { op: "write", data: ACQ }, { op: "read" }, { op: "advance", ms: 2000 },
      { op: "read" }, { op: "advance", ms: 3450 }, { op: "write", data: NMEA },
      { op: "read" }, { op: "advance", ms: 1000 }, { op: "write", data: NMEA }, { op: "read" },
      // a new acquisition invalidates the measurement
      { op: "advance", ms: 1000 }, { op: "write", data: LOST }, { op: "read" },
      { op: "advance", ms: 4000 }, { op: "write", data: NMEA }, { op: "read" },
    ],
  },
  {
    name: "clock() — fresh RMC UTC time, aged on mono (gps.test.ts:103)",
    port: "fifo",
    steps: [
      { op: "clock" }, { op: "openWriter" }, { op: "write", data: NMEA }, { op: "clock" },
      { op: "advance", ms: 10000 }, { op: "clock" }, { op: "advance", ms: 1 }, { op: "clock" },
      // the same utcMs again does not refresh the stamp
      { op: "write", data: NMEA }, { op: "clock" },
    ],
  },
  {
    name: "stale position dropped (not the group) after lock loss (gps.test.ts:128)",
    port: "fifo",
    steps: [{ op: "read" }, { op: "openWriter" }, { op: "write", data: NMEA + LOST }, { op: "read" }, { op: "hasFix" }],
  },
  {
    name: "acquiring state: fix_quality, no position (gps.test.ts:157)",
    port: "fifo",
    steps: [{ op: "read" }, { op: "openWriter" }, { op: "write", data: "$GPGGA,083745.000,,,,,0,00,,,M,,M,,\n" }, { op: "read" }],
  },
  {
    name: "silent stream: destroy + reopen, watchdog re-arms (gps.test.ts:177)",
    port: "fifo-silent",
    reopenSilentMs: 40,
    steps: [
      { op: "read" }, { op: "advance", ms: 60 }, { op: "read" }, { op: "read" },
      { op: "advance", ms: 60 }, { op: "read" },
    ],
  },
  {
    name: "sky visibility in telemetry and in gnss-sky.json (gps.test.ts:208)",
    port: "fifo",
    steps: [
      { op: "read" }, { op: "openWriter" }, { op: "write", data: SKY4 }, { op: "read" },
      { op: "skyFile" }, { op: "sky" }, { op: "hasFix" },
      // cadence: nothing changes for 14.999 s → no rewrite; 15 s → rewrite
      { op: "advance", ms: 4000 }, { op: "write", data: GSV_ONLY }, { op: "advance", ms: 1000 },
      { op: "read" }, { op: "skyFile" },
      { op: "advance", ms: 9999 }, { op: "sky" }, { op: "skyFile" },
      { op: "advance", ms: 1 }, { op: "sky" }, { op: "skyFile" },
      // data goes stale → unknown → written at once
      { op: "advance", ms: 5001 }, { op: "sky" }, { op: "skyFile" }, { op: "read" },
    ],
  },
  {
    name: "GSV keeps a position-less fix fresh, never a held position",
    port: "fifo",
    steps: [
      { op: "read" }, { op: "openWriter" }, { op: "write", data: ACQ }, { op: "read" },
      { op: "advance", ms: 6000 }, { op: "write", data: GSV_ONLY }, { op: "advance", ms: 6000 },
      { op: "read" }, { op: "sample" },
      { op: "write", data: NMEA }, { op: "advance", ms: 6000 }, { op: "write", data: GSV_ONLY },
      { op: "advance", ms: 5000 }, { op: "read" }, { op: "sample" }, { op: "hasFix" },
    ],
  },
  {
    name: "absent NMEA port: {} and one warning (gps.test.ts:245)",
    port: "absent",
    steps: [
      { op: "read" }, { op: "settle" }, { op: "advance", ms: 1000 }, { op: "read" }, { op: "settle" },
      { op: "advance", ms: 1000 }, { op: "read" }, { op: "settle" },
    ],
  },
];

const ABSENT = "/nonexistent/xorgate-no-such-nmea-port";

async function runOne(idx: number): Promise<void> {
  const sc = SCENARIOS[idx];
  const dir = mkdtempSync(join(tmpdir(), "xg-gps-dump-"));
  let port = ABSENT;
  if (sc.port !== "absent") {
    port = join(dir, "nmea.fifo");
    execFileSync("mkfifo", [port]);
  }
  process.env.GPS_NMEA_PORT = port;
  let mono = 0;
  Date.now = () => BASE + mono;
  process.hrtime.bigint = () => BigInt(mono) * 1_000_000n;
  const warns: string[] = [];
  console.warn = (...a: unknown[]) => void warns.push(a.map(String).join(" "));
  const skyPath = join(dir, "gnss-sky.json");
  const { createGpsCollector } = await import("./src/agent/collectors/gps.ts");
  const c = createGpsCollector({ skyStatePath: skyPath, reopenSilentMs: sc.reopenSilentMs });
  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
  let fh: Awaited<ReturnType<typeof open>> | null = null;
  const out: unknown[] = [];
  for (const step of sc.steps) {
    const before = warns.length;
    let result: unknown = null;
    switch (step.op) {
      case "openWriter":
        fh = await open(port, "w");
        await sleep(40);
        break;
      case "write":
        await fh!.write(step.data);
        await sleep(60);
        break;
      case "advance":
        mono += step.ms;
        break;
      case "settle":
        await sleep(40);
        break;
      case "read":
        result = await c.read();
        break;
      case "sample": {
        const s = await c.sample!();
        result = s ? { fields: s.fields, updatedMonoMs: Number(s.updatedMono / 1_000_000n) } : null;
        break;
      }
      case "clock":
        result = c.clock();
        break;
      case "sky":
        result = c.sky();
        break;
      case "hasFix":
        result = c.hasFix();
        break;
      case "skyFile":
        result = existsSync(skyPath) ? readFileSync(skyPath, "utf8") : null;
        break;
    }
    out.push({ ...step, ...(result !== null || ["read","sample","clock","sky","hasFix","skyFile"].includes(step.op) ? { result } : {}), warns: warns.slice(before) });
  }
  // Close the writer first: a reader thread blocked in read(2) on the FIFO
  // would otherwise keep process.exit() from returning.
  await fh?.close();
  // Silent FIFO: release the reader opens still blocked in the threadpool.
  if (sc.port === "fifo-silent") await (await open(port, "w")).close();
  await sleep(20);
  process.stdout.write(JSON.stringify({ name: sc.name, input: { port: sc.port === "absent" ? ABSENT : "fifo", reopenSilentMs: sc.reopenSilentMs ?? null, baseWallMs: BASE }, steps: out }));
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
        jsSourceFile: "src/agent/collectors/gps.ts (createGpsCollector) + src/hardware/gps.ts (streamGps)",
        jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
        generatedBy:
          "npx tsx dump-gps-scenarios.mts in a git-archive scratch copy of 89b5c64 (phase-2 WP2; FIFO port, Date.now/hrtime mocked to wall = baseWallMs + mono)",
        cases,
      },
      null,
      2,
    ) + "\n",
  );
}
