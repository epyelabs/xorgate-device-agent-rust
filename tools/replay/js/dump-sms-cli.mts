// Phase 4 WP2 — golden outputs of `xorgate sms` (`src/commands/sms.ts` @
// 89b5c64) over fixed sweeps: the REAL `smsCommand` with
// `SmsReader.prototype.read` replaced by a canned sweep (no modem), stdout
// captured line by line, `process.exitCode` recorded.
//
//   tools/replay/js/run-tsx.sh tools/replay/js/dump-sms-cli.mts > fixtures/sms_cli/sms-cli.json
//
// `process.stdout.columns` is undefined under docker run without -t (the
// TS's `?? 120`); one case sets it to exercise the width rule.
//
// Phase 5b WP3 (@ 09da38d): `xorgate sms clear [--yes]` cases too —
// `SmsReader.prototype.clear` replaced by a canned clear sweep; stderr is
// captured (`stderr`, only when written); `ttyAnswer` swaps `process.stdin`
// for a TTY-flagged stream that answers the confirmation prompt, and the
// prompt (written by readline straight to stdout) is captured as `prompt`.
// Run at 89b5c64 the old cases re-dump byte-identical (no clear cases there).
//   JS_COMMIT=<sha> run-tsx dump-sms-cli.mts > fixtures/sms_cli/sms-cli.json
import { mkdtempSync } from "node:fs";
import { PassThrough } from "node:stream";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { smsCommand } from "../src/commands/sms.js";
import { fixtureSmsSweep, SmsReader, type SmsSweep } from "../src/hardware/sms-modem.js";
import type { SmsMessage } from "../src/hardware/sms-pdu.js";

process.env.XORGATE_HOME = mkdtempSync(join(tmpdir(), "xg-sms-cli-"));
const NOW = Date.UTC(2026, 9, 1, 12, 34, 56);

const base = fixtureSmsSweep(NOW);
const msg = (o: Partial<SmsMessage>): SmsMessage => ({ ...base.messages[0]!, ...o });

const sweeps: Record<string, SmsSweep> = {
  fixture: { ...base, durationMs: 1234 },
  rich: {
    status: "ok",
    error: null,
    finishedAt: NOW,
    durationMs: 87,
    storage: { ME: { used: 5, total: 23 }, SM: { used: 0, total: 40 } },
    skipped: 2,
    partial: true,
    messages: [
      msg({ id: "ME:0", storage: "ME", indexes: [0], from: "VERYLONGALPHANUMERICSENDER", fromType: "alphanumeric", text: "line one\r\nline two\tTAB\fff", unread: false }),
      msg({ id: "ME:1", storage: "ME", indexes: [1], from: "+15550100001", sentAt: null, binary: true, text: "\u0001\u0002 bytes" }),
      msg({ id: "ME:2+ME:3", storage: "ME", indexes: [2, 3], incomplete: true, truncated: true, text: "x".repeat(60) }),
      msg({ id: "ME:4", storage: "ME", indexes: [4], text: "a".repeat(48) + "👍👍 emoji at the cut", from: "東京👍" }),
      msg({ id: "ME:5", storage: "ME", indexes: [5], text: "", from: "" }),
    ],
  },
  empty_ok: {
    status: "ok",
    error: null,
    finishedAt: NOW,
    durationMs: 3,
    storage: {},
    skipped: 0,
    partial: false,
    messages: [],
  },
  no_modem: {
    status: "no_modem",
    error: "no SIMCom interface 05 tty",
    finishedAt: NOW,
    durationMs: 0,
    storage: {},
    skipped: 0,
    partial: false,
    messages: [],
  },
  no_sim_null_error: {
    status: "no_sim",
    error: null,
    finishedAt: NOW,
    durationMs: 12,
    storage: { SM: { used: 1, total: 2 } },
    skipped: 1,
    partial: false,
    messages: [],
  },
};

const argvs: string[][] = [
  [],
  ["--json"],
  ["--limit", "0"],
  ["--limit=51"],
  ["--limit", "abc"],
  ["--limit", "2"],
  ["--limit", "2", "--json"],
  ["--limit"],
  ["--limit", ""],
  ["--limit", "0x3"],
  ["--limit", "2.9"],
  ["--limit", "Infinity"],
  ["--help"],
  ["-h", "--json"],
];

let current: SmsSweep = sweeps.fixture!;
SmsReader.prototype.read = async function () {
  return current;
};
let clears = 0;
(SmsReader.prototype as unknown as { clear: () => Promise<SmsSweep> }).clear = async function () {
  clears++;
  return current;
};
const P5B = typeof (SmsReader.prototype as unknown as { clear?: unknown }).clear === "function" &&
  (await import("../src/hardware/sms-modem.js").then((m) => "SMS_DELETE_TIMEOUT_MS" in m));

const cases: Record<string, unknown>[] = [];
const realLog = console.log;
const realErr = console.error;
const realStdin = Object.getOwnPropertyDescriptor(process, "stdin")!;
async function run(name: string, sweep: string, argv: string[], port: string | undefined, columns?: number, ttyAnswer?: string) {
  current = sweeps[sweep]!;
  const clears0 = clears;
  const err: string[] = [];
  console.error = (...a: unknown[]) => {
    err.push(a.map(String).join(" "));
  };
  let prompt = "";
  const realWrite = process.stdout.write.bind(process.stdout);
  if (ttyAnswer !== undefined) {
    const input = Object.assign(new PassThrough(), { isTTY: true });
    Object.defineProperty(process, "stdin", { value: input, configurable: true });
    (process.stdout as unknown as { write: unknown }).write = (chunk: unknown) => {
      prompt += String(chunk);
      if (prompt.includes("[y/N] ")) setImmediate(() => input.write(ttyAnswer));
      return true;
    };
  }
  if (port === undefined) delete process.env.XORGATE_SMS_PORT;
  else process.env.XORGATE_SMS_PORT = port;
  const out: string[] = [];
  console.log = (...a: unknown[]) => {
    // What Node writes to a UTF-8 stdout: a lone surrogate becomes U+FFFD.
    out.push(Buffer.from(a.map(String).join(" "), "utf8").toString("utf8"));
  };
  const prevCols = Object.getOwnPropertyDescriptor(process.stdout, "columns");
  if (columns !== undefined) Object.defineProperty(process.stdout, "columns", { value: columns, configurable: true });
  process.exitCode = undefined;
  try {
    await smsCommand(argv);
  } finally {
    console.log = realLog;
    console.error = realErr;
    if (ttyAnswer !== undefined) {
      Object.defineProperty(process, "stdin", realStdin);
      (process.stdout as unknown as { write: unknown }).write = realWrite;
    }
    if (columns !== undefined) {
      if (prevCols) Object.defineProperty(process.stdout, "columns", prevCols);
      else delete (process.stdout as unknown as Record<string, unknown>).columns;
    }
  }
  const exitCode = process.exitCode ?? 0;
  process.exitCode = undefined;
  const p5b = ttyAnswer !== undefined || argv[0] === "clear";
  cases.push({
    name,
    input: { sweep, argv, port: port ?? null, columns: columns ?? null, ...(ttyAnswer !== undefined ? { ttyAnswer } : {}) },
    expected: {
      stdout: out,
      exitCode,
      ...(err.length ? { stderr: err } : {}),
      ...(prompt ? { prompt } : {}),
      ...(p5b ? { cleared: clears - clears0 } : {}),
    },
  });
}

for (const argv of argvs) await run(`fixture ${JSON.stringify(argv)}`, "fixture", argv, "/dev/ttyUSB3");
for (const s of ["rich", "empty_ok", "no_modem", "no_sim_null_error"]) {
  await run(`${s} []`, s, [], "/dev/ttyUSB3");
  await run(`${s} [--json]`, s, ["--json"], "/dev/ttyUSB3");
}
await run("rich [--limit 2] no port resolvable", "rich", ["--limit", "2"], undefined);
await run("rich [] columns 200", "rich", [], "/dev/ttyUSB3", 200);
await run("rich [] columns 80 (width floor 40)", "rich", [], "/dev/ttyUSB3", 80);

if (P5B) {
  const z = { SM: { used: 0, total: 40 }, ME: { used: 0, total: 23 } };
  Object.assign(sweeps, {
    clear_ok: { status: "ok", error: null, finishedAt: NOW, durationMs: 2345, storage: z, skipped: 0, partial: false, messages: [], cleared: { SM: 7, ME: 23 } },
    clear_delete_error: { status: "error", error: "delete ME failed (+CMS ERROR: 321)", finishedAt: NOW, durationMs: 812, storage: { SM: { used: 0, total: 40 }, ME: { used: 23, total: 23 } }, skipped: 0, partial: false, messages: [], cleared: { SM: 7, ME: 23 } },
    clear_no_sim: { status: "no_sim", error: "sim not ready", finishedAt: NOW, durationMs: 9, storage: {}, skipped: 0, partial: false, messages: [], cleared: {} },
    clear_me_only: { status: "ok", error: null, finishedAt: NOW, durationMs: 40, storage: { ME: { used: 0, total: 23 } }, skipped: 0, partial: false, messages: [], cleared: { ME: 23 } },
    clear_no_cleared_key: { status: "no_modem", error: "no SIMCom interface 05 tty", finishedAt: NOW, durationMs: 0, storage: {}, skipped: 0, partial: false, messages: [] },
  });
  for (const s of ["clear_ok", "clear_delete_error", "clear_no_sim", "clear_me_only", "clear_no_cleared_key"]) {
    await run(`${s} [clear --yes]`, s, ["clear", "--yes"], "/dev/ttyUSB3");
  }
  await run("clear_ok [clear -y]", "clear_ok", ["clear", "-y"], "/dev/ttyUSB3");
  await run("clear_ok [clear --json --yes] (flags of the read are ignored)", "clear_ok", ["clear", "--json", "--yes"], "/dev/ttyUSB3");
  await run("clear [clear] no TTY, no --yes: refused, nothing cleared", "clear_ok", ["clear"], "/dev/ttyUSB3");
  await run("clear [clear --help]", "clear_ok", ["clear", "--help"], "/dev/ttyUSB3");
  await run("clear [clear -h --yes]: help wins", "clear_ok", ["clear", "-h", "--yes"], "/dev/ttyUSB3");
  await run("clear [--yes clear]: not the subcommand (a read)", "fixture", ["--yes", "clear"], "/dev/ttyUSB3");
  for (const a of ["y\n", "YES\n", "  Yes  \n", "n\n", "\n", "yess\n", "ye\n"]) {
    await run(`clear [clear] TTY answers ${JSON.stringify(a)}`, "clear_ok", ["clear"], "/dev/ttyUSB3", undefined, a.replace(/\\n/g, "\n"));
  }
}

process.stdout.write(
  JSON.stringify(
    {
      jsSourceFile: "src/commands/sms.ts:1-93 (smsCommand, flagValue, oneLine) over src/hardware/sms-modem.ts buildSmsPayload/clampSmsLimit",
      jsCommit: process.env.JS_COMMIT ?? "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
      generatedBy: "tools/replay/js/run-tsx.sh tools/replay/js/dump-sms-cli.mts (phase-4 WP2; SmsReader.prototype.read replaced by the case's sweep)",
      sweeps,
      cases,
    },
    null,
    2,
  ) + "\n",
);
