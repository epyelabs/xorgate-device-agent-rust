// Phase 4 WP2 — the TS side of the pty end-to-end SMS test (E5): the REAL
// `SmsReader` @ 89b5c64 with its default transport (`stat` → `stty -F <p> raw
// -echo min 0 time 1` → `open(O_RDWR|O_NOCTTY)`) on `XORGATE_SMS_PORT` (the
// pty slave the Rust fake modem serves), a FIXED clock so `ts`/`durationMs`
// are deterministic, then the `xorgate sms --json` text
// (`JSON.stringify(buildSmsPayload(…, {deviceId:"local", requestId:"cli"}), null, 2)`).
//
//   XORGATE_SMS_PORT=/dev/pts/N npx tsx sms-pty-read.mts [limit] [fixedNowMs]
//
// Driven by crates/xorgate-hal/tests/at_port_pty.rs when XORGATE_SMS_PTY_JS
// is set (see that file); WP6's rig can run it against its fake-modem shim.
import { buildSmsPayload, clampSmsLimit, SmsReader } from "../src/hardware/sms-modem.js";

const limit = clampSmsLimit(process.argv[2] === undefined ? undefined : Number(process.argv[2]));
const fixed = Number(process.argv[3] ?? 1_760_000_000_000);
const reader = new SmsReader({ lockPath: null, now: () => fixed });
const sweep = await reader.read();
const payload = buildSmsPayload(sweep, { deviceId: "local", requestId: "cli", limit });
console.log(JSON.stringify(payload, null, 2));
process.exit(payload.status === "ok" ? 0 : 1);
