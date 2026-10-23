// Phase 2 WP4 — dump `handleCommandMessage` (worker.ts:902-1126) traces.
//
// worker.ts is a top-level script and cannot be imported, so this driver
// slices the REAL source text of the command section out of `src/worker.ts`,
// strips its types with esbuild, and evaluates it with recording stubs for
// the worker's module-level collaborators (store, client, config, configSync,
// smsReader, replay, shutdown, writeFileSync, setTimeout, Date, console). The
// pure helpers (parseCommand, isExpired, claimCommand, executeCommand,
// clampSmsLimit, buildSmsPayload, fixtureSmsSweep, containerExitCode) are the
// real modules @ 89b5c64. Run from a `git archive 89b5c64` scratch copy:
//   npx tsx <this file> > fixtures/command_flow/handle-command.json
//
// Phase 5b WP3 (@ 09da38d, WP1 hand-off): the slice is located by its two
// marker lines at either ref (902-1126 @ 89b5c64, 1005-1293 @ 09da38d); the
// stubs gain `SmsReader.clear`, the real `fixtureSmsRead` / `fixtureSmsClear`
// (module flag reset per scenario) and an OBD-less `source`; `jsCommit` comes
// from JS_COMMIT (no git in the container); new scenarios: clear_sms real /
// sim / ledger duplicate, read after a sim clear, obd_* without a peripheral.
//   JS_COMMIT=<full sha> run-tsx … dump-command-flow.mts > fixtures/command_flow/handle-command.json
import { readFileSync, mkdtempSync, writeFileSync as realWrite, rmSync, existsSync } from "node:fs";
import { join, basename } from "node:path";
import { tmpdir } from "node:os";
import { transformSync } from "esbuild";
import {
  parseCommand, isExpired, claimCommand, executeCommand, containerExitCode,
} from "../src/agent/commands.ts";
import * as smsModem from "../src/hardware/sms-modem.ts";
const { clampSmsLimit, buildSmsPayload, fixtureSmsSweep } = smsModem;
// 09da38d only (undefined at 89b5c64, where the slice never references them).
const fixtureSmsRead = (smsModem as Record<string, unknown>).fixtureSmsRead;
const fixtureSmsClear = (smsModem as Record<string, unknown>).fixtureSmsClear;
const resetFixtureSmsInbox = (smsModem as Record<string, unknown>).resetFixtureSmsInbox as (() => void) | undefined;

const ROOT = new URL("..", import.meta.url).pathname;
const src = readFileSync(join(ROOT, "src/worker.ts"), "utf8").split("\n");
const startIdx = src.findIndex((l) => l.startsWith("/** How long to wait for the ack's PUBACK"));
const endIdx = src.findIndex((l) => l.startsWith("// Registered once; the subscriptions"));
const SLICES: Record<string, [number, number]> = { "89b5c64": [901, 1127], "09da38d": [1004, 1293] };
if (!Object.values(SLICES).some(([a, b]) => a === startIdx && b === endIdx)) {
  throw new Error(`unexpected slice ${startIdx + 1}-${endIdx}`);
}
const JS_LINES = `${startIdx + 1}-${endIdx - 1}`;
const sliceTs = src.slice(startIdx, endIdx).join("\n");
const sliceJs = transformSync(sliceTs, { loader: "ts", format: "esm" }).code;

const DEVICE = "dev-1";
const TOPIC_ACK = `xorgate/devices/${DEVICE}/command/response`;
const TOPIC_SMS = `xorgate/devices/${DEVICE}/sms`;
const T0 = 1750000000000;

type Run = { stdout: string; stderr: string; exitCode: number | null; failed: boolean; notFound: boolean };
interface Step {
  at: number;
  payload: string;
  exec?: Run; // what `sudo` returns, when the step reaches executeCommand
  ledgerError?: string; // claimCommand throws Error(ledgerError)
  puback?: boolean; // default true: the broker PUBACKs the ack at once
}
interface Scenario { name: string; mode: "real" | "sim" | "replay"; ledgerText?: string; steps: Step[] }

/** The O4 real-mode sweep (Rust: `command_flow::sms_reader_unavailable_sweep`). */
const o4Sweep = (now: number) => ({
  status: "error", error: "sms reader not available in this build", finishedAt: now,
  durationMs: 0, storage: {}, skipped: 0, partial: false, messages: [],
});

async function runScenario(sc: Scenario) {
  resetFixtureSmsInbox?.(); // the module flag is per process in the TS: one per scenario here
  const home = mkdtempSync(join(tmpdir(), "wp4-cf-"));
  process.env.XORGATE_HOME = home;
  const ledgerPath = join(home, "commands.json");
  if (sc.ledgerText !== undefined) realWrite(ledgerPath, sc.ledgerText);
  const trace: string[] = [];
  let clock = 0;
  let lastEvent: { t: number } | null = null;
  const timers: Array<{ due: number; ms: number; fn: () => void; ack: boolean }> = [];
  let step: Step = sc.steps[0];
  const stubs: Record<string, unknown> = {
    Date: { now: () => clock },
    console: {
      log: (...a: unknown[]) => trace.push(`log.info ${a.join(" ")}`),
      warn: (...a: unknown[]) => trace.push(`log.warn ${a.join(" ")}`),
      error: (...a: unknown[]) => trace.push(`log.error ${a.join(" ")}`),
    },
    setTimeout: (fn: () => void, ms: number) => {
      const ack = ms === 3000;
      if (!ack) trace.push(`timer ${ms}`);
      timers.push({ due: clock + ms, ms, fn, ack });
      return { unref() { return this; } };
    },
    store: {
      event: (level: string, msg: string, t: number) => {
        trace.push(`event ${level} ${msg}`);
        lastEvent = { t };
      },
      flush: (t: number, force: boolean) => {
        if (!lastEvent || lastEvent.t !== t || force !== true) throw new Error("event not followed by flush(t,true)");
        lastEvent = null;
      },
    },
    client: {
      publish: (topic: string, payload: string, opts: { qos: number }, cb: (e?: Error) => void) => {
        if (opts.qos !== 1) throw new Error("qos");
        if (topic === TOPIC_ACK) {
          trace.push(`ack ${payload}`);
          if (step.puback !== false) cb();
        } else if (topic === TOPIC_SMS) {
          trace.push(`sms ${payload}`);
          cb();
        } else throw new Error(`topic ${topic}`);
      },
    },
    config: { mode: sc.mode, deviceId: DEVICE, commandResponseTopic: TOPIC_ACK, smsTopic: TOPIC_SMS },
    configSync: { request: (o: string) => trace.push(`report ${o}`) },
    replay: sc.mode === "replay" ? {} : null,
    shutdown: (reason: string, code: number) => trace.push(`shutdown ${reason} ${code}`),
    writeFileSync: (p: string, c: string) => trace.push(`flag ${basename(p)} ${c}`),
    join, runtimeHome: () => home,
    SmsReader: class {
      read() { return Promise.resolve(o4Sweep(clock)); }
      // The O4 shape of a clear: `finish` adds `cleared: {}` for op clear.
      clear() { return Promise.resolve({ ...o4Sweep(clock), cleared: {} }); }
    },
    // No `peripherals.obd`: every `source.obd*?.()` is undefined.
    source: {},
    parseCommand, isExpired,
    claimCommand: (id: string, now: number) => {
      if (step.ledgerError !== undefined) throw new Error(step.ledgerError);
      const before = existsSync(ledgerPath) ? readFileSync(ledgerPath, "utf8") : null;
      const r = claimCommand(id, now);
      const after = existsSync(ledgerPath) ? readFileSync(ledgerPath, "utf8") : null;
      if (after !== before) trace.push(`ledger ${after}`);
      return r;
    },
    executeCommand: (c: string) =>
      executeCommand(c as never, async (cmd: string, args: string[]) => {
        trace.push(`exec ${[cmd, ...args].join(" ")}`);
        if (!step.exec) throw new Error("step reached exec without an exec result");
        return step.exec;
      }),
    containerExitCode, clampSmsLimit, buildSmsPayload, fixtureSmsSweep, fixtureSmsRead, fixtureSmsClear,
  };
  const names = Object.keys(stubs);
  const factory = new Function(...names, `${sliceJs}\nreturn { handleCommandMessage };`);
  const { handleCommandMessage } = factory(...names.map((n) => stubs[n]));
  const settle = async () => { for (let i = 0; i < 50; i++) await new Promise((r) => setImmediate(r)); };
  const fireDue = async (until: number) => {
    timers.sort((a, b) => a.due - b.due);
    while (timers.length && timers[0].due <= until) {
      const t = timers.shift()!;
      clock = t.due;
      if (!t.ack) trace.push(`timer.fire ${t.ms}`);
      t.fn();
      await settle();
    }
  };
  const out = [];
  for (const s of sc.steps) {
    const from = trace.length;
    await fireDue(s.at); // timers due before this message (e.g. a +45 s report)
    step = s;
    clock = s.at;
    let done = false;
    const p = handleCommandMessage(Buffer.from(s.payload)).then(() => { done = true; });
    await settle();
    // An un-PUBACKed ack holds the handler until its 3 s timer: run the
    // earliest pending timer until the handler returns.
    while (!done) {
      timers.sort((a, b) => a.due - b.due);
      const t = timers.shift();
      if (!t) throw new Error("handler stuck with no timer pending");
      clock = t.due;
      if (t.ack) trace.push(`ack.timeout ${t.ms}`); else trace.push(`timer.fire ${t.ms}`);
      t.fn();
      await settle();
    }
    await p;
    if (s.puback !== false) for (let i = timers.length - 1; i >= 0; i--) if (timers[i].ack) timers.splice(i, 1);
    out.push({ ...s, trace: trace.slice(from) });
  }
  const from = trace.length;
  await fireDue(Number.MAX_SAFE_INTEGER);
  const ledgerFinal = existsSync(ledgerPath) ? readFileSync(ledgerPath, "utf8") : null;
  rmSync(home, { recursive: true, force: true });
  return { name: sc.name, mode: sc.mode, ...(sc.ledgerText !== undefined ? { ledgerText: sc.ledgerText } : {}), steps: out, after: trace.slice(from), ledgerFinal };
}

const cmd = (over: Record<string, unknown> = {}) =>
  JSON.stringify({ v: 1, requestId: "req-1", command: "reboot", ts: T0, expiresAt: T0 + 60_000, ...over });
const OK: Run = { stdout: "", stderr: "", exitCode: 0, failed: false, notFound: false };
const REFUSED: Run = { stdout: "", stderr: "sudo: a password is required\n", exitCode: 1, failed: true, notFound: false };
const NOT_ALLOWED_RS: Run = {
  stdout: "",
  stderr: "Sorry, user pi is not allowed to execute '/usr/bin/systemctl start --no-block xorgate-updater-rs.service' as root on xorgate.\n",
  exitCode: 1, failed: true, notFound: false,
};
// What lib/exec.ts `run` really returns when `sudo` is absent (execa 9.6.1,
// reject:false — probed with tsx: notFound stays false, stderr "").
const ENOENT_EXECA: Run = { stdout: "", stderr: "", exitCode: null, failed: true, notFound: false };
const NOT_FOUND: Run = { stdout: "", stderr: "spawn sudo ENOENT", exitCode: null, failed: true, notFound: true };
const EXIT_ONLY: Run = { stdout: "out\n", stderr: "  \n", exitCode: 4, failed: true, notFound: false };

const scenarios: Scenario[] = [
  { name: "malformed payloads are dropped with a local trail, no ack", mode: "real", steps: [
    { at: T0, payload: "not json" },
    { at: T0, payload: cmd({ v: 2 }) },
    { at: T0, payload: cmd({ command: "self_destruct" }) },
    { at: T0, payload: cmd({ requestId: "" }) },
    { at: T0, payload: cmd({ ts: "now" }) },
  ] },
  { name: "expired: rejected expired, NOT claimed (age rounded)", mode: "real", steps: [
    { at: T0 + 60_001, payload: cmd() },
    { at: T0 + 200_499, payload: cmd({ requestId: "req-2", command: "power_off", expiresAt: T0 + 10_000_000 }) },
    { at: T0 + 200_500, payload: cmd({ requestId: "req-3", command: "refresh", expiresAt: T0 + 10_000_000 }) },
  ] },
  { name: "expiry boundary: now == expiresAt is still valid", mode: "real", steps: [
    { at: T0 + 60_000, payload: cmd(), exec: OK },
  ] },
  { name: "reboot ok: claim, accepted event, ack, sudo", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd(), exec: OK },
  ] },
  { name: "reboot refused by sudo: failed ack with trimmed stderr", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd(), exec: REFUSED },
  ] },
  { name: "sudo missing (execa ENOENT shape) and notFound", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ requestId: "a" }), exec: ENOENT_EXECA },
    { at: T0 + 2000, payload: cmd({ requestId: "b", command: "power_off" }), exec: NOT_FOUND },
    { at: T0 + 3000, payload: cmd({ requestId: "c" }), exec: EXIT_ONLY },
  ] },
  { name: "PUBACK never arrives: the ack resolves at 3 s and execution proceeds", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd(), exec: REFUSED, puback: false },
  ] },
  { name: "power_off ok", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ command: "power_off" }), exec: OK },
  ] },
  { name: "update ok: updater started trail", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ command: "update" }), exec: OK },
  ] },
  { name: "update refused (the rs unit has no grant before Phase 4, O3)", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ command: "update" }), exec: NOT_ALLOWED_RS },
  ] },
  { name: "duplicate: rejected duplicate, ledger untouched", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ command: "power_off" }), exec: REFUSED },
    { at: T0 + 2000, payload: cmd({ command: "power_off" }) },
  ] },
  { name: "duplicate across a restart (ledger already on disk)", mode: "real",
    ledgerText: `{"v":1,"executed":[{"requestId":"req-1","at":${T0 - 5000}}]}`, steps: [
    { at: T0 + 1000, payload: cmd() },
  ] },
  { name: "ledger write failure: refused, failed ack, nothing executed", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd(), ledgerError: "EACCES: permission denied, open '/var/lib/xorgate-rs/.commands.json.tmp.4242.0'" },
    { at: T0 + 2000, payload: cmd({ requestId: "req-2", command: "refresh" }), ledgerError: "ENOSPC: no space left on device, write" },
  ] },
  { name: "refresh: accepted, flag, report now and at +45 s; rate-limited inside 30 s (claimed, lastRefreshAt untouched)", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ command: "refresh" }) },
    { at: T0 + 11_000, payload: cmd({ requestId: "req-2", command: "refresh" }) },
    { at: T0 + 30_999, payload: cmd({ requestId: "req-3", command: "refresh", ts: T0 + 30_000, expiresAt: T0 + 90_000 }) },
    { at: T0 + 31_000, payload: cmd({ requestId: "req-4", command: "refresh", ts: T0 + 30_000, expiresAt: T0 + 90_000 }) },
    { at: T0 + 50_000, payload: cmd({ requestId: "req-5", command: "refresh", ts: T0 + 30_000, expiresAt: T0 + 90_000 }) },
  ] },
  { name: "read_sms real mode (O4 sweep)", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ command: "read_sms", args: { limit: 25 } }) },
  ] },
  { name: "read_sms sim mode (fixture inbox), limit default and clamp", mode: "sim", steps: [
    { at: T0 + 1000, payload: cmd({ command: "read_sms" }) },
    { at: T0 + 2000, payload: cmd({ requestId: "req-2", command: "read_sms", args: { limit: 2 } }) },
    { at: T0 + 3000, payload: cmd({ requestId: "req-3", command: "read_sms", args: { limit: 0 } }) },
  ] },
  { name: "sim mode: reboot still goes through sudo", mode: "sim", steps: [
    { at: T0 + 1000, payload: cmd(), exec: REFUSED },
  ] },
  { name: "replay mode: update refused before accepted (claimed); reboot/power_off exit codes; refresh and read_sms in-process", mode: "replay", steps: [
    { at: T0 + 1000, payload: cmd({ command: "update" }) },
    { at: T0 + 1500, payload: cmd({ command: "update" }) },
    { at: T0 + 2000, payload: cmd({ requestId: "req-2", command: "refresh" }) },
    { at: T0 + 3000, payload: cmd({ requestId: "req-3", command: "read_sms", args: { limit: 1 } }) },
    { at: T0 + 4000, payload: cmd({ requestId: "req-4", command: "power_off" }) },
    { at: T0 + 5000, payload: cmd({ requestId: "req-5" }) },
  ] },
  // ---- Phase 5b WP3 (@ 09da38d) ----
  { name: "clear_sms real mode (O4 sweep, cleared {})", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ command: "clear_sms", args: { limit: 5 } }) },
  ] },
  { name: "clear_sms sim mode empties the fixture inbox; a later read is empty; a 2nd clear clears 0", mode: "sim", steps: [
    { at: T0 + 1000, payload: cmd({ command: "read_sms", args: { limit: 1 } }) },
    { at: T0 + 2000, payload: cmd({ requestId: "req-2", command: "clear_sms" }) },
    { at: T0 + 3000, payload: cmd({ requestId: "req-3", command: "read_sms" }) },
    { at: T0 + 4000, payload: cmd({ requestId: "req-4", command: "clear_sms", args: { limit: 99 } }) },
  ] },
  { name: "clear_sms redelivered (same requestId): ledger duplicate, never a second clear", mode: "sim", steps: [
    { at: T0 + 1000, payload: cmd({ command: "clear_sms" }) },
    { at: T0 + 1500, payload: cmd({ command: "clear_sms" }) },
    { at: T0 + 2000, payload: cmd({ requestId: "req-2", command: "read_sms" }) },
  ] },
  { name: "clear_sms expired: rejected, not claimed, nothing cleared", mode: "sim", steps: [
    { at: T0 + 60_001, payload: cmd({ command: "clear_sms" }) },
    { at: T0 + 60_002, payload: cmd({ requestId: "req-2", command: "read_sms", ts: T0 + 60_000, expiresAt: T0 + 120_000 }) },
  ] },
  { name: "clear_sms replay mode answers from the fixture", mode: "replay", steps: [
    { at: T0 + 1000, payload: cmd({ command: "clear_sms" }) },
  ] },
  { name: "obd_scan / obd_forget without the OBD peripheral: rejected", mode: "real", steps: [
    { at: T0 + 1000, payload: cmd({ command: "obd_scan" }) },
    { at: T0 + 2000, payload: cmd({ requestId: "req-2", command: "obd_forget" }) },
  ] },
];

// The 09da38d-only scenarios cannot run at 89b5c64 (unknown commands there).
if (!fixtureSmsRead) scenarios.splice(scenarios.findIndex((x) => x.name.startsWith("clear_sms real")));
const cases = [];
for (const sc of scenarios) cases.push(await runScenario(sc));
const jsCommit = process.env.JS_COMMIT_FULL ?? process.env.JS_COMMIT ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d";
console.log(JSON.stringify({
  jsSourceFile: "src/worker.ts",
  jsLines: JS_LINES,
  jsCommit,
  generatedBy:
    `tools/replay/js/dump-command-flow.mts (phase-2 WP4; phase-5b WP3 @ 09da38d) in the shared git-archive scratch copy: the REAL worker.ts:${JS_LINES} text, types stripped by esbuild, evaluated over recording stubs; commands.ts/sms-modem.ts helpers are the real modules`,
  deviceId: DEVICE,
  cases,
}, null, 2));
