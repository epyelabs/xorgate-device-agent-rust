// Phase 4 WP2 — dump the `test/sms-modem.test.ts` I/O scenarios (AtSession,
// sweepTransport, SmsReader, acquireSmsLock, resolveSmsPort: CF29, the 15
// M/F its) + extras, by running the REAL `src/hardware/sms-modem.ts` @ 89b5c64
// against the test file's FakeModem (copied VERBATIM from
// `test/sms-modem.test.ts:20-119`, plus one marked extension: `answers`).
//
//   tools/replay/js/run-tsx.sh tools/replay/js/dump-sms-reader.mts > fixtures/sms_reader/sms-modem-io.json
//
// Output: the Phase 1 fixture format. Every case carries the serialisable
// FakeModem setup (`fake`), the call, and what the TS produced: the exact
// command log the fake received, the result (or the thrown error with its
// class), and the fake's end state. The Rust tests replay each case over the
// Rust FakeModem (crates/xorgate-hal/src/at_port.rs `tests`).
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  acquireSmsLock,
  AtSession,
  AtTimeoutError,
  resolveSmsPort,
  SmsReader,
  SmsReadError,
  sweepTransport,
  type AtTransport,
} from "../src/hardware/sms-modem.js";
import type { SmsStorage } from "../src/hardware/sms-pdu.js";
import { buildDeliver, buildSubmit, scts } from "../test/sms-pdu-builder.js";

interface Slot {
  stat: number;
  length: number;
  pdu: string;
}

interface FakeOpts {
  stores?: Partial<Record<SmsStorage, { total: number; slots: Record<number, Slot> }>>;
  cmgf?: number;
  mem1?: string;
  silent?: boolean;
  cpmsError?: string;
  urcs?: string[];
  hang?: (cmd: string) => boolean;
  onCommand?: (cmd: string) => void;
  /** EXTENSION (not in the test file): a scripted answer for an exact command. */
  answers?: Record<string, string[]>;
}

/** `test/sms-modem.test.ts:43-103`, verbatim bar the marked `answers` branch. */
class FakeModem implements AtTransport {
  log: string[] = [];
  cmgf: number;
  mem1: string;
  closed = false;
  private cb: (line: string) => void = () => {};
  private readonly stores: NonNullable<FakeOpts["stores"]>;

  constructor(private readonly o: FakeOpts = {}) {
    this.cmgf = o.cmgf ?? 0;
    this.mem1 = o.mem1 ?? "SR";
    this.stores = o.stores ?? {};
  }

  onLine(cb: (line: string) => void): void {
    this.cb = cb;
  }

  async close(): Promise<void> {
    this.closed = true;
  }

  private used(mem: string): number {
    return Object.keys(this.stores[mem as SmsStorage]?.slots ?? {}).length;
  }
  private total(mem: string): number {
    return mem === "SR" ? 0 : (this.stores[mem as SmsStorage]?.total ?? 0);
  }
  private triple(mem: string): string {
    return `"${mem}",${this.used(mem)},${this.total(mem)}`;
  }

  async write(data: string): Promise<void> {
    const cmd = data.replace(/\r$/, "");
    this.log.push(cmd);
    this.o.onCommand?.(cmd);
    if (this.o.silent || this.o.hang?.(cmd)) return;
    const out: string[] = [...(this.o.urcs ?? [])];
    let m: RegExpExecArray | null;
    if (this.o.answers?.[cmd]) out.push(...this.o.answers[cmd]!); // EXTENSION
    else if (cmd === "ATE0") out.push("ATE0", "OK");
    else if (cmd === "AT+CMGF?") out.push(`+CMGF: ${this.cmgf}`, "OK");
    else if ((m = /^AT\+CMGF=(\d)$/.exec(cmd))) {
      this.cmgf = Number(m[1]);
      out.push("OK");
    } else if (cmd === "AT+CPMS?") {
      if (this.o.cpmsError) out.push(this.o.cpmsError);
      else out.push(`+CPMS: ${this.triple(this.mem1)},${this.triple("ME")},${this.triple("ME")}`, "OK");
    } else if ((m = /^AT\+CPMS="(\w+)"$/.exec(cmd))) {
      this.mem1 = m[1]!;
      out.push(`+CPMS: ${this.used(this.mem1)},${this.total(this.mem1)},23,23,23,23`, "OK");
    } else if ((m = /^AT\+CMGRO=(\d+)$/.exec(cmd))) {
      if (this.cmgf !== 0) throw new Error("CMGRO sent in text mode");
      const slot = this.stores[this.mem1 as SmsStorage]?.slots[Number(m[1])];
      if (slot) out.push(`+CMGRO: ${slot.stat},"",${slot.length}`, slot.pdu, "OK");
      else out.push("ERROR");
    } else out.push("ERROR");
    queueMicrotask(() => {
      for (const l of out) this.cb(l);
    });
  }
}

const INTL = { kind: "international", value: "15550100001" } as const;

function slot(text: string, day: number, stat = 1): Slot {
  const { pdu, length } = buildDeliver({ from: INTL, text, scts: scts(2026, 8, day, 12, 0, 0, -16) });
  return { stat, length, pdu };
}

/** `sms-modem.test.ts:113-119`. */
function benchStores(): NonNullable<FakeOpts["stores"]> {
  const sm: Record<number, Slot> = {};
  for (let i = 0; i < 7; i++) sm[i] = slot(`sm${i}`, 20 + i, 0);
  const me: Record<number, Slot> = {};
  for (let i = 0; i < 23; i++) me[i] = slot(`me${i}`, 1 + (i % 15), 1);
  return { SM: { total: 40, slots: sm }, ME: { total: 23, slots: me } };
}

/** The serialisable half of FakeOpts (`hang` as a command list, `tickMs` = the budget test's onCommand). */
interface FakeSpec {
  stores?: FakeOpts["stores"];
  cmgf?: number;
  mem1?: string;
  silent?: boolean;
  cpmsError?: string;
  urcs?: string[];
  hang?: string[];
  answers?: Record<string, string[]>;
  /** onCommand: `t += tickMs` on a clock that starts at `t0`. */
  tickMs?: number;
  t0?: number;
}

function makeFake(spec: FakeSpec, clock?: { t: number }): FakeModem {
  const o: FakeOpts = {
    stores: spec.stores,
    cmgf: spec.cmgf,
    mem1: spec.mem1,
    silent: spec.silent,
    cpmsError: spec.cpmsError,
    urcs: spec.urcs,
    answers: spec.answers,
  };
  if (spec.hang) {
    const h = spec.hang;
    o.hang = (c) => h.includes(c);
  }
  if (spec.tickMs !== undefined && clock) {
    const tick = spec.tickMs;
    o.onCommand = () => {
      clock.t += tick;
    };
  }
  return new FakeModem(o);
}

function errOf(err: unknown): Record<string, unknown> {
  if (err instanceof SmsReadError) return { kind: "SmsReadError", status: err.status, message: err.message };
  if (err instanceof AtTimeoutError) return { kind: "AtTimeoutError", message: err.message };
  return { kind: "Error", message: (err as Error).message };
}

const cases: Record<string, unknown>[] = [];

async function sessionCase(name: string, spec: FakeSpec, timeoutMs: number, cmd: string, prefix: string | null) {
  const fake = makeFake(spec);
  const at = new AtSession(fake, timeoutMs);
  let expected: Record<string, unknown>;
  try {
    expected = { response: await at.command(cmd, prefix) };
  } catch (err) {
    expected = { error: errOf(err) };
  }
  cases.push({
    name,
    kind: "session",
    input: { fake: spec, timeoutMs, cmd, prefix },
    expected: { ...expected, log: fake.log },
  });
}

async function sweepCase(
  name: string,
  spec: FakeSpec,
  opts: { commandTimeoutMs?: number; budgetMs?: number } = {},
) {
  const clock = { t: spec.t0 ?? 0 };
  const fake = makeFake(spec, clock);
  let expected: Record<string, unknown>;
  try {
    const now = spec.tickMs !== undefined ? () => clock.t : undefined;
    const r = await sweepTransport(fake, { ...opts, ...(now ? { now } : {}) });
    expected = { result: r };
  } catch (err) {
    expected = { error: errOf(err) };
  }
  // The same scenario through SmsReader (fixed clock), as the agent sees it.
  const clock2 = { t: spec.t0 ?? 0 };
  const fake2 = makeFake(spec, clock2);
  const fixedNow = 1_760_000_000_000;
  const reader = new SmsReader({
    resolvePort: () => "/dev/fake",
    openTransport: async () => fake2,
    now: spec.tickMs !== undefined ? () => clock2.t : () => fixedNow,
    sweep: opts,
  });
  const sweep = await reader.read();
  cases.push({
    name,
    kind: "sweep",
    input: { fake: spec, opts, readerNow: spec.tickMs !== undefined ? null : fixedNow },
    expected: {
      ...expected,
      log: fake.log,
      end: { cmgf: fake.cmgf, mem1: fake.mem1 },
      reader: { sweep, log: fake2.log, closed: fake2.closed },
    },
  });
}

// ---- AtSession (sms-modem.test.ts:131-143) --------------------------------
await sessionCase(
  "drops URCs and echoes, keeps only lines for the command in flight",
  { urcs: ['+CMTI: "SM",3', "+SMS FULL", "RDY", "SMS DONE"] },
  200,
  "AT+CMGF?",
  "+CMGF:",
);
await sessionCase("times out a command nobody answers", { silent: true }, 30, "ATE0", null);
// extras: the timeout text is the command up to the first '='; a CMS error is a final line.
await sessionCase("extra: timeout text cuts at '='", { silent: true }, 30, 'AT+CPMS="SM"', "+CPMS:");
await sessionCase("extra: +CMS ERROR is final, not ok", { answers: { "AT+CPMS?": ["+CMS ERROR: 321"] } }, 200, "AT+CPMS?", "+CPMS:");
await sessionCase("extra: lines after the final result are dropped", { answers: { "AT+CMGF?": ["+CMGF: 1", "OK", "+CMGF: 7"] } }, 200, "AT+CMGF?", "+CMGF:");
await sessionCase("extra: prefix null keeps nothing but the final", { answers: { ATI: ["SIMCOM INCORPORATED", "SIMCOM_SIM7600G-H", "OK"] } }, 200, "ATI", null);
await sessionCase("extra: NO CARRIER is final", { answers: { ATD: ["NO CARRIER"] } }, 200, "ATD", null);

// ---- sweepTransport (sms-modem.test.ts:146-220) ---------------------------
await sweepCase("reads both stores, newest first, and leaves the modem as it found it", {
  stores: benchStores(),
  cmgf: 0,
  mem1: "SR",
});
await sweepCase("stops scanning a store once `used` messages are found", {
  stores: { SM: { total: 40, slots: { 0: slot("a", 1), 2: slot("b", 2) } }, ME: { total: 23, slots: {} } },
});
await sweepCase("switches to PDU mode only when needed and restores text mode", {
  stores: { SM: { total: 5, slots: { 1: slot("x", 3) } } },
  cmgf: 1,
  mem1: "ME",
});
{
  const s = buildSubmit();
  await sweepCase("counts stored SUBMIT PDUs as skipped", {
    stores: { SM: { total: 5, slots: { 0: slot("x", 3), 1: { stat: 3, length: s.length, pdu: s.pdu } } } },
  });
}
await sweepCase(
  "returns partial when the budget runs out, and still restores",
  { stores: benchStores(), tickMs: 1000, t0: 0 },
  { budgetMs: 10_000 },
);
await sweepCase("returns partial when a CMGRO hangs", { stores: benchStores(), hang: ["AT+CMGRO=3"] }, { commandTimeoutMs: 30 });
await sweepCase("maps a silent port to modem_busy and a SIM error to no_sim [silent]", { silent: true }, { commandTimeoutMs: 30 });
await sweepCase("maps a silent port to modem_busy and a SIM error to no_sim [CMS 310]", { cpmsError: "+CMS ERROR: 310" });
await sweepCase("maps a silent port to modem_busy and a SIM error to no_sim [CME 10]", { cpmsError: "+CME ERROR: 10" });
await sweepCase("maps a silent port to modem_busy and a SIM error to no_sim [ERROR]", { cpmsError: "ERROR" });
// extras: every other exit path of sweepTransport (:315-417).
await sweepCase("extra: CMGF? unanswered is an AtTimeoutError (no restore)", { stores: benchStores(), hang: ["AT+CMGF?"] }, { commandTimeoutMs: 30 });
await sweepCase("extra: CMGF? ERROR keeps origCmgf NaN: CMGF=0 sent, never restored", {
  stores: { SM: { total: 5, slots: { 0: slot("x", 3) } } },
  answers: { "AT+CMGF?": ["ERROR"] },
});
await sweepCase("extra: unparseable CPMS? answer", { answers: { "AT+CPMS?": ["+CPMS: 1,2,3", "OK"] } });
await sweepCase("extra: CMGF=0 refused, nothing selected yet", {
  stores: benchStores(),
  cmgf: 1,
  answers: { "AT+CMGF=0": ["+CMS ERROR: 302"] },
});
await sweepCase("extra: SM select no-sim is skipped, ME still read, restored", {
  stores: { ME: { total: 23, slots: { 0: slot("m", 4) } } },
  mem1: "ME",
  answers: {
    'AT+CPMS="SM"': ["+CMS ERROR: 310"],
    "AT+CPMS?": ['+CPMS: "ME",1,23,"ME",1,23,"ME",1,23', "OK"],
  },
});
await sweepCase("extra: ME select error after SM: error, both restores", {
  stores: { SM: { total: 5, slots: { 0: slot("s", 4) } }, ME: { total: 23, slots: { 0: slot("m", 5) } } },
  cmgf: 1,
  answers: { 'AT+CPMS="ME"': ["ERROR"] },
});
await sweepCase("extra: select answer without counts falls back to the CPMS? counts", {
  stores: { SM: { total: 5, slots: { 0: slot("s", 4) } } },
  answers: { 'AT+CPMS="SM"': ["OK"] },
});
await sweepCase("extra: select timeout: AtTimeoutError before mem1Changed is set, so NO restore", {
  stores: benchStores(),
  hang: ['AT+CPMS="SM"'],
}, { commandTimeoutMs: 30 });
await sweepCase("extra: URCs before every answer do not change the sweep", {
  stores: { SM: { total: 5, slots: { 0: slot("a", 1), 3: slot("b", 2) } } },
  urcs: ['+CMTI: "SM",3', "RDY"],
});
await sweepCase("extra: header without length and a non-hex PDU line", {
  stores: { SM: { total: 3, slots: { 0: slot("a", 1) } } },
  answers: {
    "AT+CMGRO=0": ["+CMGRO: 1", "OK"],
    "AT+CMGRO=1": ["+CMGRO: 0,,", "ZZ", "OK"],
  },
});
await sweepCase("extra: budget runs out before the first store", { stores: benchStores(), tickMs: 4000, t0: 0 }, { budgetMs: 10_000 });
await sweepCase("extra: empty stores, nothing selected, no restore", { stores: { SM: { total: 40, slots: {} } } });

// ---- SmsReader (sms-modem.test.ts:224-271) --------------------------------
{
  let opens = 0;
  let t = 1_000_000;
  const logs: string[][] = [];
  const reader = new SmsReader({
    resolvePort: () => "/dev/fake",
    openTransport: async () => {
      opens++;
      const f = new FakeModem({ stores: benchStores() });
      logs.push(f.log);
      return f;
    },
    now: () => t,
  });
  const [a, b] = await Promise.all([reader.read(), reader.read()]);
  const steps: Record<string, unknown>[] = [{ at: t, opens, sameAsFirst: a === b, sweep: a }];
  t += 4000;
  const a2 = await reader.read();
  steps.push({ at: t, opens, sameAsFirst: a2 === a });
  t += 2000;
  const c = await reader.read();
  steps.push({ at: t, opens, sameAsFirst: c === a, status: c.status, sweep: c });
  cases.push({
    name: "joins concurrent callers onto one sweep and reuses it inside the window",
    kind: "reader_join",
    input: { t0: 1_000_000, fake: { stores: benchStores() } },
    expected: { steps, logs },
  });
}
{
  const none = await new SmsReader({ resolvePort: () => null, now: () => 5 }).read();
  const spec: FakeSpec = { cpmsError: "+CMS ERROR: 310" };
  const fake = makeFake(spec);
  const r = await new SmsReader({ resolvePort: () => "/dev/x", openTransport: async () => fake, now: () => 5 }).read();
  cases.push({
    name: "reports no_modem when there is no port, and closes the transport on failure",
    kind: "reader_fail",
    input: { now: 5, fake: spec },
    expected: { noPort: none, failed: r, closed: fake.closed, log: fake.log },
  });
}
{
  const home = mkdtempSync(join(tmpdir(), "xg-sms-"));
  const lock = join(home, "sms.lock");
  writeFileSync(lock, String(process.pid));
  let opened = false;
  const r = await new SmsReader({
    lockPath: lock,
    resolvePort: () => "/dev/x",
    openTransport: async () => {
      opened = true;
      return new FakeModem({ stores: benchStores() });
    },
    now: () => 7,
  }).read();
  rmSync(home, { recursive: true, force: true });
  cases.push({
    name: "reports modem_busy when another process holds the lock",
    kind: "reader_locked",
    input: { now: 7 },
    expected: { sweep: r, opened },
  });
}
// extra: an open failure (SmsReadError from openTransport) still releases the lock.
{
  const home = mkdtempSync(join(tmpdir(), "xg-sms-"));
  const lock = join(home, "sms.lock");
  const r = await new SmsReader({
    lockPath: lock,
    resolvePort: () => "/dev/x",
    openTransport: async () => {
      throw new SmsReadError("modem_busy", "sms port busy");
    },
    now: () => 9,
  }).read();
  let lockLeft = true;
  try {
    (await import("node:fs")).statSync(lock);
  } catch {
    lockLeft = false;
  }
  rmSync(home, { recursive: true, force: true });
  cases.push({
    name: "extra: an open failure maps its status and releases the lock",
    kind: "reader_open_fails",
    input: { now: 9, openError: { status: "modem_busy", message: "sms port busy" } },
    expected: { sweep: r, lockLeft },
  });
}

// ---- acquireSmsLock (sms-modem.test.ts:281-289) ---------------------------
{
  const home = mkdtempSync(join(tmpdir(), "xg-sms-"));
  const p = join(home, "l");
  const steps: Record<string, unknown>[] = [];
  const release = acquireSmsLock(p)!;
  const fs = await import("node:fs");
  steps.push({ step: "free", got: typeof release, content: fs.readFileSync(p, "utf8") === String(process.pid) ? "<pid>" : "?", mode: (fs.statSync(p).mode & 0o777).toString(8) });
  steps.push({ step: "held", got: acquireSmsLock(p) });
  release();
  steps.push({ step: "released", exists: fs.existsSync(p) });
  writeFileSync(p, "999999999");
  const r2 = acquireSmsLock(p);
  steps.push({ step: "stale", got: typeof r2, content: fs.readFileSync(p, "utf8") === String(process.pid) ? "<pid>" : "?" });
  r2?.();
  writeFileSync(p, "garbage");
  const r3 = acquireSmsLock(p);
  steps.push({ step: "extra: unparseable pid is stale", got: typeof r3 });
  r3?.();
  writeFileSync(p, "1");
  steps.push({ step: "extra: pid 1 (EPERM) counts as alive", got: acquireSmsLock(p) });
  rmSync(home, { recursive: true, force: true });
  cases.push({ name: "takes a free lock, refuses a held one, and takes over a stale one", kind: "lock", input: {}, expected: { steps } });
}

// ---- resolveSmsPort (sms-modem.test.ts:316-330) ---------------------------
function fakeSys(home: string, ports: { name: string; iface: string; vendor: string }[]): string {
  const sys = join(home, "sys");
  mkdirSync(sys);
  for (const p of ports) {
    const usb = join(home, "usb", `${p.vendor}-${p.name}`);
    const iface = join(usb, `1.${p.iface}`);
    const dev = join(iface, p.name);
    mkdirSync(dev, { recursive: true });
    writeFileSync(join(usb, "idVendor"), `${p.vendor}\n`);
    writeFileSync(join(iface, "bInterfaceNumber"), `${p.iface}\n`);
    mkdirSync(join(sys, p.name));
    symlinkSync(dev, join(sys, p.name, "device"));
  }
  return sys;
}
function resolveCase(name: string, ports: { name: string; iface: string; vendor: string }[] | null, env: Record<string, string>) {
  const home = mkdtempSync(join(tmpdir(), "xg-sms-"));
  const sys = ports ? fakeSys(home, ports) : join(home, "nope");
  const got = resolveSmsPort({ sysClassTty: sys, env });
  rmSync(home, { recursive: true, force: true });
  cases.push({ name, kind: "resolve", input: { ports, env }, expected: { port: got } });
}
resolveCase(
  "finds interface 05 on the SIMCom device, by identity not name",
  [
    { name: "ttyUSB0", iface: "05", vendor: "0403" },
    { name: "ttyUSB3", iface: "04", vendor: "1e0e" },
    { name: "ttyUSB4", iface: "05", vendor: "1e0e" },
  ],
  {},
);
resolveCase("returns null with no match, and honours XORGATE_SMS_PORT [no match]", [{ name: "ttyUSB2", iface: "04", vendor: "1e0e" }], {});
resolveCase("returns null with no match, and honours XORGATE_SMS_PORT [no dir]", null, {});
resolveCase("returns null with no match, and honours XORGATE_SMS_PORT [override]", [{ name: "ttyUSB2", iface: "04", vendor: "1e0e" }], { XORGATE_SMS_PORT: "/dev/ttyACM9" });
// extras: the bench layout (WP2 rule: ttyUSB2 = iface 04, ttyUSB3 = iface 05), numeric sort, case, blanks.
resolveCase(
  "extra: bench layout — ttyUSB2 iface 04 of 1e0e, ttyUSB3 iface 05 → ttyUSB3",
  [
    { name: "ttyUSB0", iface: "00", vendor: "1e0e" },
    { name: "ttyUSB1", iface: "01", vendor: "1e0e" },
    { name: "ttyUSB2", iface: "04", vendor: "1e0e" },
    { name: "ttyUSB3", iface: "05", vendor: "1e0e" },
  ],
  {},
);
resolveCase(
  "extra: numeric order (ttyUSB10 after ttyUSB9), first match wins",
  [
    { name: "ttyUSB10", iface: "05", vendor: "1e0e" },
    { name: "ttyUSB9", iface: "05", vendor: "1E0E" },
  ],
  {},
);
resolveCase("extra: blank override is ignored", [{ name: "ttyUSB3", iface: "05", vendor: "1e0e" }], { XORGATE_SMS_PORT: "   " });
resolveCase("extra: override is trimmed", [], { XORGATE_SMS_PORT: "  /dev/pts/7 \n" });

// ---- the pty end-to-end inbox (tests/at_port_pty.rs + sms-pty-read.mts) ----
// The bench shape + a stored SUBMIT in SM and a two-part concatenated UCS2
// message (emoji + CJK) in ME — the `p4-sms` tape's inbox.
const ptySpec: FakeSpec = (() => {
  const st = benchStores();
  const sub = buildSubmit();
  st.SM!.slots[7] = { stat: 3, length: sub.length, pdu: sub.pdu };
  const part = (seq: number, ucs2: string) => {
    const { pdu, length } = buildDeliver({
      from: { kind: "alphanumeric", value: "Carrier" },
      ucs2,
      concat: { ref: 0x2a, total: 2, seq },
      scts: scts(2026, 9, 30, 8, 15, 0, -16),
    });
    return { stat: 0, length, pdu };
  };
  st.ME = { total: 25, slots: { ...st.ME!.slots, 23: part(1, "Hello \u{1F44B} "), 24: part(2, "w\u00f6rld \u6771\u4eac") } };
  return { stores: st, cmgf: 1, mem1: "SR" };
})();

process.stdout.write(
  JSON.stringify(
    {
      ptySpec,
      jsSourceFile: "src/hardware/sms-modem.ts (resolveSmsPort :76-99, AtSession :212-270, sweepTransport :315-417, acquireSmsLock :431-470, SmsReader :487-541) + test/sms-modem.test.ts FakeModem :20-119",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy: "tools/replay/js/run-tsx.sh tools/replay/js/dump-sms-reader.mts (phase-4 WP2)",
      cases,
    },
    null,
    2,
  ) + "\n",
);
