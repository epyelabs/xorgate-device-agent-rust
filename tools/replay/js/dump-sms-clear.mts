// Phase 5b WP3 — dump the clear scenarios of `test/sms-modem.test.ts` @
// 09da38d (`clearTransport` 5 its, `SmsReader.clear` 6 its, the fixture inbox
// it is in dump-sms-p5b.mts) + extras for every exit path of clearTransport
// (`sms-modem.ts:478-544`), by running the REAL `src/hardware/sms-modem.ts`
// against the test file's FakeModem (copied VERBATIM from
// `test/sms-modem.test.ts:24-120` @ 09da38d, plus two marked extensions:
// `answers` (a scripted reply for an exact command, as dump-sms-reader.mts)
// and `allowClear` (the rig rule: a delete is answered only when the case
// allows it; otherwise the fake THROWS, failing the dump)).
//
//   JS_COMMIT=<sha> run-tsx dump-sms-clear.mts > fixtures/sms_reader/sms-clear-io.json
//
// Each `clear` case: the serialisable fake (`fake`), the options, and what the
// TS produced: the ClearOutcome or the thrown error, the exact command log,
// the stores' delete order, the fake's end state; plus the `reader` half:
// `SmsReader.clear()` over the same scenario (fixed clock) with the sweep.
// Each `reader_*` case: one SmsReader it, its sweeps and every opened fake.
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  AtTimeoutError,
  buildSmsPayload,
  clearTransport,
  SMS_DELETE_TIMEOUT_MS,
  SmsReader,
  SmsReadError,
  type AtTransport,
  type SmsSweep,
} from "../src/hardware/sms-modem.js";
import type { SmsStorage } from "../src/hardware/sms-pdu.js";
import { buildDeliver, scts } from "../test/sms-pdu-builder.js";

interface Slot {
  stat: number;
  length: number;
  pdu: string;
}
type Stores = Partial<Record<SmsStorage, { total: number; slots: Record<number, Slot> }>>;

interface FakeOpts {
  stores?: Stores;
  cmgf?: number;
  mem1?: string;
  silent?: boolean;
  cpmsError?: string;
  urcs?: string[];
  hang?: (cmd: string) => boolean;
  onCommand?: (cmd: string) => void;
  cmgdError?: Partial<Record<SmsStorage, string>>;
  /** EXTENSION: a scripted answer for an exact command. */
  answers?: Record<string, string[]>;
  /** EXTENSION (rig rule, phase-5b §6.7): without it a delete THROWS. */
  allowClear?: boolean;
  /** EXTENSION: a write of this command throws this message (a dead port). */
  writeError?: Record<string, string>;
}

/** `test/sms-modem.test.ts:48-120` @ 09da38d, verbatim bar the marked lines. */
class FakeModem implements AtTransport {
  log: string[] = [];
  cmgf: number;
  mem1: string;
  closed = false;
  deletes: string[] = [];
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
    if (this.o.writeError?.[cmd]) throw new Error(this.o.writeError[cmd]); // EXTENSION
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
    } else if (cmd === "AT+CMGD=0,4") {
      if (!this.o.allowClear) throw new Error("rig: delete without allowClear"); // EXTENSION
      this.deletes.push(this.mem1);
      const err = this.o.cmgdError?.[this.mem1 as SmsStorage];
      if (err) out.push(err);
      else {
        const st = this.stores[this.mem1 as SmsStorage];
        if (st) st.slots = {};
        out.push("OK");
      }
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
/** `sms-modem.test.ts:123-131`: SM 7/40 unread, ME full 23/23. */
function benchStores(): Stores {
  const sm: Record<number, Slot> = {};
  for (let i = 0; i < 7; i++) sm[i] = slot(`sm${i}`, 20 + i, 0);
  const me: Record<number, Slot> = {};
  for (let i = 0; i < 23; i++) me[i] = slot(`me${i}`, 1 + (i % 15), 1);
  return { SM: { total: 40, slots: sm }, ME: { total: 23, slots: me } };
}

/** The serialisable half of FakeOpts (`hang` as a command list). */
interface FakeSpec {
  stores?: Stores;
  cmgf?: number;
  mem1?: string;
  silent?: boolean;
  cpmsError?: string;
  urcs?: string[];
  hang?: string[];
  answers?: Record<string, string[]>;
  cmgdError?: Partial<Record<SmsStorage, string>>;
  allowClear?: boolean;
  writeError?: Record<string, string>;
}
const clone = <T,>(v: T): T => JSON.parse(JSON.stringify(v)) as T;
function makeFake(spec: FakeSpec, stores?: Stores): FakeModem {
  const { hang, ...rest } = spec;
  const o: FakeOpts = { ...clone(rest), stores: stores ?? clone(spec.stores ?? {}) };
  if (hang) o.hang = (c) => hang.includes(c);
  return new FakeModem(o);
}
function errOf(err: unknown): Record<string, unknown> {
  if (err instanceof SmsReadError) return { kind: "SmsReadError", status: err.status, message: err.message };
  if (err instanceof AtTimeoutError) return { kind: "AtTimeoutError", message: err.message };
  return { kind: "Error", message: (err as Error).message };
}

const FIXED = 1_760_000_000_000;
const cases: Record<string, unknown>[] = [];

async function clearCase(name: string, spec: FakeSpec, opts: { commandTimeoutMs?: number; deleteTimeoutMs?: number } = {}) {
  spec = { allowClear: true, ...spec };
  const fake = makeFake(spec);
  let expected: Record<string, unknown>;
  try {
    expected = { outcome: await clearTransport(fake, opts) };
  } catch (err) {
    expected = { error: errOf(err) };
  }
  // The same scenario through SmsReader.clear (fixed clock), as the agent sees it.
  const fake2 = makeFake(spec);
  const reader = new SmsReader({
    resolvePort: () => "/dev/fake",
    openTransport: async () => fake2,
    now: () => FIXED,
    clear: opts,
    sweep: { commandTimeoutMs: opts.commandTimeoutMs },
  });
  const sweep = await reader.clear();
  cases.push({
    name,
    kind: "clear",
    input: { fake: spec, opts },
    expected: {
      ...expected,
      log: fake.log,
      deletes: fake.deletes,
      end: { cmgf: fake.cmgf, mem1: fake.mem1 },
      reader: {
        sweep,
        payload: buildSmsPayload(sweep, { deviceId: "d", requestId: "r", limit: 10, action: "clear" }),
        log: fake2.log,
        deletes: fake2.deletes,
        closed: fake2.closed,
        end: { cmgf: fake2.cmgf, mem1: fake2.mem1 },
      },
    },
  });
}

// ---- clearTransport (sms-modem.test.ts @ 09da38d :291-347) ----------------
await clearCase("deletes SM then ME, reports what was there, and restores mem1 without touching CMGF", { stores: benchStores(), cmgf: 1, mem1: "SR" });
await clearCase("restores mem1 when a delete fails, names the store, and still clears the other one", { stores: benchStores(), mem1: "ME", cmgdError: { SM: "+CMS ERROR: 500" } });
await clearCase("stops after a delete that times out, and still restores mem1", { stores: benchStores(), hang: ["AT+CMGD=0,4"] }, { commandTimeoutMs: 30, deleteTimeoutMs: 40 });
await clearCase("skips an empty store (cleared 0, no CMGD), known from CPMS? [SM is mem1 and empty]", { stores: { SM: { total: 40, slots: {} }, ME: benchStores().ME }, mem1: "SM" });
await clearCase("skips an empty store (cleared 0, no CMGD), known from CPMS? [SM unknown, found empty by the select]", { stores: { SM: { total: 40, slots: {} }, ME: { total: 23, slots: {} } } });
await clearCase("throws the read statuses before anything is deleted [silent]", { silent: true }, { commandTimeoutMs: 30 });
await clearCase("throws the read statuses before anything is deleted [CMS 310]", { stores: benchStores(), cpmsError: "+CMS ERROR: 310" });
// ---- extras: every other exit path of clearTransport (:478-544) ----------
await clearCase("extra: CPMS? plain ERROR → error, nothing deleted", { stores: benchStores(), cpmsError: "ERROR" });
await clearCase("extra: CPMS? unanswered → AtTimeoutError text, nothing deleted", { stores: benchStores(), hang: ["AT+CPMS?"] }, { commandTimeoutMs: 30 });
await clearCase("extra: unparseable CPMS? answer", { stores: benchStores(), answers: { "AT+CPMS?": ["+CPMS: 1,2,3", "OK"] } });
await clearCase("extra: SM select timeout stops the phase (no ME), mem1 still restored", { stores: benchStores(), hang: ['AT+CPMS="SM"'] }, { commandTimeoutMs: 30 });
await clearCase("extra: ME select timeout after an SM delete: SM cleared, restore sent", { stores: benchStores(), hang: ['AT+CPMS="ME"'] }, { commandTimeoutMs: 30 });
await clearCase("extra: SM select no-sim is skipped (no cleared.SM), ME still cleared", { stores: { ME: benchStores().ME }, mem1: "ME", answers: { 'AT+CPMS="SM"': ["+CMS ERROR: 310"], "AT+CPMS?": ['+CPMS: "ME",23,23,"ME",23,23,"ME",23,23', "OK"] } });
await clearCase("extra: SM select ERROR is recorded, ME still cleared", { stores: benchStores(), answers: { 'AT+CPMS="SM"': ["ERROR"] } });
await clearCase("extra: both deletes fail: joined with '; '", { stores: benchStores(), cmgdError: { SM: "+CMS ERROR: 321", ME: "ERROR" } });
await clearCase("extra: select answer without counts falls back to CPMS? counts (ME known)", { stores: benchStores(), answers: { 'AT+CPMS="ME"': ["OK"] } });
await clearCase("extra: select answer without counts and no known count: deleted anyway, cleared 0", { stores: benchStores(), answers: { 'AT+CPMS="SM"': ["OK"] } });
await clearCase("extra: URCs before every answer do not change the clear", { stores: benchStores(), urcs: ['+CMTI: "SM",3', "RDY"] });
await clearCase("extra: a transport error on the delete is rethrown after the restore", { stores: benchStores(), writeError: { "AT+CMGD=0,4": "EIO: i/o error, write" } });
await clearCase("extra: the post-clear sweep fails (CMGF? unanswered): error, cleared kept", { stores: benchStores(), hang: ["AT+CMGF?"] }, { commandTimeoutMs: 30 });
await clearCase("extra: delete error AND a failing post-clear sweep: the delete error wins", { stores: benchStores(), hang: ["AT+CMGF?"], cmgdError: { ME: "+CMS ERROR: 321" } }, { commandTimeoutMs: 30 });

// ---- SmsReader.clear (sms-modem.test.ts @ 09da38d :349-461) -------------
/** `bench(o)`: one modem whose stores persist across opens. */
function bench(spec: FakeSpec = {}) {
  const stores = benchStores();
  const opened: FakeModem[] = [];
  const clock = { t: 1_000_000 };
  const reader = new SmsReader({
    resolvePort: () => "/dev/fake",
    openTransport: async () => {
      const f = makeFake({ allowClear: true, ...spec }, stores);
      opened.push(f);
      return f;
    },
    now: () => clock.t,
  });
  return { reader, opened, clock };
}
const fakes = (o: FakeModem[]) => o.map((f) => ({ log: f.log, deletes: f.deletes, closed: f.closed, end: { cmgf: f.cmgf, mem1: f.mem1 } }));
const pay = (s: SmsSweep, action: "read" | "clear") => buildSmsPayload(s, { deviceId: "d", requestId: "r", limit: 10, action });

{
  const { reader, opened } = bench();
  const sweep = await reader.clear();
  cases.push({ name: "clears, then sweeps on the same port: ok, cleared counts, empty post-clear storage", kind: "reader_clear", input: { spec: {} }, expected: { sweep, payload: pay(sweep, "clear"), opened: fakes(opened) } });
}
{
  const spec = { cmgdError: { ME: "+CMS ERROR: 321" } };
  const { reader, opened } = bench(spec);
  const sweep = await reader.clear();
  cases.push({ name: "reports error naming the store, with what was there and the state after", kind: "reader_clear", input: { spec }, expected: { sweep, payload: pay(sweep, "clear"), opened: fakes(opened) } });
}
{
  const spec = { cpmsError: "+CMS ERROR: 310" };
  const { reader, opened } = bench(spec);
  const sweep = await reader.clear();
  cases.push({ name: "maps a no-SIM modem to no_sim with nothing deleted", kind: "reader_clear", input: { spec }, expected: { sweep, payload: pay(sweep, "clear"), opened: fakes(opened) } });
}
{
  const { reader, opened, clock } = bench();
  const before = await reader.read();
  clock.t += 1000;
  const c = await reader.clear();
  clock.t += 1000;
  const after = await reader.read();
  cases.push({
    name: "drops the cached read: a read right after a clear sweeps afresh",
    kind: "reader_drop_cache",
    input: { t0: 1_000_000 },
    expected: { before, clear: c, after, afterIsBefore: after === before, opened: fakes(opened) },
  });
}
{
  const { reader, opened } = bench();
  const read1 = reader.read();
  const clear = reader.clear();
  const clear2 = reader.clear();
  const read2 = reader.read();
  const [r1, c, c2, r2] = await Promise.all([read1, clear, clear2, read2]);
  const r3 = await reader.read();
  cases.push({
    name: "never joins an in-flight read; a read during a clear waits and sweeps afresh",
    kind: "reader_queue",
    input: { t0: 1_000_000 },
    expected: { r1, c, r2, clearsJoined: c === c2, r2IsC: r2 === c, r3IsR2: r3 === r2, opened: fakes(opened) },
  });
}
{
  const home = mkdtempSync(join(tmpdir(), "xg-sms-"));
  const lock = join(home, "sms.lock");
  writeFileSync(lock, String(process.pid));
  let opens = 0;
  const r = await new SmsReader({
    lockPath: lock,
    resolvePort: () => "/dev/x",
    openTransport: async () => {
      opens++;
      return makeFake({ allowClear: true, stores: benchStores() });
    },
    now: () => 7,
  }).clear();
  rmSync(home, { recursive: true, force: true });
  cases.push({ name: "reports modem_busy when another process holds the lock, and opens nothing", kind: "reader_locked", input: { now: 7 }, expected: { sweep: r, opens } });
}
{
  const r = await new SmsReader({ resolvePort: () => null, now: () => 5 }).clear();
  cases.push({ name: "extra: no port → no_modem with cleared {}", kind: "reader_noport", input: { now: 5 }, expected: { sweep: r, payload: pay(r, "clear") } });
}

process.stdout.write(
  JSON.stringify(
    {
      jsSourceFile: "src/hardware/sms-modem.ts clearTransport + SmsReader.clear over test/sms-modem.test.ts FakeModem",
      jsCommit: process.env.JS_COMMIT ?? "09da38d",
      generatedBy: "tools/replay/js/dump-sms-clear.mts (phase-5b WP3; FakeModem verbatim from the test file + `answers`/`allowClear`/`writeError` extensions)",
      SMS_DELETE_TIMEOUT_MS,
      fixedNow: FIXED,
      cases,
    },
    null,
    1,
  ) + "\n",
);
