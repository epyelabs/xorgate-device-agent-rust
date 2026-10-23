// WP1 (Phase 4) dump driver: re-runs every LedDriver it() of test/led.test.ts
// (14) and every it() of test/led-manager.test.ts (12) @ 89b5c64 step by step,
// against the REAL LedDriver / LedManager, under vitest fake timers (each
// `await new Promise(r => setTimeout(r, N))` / `settle(N)` of the original
// becomes `await vi.advanceTimersByTimeAsync(N)`), and records the full trace:
// every PinWriter call `{t, pins, high}` and every log line `{t, msg}` in
// virtual ms, plus `marks` (the trace lengths at the points where the original
// test truncates or snapshots `writes`). The Rust tests replay the same steps
// under tokio::time::pause() and compare the traces exactly, then re-assert
// the original expectations.
//
// Run (scratch copy ONLY):
//   cp zz-wp1-dump-led-traces.test.ts <rust>/tools/replay/.cache/js-89b5c64/test/
//   docker run --rm --platform linux/arm64 -e TZ=UTC -v <that dir>:/js -w /js node:20-bookworm \
//     npx --no-install vitest run test/zz-wp1-dump-led-traces.test.ts
// Output: test/zz-wp1-led-driver.json → fixtures/led_driver/led-driver.json,
//         test/zz-wp1-led-manager.json → fixtures/led_manager/led-manager.json
import { writeFileSync } from "node:fs";
import { afterAll, it, vi } from "vitest";
import { LedManager } from "../src/agent/led-manager.js";
import { LedDriver, STATUS_LED_MAP, type LedPinMap } from "../src/hardware/led.js";
import type { LedSignals } from "../src/lib/led-state.js";
import type { RecordingStatus } from "../src/agent/telemetry.js";

type Call = { t: number; pins: number[]; high: boolean };
type Log = { t: number; msg: string };
type Trace = { calls: Call[]; logs: Log[]; marks: Record<string, number>; extra: Record<string, unknown> };

let T0 = 0;
const now = (): number => Date.now() - T0;
const adv = (ms: number): Promise<void> => vi.advanceTimersByTimeAsync(ms);

function newTrace(): Trace {
  return { calls: [], logs: [], marks: {}, extra: {} };
}

/** A writer that records and answers `ok(pins, high)` (default: success). */
function writer(tr: Trace, ok: (pins: readonly number[], high: boolean) => boolean = () => true) {
  return async (pins: readonly number[], high: boolean): Promise<boolean> => {
    tr.calls.push({ t: now(), pins: [...pins], high });
    return ok(pins, high);
  };
}

function driverFor(tr: Trace, opts: { map?: LedPinMap; ok?: (p: readonly number[], h: boolean) => boolean; probeOk?: boolean } = {}) {
  return new LedDriver({
    map: opts.map ?? STATUS_LED_MAP,
    log: (m) => tr.logs.push({ t: now(), msg: m }),
    probe: async () => (opts.probeOk === false ? { ok: false, reason: "not a Raspberry Pi" } : { ok: true }),
    write: writer(tr, opts.ok),
  });
}

const driverCases: { name: string; ts: string; run: (tr: Trace) => Promise<void> }[] = [];
const D = (name: string, ts: string, run: (tr: Trace) => Promise<void>) => driverCases.push({ name, ts, run });

for (const colour of ["red", "green", "blue", "purple", "cyan", "white", "off"] as const) {
  D(`colour ${colour} on the Argus pins, active high`, "led.test.ts:72", async (tr) => {
    const d = driverFor(tr);
    await d.init();
    await d.set(colour);
  });
}
D("active low inverts the channels", "led.test.ts:81", async (tr) => {
  const d = driverFor(tr, { map: { ...STATUS_LED_MAP, activeHigh: false } });
  await d.init();
  await d.set("red");
});
D("non-repeating pattern ends dark", "led.test.ts:126", async (tr) => {
  const d = driverFor(tr);
  await d.init();
  const p = d.play("white", "flash2");
  await vi.runAllTimersAsync();
  await p;
});
D("solid stays lit until stop()", "led.test.ts:133", async (tr) => {
  const d = driverFor(tr);
  await d.init();
  const running = d.play("red", "solid");
  await adv(10);
  d.stop();
  await running;
});
D("stop a repeating pattern, then blank", "led.test.ts:143", async (tr) => {
  const d = driverFor(tr);
  await d.init();
  const running = d.play("purple", "blink-fast");
  await adv(300);
  d.stop();
  await running;
  await d.blank();
});
D("only changing channels are written within a pattern", "led.test.ts:157", async (tr) => {
  const d = driverFor(tr);
  await d.init();
  const running = d.play("green", "blink-slow");
  await adv(1300);
  d.stop();
  await running;
});
D("one call per transition for a two-channel colour", "led.test.ts:175", async (tr) => {
  const d = driverFor(tr);
  await d.init();
  const running = d.play("purple", "blink-fast");
  await adv(700);
  d.stop();
  await running;
});
D("a new state re-drives all three", "led.test.ts:197", async (tr) => {
  const d = driverFor(tr);
  await d.init();
  await d.set("green");
  tr.marks.cleared = tr.calls.length;
  await d.set("green");
});
{
  let fail = false;
  D("fail-safe blank writes all three", "led.test.ts:209", async (tr) => {
    fail = false;
    const d = driverFor(tr, { ok: () => !fail });
    await d.init();
    await d.set("off");
    fail = true;
    tr.marks.cleared = tr.calls.length;
    await d.set("red");
  });
}
D("a failed write blanks all three, logged once", "led.test.ts:280", async (tr) => {
  const d = driverFor(tr, { ok: (pins) => !pins.includes(21) });
  await d.init();
  await d.set("white");
  tr.marks.firstSet = tr.calls.length;
  await d.set("white");
});
{
  let fail = false;
  D("a write failure aborts a running pattern", "led.test.ts:304", async (tr) => {
    fail = false;
    const d = driverFor(tr, { ok: () => !fail });
    await d.init();
    let done = false;
    const running = d.play("green", "blink-fast").then(() => {
      done = true;
    });
    await adv(60);
    fail = true;
    // The original awaits `running` in real time: the next step (t=125)
    // fails and cancels the pattern.
    while (!done) await adv(1);
    await running;
    tr.marks.after = tr.calls.length;
    tr.extra.resolvedAt = now();
    await adv(400);
  });
}
D("a throwing writer is survived", "led.test.ts:327", async (tr) => {
  const d = new LedDriver({
    map: STATUS_LED_MAP,
    log: (m) => tr.logs.push({ t: now(), msg: m }),
    probe: async () => ({ ok: true }),
    write: async (pins, high) => {
      tr.calls.push({ t: now(), pins: [...pins], high });
      throw new Error("pinctrl exploded");
    },
  });
  await d.init();
  tr.extra.resolved = (await d.set("red")) === undefined;
});
D("inert when the probe fails: writes nothing", "led.test.ts:342", async (tr) => {
  const d = driverFor(tr, { probeOk: false });
  const init = await d.init();
  tr.extra.init = init;
  tr.extra.available = d.available;
  await d.set("red");
  const p = d.play("white", "flash2");
  await vi.runAllTimersAsync();
  await p;
  await d.blank();
});
D("logs the resolved map exactly once", "led.test.ts:362", async (tr) => {
  const d = driverFor(tr);
  tr.extra.init1 = await d.init();
  tr.extra.init2 = await d.init();
});

// ---------------- LedManager (led-manager.test.ts) ----------------
const RECORDING: RecordingStatus = {
  enabled: true,
  sessionId: "s1",
  backlogBytes: 0,
  backlogSegments: 0,
  droppedSegments: 0,
  uploading: false,
  lastUploadAt: null,
};
function healthy(over: Partial<LedSignals> = {}): LedSignals {
  return {
    connection: "connected",
    recording: { ...RECORDING },
    lte: { status: "connected" },
    gnss: { fix: true, sky: "open" },
    starting: false,
    ...over,
  };
}
function harness(tr: Trace, signals: () => LedSignals, ok = true, readLte?: () => Promise<{ status?: string } | null>) {
  const driver = new LedDriver({
    log: () => {},
    probe: async () => (ok ? { ok: true } : { ok: false, reason: "not a Raspberry Pi" }),
    write: writer(tr),
  });
  let signalCalls = 0;
  let lteReads = 0;
  const led = new LedManager({
    signals: () => {
      signalCalls++;
      const { lte: _ignored, ...rest } = signals();
      return rest;
    },
    readLte: async () => {
      lteReads++;
      return (readLte ?? (async () => signals().lte))();
    },
    driver,
    log: (m) => tr.logs.push({ t: now(), msg: m }),
  });
  return { led, counts: () => ({ signalCalls, lteReads }) };
}
const settle = (ms = 20) => adv(ms);

const managerCases: { name: string; ts: string; run: (tr: Trace) => Promise<void> }[] = [];
const M = (name: string, ts: string, run: (tr: Trace) => Promise<void>) => managerCases.push({ name, ts, run });

M("shows the starting rung as soon as it takes over", "led-manager.test.ts:73", async (tr) => {
  const { led } = harness(tr, () => healthy());
  await led.start();
  await settle();
});
M("re-renders only when the resolved state actually changes", "led-manager.test.ts:80", async (tr) => {
  let signals = healthy();
  const { led } = harness(tr, () => signals);
  await led.start();
  await settle();
  led.tick();
  await settle();
  tr.marks.afterFirst = tr.calls.length;
  led.tick();
  led.tick();
  led.tick();
  tr.marks.afterThree = tr.calls.length;
  signals = healthy({ connection: "closed" });
  led.tick();
  await settle();
});
M("drives the colour the ladder picked for the signals", "led-manager.test.ts:105", async (tr) => {
  let signals = healthy({ starting: false, gnss: { fix: false, sky: "open" } });
  const { led } = harness(tr, () => signals);
  await led.start();
  await settle();
  tr.marks.cleared1 = tr.calls.length;
  led.tick();
  await settle();
  signals = healthy({ recording: { ...RECORDING, liveBlock: { reason: "thermal", detail: "", since: 0 } } });
  tr.marks.cleared2 = tr.calls.length;
  led.tick();
  await settle();
});
M("flashes white for an applied config, then returns to the real state", "led-manager.test.ts:124", async (tr) => {
  const { led } = harness(tr, () => healthy());
  await led.start();
  await settle();
  led.tick();
  await settle();
  tr.marks.cleared = tr.calls.length;
  led.configApplied();
  await settle(40);
  tr.marks.at40 = tr.calls.length;
  await settle(700);
});
M("ignores ticks while the overlay is running", "led-manager.test.ts:144", async (tr) => {
  let signals = healthy();
  const { led } = harness(tr, () => signals);
  await led.start();
  await settle();
  led.configApplied();
  signals = healthy({ connection: "closed" });
  led.tick();
  await settle(40);
  await settle(700);
});
M("holds its current state when signal gathering throws", "led-manager.test.ts:158", async (tr) => {
  let boom = false;
  const { led } = harness(tr, () => {
    if (boom) throw new Error("collector exploded");
    return healthy();
  });
  await led.start();
  led.tick();
  await settle();
  boom = true;
  let threw = false;
  try {
    led.tick();
  } catch {
    threw = true;
  }
  tr.extra.threw = threw;
});
M("blanks on stop and ignores everything afterwards", "led-manager.test.ts:172", async (tr) => {
  const { led } = harness(tr, () => healthy());
  await led.start();
  await settle();
  tr.marks.cleared1 = tr.calls.length;
  led.stop();
  await settle();
  tr.marks.cleared2 = tr.calls.length;
  led.tick();
  led.configApplied();
  await settle(40);
});
M("is a total no-op when the driver is inert", "led-manager.test.ts:188", async (tr) => {
  const h = harness(tr, () => healthy(), false);
  await h.led.start();
  h.led.tick();
  h.led.configApplied();
  h.led.stop();
  await settle(40);
  tr.extra.available = h.led.available;
});
M("reads LTE status for itself, not via the publish loop", "led-manager.test.ts:200", async (tr) => {
  let reads = 0;
  const h = harness(tr, () => healthy({ connection: "closed" }), true, async () => {
    reads++;
    return { status: "dead_zone" };
  });
  await h.led.start();
  h.led.tick();
  await settle(30);
  tr.extra.readsAfterFirst = reads;
  await settle(1100);
  h.led.tick();
  await settle(30);
  h.led.tick();
  await settle(30);
  tr.extra.readsEnd = reads;
});
M("a dead zone with the cloud down stays green, not purple", "led-manager.test.ts:229", async (tr) => {
  const { led } = harness(tr, () => healthy({ connection: "closed" }), true, async () => ({ status: "dead_zone" }));
  await led.start();
  led.tick();
  await settle(30);
  led.tick();
  await settle(30);
});
M("an unreadable LTE file fails open", "led-manager.test.ts:242", async (tr) => {
  const { led } = harness(tr, () => healthy(), true, async () => {
    throw new Error("ENOENT");
  });
  await led.start();
  led.tick();
  await settle(30);
  led.tick();
  await settle(30);
});
// ---- Phase 5b WP3 (@ 09da38d): the 89b5c64 it "never calls the signal
// function when inert" was REPLACED by these five (led-manager.test.ts:254-321).
// Run at 89b5c64 they throw (no telemetry()): dump them at 09da38d only.
const P5B = typeof (LedManager.prototype as unknown as { telemetry?: unknown }).telemetry === "function";
if (!P5B) {
  M("never calls the signal function when inert", "led-manager.test.ts:254", async (tr) => {
    const h = harness(tr, () => healthy(), false);
    await h.led.start();
    h.led.tick();
    tr.extra.counts = h.counts();
  });
} else {
  type Tel = { telemetry(): unknown; snapshot(): { state: { reason: string } } | null };
  const tel = (l: unknown) => (l as Tel).telemetry();
  const reason = (l: unknown) => (l as Tel).snapshot()?.state.reason ?? null;
  M("still resolves the state when inert, for telemetry — but never touches a pin", "led-manager.test.ts:254", async (tr) => {
    let signals = healthy({ gnss: { fix: false, sky: "open" } as never });
    const h = harness(tr, () => signals, false);
    await h.led.start();
    tr.extra.before = tel(h.led);
    h.led.tick();
    await settle();
    tr.extra.reason1 = reason(h.led);
    tr.extra.t1 = tel(h.led);
    signals = healthy({ connection: "closed" });
    h.led.tick();
    await settle();
    tr.extra.t2 = tel(h.led);
    tr.extra.counts = h.counts();
  });
  M("telemetry carries the LTE status the manager read for itself", "led-manager.test.ts:273", async (tr) => {
    const h = harness(tr, () => healthy(), false, async () => ({ status: "disconnected" }));
    await h.led.start();
    h.led.tick();
    await settle(30);
    h.led.tick();
    tr.extra.t = tel(h.led);
  });
  M("the config-applied overlay never shows in telemetry; the state underneath does", "led-manager.test.ts:282", async (tr) => {
    let signals = healthy();
    const h = harness(tr, () => signals);
    await h.led.start();
    await settle();
    h.led.tick();
    tr.extra.t0 = tel(h.led);
    h.led.configApplied();
    signals = healthy({ connection: "closed" });
    h.led.tick();
    tr.extra.reason1 = reason(h.led);
    tr.extra.t1 = tel(h.led);
    await settle(700);
  });
  M("holds the last resolved telemetry when signal gathering throws", "led-manager.test.ts:299", async (tr) => {
    let boom = false;
    const h = harness(tr, () => {
      if (boom) throw new Error("collector exploded");
      return healthy({ gnss: { fix: false, sky: "open" } as never });
    }, false);
    await h.led.start();
    h.led.tick();
    boom = true;
    h.led.tick();
    tr.extra.t = tel(h.led);
  });
  M("stops resolving after stop()", "led-manager.test.ts:312", async (tr) => {
    const h = harness(tr, () => healthy(), false);
    await h.led.start();
    h.led.stop();
    h.led.tick();
    tr.extra.counts = h.counts();
    tr.extra.t = tel(h.led);
  });
}

const out: { driver: unknown[]; manager: unknown[] } = { driver: [], manager: [] };

it("dumps", async () => {
  vi.useFakeTimers();
  for (const [list, dest] of [
    [driverCases, out.driver],
    [managerCases, out.manager],
  ] as const) {
    for (const c of list) {
      const tr = newTrace();
      T0 = Date.now();
      await c.run(tr);
      // Let anything still queued land (repeating patterns are stopped or the
      // manager is left running; cap the drain so nothing loops forever).
      tr.extra.endMs = now();
      dest.push({ name: c.name, ts: c.ts, expected: tr });
      vi.clearAllTimers();
    }
  }
  vi.useRealTimers();
});

afterAll(() => {
  const head = (file: string) => ({
    jsSourceFile: file,
    jsCommit: process.env.JS_COMMIT_FULL ?? process.env.JS_COMMIT ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
    generatedBy:
      "plans/device-agent-js-to-rust/phase-4/evidence/wp1-tools/zz-wp1-dump-led-traces.test.ts (vitest fake timers) in a git-archive scratch copy of 89b5c64: each it() re-run step by step against the REAL LedDriver/LedManager with a recording writer; setTimeout/settle waits → advanceTimersByTimeAsync; trace = writer calls {t,pins,high} + log lines {t,msg} in virtual ms",
  });
  writeFileSync("test/zz-wp1-led-driver.json", JSON.stringify({ ...head("test/led.test.ts"), cases: out.driver }, null, 1) + "\n");
  writeFileSync("test/zz-wp1-led-manager.json", JSON.stringify({ ...head("test/led-manager.test.ts"), cases: out.manager }, null, 1) + "\n");
});
