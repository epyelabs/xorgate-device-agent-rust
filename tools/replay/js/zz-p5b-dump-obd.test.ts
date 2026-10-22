// Phase 5b WP2 dump driver (OBD). Runs against the REAL TS @ 09da38d:
//  - every pure helper of src/hardware/obd.ts, src/agent/collectors/obd.ts,
//    src/commands/obd.ts and src/commands/status.ts (`obdStatusLine`) on the
//    test/obd.test.ts + test/obd-cli.test.ts vectors plus extra edge vectors;
//  - every `createObdCollector` it() of test/obd.test.ts (27) as a STEP SCRIPT
//    (read / sample / feed / exit / advance / set / reapply / scan / forget /
//    stop) over the test's own fake spawn (`fakeHelper`) and vitest fake timers,
//    recording after every step: the virtual time, the step's output, the
//    helper runs `{cfg, stopped}`, the settings-read count, the NEW activity
//    events and console lines, `status()` and `report()`;
//  - the mock helper stream (`mockObdStream`) under fake timers.
// The Rust tests (crates/xorgate-core/tests/obd_fixtures.rs) replay the same
// steps on `obd_supervisor` and compare every observation exactly.
//
// Run (the shared scratch copy ONLY; never the JS repo):
//   cp zz-p5b-dump-obd.test.ts $RIG_JS_CACHE/js-09da38d/test/
//   docker run --rm --platform linux/arm64 -e TZ=UTC -v $RIG_JS_CACHE/js-09da38d:/js -w /js \
//     node:20-bookworm npx --no-install vitest run test/zz-p5b-dump-obd.test.ts
// Output: test/zz-p5b-obd-{pure,traces,mock}.json → fixtures/obd/{pure,supervisor-traces,mock-stream}.json
import { writeFileSync } from "node:fs";
import { afterAll, it, vi } from "vitest";
import {
  buildObdHelperArgs,
  isObdConfigured,
  mockObdStream,
  normalizeScanCandidates,
  parseObdLine,
  readObdEnvConfig,
  resolveObdConfig,
  type ObdHelperConfig,
  type ObdMessage,
  type ObdSpawn,
  type ObdStreamHandlers,
} from "../src/hardware/obd.js";
import {
  auditLine,
  boundInventory,
  createObdCollector,
  obdReportDue,
  profileSummary,
  requestRate,
} from "../src/agent/collectors/obd.js";
import { obdStatusLine } from "../src/commands/status.js";
import { formatScanTable, parseObdArgs } from "../src/commands/obd.js";
import type { ObdSettings } from "../src/daemon/settings.js";
import type { ObdStatus } from "../src/agent/telemetry.js";
import type { RunResult } from "../src/lib/exec.js";

const JS = { jsCommit: "09da38d" };
const T0 = Date.parse("2026-10-13T12:00:00Z");

// --------------------------------------------------------------------------
// vectors (test/obd.test.ts LINES + extras)
// --------------------------------------------------------------------------
const LINES = {
  ready: '{"type":"ready","helper":"obd_read","version":"0.1.0","allowlist_sha256":"abc123"}',
  statusLive: '{"type":"status","state":"live","link":2,"detail":"7E8 answering","ts":1760000000}',
  statusSilent: '{"type":"status","state":"ecu_silent","link":1,"detail":"3 misses","ts":1760000100}',
  profile:
    '{"type":"profile","ts":1760000001,"adapter":{"mac":"AA:BB:CC:DD:EE:FF","ati":"ELM327 v1.5","at1":"OBDII to RS232"},' +
    '"protocol":{"dpn":"A6","dp":"AUTO, ISO 15765-4 (CAN 11/500)"},"responders":["7E8","7E9"],' +
    '"pids_by_ecu":{"7E8":["04","05","0C"]},"supported_fields":["rpm","speed"],"unsupported_fields":["fuel_rate"],' +
    '"powertrain":"ice","rate_req_s":28.1}',
  sample:
    '{"type":"sample","ts":1760000002,"fields":{"rpm":926,"speed":0,"coolant_temp":88,"fuel_level":61.5,"module_voltage":14.12,"odometer":48213.4,"mil":0,"link":2}}',
  audit:
    '{"type":"audit","ts":1760000060,"counters":{"requests":600,"ok":598,"no_data":2,"timeouts":0,"err_adapter":0,"err_protocol":0,' +
    '"err_identity":0,"err_voltage":0,"reconnects":1,"forbidden_attempts":0,"budget_violations":0,"bytes_tx":3600},' +
    '"inventory":{"010C1":100,"010D1":100},"last_stop":{"ts":1760000030,"reason":"BUS ERROR"}}',
  error: '{"type":"error","code":6,"error":"forbidden command ATSH7E0"}',
};

const PARSE_LINES: string[] = [
  ...Object.values(LINES),
  '{"type":"sample","fields":{"rpm":900,"vin":"WVG","gear":null,"speed":"12","nan":null,"link":2}}',
  '{"type":"audit","ts":1,"counters":{"requests":1},"last_stop":null}',
  "Traceback (most recent call last):",
  '{"type":"sample","fields":{"rpm":9',
  '{"type":"telemetry_v2"}',
  '{"type":"status","state":"flying","link":2}',
  '{"type":"status","state":"live","link":3}',
  '{"type":"sample"}',
  '{"type":"audit"}',
  '{"type":"error","error":"no code"}',
  "",
  "[1,2]",
  // extras: whitespace, key order (V8 puts array-index keys first), types
  '   {"type":"status","state":"backoff","link":0,"detail":7,"ts":"x"}  ',
  '{"type":"status","state":"live","link":2.0}',
  '{"type":"status","state":"live","link":"2"}',
  '{"type":"status","link":1,"state":"probing","extra":true}',
  '{"type":"ready"}',
  '{"type":"ready","helper":1,"version":"0.2.0","allowlist_sha256":null}',
  '{"type":"profile"}',
  '{"type":"profile","rate_req_s":5,"10":"x","2":"y","zeta":{"b":1,"1":2},"type2":[1,{"3":0,"a":1}]}',
  '{"type":"sample","fields":{"b":1,"10":2,"a":-0.5,"2":3e3,"big":1.5e300,"neg":-12}}',
  '{"type":"sample","fields":[]}',
  '{"type":"sample","fields":{},"ts":12.5}',
  '{"type":"audit","counters":{"requests":"1","ok":2},"inventory":{"z":1,"010C1":"x","5":2},"last_stop":{"reason":5,"ts":3}}',
  '{"type":"audit","counters":{},"inventory":[1],"last_stop":"BUS"}',
  '{"type":"audit","counters":[],"inventory":{}}',
  '{"type":"error","code":4.5}',
  '{"type":"error","code":"6","error":"x"}',
  '{"type":"error","code":2,"error":null}',
  '{"type":null}',
  "null",
  "{}",
  '{"type":"sample","fields":{"rpm":1},"type":"ready"}',
  "\t{\"type\":\"status\",\"state\":\"ecu_silent\",\"link\":1}\r",
];

const ENV_CASES: NodeJS.ProcessEnv[] = [
  {},
  {
    XORGATE_OBD_ADAPTER: "aa:bb:cc:dd:ee:ff",
    XORGATE_OBD_PIN: "0000",
    XORGATE_OBD_POLL_HZ: "2",
    XORGATE_OBD_FIELDS: "rpm, speed,coolant_temp",
    XORGATE_OBD_POWER_SOURCE: "constant",
  },
  {
    XORGATE_OBD_ADAPTER: "not-a-mac",
    XORGATE_OBD_SERIAL: "/dev/pts/7",
    XORGATE_OBD_POLL_HZ: "500",
    XORGATE_OBD_POWER_SOURCE: "solar",
  },
  { XORGATE_OBD_ADAPTER: "  10:21:3e:4f:33:65  ", XORGATE_OBD_POLL_HZ: "0.5" },
  { XORGATE_OBD_ADAPTER: "10-21-3E-4F-33-65" },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: "20", XORGATE_OBD_PIN: "  " },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: "20.0001" },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: "0" },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: "-1" },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: "1e1" },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: "0x2" },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: " 3 " },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: "abc" },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POLL_HZ: "Infinity" },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_FIELDS: ",,RPM, rpm ,_x1,9bad,a-b,speed," },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_FIELDS: " , " },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POWER_SOURCE: " constant " },
  { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_POWER_SOURCE: "Constant" },
  { XORGATE_OBD_SERIAL: "  " },
  { XORGATE_OBD_SERIAL: " /dev/pts/3 ", XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_PIN: " 4321 " },
];

const RESOLVE_CASES: { settings: ObdSettings | undefined; env: NodeJS.ProcessEnv }[] = [
  { settings: undefined, env: { XORGATE_OBD_ADAPTER: "aa:bb:cc:dd:ee:ff", XORGATE_OBD_POLL_HZ: "2" } },
  { settings: undefined, env: {} },
  ...(() => {
    const env = { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_PIN: "9999", XORGATE_OBD_POLL_HZ: "2" };
    return [
      { settings: { enabled: true, adapterMac: "10:21:3E:4F:33:65", powerSource: "switched" as const }, env },
      { settings: { enabled: false, adapterMac: "10:21:3E:4F:33:65" }, env },
      { settings: { enabled: true, adapterMac: null }, env },
      { settings: { enabled: true }, env },
      { settings: { enabled: true, adapterMac: "10:21:3E:4F:33:65", fields: ["rpm", "speed"] }, env },
      { settings: {}, env },
      { settings: { adapterMac: "10:21:3E:4F:33:65" }, env },
      {
        settings: { enabled: true, adapterMac: "10:21:3E:4F:33:65", pin: "123456", pollHz: 0.2, fields: [], powerSource: "constant" as const },
        env,
      },
    ];
  })(),
  { settings: undefined, env: { XORGATE_OBD_SERIAL: "/dev/pts/4" } },
  { settings: { enabled: true, adapterMac: null }, env: { XORGATE_OBD_SERIAL: "/dev/pts/4" } },
  { settings: { enabled: false, adapterMac: null }, env: { XORGATE_OBD_SERIAL: "/dev/pts/4" } },
  { settings: { enabled: true, adapterMac: "10:21:3E:4F:33:65" }, env: { XORGATE_OBD_SERIAL: "/dev/pts/4", XORGATE_OBD_PIN: "1111" } },
  { settings: undefined, env: { XORGATE_OBD_SERIAL: "/dev/pts/4", XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF", XORGATE_OBD_FIELDS: "rpm" } },
];

const SCAN_RAW: unknown[] = [
  [
    { mac: "10:21:3e:4f:33:65", name: "OBDII", rssi: -58 },
    { mac: "10:21:3E:4F:33:65", name: "OBDII dup", rssi: -10 },
    { mac: "6C:3B:00:00:00:01", name: "6C-3B-00-00-00-01", rssi: -40 },
    { mac: "6C:3B:00:00:00:02", name: "", rssi: -40 },
    { mac: "not a mac", name: "x", rssi: -1 },
    { mac: "00:1D:A5:68:98:8B", name: "V-LINK" },
    { mac: "AA:AA:AA:AA:AA:01", name: "TV", rssi: -70.4 },
    ...Array.from({ length: 25 }, (_, i) => ({
      mac: `BB:BB:BB:BB:BB:${(i + 10).toString(16).padStart(2, "0").toUpperCase()}`,
      name: `dev${i}`,
      rssi: -80 - i,
    })),
  ],
  "garbage",
  [{ mac: "00:1D:A5:68:98:8B", name: "V-LINK" }],
  null,
  { mac: "00:1D:A5:68:98:8B", name: "V-LINK" },
  [
    null,
    1,
    "x",
    { mac: 5, name: "n" },
    { mac: " 00:1d:a5:68:98:8b ", name: "  padded  ", rssi: -70.5 },
    { mac: "00:1D:A5:68:98:8C", name: "aa:bb", rssi: "-3" },
    { mac: "00:1D:A5:68:98:8D", name: "00:1d:a5:68:98:8d", rssi: -1 },
    { mac: "00:1D:A5:68:98:8E", name: "00-1d-a5-68-98-8e", rssi: -1 },
    { mac: "00:1D:A5:68:98:8F", name: 7, rssi: -1 },
    { mac: "00:1D:A5:68:98:90", name: "x".repeat(80), rssi: -60.5 },
    { mac: "00:1D:A5:68:98:91", name: "same", rssi: -60 },
    { mac: "00:1D:A5:68:98:89", name: "same2", rssi: -60 },
    { mac: "00:1D:A5:68:98:88", name: "nul" },
    { mac: "00:1D:A5:68:98:87", name: "nul2", rssi: null },
    { mac: "00:1D:A5:68:98:86", name: "inf", rssi: 1e400 },
  ],
  [{ mac: "00:1D:A5:68:98:8B", name: "", rssi: -1 }, { mac: "00:1D:A5:68:98:8B", name: "second", rssi: -2 }],
];

// --------------------------------------------------------------------------
// pure section
// --------------------------------------------------------------------------
function ser(v: unknown): unknown {
  return v === undefined ? "<undefined>" : JSON.parse(JSON.stringify(v));
}

function statusBase(extra: Partial<ObdStatus> = {}): ObdStatus {
  return {
    state: "live",
    link: 2,
    reqPerSec: 10,
    errors: 0,
    forbidden: 0,
    voltage: 14.12,
    detail: null,
    since: 0,
    helperVersion: "0.1.0",
    ...extra,
  };
}

function pure() {
  const parse = PARSE_LINES.map((line) => ({ line, out: ser(parseObdLine(line)) }));
  const env = ENV_CASES.map((e) => {
    const cfg = readObdEnvConfig(e);
    return { env: e, cfg, configured: isObdConfigured(cfg), argv: buildObdHelperArgs(cfg) };
  });
  const argv: { cfg: ObdHelperConfig; argv: string[] }[] = [
    { adapter: "AA:BB:CC:DD:EE:FF", serial: null, pin: "1234", pollHz: 1, fields: null, powerSource: "switched" },
    { adapter: "AA:BB:CC:DD:EE:FF", serial: "/dev/pts/1", pin: "1234", pollHz: 0.5, fields: ["rpm"], powerSource: "constant" },
    { adapter: null, serial: null, pin: "1234", pollHz: 0.2, fields: [], powerSource: "switched" },
    { adapter: "AA:BB:CC:DD:EE:FF", serial: null, pin: "000000", pollHz: 1.3333333333333333, fields: ["a", "b"], powerSource: "switched" },
  ].map((cfg) => ({ cfg: cfg as ObdHelperConfig, argv: buildObdHelperArgs(cfg as ObdHelperConfig) }));
  const resolve = RESOLVE_CASES.map((c) => ({
    settings: c.settings === undefined ? "<undefined>" : c.settings,
    env: c.env,
    out: ser(resolveObdConfig(c.settings, c.env)),
  }));
  const normalize = SCAN_RAW.map((raw) => ({ raw: ser(raw), out: normalizeScanCandidates(raw) }));
  const snap = { counters: { requests: 10, ok: 9, no_data: 1 }, inventory: { "010C1": 10 }, last_stop: null, ts: 1 };
  const auditLines = [
    { seq: 3, audit: snap, rate: 1.25 },
    {
      seq: 1,
      audit: { ...snap, counters: { err_voltage: 1, forbidden_attempts: 1, voltage_low_events: 3 }, last_stop: { reason: "voltage: x" } },
      rate: null,
    },
    { seq: 12, audit: { counters: { requests: 1.5, ok: 0.25, err_adapter: 2, err_protocol: 3, err_identity: 4, budget_violations: 7, reconnects: 9, timeouts: 1e21 }, inventory: {}, last_stop: { ts: 1 }, ts: 5 }, rate: 0.05 },
    { seq: 2, audit: snap, rate: 12.25 },
    { seq: 2, audit: snap, rate: 0 },
  ].map((c) => ({ ...c, out: auditLine(c.seq, c.audit as never, c.rate) }));
  const bound = [
    { inventory: { a: 1, b: 5, c: 3 }, max: 2 },
    { inventory: { x: 1 }, max: 10 },
    { inventory: { b: 1, a: 1, c: 1 }, max: 2 },
    { inventory: { "10": 1, "9": 1, z: 9, "010C1": 9 }, max: 3 },
    { inventory: { "10": 1, "9": 1, z: 9 }, max: 10 },
    { inventory: Object.fromEntries(Array.from({ length: 350 }, (_, i) => [`01${i.toString(16).padStart(2, "0")}1`, i])), max: 200 },
  ].map((c) => ({ ...c, out: boundInventory(c.inventory, c.max) }));
  const A = (requests: unknown, ts: number) => ({ counters: requests === undefined ? {} : { requests }, inventory: {}, last_stop: null, ts });
  const rate = [
    [A(10, 100), A(70, 106)],
    [null, A(70, 106)],
    [A(70, 106), A(10, 100)],
    [A(10, 100), A(70, 100)],
    [A(undefined, 100), A(70, 106)],
    [A(10, 100), A(undefined, 106)],
    [A(0, 1000), A(600, 1060)],
    [A(0, 0), A(1, 3)],
    [A(0, 0), A(5, 2)],
    [A(0, 0), A(1, 0.0001)],
    [A(0, 1), null],
  ].map(([p, c]) => ({ prev: p, cur: c, out: requestRate(p as never, c as never) }));
  const st = (link: 0 | 1 | 2, state = "live", extra: Partial<ObdStatus> = {}): ObdStatus =>
    ({ state, link, reqPerSec: null, errors: 0, forbidden: 0, voltage: null, detail: null, since: 0, helperVersion: null, ...extra }) as ObdStatus;
  const due: [ObdStatus | null, ObdStatus | null][] = [
    [null, null],
    [null, st(0, "connecting")],
    [st(0, "connecting"), null],
    [st(0, "connecting"), st(0, "adapter_not_found")],
    [st(0, "adapter_not_found"), st(0, "backoff")],
    [st(1, "probing"), st(2, "live")],
    [st(2, "live"), st(1, "ecu_silent")],
    [st(0, "retrying"), st(0, "parked")],
    [st(0, "parked"), st(0, "parked")],
    [st(0, "parked"), st(0, "starting")],
    [st(0, "retrying"), st(0, "exited")],
    ...[[0, 1], [1, 2], [9, 10], [10, 11], [19, 20], [5, 5], [3, 0], [20, 10], [0, 0]].map(
      ([a, b]) => [st(2, "live", { auditSeq: a }), st(2, "live", { auditSeq: b })] as [ObdStatus, ObdStatus],
    ),
    [st(2, "live"), st(2, "live")],
    [st(2, "live"), st(2, "live", { auditSeq: 1 })],
    [st(2, "live", { auditSeq: 3, errors: 0 }), st(2, "live", { auditSeq: 3, errors: 1 })],
    [st(2, "live", { auditSeq: 3, forbidden: 0 }), st(2, "live", { auditSeq: 3, forbidden: 1 })],
    [st(2, "live", { auditSeq: 3, voltageLowEvents: 0 }), st(2, "live", { auditSeq: 3, voltageLowEvents: 1 })],
    [st(2, "live", { auditSeq: 3 }), st(2, "live", { auditSeq: 3, voltageLowEvents: 0 })],
    [st(2, "live", { auditSeq: 3, lastStop: null }), st(2, "live", { auditSeq: 3, lastStop: "voltage: 11.5 V" })],
    [st(2, "live", { auditSeq: 3 }), st(2, "live", { auditSeq: 3, lastStop: null })],
    [st(2, "live", { auditSeq: 3, lastStop: "a" }), st(2, "live", { auditSeq: 3, lastStop: "a" })],
    [st(2, "live", { auditSeq: 3, reqPerSec: 17 }), st(2, "live", { auditSeq: 3, reqPerSec: 18 })],
    [st(2, "live", { auditSeq: 3, voltage: 12 }), st(2, "live", { auditSeq: 3, voltage: 9.8 })],
  ];
  const reportDue = due.map(([prev, next]) => ({ prev, next, out: obdReportDue(prev, next) }));
  const statusLine = [
    statusBase(),
    statusBase({ state: "connecting", link: 0, reqPerSec: null, voltage: null }),
    statusBase({ adapter: "10:21:3E:4F:33:65" }),
    statusBase({ state: "disabled", link: 0, reqPerSec: null, voltage: null, adapter: null }),
    statusBase({ reqPerSec: 0.25, voltage: 12.25, adapter: "" }),
    statusBase({ reqPerSec: 28.1, voltage: 9.95, errors: 12, forbidden: 3, adapter: "/dev/pts/4" }),
  ].map((s) => ({ status: s, out: obdStatusLine(s) }));
  const profiles = [
    null,
    {
      protocol: { dpn: "A6", dp: "AUTO, ISO 15765-4 (CAN 11/500)" },
      responders: ["7E8", "7E9"],
      supported_fields: ["rpm", "speed"],
      unsupported_fields: ["fuel_rate"],
      powertrain: "ice",
      adapter: { ati: "ELM327 v1.5" },
    },
    {},
    { protocol: { dpn: "A6" }, responders: [], adapter: { ati: "" } },
    { protocol: {}, supported_fields: [], unsupported_fields: ["a", "b"], powertrain: "ev" },
    JSON.parse(LINES.profile.replace('"type":"profile",', "")),
  ].map((p) => ({ profile: p, out: profileSummary(p as never) }));
  const obdArgs = [
    [],
    ["--help"],
    ["-h"],
    ["help"],
    ["status"],
    ["status", "extra"],
    ["scan"],
    ["scan", "--json", "--seconds", "10"],
    ["scan", "--seconds=120"],
    ["scan", "--seconds", "0"],
    ["scan", "--seconds", "x"],
    ["scan", "--seconds"],
    ["scan", "--seconds="],
    ["scan", "--seconds", "121"],
    ["scan", "--seconds", "1.5"],
    ["scan", "--seconds", " 7 "],
    ["scan", "--seconds", "1e1"],
    ["scan", "--seconds", "0x10"],
    ["scan", "--seconds=5", "--seconds", "9"],
    ["scan", "--seconds", "9", "--seconds=5"],
    ["scan", "--json"],
    ["scan", "extra"],
    ["pair", "10:21:3E:4F:33:65"],
    ["SCAN"],
  ].map((args) => ({ args, out: parseObdArgs(args) }));
  const tables = [
    [
      { mac: "10:21:3E:4F:33:65", name: "OBDII", rssi: -58 },
      { mac: "00:1D:A5:68:98:8B", name: "V-LINK", rssi: null },
    ],
    [],
    [
      { mac: "10:21:3E:4F:33:65", name: "A", rssi: -5 },
      { mac: "10:21:3E:4F:33:66", name: "B", rssi: -100 },
      { mac: "10:21:3E:4F:33:67", name: "C", rssi: -10000 },
      { mac: "10:21:3E:4F:33:68", name: "D", rssi: 0 },
    ],
  ].map((c) => ({ candidates: c, out: formatScanTable(c) }));
  return {
    ...JS,
    jsSourceFile: ["src/hardware/obd.ts", "src/agent/collectors/obd.ts", "src/commands/obd.ts", "src/commands/status.ts"],
    parse,
    env,
    argv,
    resolve,
    normalize,
    auditLines,
    bound,
    rate,
    reportDue,
    statusLine,
    profiles,
    obdArgs,
    tables,
  };
}

// --------------------------------------------------------------------------
// supervisor traces
// --------------------------------------------------------------------------
type Step =
  | { op: "read" }
  | { op: "sample" }
  | { op: "feed"; line: string }
  | { op: "exit"; run: number; code: number | null }
  | { op: "advance"; ms: number }
  | { op: "set"; settings: ObdSettings | null }
  | { op: "reapply" }
  | { op: "scan" }
  | { op: "scanJoin" }
  | { op: "forget" }
  | { op: "stop" };

interface Case {
  name: string;
  ts: string;
  env: NodeJS.ProcessEnv;
  /** null = `settings: () => undefined` (no namespace). */
  settings: ObdSettings | null;
  scanner?: { candidates?: unknown[]; error?: string };
  unbond?: { error?: string };
  steps: Step[];
}

const ENV = { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" };
const MAC = "10:21:3E:4F:33:65";
const ENABLED: ObdSettings = { enabled: true, adapterMac: MAC, powerSource: "switched" };
const read: Step = { op: "read" };
const sample: Step = { op: "sample" };
const feed = (line: string): Step => ({ op: "feed", line });
const exit = (run: number, code: number | null): Step => ({ op: "exit", run, code });
const adv = (ms: number): Step => ({ op: "advance", ms });
const set = (settings: ObdSettings | null): Step => ({ op: "set", settings });
const reapply: Step = { op: "reapply" };
const repeat = (n: number, steps: Step[]): Step[] => Array.from({ length: n }, () => steps).flat();
const cycle = (ts: number): Step[] => [
  feed(`{"type":"status","state":"connecting","link":0,"ts":${ts}}`),
  feed(`{"type":"status","state":"adapter_not_found","link":0,"detail":"Host is down","ts":${ts}}`),
  feed(`{"type":"status","state":"backoff","link":0,"ts":${ts}}`),
];
const inventory350: Record<string, number> = {};
for (let i = 0; i < 350; i++) inventory350[`01${i.toString(16).padStart(2, "0")}1`] = i;

const CASES: Case[] = [
  { name: "no adapter: link 0 only, warned once", ts: "obd.test.ts:217", env: {}, settings: null, steps: [read, adv(5_000), read] },
  {
    name: "fresh sample → numeric leaves with units",
    ts: "obd.test.ts:237",
    env: ENV,
    settings: null,
    steps: [
      read,
      feed(LINES.ready),
      feed('{"type":"status","state":"connecting","link":0,"ts":1}'),
      read,
      feed('{"type":"status","state":"probing","link":1,"ts":2}'),
      read,
      feed(LINES.profile),
      feed(LINES.statusLive),
      feed(LINES.sample),
      read,
    ],
  },
  {
    name: "torque fields",
    ts: "obd.test.ts:272",
    env: ENV,
    settings: null,
    steps: [
      read,
      feed(LINES.ready),
      feed(LINES.profile),
      feed(LINES.statusLive),
      feed('{"type":"sample","ts":10,"fields":{"engine_torque_pct":-5,"reference_torque":280,"engine_torque":-14,"link":2}}'),
      read,
    ],
  },
  {
    name: "stale sample dropped, link kept",
    ts: "obd.test.ts:290",
    env: ENV,
    settings: null,
    steps: [read, feed(LINES.statusLive), feed(LINES.sample), adv(4_999), read, adv(2), read, feed(LINES.sample), read],
  },
  {
    name: "sample() freshness stamp",
    ts: "obd.test.ts:305",
    env: ENV,
    settings: null,
    steps: [read, sample, feed(LINES.statusLive), feed(LINES.sample), sample, sample, feed(LINES.sample), sample, adv(6_000), sample, feed('{"type":"status","state":"probing","link":1}'), sample, feed('{"type":"status","state":"live","link":1}'), sample],
  },
  {
    name: "events on the transitions that matter",
    ts: "obd.test.ts:321",
    env: ENV,
    settings: null,
    steps: [
      read,
      feed('{"type":"status","state":"connecting","link":0}'),
      feed('{"type":"status","state":"pairing","link":0}'),
      feed('{"type":"status","state":"probing","link":1}'),
      feed(LINES.statusLive),
      feed(LINES.statusLive),
      feed(LINES.statusSilent),
      feed('{"type":"status","state":"stopped","link":1,"detail":"LV RESET"}'),
      feed('{"type":"status","state":"adapter_not_found","link":0}'),
      feed('{"type":"status","state":"live","link":2,"detail":"changed detail"}'),
      feed('{"type":"status","state":"live","link":2}'),
    ],
  },
  {
    name: "exit 6 parks 60 s",
    ts: "obd.test.ts:343",
    env: ENV,
    settings: null,
    steps: [read, feed(LINES.statusLive), feed(LINES.sample), feed(LINES.error), exit(0, 6), read, adv(59_999), adv(2), read],
  },
  {
    name: "exit 4 parks 600 s",
    ts: "obd.test.ts:365",
    env: ENV,
    settings: null,
    steps: [read, feed('{"type":"error","code":4,"error":"ATDPN A1 is not ISO 15765-4"}'), exit(0, 4), adv(599_000), adv(2_000)],
  },
  {
    name: "backoff 2 s, 4 s, third quick failure parks 60 s",
    ts: "obd.test.ts:381",
    env: ENV,
    settings: null,
    steps: [read, exit(0, 2), adv(1_999), adv(2), exit(1, 2), adv(3_999), adv(2), exit(2, 2), adv(60_001), exit(3, null), adv(2_000)],
  },
  {
    name: "ran >= 15 s then exited: restart in 2 s with a warn",
    ts: "obd.test.ts:408",
    env: ENV,
    settings: null,
    steps: [read, feed(LINES.statusLive), adv(20_000), exit(0, 3), adv(2_001), feed('{"type":"error","code":5,"error":"stop"}'), feed("not json"), adv(14_999), exit(1, 5), adv(2_000)],
  },
  {
    name: "status(): req/s, sums, voltage",
    ts: "obd.test.ts:423",
    env: ENV,
    settings: null,
    steps: [
      read,
      feed(LINES.ready),
      feed(LINES.statusLive),
      feed(LINES.sample),
      feed('{"type":"audit","ts":1000,"counters":{"requests":0,"err_adapter":0,"forbidden_attempts":0}}'),
      feed(
        '{"type":"audit","ts":1060,"counters":{"requests":600,"err_adapter":1,"err_protocol":0,"err_identity":0,"err_voltage":2,"forbidden_attempts":0,"budget_violations":1}}',
      ),
      feed('{"type":"audit","ts":1120,"counters":{"requests":1200,"voltage_low_events":2},"last_stop":{"ts":1100,"reason":"voltage: 11.50 V"}}'),
      feed('{"type":"sample","fields":{"adapter_voltage":13.3,"link":2}}'),
      feed('{"type":"sample","fields":{"rpm":800,"link":1}}'),
      feed('{"type":"audit","counters":{"requests":1300}}'),
    ],
  },
  {
    name: "one journal line per audit, auditSeq reset per run",
    ts: "obd.test.ts:458",
    env: ENV,
    settings: null,
    steps: [read, feed(LINES.statusLive), feed(LINES.audit), feed('{"type":"audit","ts":1760000120,"counters":{"requests":1200,"voltage_low_events":1}}'), exit(0, 0), adv(2_000)],
  },
  {
    name: "report(): profile, bounded inventory, helper identity",
    ts: "obd.test.ts:480",
    env: ENV,
    settings: null,
    steps: [
      read,
      feed(LINES.ready),
      feed(LINES.profile),
      feed(LINES.statusLive),
      feed(JSON.stringify({ type: "audit", ts: 5, counters: { requests: 9 }, inventory: inventory350, last_stop: null })),
      feed('{"type":"ready","helper":"obd_read"}'),
      feed('{"type":"ready","version":"0.2.0","allowlist_sha256":"def456"}'),
    ],
  },
  {
    name: "stop(): SIGINT, no respawn, link 0",
    ts: "obd.test.ts:506",
    env: ENV,
    settings: null,
    steps: [read, feed(LINES.statusLive), { op: "stop" }, exit(0, 0), adv(120_000), read, reapply, { op: "stop" }],
  },
  // --- P3: the namespace ---
  {
    name: "starts from the namespace (env ignored)",
    ts: "obd.test.ts:786",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [read],
  },
  {
    name: "re-reads the namespace about once a second",
    ts: "obd.test.ts:811",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [reapply, read, read, sample, adv(999), read, adv(1), read, reapply, adv(500), read, adv(500), read],
  },
  {
    name: "enabled:false stops the helper; true again restarts it",
    ts: "obd.test.ts:824",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [
      read,
      feed(LINES.ready),
      feed(LINES.statusLive),
      feed(LINES.sample),
      read,
      set({ ...ENABLED, enabled: false }),
      reapply,
      read,
      exit(0, 0),
      adv(60_000),
      ...repeat(5, [adv(1_000), read]),
      set(ENABLED),
      adv(1_000),
      read,
    ],
  },
  {
    name: "adapterMac null → not_configured; a MAC appearing starts it",
    ts: "obd.test.ts:858",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: { enabled: true, adapterMac: null },
    steps: [read, adv(3_000), read, set(ENABLED), reapply],
  },
  {
    name: "changed adapter/pin/pollHz/fields/powerSource → SIGINT + respawn, no backoff",
    ts: "obd.test.ts:872",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [
      read,
      feed(LINES.statusLive),
      set({ ...ENABLED, adapterMac: "AA:BB:CC:DD:EE:01", pin: "4321", pollHz: 0.5, fields: ["rpm"], powerSource: "constant" }),
      reapply,
      exit(0, 0),
      reapply,
      set({ ...ENABLED, adapterMac: "AA:BB:CC:DD:EE:01", pin: "4321", pollHz: 0.5, fields: ["rpm", "speed"], powerSource: "constant" }),
      reapply,
      set({ ...ENABLED, adapterMac: "AA:BB:CC:DD:EE:02" }),
      reapply,
      exit(1, null),
    ],
  },
  {
    name: "a settings change while parked/retrying starts right away",
    ts: "obd.test.ts:900",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [read, exit(0, 2), set({ ...ENABLED, pollHz: 2 }), reapply, adv(30_000), exit(1, 6), set({ ...ENABLED, pollHz: 1 }), reapply, adv(60_000)],
  },
  {
    name: "namespace removed → env seam",
    ts: "obd.test.ts:914",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [read, set(null), reapply, exit(0, 0)],
  },
  {
    name: "namespace removed, no env → not_configured",
    ts: "obd.test.ts:923",
    env: {},
    settings: ENABLED,
    steps: [read, set(null), reapply, exit(0, 0)],
  },
  {
    name: "adapter_not_found warns once per episode, then every 10 min",
    ts: "obd.test.ts:933",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [
      read,
      ...Array.from({ length: 20 }, (_, i) => [...cycle(i), adv(8_000)]).flat(),
      adv(600_000),
      ...cycle(99),
      feed('{"type":"status","state":"probing","link":1,"ts":100}'),
      ...cycle(101),
      exit(0, 3),
      adv(2_000),
      ...cycle(102),
      set({ ...ENABLED, pollHz: 2 }),
      reapply,
      exit(1, 0),
      ...cycle(103),
    ],
  },
  {
    name: "req/s seeded from profile.rate_req_s",
    ts: "obd.test.ts:964",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [
      read,
      feed(LINES.profile),
      feed('{"type":"audit","ts":1000,"counters":{"requests":0}}'),
      feed('{"type":"audit","ts":1060,"counters":{"requests":600}}'),
      exit(0, 0),
      adv(2_000),
      feed('{"type":"profile","rate_req_s":-1}'),
      feed('{"type":"profile","rate_req_s":12.25}'),
      feed('{"type":"audit","ts":1000,"counters":{"requests":10}}'),
    ],
  },
  {
    name: "scan(): once, joined, parked in report().scan",
    ts: "obd.test.ts:979",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    scanner: { candidates: [{ mac: "00:1D:A5:68:98:8B", name: "V-LINK", rssi: -61 }] },
    steps: [read, { op: "scanJoin" }, adv(1_000), { op: "scan" }],
  },
  {
    name: "scan(): failure → empty result with error, warn",
    ts: "obd.test.ts:997",
    env: {},
    settings: ENABLED,
    scanner: { error: "bluetoothctl not found" },
    steps: [read, { op: "scan" }],
  },
  {
    name: "forget(): stop, unbond, clear, hold not_configured",
    ts: "obd.test.ts:1014",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [
      read,
      feed(LINES.profile),
      feed(LINES.statusLive),
      feed(LINES.audit),
      { op: "forget" },
      exit(0, 0),
      ...repeat(3, [adv(1_000), read]),
      reapply,
      set({ enabled: true, adapterMac: null }),
      reapply,
      set({ ...ENABLED, adapterMac: "AA:BB:CC:DD:EE:01" }),
      reapply,
      { op: "forget" },
    ],
  },
  {
    name: "forget(): the same MAC after a clear pairs afresh",
    ts: "obd.test.ts:1052",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [read, { op: "forget" }, exit(0, 0), set({ enabled: true, adapterMac: null }), reapply, set(ENABLED), reapply],
  },
  // --- extra traces (WP2): the paths the its touch only in part ---
  {
    name: "forget with no adapter configured; bond removal failure warns",
    ts: "extra",
    env: {},
    settings: { enabled: true, adapterMac: null },
    unbond: { error: "bluetoothctl not found" },
    steps: [read, { op: "forget" }, set(ENABLED), reapply, { op: "forget" }, exit(0, 0), adv(1_000), read],
  },
  {
    name: "forget while running from the emulator serial keeps running on the serial",
    ts: "extra",
    env: { XORGATE_OBD_SERIAL: "/dev/pts/9" },
    settings: ENABLED,
    steps: [read, { op: "forget" }, exit(0, 0), adv(1_000), read, adv(1_000), read],
  },
  {
    name: "disabled before the first read; reapply before start is a no-op",
    ts: "extra",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: { enabled: false, adapterMac: MAC },
    steps: [reapply, read, set(ENABLED), adv(999), read, adv(1), read, feed(LINES.statusLive), feed(LINES.sample), sample, exit(0, 0), sample, adv(2_000), read],
  },
  {
    name: "stderr lines become lastError; quick exit after an error line",
    ts: "extra",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: null,
    steps: [read, { op: "stderr", line: "Traceback (most recent call last):" } as unknown as Step, { op: "stderr", line: "ImportError: x" } as unknown as Step, exit(0, 1), adv(2_000), { op: "stderr", line: "boom" } as unknown as Step, feed('{"type":"error","code":2,"error":"no controller"}'), exit(1, 2), adv(4_000), exit(2, 3)],
  },
  {
    name: "stop while retrying; stop while parked",
    ts: "extra",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: null,
    steps: [read, exit(0, 2), { op: "stop" }, adv(10_000), read],
  },
  {
    name: "reconfigure while live then the old helper exits non-zero",
    ts: "extra",
    env: { XORGATE_OBD_ADAPTER: "AA:BB:CC:DD:EE:FF" },
    settings: ENABLED,
    steps: [read, feed(LINES.statusLive), feed(LINES.sample), adv(16_000), set({ ...ENABLED, pollHz: 2 }), reapply, read, exit(0, 6), feed(LINES.statusLive), set({ ...ENABLED, enabled: false }), reapply, set(ENABLED), reapply, exit(1, 0), adv(1_000), read],
  },
];

interface Obs {
  t: number;
  step: Step;
  out?: unknown;
  runs: { cfg: ObdHelperConfig; stopped: string | null }[];
  reads: number;
  events: { level: string; msg: string }[];
  logs: { level: string; line: string }[];
  status: ObdStatus;
  report: unknown;
}

async function settle(): Promise<void> {
  for (let i = 0; i < 6; i++) await Promise.resolve();
}

async function runCase(c: Case): Promise<{ case: Omit<Case, "steps">; steps: Obs[] }> {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(T0));
  const logs: { level: string; line: string }[] = [];
  const cap = (level: string) => (...a: unknown[]) => {
    logs.push({ level, line: a.map(String).join(" ") });
  };
  const sl = vi.spyOn(console, "log").mockImplementation(cap("info"));
  const sw = vi.spyOn(console, "warn").mockImplementation(cap("warn"));
  const se = vi.spyOn(console, "error").mockImplementation(cap("error"));
  try {
    const runs: { cfg: ObdHelperConfig; handlers: ObdStreamHandlers; stopped: string | null; exit: (code: number | null) => void }[] = [];
    const spawn: ObdSpawn = (cfg, handlers) => {
      let resolveDone!: (r: RunResult) => void;
      const done = new Promise<RunResult>((r) => (resolveDone = r));
      const run = {
        cfg,
        handlers,
        stopped: null as string | null,
        exit: (code: number | null) =>
          resolveDone({ stdout: "", stderr: "", exitCode: code as number, failed: code !== 0, notFound: false }),
      };
      runs.push(run);
      return { done, stop: (sig) => (run.stopped = sig ?? "SIGINT") };
    };
    let settings: ObdSettings | undefined = c.settings ?? undefined;
    let reads = 0;
    const events: { level: string; msg: string }[] = [];
    let mono = 1n;
    const col = createObdCollector({
      env: c.env,
      settings: () => {
        reads += 1;
        return settings;
      },
      spawn,
      onEvent: (level, msg) => events.push({ level, msg }),
      mono: () => mono++,
      scanner: async () => {
        if (c.scanner?.error) throw new Error(c.scanner.error);
        return (c.scanner?.candidates ?? []) as never;
      },
      unbond: async () => {
        if (c.unbond?.error) throw new Error(c.unbond.error);
      },
    });
    const out: Obs[] = [];
    let ev0 = 0;
    let log0 = 0;
    let lastStamp: bigint | null = null;
    for (const step of c.steps) {
      let o: unknown;
      switch (step.op) {
        case "read":
          o = await col.read();
          break;
        case "sample": {
          const s = await col.sample();
          o = { fields: s.fields, fresh: s.updatedMono !== lastStamp };
          lastStamp = s.updatedMono;
          break;
        }
        case "feed": {
          const msg = parseObdLine(step.line);
          if (msg) runs[runs.length - 1]!.handlers.onMessage(msg as ObdMessage);
          o = msg !== null;
          break;
        }
        case "exit":
          runs[step.run]!.exit(step.code);
          await settle();
          break;
        case "advance":
          vi.advanceTimersByTime(step.ms);
          break;
        case "set":
          settings = step.settings ?? undefined;
          break;
        case "reapply":
          col.reapply();
          break;
        case "scan":
          o = await col.scan();
          break;
        case "scanJoin": {
          const p1 = col.scan();
          const p2 = col.scan();
          o = { joined: p1 === p2, result: await p1 };
          break;
        }
        case "forget":
          o = await col.forget();
          break;
        case "stop":
          col.stop();
          break;
        default: {
          const s = step as unknown as { op: string; line: string };
          if (s.op === "stderr") runs[runs.length - 1]!.handlers.onStderr?.(s.line);
          else throw new Error(`unknown op ${s.op}`);
        }
      }
      out.push({
        t: Date.now() - T0,
        step,
        ...(o !== undefined ? { out: ser(o) } : {}),
        runs: runs.map((r) => ({ cfg: { ...r.cfg, fields: r.cfg.fields ? [...r.cfg.fields] : null }, stopped: r.stopped })),
        reads,
        events: events.slice(ev0),
        logs: logs.slice(log0),
        status: ser(col.status()) as ObdStatus,
        report: ser(col.report()),
      });
      ev0 = events.length;
      log0 = logs.length;
    }
    const { steps: _s, ...meta } = c;
    return { case: meta, steps: out };
  } finally {
    sl.mockRestore();
    sw.mockRestore();
    se.mockRestore();
    vi.useRealTimers();
  }
}

// --------------------------------------------------------------------------
// mock stream
// --------------------------------------------------------------------------
function mockTrace(cfg: ObdHelperConfig, ms: number, stopAt: number) {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(T0));
  try {
    const msgs: { t: number; msg: ObdMessage }[] = [];
    const h = mockObdStream(cfg, { onMessage: (m) => msgs.push({ t: Date.now() - T0, msg: JSON.parse(JSON.stringify(m)) }) });
    vi.advanceTimersByTime(stopAt);
    h.stop();
    vi.advanceTimersByTime(ms - stopAt);
    return { cfg, stopAt, ms, msgs };
  } finally {
    vi.useRealTimers();
  }
}

const results: Record<string, unknown> = {};
it("dumps the pure vectors", () => {
  results.pure = pure();
});
it("dumps the supervisor traces", async () => {
  const traces = [];
  for (const c of CASES) traces.push(await runCase(c));
  results.traces = {
    ...JS,
    jsSourceFile: ["src/agent/collectors/obd.ts", "test/obd.test.ts"],
    t0: T0,
    cases: traces,
  };
});
it("dumps the mock stream", () => {
  const base: ObdHelperConfig = { adapter: "AA:BB:CC:DD:EE:FF", serial: null, pin: "1234", pollHz: 1, fields: null, powerSource: "switched" };
  results.mock = {
    ...JS,
    jsSourceFile: ["src/hardware/obd.ts"],
    t0: T0,
    runs: [
      mockTrace(base, 25_000, 21_500),
      mockTrace({ ...base, pollHz: 2 }, 12_000, 12_000),
      mockTrace({ ...base, adapter: null, serial: "/dev/pts/1", pollHz: 0.2 }, 22_000, 22_000),
      mockTrace({ ...base, pollHz: 30 }, 1_500, 1_500),
    ],
  };
});
afterAll(() => {
  writeFileSync("test/zz-p5b-obd-pure.json", JSON.stringify(results.pure, null, 2) + "\n");
  writeFileSync("test/zz-p5b-obd-traces.json", JSON.stringify(results.traces, null, 2) + "\n");
  writeFileSync("test/zz-p5b-obd-mock.json", JSON.stringify(results.mock, null, 2) + "\n");
});
