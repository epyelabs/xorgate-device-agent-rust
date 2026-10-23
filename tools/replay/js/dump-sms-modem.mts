// Phase 4 (WP6): the FakeModem store contents of the `p4-sms` synthetic tape
// (`meta.rig.modem`, crates/xorgate-replay/src/p4.rs), built with the TS test's
// own PDU builder @89b5c64 so no PDU is hand-typed:
//   tools/replay/js/run-tsx.sh tools/replay/js/dump-sms-modem.mts > fixtures/replay/p4-sms-modem.json
// Shape = the TS `FakeOpts` ({cmgf, mem1, stores}): the bench shape of
// test/sms-modem.test.ts `benchStores()` (SM 7/40 unread, ME 23/23 read), plus
// in SM a two-part concatenated UCS2 message and one stored SUBMIT (skipped),
// starting in TEXT mode (cmgf 1) so the sweep switches to PDU and restores.
import { buildDeliver, buildSubmit, scts } from "../test/sms-pdu-builder.js";

const INTL = { kind: "international", value: "15550100001" } as const;
const slot = (text: string, day: number, stat: number) => {
  const { pdu, length } = buildDeliver({ from: INTL, text, scts: scts(2026, 8, day, 12, 0, 0, -16) });
  return { stat, length, pdu };
};
const sm: Record<number, { stat: number; length: number; pdu: string }> = {};
for (let i = 0; i < 7; i++) sm[i] = slot(`sm${i}`, 20 + i, 0);
const part = (seq: number, ucs2: string) => {
  const { pdu, length } = buildDeliver({
    from: { kind: "alphanumeric", value: "Carrier" },
    ucs2,
    scts: scts(2026, 8, 28, 9, 30, seq, -16),
    concat: { ref: 0x2a, total: 2, seq },
  });
  return { stat: 0, length, pdu };
};
sm[7] = part(1, "Ваш тарифный план ");
sm[8] = part(2, "обновится завтра ✓");
const s = buildSubmit();
sm[9] = { stat: 3, length: s.length, pdu: s.pdu };
const me: Record<number, { stat: number; length: number; pdu: string }> = {};
for (let i = 0; i < 23; i++) me[i] = slot(`me${i}`, 1 + (i % 15), 1);
const out = {
  jsSourceFile: "test/sms-modem.test.ts + test/sms-pdu-builder.ts",
  jsCommit: (process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d").slice(0, 7),
  generatedBy: "tools/replay/js/dump-sms-modem.mts",
  modem: { cmgf: 1, mem1: "SR", stores: { SM: { total: 40, slots: sm }, ME: { total: 23, slots: me } } },
};
process.stdout.write(JSON.stringify(out, null, 2) + "\n");
