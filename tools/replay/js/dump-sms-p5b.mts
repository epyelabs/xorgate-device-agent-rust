// Phase 5b WP3 — re-dump `fixtures/sms/sms-modem.json` from the REAL
// `src/hardware/sms-modem.ts` (pure subset) at the JS ref of the scratch copy
// it runs in, and append the 09da38d clear cases (`action`, `cleared`, the
// sim fixture inbox read/clear sequence, SMS_DELETE_TIMEOUT_MS).
//
// The Phase 1 generator of that fixture was never committed; this driver
// takes every existing case's INPUT from the committed fixture (argv[2]) and
// re-runs it, so the old cases' expectations are re-derived, not copied.
// Fidelity check: run at 89b5c64 the output's old cases are byte-identical
// to the committed 89b5c64 fixture (only the new cases fail to run there).
//
//   docker run --rm --platform linux/arm64 -e TZ=UTC -v $RIG_JS_CACHE/js-09da38d:/js \
//     -v $PWD/fixtures:/fx:ro -w /js node:20-bookworm \
//     npx --no-install tsx .rig-drivers/dump-sms-p5b.mts /fx/sms/sms-modem.json > fixtures/sms/sms-modem.json
import { readFileSync } from "node:fs";
import * as sms from "../src/hardware/sms-modem.js";
import { SMS_TEXT_MAX } from "../src/hardware/sms-pdu.js";

const JS_COMMIT = process.env.JS_COMMIT ?? "09da38d";
const old = JSON.parse(readFileSync(process.argv[2]!, "utf8")) as {
  cases: { name: string; op: string; input: any; expectedJson: string }[];
};

function decode(v: any): unknown {
  if (v && typeof v === "object" && !Array.isArray(v)) {
    if (v.$undefined === true) return undefined;
    if (v.$nan === true) return NaN;
    if (typeof v.$inf === "number") return v.$inf < 0 ? -Infinity : Infinity;
  }
  return v;
}

function run(op: string, input: any): string {
  switch (op) {
    case "parseCpmsQuery":
      return JSON.stringify(sms.parseCpmsQuery(input));
    case "clampSmsLimit":
      return JSON.stringify(sms.clampSmsLimit(decode(input)));
    case "fixtureSmsSweep":
      return JSON.stringify(sms.fixtureSmsSweep(input));
    case "buildSmsPayload":
      return JSON.stringify(sms.buildSmsPayload(input.sweep, input.req));
    case "buildSmsPayloadCap": {
      const sweep = { ...input.sweep };
      const msg = { ...input.messageTemplate, text: String(input.messageTemplate.text).repeat(input.textRepeat) };
      sweep.messages = Array.from({ length: input.messageRepeat }, () => ({ ...msg }));
      const p = sms.buildSmsPayload(sweep, input.req);
      const bytes = Buffer.byteLength(JSON.stringify(p));
      const uncapped = { ...p, partial: false, messages: sweep.messages };
      return JSON.stringify({
        bytes,
        messagesLength: p.messages.length,
        partial: p.partial,
        total: p.total,
        uncappedBytes: Buffer.byteLength(JSON.stringify(uncapped)),
      });
    }
    // ---- 09da38d additions ----
    case "fixtureSmsRead": {
      // input: {now, steps: ("read"|"clear"|"reset")[]} — the module flag
      // carried across the steps, reset before the case.
      (sms as any).resetFixtureSmsInbox();
      const out: unknown[] = [];
      for (const s of input.steps as string[]) {
        if (s === "read") out.push({ step: s, sweep: (sms as any).fixtureSmsRead(input.now) });
        else if (s === "clear") out.push({ step: s, sweep: (sms as any).fixtureSmsClear(input.now) });
        else {
          (sms as any).resetFixtureSmsInbox();
          out.push({ step: s });
        }
      }
      (sms as any).resetFixtureSmsInbox();
      return JSON.stringify(out);
    }
    default:
      throw new Error(`unknown op ${op}`);
  }
}

const cases = old.cases
  .filter((c) => !c.name.startsWith("p5b: "))
  .map((c) => ({ name: c.name, op: c.op, input: c.input, expectedJson: run(c.op, c.input) }));

// ---- the 09da38d additions (sms-modem.test.ts @ 09da38d :448-466, :526-531 + extras) ----
const NOW = Date.UTC(2026, 9, 1);
const fixture = sms.fixtureSmsSweep(NOW);
const failed = { ...fixture, status: "error", error: "delete ME failed (+CMS ERROR: 321)", messages: [] };
const added: { name: string; op: string; input: unknown }[] = [
  { name: "p5b: a read reply carries action read and no cleared", op: "buildSmsPayload", input: { sweep: fixture, req: { deviceId: "d", requestId: "r", limit: 2 } } },
  { name: "p5b: explicit action read", op: "buildSmsPayload", input: { sweep: fixture, req: { deviceId: "d", requestId: "r", limit: 2, action: "read" } } },
  { name: "p5b: a read never carries cleared even if the sweep has it", op: "buildSmsPayload", input: { sweep: { ...fixture, cleared: { SM: 1 } }, req: { deviceId: "d", requestId: "r", limit: 10, action: "read" } } },
  {
    name: "p5b: clear ok: cleared last, post-clear storage",
    op: "buildSmsPayload",
    input: {
      sweep: { ...fixture, storage: { SM: { used: 0, total: 40 }, ME: { used: 0, total: 23 } }, messages: [], cleared: { SM: 7, ME: 23 } },
      req: { deviceId: "d", requestId: "r", limit: 10, action: "clear" },
    },
  },
  { name: "p5b: clear with no cleared on the sweep → {}", op: "buildSmsPayload", input: { sweep: fixture, req: { deviceId: "d", requestId: "r", limit: 1, action: "clear" } } },
  { name: "p5b: clear error keeps cleared and storage, drops messages", op: "buildSmsPayload", input: { sweep: { ...failed, cleared: { SM: 7, ME: 23 } }, req: { deviceId: "d", requestId: "r", limit: 10, action: "clear" } } },
  { name: "p5b: clear no_sim with empty cleared", op: "buildSmsPayload", input: { sweep: { ...failed, status: "no_sim", error: "sim not ready", storage: {}, cleared: {} }, req: { deviceId: "d", requestId: "r", limit: 10, action: "clear" } } },
  { name: "p5b: clear ME-only cleared order", op: "buildSmsPayload", input: { sweep: { ...fixture, messages: [], cleared: { ME: 3 } }, req: { deviceId: "d", requestId: "r", limit: 10, action: "clear" } } },
  { name: "p5b: a clear reports the fixture counts, then reads come back empty", op: "fixtureSmsRead", input: { now: 0, steps: ["read", "clear", "read", "clear", "reset", "read"] } },
  { name: "p5b: fixture inbox at a real instant", op: "fixtureSmsRead", input: { now: NOW, steps: ["clear", "read"] } },
];
if (!process.env.ONLY_OLD) for (const a of added) cases.push({ ...a, expectedJson: run(a.op, a.input) });

process.stdout.write(
  JSON.stringify(
    {
      jsSourceFile: "src/hardware/sms-modem.ts (pure subset: parseCpmsQuery, clampSmsLimit, buildSmsPayload, fixtureSmsSweep/Read/Clear)",
      jsCommit: JS_COMMIT,
      generatedBy:
        "tools/replay/js/dump-sms-p5b.mts <old fixture> (phase-5b WP3): every case's input re-run through the REAL TS; the Phase 1 cases keep their inputs, the p5b: cases are new",
      SMS_PAYLOAD_MAX_BYTES: sms.SMS_PAYLOAD_MAX_BYTES,
      SMS_LIMIT_DEFAULT: sms.SMS_LIMIT_DEFAULT,
      SMS_LIMIT_MAX: sms.SMS_LIMIT_MAX,
      SMS_TEXT_MAX,
      SMS_DELETE_TIMEOUT_MS: (sms as any).SMS_DELETE_TIMEOUT_MS ?? null,
      cases,
    },
    null,
    2,
  ) + "\n",
);
