// WP6 tsx driver: runs the REAL `setCommand` (src/commands/set.ts:391-717) over
// invocation × seed-settings × modem-telemetry scripts and records what the CLI
// prints (console.log → out, console.error → err, in call order), the exit code
// and the exact settings.json bytes left behind. Run from a scratch copy of the
// JS repo at 89b5c64 (non-root):
//   cd <scratch>/js-89b5c64 && npx tsx <this file> > fixtures/cli_set/set-command.json
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

const cases: Case[] = [
  // routing / usage
  { name: "no args → usage", args: [] },
  { name: "flags only → usage", args: ["--no-restart", "--force"] },
  { name: "empty key → usage (set.ts:398 `!key`)", args: ["", "on"] },
  { name: "unknown key", args: ["nonsense", "1"] },
  { name: "retired media", args: ["media", "off"] },
  // mode
  { name: "mode sim --no-restart (fresh)", args: ["mode", "sim", "--no-restart"] },
  { name: "mode real, no pidfile → not running", args: ["mode", "real"] },
  { name: "mode real, stale pidfile → not running", args: ["mode", "real"], pidfile: JSON.stringify({ pid: 2147483000, startedAt: 1, deviceId: "d" }) },
  { name: "mode bogus", args: ["mode", "bogus"] },
  { name: "mode missing value", args: ["mode"] },
  { name: "mode appended after existing namespaces", args: ["mode", "real", "--no-restart"], settings: '{"recording":{"telemetry":{"enabled":true}},"cellular":{"apn":"x"}}' },
  { name: "mode keeps its slot when present", args: ["--force", "mode", "sim", "--no-restart"], settings: '{"mode":"real","gnssAntBias":true}' },
  // gps-antenna
  { name: "gps-antenna active", args: ["gps-antenna", "active"] },
  { name: "gps-antenna passive over unknown keys", args: ["gps-antenna", "passive"], settings: '{"foo":1,"mode":"real","gnssAntBias":"yes","timeSync":{"stepThresholdSecs":30}}' },
  { name: "gps-antenna sideways", args: ["gps-antenna", "sideways"] },
  { name: "gps-antenna missing", args: ["gps-antenna"] },
  // lteRecovery knobs
  { name: "gps-fix-reset-policy veto-deadzone", args: ["gps-fix-reset-policy", "veto-deadzone"] },
  { name: "gps-fix-reset-policy defer merges after deadZoneProfile", args: ["gps-fix-reset-policy", "defer"], settings: '{"lteRecovery":{"deadZoneProfile":"balanced"}}' },
  { name: "gps-fix-reset-policy bad", args: ["gps-fix-reset-policy", "never"] },
  { name: "deadzone-profile aggressive", args: ["deadzone-profile", "aggressive"] },
  { name: "deadzone-profile conservative replaces", args: ["deadzone-profile", "conservative"], settings: '{"lteRecovery":{"gpsFixResetPolicy":"defer","deadZoneProfile":"aggressive"}}' },
  { name: "deadzone-profile wild", args: ["deadzone-profile", "wild"] },
  // recording.*
  { name: "recording.telemetry.enabled on", args: ["recording.telemetry.enabled", "on"] },
  { name: "recording.telemetry.enabled 0", args: ["recording.telemetry.enabled", "0"] },
  { name: "recording.telemetry.enabled yes", args: ["recording.telemetry.enabled", "yes"] },
  { name: "recording.telemetry.rateHz 10", args: ["recording.telemetry.rateHz", "10"] },
  { name: "recording.telemetry.rateHz 10.6 rounds", args: ["recording.telemetry.rateHz", "10.6"] },
  { name: "recording.telemetry.rateHz 999", args: ["recording.telemetry.rateHz", "999"] },
  { name: "recording.telemetry.rateHz 0.5", args: ["recording.telemetry.rateHz", "0.5"] },
  { name: "recording.telemetry.rateHz abc", args: ["recording.telemetry.rateHz", "abc"] },
  { name: "recording.telemetry.rateHz empty", args: ["recording.telemetry.rateHz", ""] },
  { name: "recording.telemetry.rateHz -3 (flag-filtered → missing)", args: ["recording.telemetry.rateHz", "-3"] },
  { name: "recording.telemetry.rateHz 0x10 (hex Number)", args: ["recording.telemetry.rateHz", "0x10"] },
  { name: "recording.video.enabled off", args: ["recording.video.enabled", "off"] },
  { name: "recording.video.segmentSeconds 5 clamps", args: ["recording.video.segmentSeconds", "5"] },
  { name: "recording.video.segmentSeconds 9999 clamps", args: ["recording.video.segmentSeconds", "9999"] },
  { name: "recording.video.preset valid", args: ["recording.video.preset", "720p15-1mbps"] },
  { name: "recording.video.preset bogus", args: ["recording.video.preset", "8k120"] },
  { name: "recording.video.preset missing", args: ["recording.video.preset"] },
  { name: "recording.video.bootDefer always", args: ["recording.video.bootDefer", "always"] },
  { name: "recording.video.gpsGate maybe", args: ["recording.video.gpsGate", "maybe"] },
  { name: "recording.video.gpsGateMaxSecs 99999", args: ["recording.video.gpsGateMaxSecs", "99999"] },
  { name: "recording.video.spawnStaggerSecs 2.5", args: ["recording.video.spawnStaggerSecs", "2.5"] },
  { name: "recording.video.cameras.cam0 off", args: ["recording.video.cameras.cam0", "off"] },
  { name: "recording.video.cameras.cam1 on merges", args: ["recording.video.cameras.cam1", "on"], settings: '{"recording":{"video":{"enabled":true,"cameras":{"cam0":false}}}}' },
  { name: "recording.video.cameras.7 index key orders first", args: ["recording.video.cameras.7", "off"], settings: '{"recording":{"video":{"cameras":{"cam0":false,"cam1":true}}}}' },
  { name: "recording.video.cameras bad key", args: ["recording.video.cameras.cam 0", "off"] },
  { name: "recording.video.cameras bad value", args: ["recording.video.cameras.cam0", "maybe"] },
  { name: "recording.upload.mode auto", args: ["recording.upload.mode", "auto"] },
  { name: "recording.upload.mode sideways", args: ["recording.upload.mode", "sideways"] },
  { name: "recording.retention.maxBytes 1000000", args: ["recording.retention.maxBytes", "1000000"] },
  { name: "recording.retention.maxBytes 1e21", args: ["recording.retention.maxBytes", "1e21"] },
  { name: "recording.retention.maxBytes 0.000005", args: ["recording.retention.maxBytes", "0.000005"] },
  { name: "recording.retention.minFreeBytes 0", args: ["recording.retention.minFreeBytes", "0"] },
  { name: "recording.powerShedding.railLowVolts 4.123456789", args: ["recording.powerShedding.railLowVolts", "4.123456789"] },
  { name: "recording.powerShedding.railLowVolts 3 clamps", args: ["recording.powerShedding.railLowVolts", "3"] },
  { name: "recording.powerShedding.cpuTempC 999 clamps", args: ["recording.powerShedding.cpuTempC", "999"] },
  { name: "recording.powerShedding.enabled on into existing section", args: ["recording.powerShedding.enabled", "on"], settings: '{"recording":{"powerShedding":{"uvSamples":3},"telemetry":{"rateHz":5}},"mode":"real"}' },
  { name: "recording.powerShedding.cpuTempSecs merges (TS test)", args: ["recording.powerShedding.cpuTempSecs", "15"], settings: '{"recording":{"powerShedding":{"enabled":true}}}' },
  { name: "recording.nope.field", args: ["recording.nope.field", "1"] },
  { name: "recording.telemetry (2 parts)", args: ["recording.telemetry", "on"] },
  { name: "recording.a.b.c.d (5 parts)", args: ["recording.a.b.c.d", "on"] },
  { name: "recording.telemetry.preset (field of another section)", args: ["recording.telemetry.preset", "720p15-1mbps"] },
  { name: "recording.telemetry.constructor", args: ["recording.telemetry.constructor", "x"] },
  // timeSync.*
  { name: "timeSync.stepThresholdSecs 15", args: ["timeSync.stepThresholdSecs", "15"] },
  { name: "timeSync.stepThresholdSecs 1 clamps", args: ["timeSync.stepThresholdSecs", "1"] },
  { name: "timeSync.stepThresholdSecs 1e9 clamps", args: ["timeSync.stepThresholdSecs", "1e9"] },
  { name: "timeSync.stepThresholdSecs abc", args: ["timeSync.stepThresholdSecs", "abc"] },
  { name: "timeSync.maxStepsPerBoot 2.7", args: ["timeSync.maxStepsPerBoot", "2.7"] },
  { name: "timeSync.offlineSources false merges", args: ["timeSync.offlineSources", "false"], settings: '{"timeSync":{"stepThresholdSecs":30}}' },
  { name: "timeSync.offlineSources maybe", args: ["timeSync.offlineSources", "maybe"] },
  { name: "timeSync.bogus", args: ["timeSync.bogus", "1"] },
  { name: "timeSync.a.b", args: ["timeSync.a.b", "1"] },
  { name: "timeSync. (empty field)", args: ["timeSync.", "1"] },
  // cameraMount.*
  { name: "cameraMount.cam0.rotationDeg 180", args: ["cameraMount.cam0.rotationDeg", "180"] },
  { name: "cameraMount.cam1.rotationDeg 0 merges", args: ["cameraMount.cam1.rotationDeg", "0"], settings: '{"cameraMount":{"cameras":{"cam0":{"rotationDeg":180}}},"gnssAntBias":true}' },
  { name: "cameraMount.2 index key orders first", args: ["cameraMount.2.rotationDeg", "180"], settings: '{"cameraMount":{"cameras":{"cam0":{"rotationDeg":0}}}}' },
  { name: "cameraMount 90", args: ["cameraMount.cam0.rotationDeg", "90"] },
  { name: "cameraMount true", args: ["cameraMount.cam0.rotationDeg", "true"] },
  { name: "cameraMount missing value", args: ["cameraMount.cam0.rotationDeg"] },
  { name: "cameraMount bad key", args: ["cameraMount.cam 0.rotationDeg", "180"] },
  { name: "cameraMount wrong field", args: ["cameraMount.cam0.rotation", "180"] },
  { name: "cameraMount 2 parts", args: ["cameraMount.cam0", "180"] },
  // seeds
  { name: "corrupt settings.json is rewritten", args: ["timeSync.maxStepsPerBoot", "3"], settings: "{not json" },
  { name: "array settings.json is rewritten", args: ["gps-antenna", "active"], settings: "[1,2]" },
  { name: "full document keeps parser order", args: ["recording.retention.maxBytes", "2000000"], settings: '{"simulation":{"scenarioId":"city-loop","loop":true},"cameraMount":{"cameras":{"cam0":{"rotationDeg":180}}},"cellular":{"apn":"internet"},"timeSync":{"maxStepsPerBoot":3,"offlineSources":true},"lteRecovery":{"deadZoneProfile":"balanced"},"recording":{"retention":{"minFreeBytes":5},"telemetry":{"rateHz":5,"enabled":true}},"imuMount":{"roll":0,"pitch":0,"yaw":90},"gnssAntBias":false,"mode":"real"}' },
  { name: "imuMount + decimal knobs survive a rewrite", args: ["gps-antenna", "active"], settings: '{"imuMount":{"enabled":true,"rollDeg":0,"pitchDeg":1.5,"yawDeg":-90},"recording":{"powerShedding":{"railLowVolts":4.75,"cpuTempC":82.5}}}' },
  // `__proto__` camera keys (review-wp6 row 5): JSON.parse keeps them as own
  // properties, but the parsers' plain-object assignments (settings.ts:276,481)
  // hit the Object.prototype.__proto__ setter, so loadSettings drops them; the
  // set handlers' computed-key literals (set.ts:245,299) write them as own keys.
  { name: "__proto__ camera enable dropped on load → re-save", args: ["gps-antenna", "active"], settings: '{"recording":{"video":{"cameras":{"__proto__":true,"cam0":false,"constructor":true}}}}' },
  { name: "__proto__-only camera enables drop the whole video section", args: ["gps-antenna", "active"], settings: '{"recording":{"video":{"cameras":{"__proto__":false}},"telemetry":{"enabled":true}}}' },
  { name: "__proto__ cameraMount entry dropped on load → re-save", args: ["gps-antenna", "active"], settings: '{"cameraMount":{"cameras":{"__proto__":{"rotationDeg":180},"cam1":{"rotationDeg":0},"constructor":{"rotationDeg":180}}}}' },
  { name: "__proto__-only cameraMount keeps an empty cameras map", args: ["gps-antenna", "active"], settings: '{"cameraMount":{"cameras":{"__proto__":{"rotationDeg":180}}}}' },
  { name: "set recording.video.cameras.__proto__ writes an own key", args: ["recording.video.cameras.__proto__", "on"], settings: '{"recording":{"video":{"cameras":{"cam0":false}}}}' },
  { name: "set cameraMount.__proto__.rotationDeg writes an own key", args: ["cameraMount.__proto__.rotationDeg", "180"], settings: '{"cameraMount":{"cameras":{"cam0":{"rotationDeg":0}}}}' },
  { name: "set over a stored __proto__ camera drops it", args: ["recording.video.cameras.cam1", "on"], settings: '{"recording":{"video":{"cameras":{"__proto__":true,"cam0":false}}}}' },
  { name: "set over a stored __proto__ cameraMount drops it", args: ["cameraMount.cam1.rotationDeg", "180"], settings: '{"cameraMount":{"cameras":{"cam0":{"rotationDeg":0},"__proto__":{"rotationDeg":180}}}}' },
  { name: "flags around the key", args: ["--no-restart", "recording.telemetry.rateHz", "--force", "3"] },
  // apn
  { name: "apn missing value", args: ["apn"] },
  { name: "apn invalid chars", args: ["apn", "bad apn!"] },
  { name: "apn too long", args: ["apn", "a".repeat(101)] },
  { name: "apn clear", args: ["apn", ""], settings: '{"cellular":{"apn":"old"},"mode":"real"}' },
  { name: "apn --no-wait (no telemetry)", args: ["apn", "synth.apn", "--no-wait"] },
  { name: "apn appended after other namespaces", args: ["apn", "internet", "--no-wait"], settings: '{"mode":"real","simulation":{"loop":false}}' },
  { name: "apn already in force", args: ["apn", "internet"], lte: [cell({ state: "in-force", apn_applied: "internet" })] },
  { name: "apn already rejected", args: ["apn", "bad.apn"], lte: [cell({ state: "rejected", apn_requested: "bad.apn", fails: 3 })] },
  { name: "apn already rejected (no fails)", args: ["apn", "bad.apn", "--no-wait"], lte: [cell({ state: "rejected", apn_requested: "bad.apn" })] },
  { name: "apn watch: applying then in-force", args: ["apn", "new.apn"], lte: [cell({ state: "in-force", apn_applied: "old" }), cell({ state: "in-force", apn_applied: "old" }), cell({ state: "applying", apn_requested: "new.apn", apn_applied: "old" }), cell({ state: "applying", apn_requested: "new.apn", apn_applied: "old", changed_at: "x" }), cell({ state: "in-force", apn_applied: "new.apn" })] },
  { name: "apn watch: rolled back", args: ["apn", "new.apn"], lte: [cell({ state: "in-force", apn_applied: "old" }), cell({ state: "applying", apn_requested: "new.apn" }), cell({ state: "rolled-back", apn_requested: "new.apn", apn_applied: "old", last_error: "no traffic", fails: 1 })] },
  { name: "apn watch: rolled back, bare", args: ["apn", "new.apn"], lte: [null, cell({ state: "rolled-back", apn_requested: "new.apn" })] },
  { name: "apn watch: rejected", args: ["apn", "new.apn"], lte: [null, cell({ state: "rejected", apn_requested: "new.apn", fails: 3, last_error: "nope" })] },
  { name: "apn watch: stale verdict for this value is history", args: ["apn", "new.apn"], lte: [cell({ state: "rolled-back", apn_requested: "new.apn", fails: 1 }), cell({ state: "rolled-back", apn_requested: "new.apn", fails: 1 }), cell({ state: "in-force", apn_applied: "new.apn" })] },
  { name: "apn watch: old daemon (never any block)", args: ["apn", "new.apn"], lte: [null] },
  { name: "apn watch: garbage telemetry counts as no block", args: ["apn", "new.apn"], lte: ["not json", "[1]", lte({ cellular: { state: "weird" } })] },
  { name: "apn watch: no verdict after 8 minutes", args: ["apn", "new.apn"], lte: [cell({ state: "in-force", apn_applied: "old" })] },
  { name: "apn watch: other value's verdict ignored", args: ["apn", "new.apn"], lte: [cell({ state: "in-force", apn_applied: "old" }), cell({ state: "rolled-back", apn_requested: "other" }), cell({ state: "in-force", apn_applied: "old", changed_at: "t2" })] },
];

const out: unknown[] = [];
for (const c of cases) {
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
process.stdout.write(JSON.stringify({
  jsSourceFile: "src/commands/set.ts (setCommand :391-531, setRecordingCommand :109, setCameraEnable :227, setCameraMountCommand :266, setTimeSyncCommand :316, setApnCommand + watch :533-671) over src/daemon/settings.ts loadSettings/saveSettings",
  jsCommit: "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  generatedBy: "npx tsx dump-cli-set.mts in a git-archive scratch copy of 89b5c64, non-root, fake timers (phase-2 WP6; script in evidence/wp6-tools, handed to WP7)",
  cases: out,
}, null, 2) + "\n");
