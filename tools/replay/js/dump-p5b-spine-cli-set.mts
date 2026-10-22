// Phase 5b WP1 tsx driver — `xorgate set obd.*` (setObdCommand, set.ts:380-460
// @ 09da38d) + the changed usage / unknown-key strings, with the SAME runner as
// dump-cli-set.mts (Phase 2 WP6): what the CLI prints (console.log → out,
// console.error → err), the exit code and the settings.json bytes left behind.
// Run in the shared js-09da38d copy (non-root):
//   npx --no-install tsx .rig-drivers/dump-p5b-spine-cli-set.mts > fixtures/cli_set/set-obd.json
// Re-dump mode (H2): `… dump-p5b-spine-cli-set.mts --redump /out/cli_set/set-command.json`
// re-runs EVERY case of an existing fixture from its recorded input (args,
// settings, lte script, pidfile) at 09da38d and prints the fixture with the
// new expectations (moved cases listed on stderr).
// Time is faked: setTimeout fires at once and advances Date.now by its delay, and
// every 3 s APN poll swaps the modem telemetry file to the next scripted text
// (null = absent), so the 8-minute APN watch runs in milliseconds.
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync, unlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const scratch = mkdtempSync(join(tmpdir(), "xg-wp6-set-"));
const lteFile = join(scratch, "telemetry.json");
process.env.LTE_TELEMETRY_FILE = lteFile;
delete process.env.NO_COLOR;

let fakeNow = 1_800_000_000_000;
let lteScript: (string | null)[] = [];
let lteIdx = 0;
const writeLte = () => {
  const t = lteScript[Math.min(lteIdx, lteScript.length - 1)] ?? null;
  if (t === null) { if (existsSync(lteFile)) unlinkSync(lteFile); } else writeFileSync(lteFile, t);
};
Date.now = () => fakeNow;
(globalThis as any).setTimeout = (fn: () => void, ms: number) => {
  fakeNow += ms;
  lteIdx++;
  if (lteScript.length) writeLte();
  queueMicrotask(fn);
  return 0;
};

const { setCommand } = await import(join(process.cwd(), "src/commands/set.ts"));

type Case = { name: string; args: string[]; settings?: string | null; lte?: (string | null)[]; pidfile?: string };
const lte = (o: unknown) => JSON.stringify(o);
const cell = (c: unknown) => lte({ csq: 20, cellular: c });

const MAC = "10:21:3E:4F:33:65";
const cases: Case[] = [
  // set-command.test.ts "writes each field through the namespace parser, merging into the held namespace"
  { name: "obd.adapter lowercase → uppercased, enabled-off note", args: ["obd.adapter", "10:21:3e:4f:33:65"] },
  { name: "obd.enabled on merges after adapterMac", args: ["obd.enabled", "on"], settings: `{"obd":{"adapterMac":"${MAC}"}}` },
  { name: "obd.pin 4321", args: ["obd.pin", "4321"], settings: `{"obd":{"adapterMac":"${MAC}","enabled":true}}` },
  { name: "obd.poll-hz 0.5", args: ["obd.poll-hz", "0.5"] },
  { name: "obd.power-source constant", args: ["obd.power-source", "constant"] },
  { name: "obd.fields csv trimmed", args: ["obd.fields", "rpm,speed, fuel_level"] },
  { name: "obd.fields all → []", args: ["obd.fields", "all"], settings: '{"obd":{"fields":["rpm"]}}' },
  { name: "obd.adapter none → null (no note)", args: ["obd.adapter", "none"], settings: `{"obd":{"adapterMac":"${MAC}","enabled":true}}` },
  { name: "obd.adapter null → null", args: ["obd.adapter", "null"] },
  { name: "obd.adapter empty → null", args: ["obd.adapter", ""] },
  { name: "obd.adapter missing → null", args: ["obd.adapter"] },
  { name: "obd.adapter with enabled on → no note", args: ["obd.adapter", MAC], settings: '{"obd":{"enabled":true}}' },
  { name: "obd.adapterMac alias", args: ["obd.adapterMac", MAC] },
  { name: "obd.pollHz alias", args: ["obd.pollHz", "2"] },
  { name: "obd.powerSource alias", args: ["obd.powerSource", "switched"] },
  { name: "obd.enabled off", args: ["obd.enabled", "off"] },
  { name: "obd.enabled 1", args: ["obd.enabled", "1"] },
  { name: "obd.poll-hz 0.2 bound", args: ["obd.poll-hz", "0.2"] },
  { name: "obd.poll-hz 2 bound", args: ["obd.poll-hz", "2"] },
  { name: "obd.poll-hz 0x1 (hex Number)", args: ["obd.poll-hz", "0x1"] },
  { name: "obd.poll-hz 1e0", args: ["obd.poll-hz", "1e0"] },
  { name: "obd.fields with invalid names only → [] kept", args: ["obd.fields", "Bad,1x"] },
  { name: "obd.fields dedupe", args: ["obd.fields", "rpm,rpm,speed"] },
  { name: "obd namespace appended after other keys", args: ["obd.pin", "1111"], settings: '{"mode":"real","simulation":{"loop":true}}' },
  { name: "obd keeps its slot and spread order", args: ["obd.enabled", "on"], settings: '{"obd":{"pin":"1234"},"mode":"real"}' },
  { name: "obd value over an invalid held namespace", args: ["obd.pin", "1234"], settings: '{"obd":{"pin":"x"},"mode":"real"}' },
  // "rejects what the collector would drop, and never clobbers other settings"
  { name: "obd.adapter short MAC", args: ["obd.adapter", "10:21:3E:4F:33"], settings: '{"mode":"real","obd":{"enabled":true}}' },
  { name: "obd.pin 12", args: ["obd.pin", "12"], settings: '{"mode":"real","obd":{"enabled":true}}' },
  { name: "obd.poll-hz 5", args: ["obd.poll-hz", "5"], settings: '{"mode":"real","obd":{"enabled":true}}' },
  { name: "obd.power-source solar", args: ["obd.power-source", "solar"], settings: '{"mode":"real","obd":{"enabled":true}}' },
  { name: "obd.enabled maybe", args: ["obd.enabled", "maybe"], settings: '{"mode":"real","obd":{"enabled":true}}' },
  { name: "obd.bogus", args: ["obd.bogus", "1"], settings: '{"mode":"real","obd":{"enabled":true}}' },
  { name: "obd.pin missing", args: ["obd.pin"] },
  { name: "obd.poll-hz missing", args: ["obd.poll-hz"] },
  { name: "obd.poll-hz abc", args: ["obd.poll-hz", "abc"] },
  { name: "obd.enabled missing", args: ["obd.enabled"] },
  { name: "obd.power-source missing", args: ["obd.power-source"] },
  { name: "obd. (empty field)", args: ["obd.", "1"] },
  { name: "obd.a.b (3 parts)", args: ["obd.enabled.x", "on"] },
  { name: "obd.toString (prototype key) → invalid, not unknown", args: ["obd.toString", "x"] },
  { name: "obd.__proto__ (prototype key) → invalid", args: ["obd.__proto__", "x"] },
  { name: "obd.constructor missing value", args: ["obd.constructor"] },
  // the changed strings (X6)
  { name: "usage (no args) names obd", args: [] },
  { name: "unknown key lists obd.*", args: ["nonsense", "1"] },
];

const REDUMP = process.argv[2] === "--redump" ? JSON.parse(readFileSync(process.argv[3]!, "utf8")) : null;
const run: Case[] = REDUMP
  ? (REDUMP.cases as Array<{ name: string; input: { args: string[]; settings: string | null; lte: (string | null)[]; pidfile: string | null } }>).map(
      (c) => ({ name: c.name, args: c.input.args, settings: c.input.settings, lte: c.input.lte, ...(c.input.pidfile ? { pidfile: c.input.pidfile } : {}) }),
    )
  : cases;
const out: unknown[] = [];
for (const c of run) {
  const home = mkdtempSync(join(scratch, "home-"));
  process.env.XORGATE_HOME = home;
  if (c.settings != null) writeFileSync(join(home, "settings.json"), c.settings);
  if (c.pidfile) writeFileSync(join(home, "agent.pid"), c.pidfile);
  lteScript = c.lte ?? [null];
  lteIdx = 0;
  writeLte();
  const start = fakeNow;
  const lines: { stream: string; text: string }[] = [];
  const ol = console.log, oe = console.error;
  console.log = (...a: unknown[]) => lines.push({ stream: "out", text: a.join(" ") });
  console.error = (...a: unknown[]) => lines.push({ stream: "err", text: a.join(" ") });
  process.exitCode = undefined;
  try {
    await setCommand(c.args);
  } catch (err) {
    lines.push({ stream: "err", text: err instanceof Error ? err.message : String(err) });
    process.exitCode = 1;
  } finally {
    console.log = ol; console.error = oe;
  }
  const sp = join(home, "settings.json");
  out.push({
    name: c.name,
    input: { args: c.args, settings: c.settings ?? null, lte: c.lte ?? [null], pidfile: c.pidfile ?? null },
    expected: {
      lines,
      exitCode: process.exitCode ?? 0,
      settingsAfter: existsSync(sp) ? readFileSync(sp, "utf8") : null,
      elapsedMs: fakeNow - start,
    },
  });
  process.exitCode = undefined;
  rmSync(home, { recursive: true, force: true });
}
rmSync(scratch, { recursive: true, force: true });
if (REDUMP) {
  const before = new Map((REDUMP.cases as Array<{ name: string; expected: unknown }>).map((c) => [c.name, JSON.stringify(c.expected)]));
  const moved = (out as Array<{ name: string; expected: unknown }>).filter((c) => before.get(c.name) !== JSON.stringify(c.expected)).map((c) => c.name);
  process.stdout.write(JSON.stringify({
    ...REDUMP,
    jsCommit: "09da38dcc8b75c248625a3a0bee43b70e7def55d",
    generatedBy: `${REDUMP.generatedBy}; re-dumped @ 09da38d from the recorded inputs by tools/replay/js/dump-p5b-spine-cli-set.mts --redump (Phase 5b WP1)`,
    cases: out,
  }, null, 2) + "\n");
  process.stderr.write(`redump: ${out.length} cases, moved ${moved.length}: ${moved.join(" | ")}\n`);
} else {
process.stdout.write(JSON.stringify({
    jsSourceFile: "src/commands/set.ts (setObdCommand :400-460, USAGE :107-108, setCommand unknown key :601-603) over src/daemon/settings.ts parseObdSettings/loadSettings/saveSettings",
    jsCommit: "09da38dcc8b75c248625a3a0bee43b70e7def55d",
    generatedBy: "tools/replay/js/dump-p5b-spine-cli-set.mts (Phase 5b WP1; runner = dump-cli-set.mts) in the shared js-09da38d copy, non-root",
    cases: out,
  }, null, 2) + "\n");
}
