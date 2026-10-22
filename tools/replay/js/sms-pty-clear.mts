// Phase 5b WP3 — the TS side of the pty end-to-end SMS CLEAR test (H11): the
// REAL `SmsReader.clear()` @ 09da38d with its default transport (`stat` →
// `stty -F <p> raw -echo min 0 time 1` → `open(O_RDWR|O_NOCTTY)`) on
// `XORGATE_SMS_PORT` (the pty slave the Rust fake modem serves, `allowClear`
// set), a FIXED clock so `ts`/`durationMs` are deterministic, then
// `JSON.stringify(buildSmsPayload(…, {deviceId:"local", requestId:"cli",
// limit, action:"clear"}), null, 2)` and the `xorgate sms clear` line.
//
//   XORGATE_SMS_PORT=/dev/pts/N npx tsx sms-pty-clear.mts [limit] [fixedNowMs]
//
// Driven by crates/xorgate-agent/tests/sms_clear_pty.rs when
// XORGATE_SMS_PTY_CLEAR_JS is set (see that file).
import { buildSmsPayload, clampSmsLimit, SmsReader } from "../src/hardware/sms-modem.js";

const limit = clampSmsLimit(process.argv[2] === undefined ? undefined : Number(process.argv[2]));
const fixed = Number(process.argv[3] ?? 1_760_000_000_000);
const reader = new SmsReader({ lockPath: null, now: () => fixed });
const sweep = await reader.clear();
const payload = buildSmsPayload(sweep, { deviceId: "local", requestId: "cli", limit, action: "clear" });
console.log(JSON.stringify(payload, null, 2));
// A read right after, on the same reader: the cached read was dropped, so it
// sweeps the (now empty) stores afresh over the port.
const after = await reader.read();
console.log(JSON.stringify(buildSmsPayload(after, { deviceId: "local", requestId: "cli", limit }), null, 2));
process.exit(payload.status === "ok" ? 0 : 1);
