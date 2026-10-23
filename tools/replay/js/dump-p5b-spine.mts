// Phase 5b WP1 tsx driver: the spine's NEW fixtures, dumped from the real TS
// @ 09da38d (every expectation is what the real module returned / printed).
//
//   fixtures/settings/obd.json            parseObdSettings, OBD_DEFAULTS, loadSettings/saveSettings round trips
//   fixtures/cloud_config/obd.json        parseCloudConfig(obd), applyCloudConfig(obd) sequences (file bytes)
//   fixtures/hardware_block/peripherals.json  parseHardwareBlock(+warn), loadHardwareBlock(+log), hardwareBlocksEqual
//   fixtures/commands/p5b-commands.json   parseCommand(clear_sms/obd_*), executeCommand in-process guard
//   fixtures/clock_trust/clock-trust.json createClockTrust scripted traces (fake probe + fake mono)
//
// Usage (cwd = the shared js-09da38d copy, the fixtures dir mounted at /out):
//   npx --no-install tsx .rig-drivers/dump-p5b-spine.mts /out
import { mkdtempSync, readFileSync, writeFileSync, existsSync, rmSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const OUT = process.argv[2];
if (!OUT) throw new Error("usage: dump-p5b-spine.mts <fixtures dir>");
const COMMIT = "09da38dcc8b75c248625a3a0bee43b70e7def55d";
const GEN = "tools/replay/js/dump-p5b-spine.mts (Phase 5b WP1) in the shared js-09da38d copy via npx tsx";

const st = await import(join(process.cwd(), "src/daemon/settings.ts"));
const cc = await import(join(process.cwd(), "src/agent/cloud-config.ts"));
const hw = await import(join(process.cwd(), "src/hardware/hardware-config.ts"));
const cm = await import(join(process.cwd(), "src/agent/commands.ts"));
const ct = await import(join(process.cwd(), "src/agent/clock-trust.ts"));

const work = mkdtempSync(join(tmpdir(), "xg-p5b-spine-"));
let seq = 0;
const tmp = (name: string): string => join(work, `${seq++}-${name}`);
const S = (v: unknown): string | null => (v === undefined ? null : JSON.stringify(v));
function write(rel: string, body: Record<string, unknown>): void {
  const p = join(OUT, rel);
  mkdirSync(join(p, ".."), { recursive: true });
  writeFileSync(p, JSON.stringify({ ...body, jsCommit: COMMIT, generatedBy: GEN }, null, 2) + "\n");
  process.stderr.write(`${rel}: ${(body.cases as unknown[]).length} cases\n`);
}

// ---------------------------------------------------------------------------
// settings/obd.json
// ---------------------------------------------------------------------------
{
  const cases: unknown[] = [];
  const parse = (name: string, input: unknown): void => {
    cases.push({ name, op: "parseObdSettings", input, expectedJson: S(st.parseObdSettings(input)) });
  };
  // settings.test.ts "parses the contract shape …"
  parse("contract shape: MAC uppercased, pin, pollHz, fields deduped, enum power", {
    enabled: true,
    adapterMac: " 10:21:3e:4f:33:65 ",
    pin: "123456",
    pollHz: 0.5,
    fields: ["rpm", "speed", "rpm", "Bad Name", 7],
    powerSource: "constant",
  });
  parse("null MAC kept, empty fields kept", { adapterMac: null, fields: [] });
  parse("pollHz 50 clamps to max", { pollHz: 50 });
  parse("pollHz 0 clamps to min", { pollHz: 0 });
  // "DROPS (never repairs) …"
  parse("malformed MAC/PIN, bad enum, mistyped enabled → undefined", {
    adapterMac: "10:21:3E:4F:33",
    pin: "12",
    powerSource: "solar",
    enabled: "yes",
  });
  parse("dashed MAC dropped, enabled kept", { adapterMac: "10-21-3E-4F-33-65", enabled: true });
  parse("letters PIN dropped", { pin: "abcd", enabled: false });
  parse("null → undefined", null);
  parse("array → undefined", []);
  parse("{} → undefined", {});
  // config-contract.test.ts obd it() + extra edges
  parse("contract defaults round-trip", { enabled: false, adapterMac: null, pin: "1234", pollHz: 1, fields: [], powerSource: "switched" });
  parse("pollHz min bound", { pollHz: 0.2 });
  parse("pollHz max bound", { pollHz: 2 });
  parse("lowercase MAC normalizes", { adapterMac: "10:21:3e:4f:33:65" });
  parse("adapterMac null alone", { adapterMac: null });
  parse("pin 7 digits dropped", { pin: "1234567" });
  parse("pin 4 digits kept", { pin: "0000" });
  parse("pin number type dropped", { pin: 1234 });
  parse("pollHz NaN-ish string dropped", { pollHz: "1" });
  parse("pollHz negative clamps", { pollHz: -3 });
  parse("pollHz fraction kept", { pollHz: 0.33 });
  parse("fields not an array dropped", { fields: "rpm" });
  parse("fields filtered to identifiers", { fields: ["rpm", "", "1abc", "_x", "a_1", "UP", "a-b", "rpm"] });
  parse("fields all invalid → [] (key kept)", { fields: ["Bad"] });
  parse("powerSource case-sensitive", { powerSource: "Switched" });
  parse("MAC with tab/newline trim", { adapterMac: "\t10:21:3E:4F:33:65\n" });
  parse("MAC lowercase hex with spaces inside dropped", { adapterMac: "10:21:3E:4F:33: 65" });
  parse("MAC with ligature ff upper-cases to FF", { adapterMac: "10:21:3E:4F:33:ﬀ" });
  parse("unknown keys ignored", { bogus: 1, enabled: true });
  parse("key order follows the parser, not the input", { powerSource: "constant", fields: ["rpm"], pollHz: 1.5, pin: "9999", adapterMac: null, enabled: true });
  parse("string top level → undefined", "obd");
  cases.push({ name: "OBD_DEFAULTS", op: "OBD_DEFAULTS", input: null, expectedJson: S(st.OBD_DEFAULTS) });
  cases.push({ name: "OBD_POLL_HZ bounds", op: "OBD_POLL_HZ", input: null, expectedJson: S([st.OBD_POLL_HZ_MIN, st.OBD_POLL_HZ_MAX]) });
  // loadSettings round trips ("is kept by the allowlist rewriter …")
  const load = (name: string, text: string): void => {
    const p = tmp("settings.json");
    writeFileSync(p, text);
    const loaded = st.loadSettings(p);
    const p2 = tmp("settings.json");
    st.saveSettings({ ...loaded, gnssAntBias: true }, p2);
    cases.push({
      name,
      op: "loadSettings",
      input: text,
      expectedJson: S(loaded),
      savedAfterGnss: readFileSync(p2, "utf8"),
    });
  };
  load(
    "allowlist keeps obd, drops unknown keys inside, normalizes the MAC",
    JSON.stringify({ mode: "real", obd: { enabled: true, adapterMac: "10:21:3e:4f:33:65", powerSource: "switched", bogus: 1 } }),
  );
  load("an all-invalid obd namespace is dropped", JSON.stringify({ mode: "sim", obd: { pin: "1" } }));
  load("obd lands after simulation in the rewrite", JSON.stringify({ obd: { enabled: false }, simulation: { loop: false }, mode: "real" }));
  load("obd null in the file is dropped", JSON.stringify({ obd: null }));
  write("settings/obd.json", {
    jsSourceFile: "src/daemon/settings.ts (parseObdSettings :388-412, OBD_DEFAULTS :362-369) + test/settings.test.ts + test/config-contract.test.ts",
    cases,
  });
}

// ---------------------------------------------------------------------------
// cloud_config/obd.json
// ---------------------------------------------------------------------------
{
  const cases: unknown[] = [];
  const MSG = (obd: unknown): string => JSON.stringify({ v: 1, rev: 51, obd });
  const parse = (name: string, payload: string): void => {
    cases.push({ name, op: "parseCloudConfig", input: payload, expectedJson: S(cc.parseCloudConfig(payload)) });
  };
  // cloud-config.test.ts "parses the namespace normalized like local settings …"
  parse("normalized like local settings", MSG({ enabled: true, adapterMac: "10:21:3e:4f:33:65", powerSource: "switched" }));
  parse("{} → null", MSG({}));
  parse("all-invalid → null", MSG({ pin: "x" }));
  parse("string → ignored", MSG("10:21:3E:4F:33:65"));
  parse("array → ignored", MSG([1]));
  parse("null → ignored", MSG(null));
  parse("obd sits between simulation and hardware in the parse",
    JSON.stringify({ v: 1, hardware: { v: 2, platform: "rpi-cm5", rails: true, statusLed: null, imu: null, video: [] }, obd: { enabled: true }, simulation: { loop: true }, rev: 3 }));
  // "replaces the whole namespace, no-ops on an identical redelivery, clears on null, leaves alone when absent"
  const seqCase = (name: string, initial: string | null, steps: unknown[]): void => {
    const p = tmp("settings.json");
    if (initial !== null) writeFileSync(p, initial);
    const results = steps.map((config) => {
      const r = cc.applyCloudConfig(config, p);
      return { changed: r.changed, fileAfter: existsSync(p) ? readFileSync(p, "utf8") : null };
    });
    cases.push({ name, op: "applyCloudConfig", input: { initial, steps }, expected: results });
  };
  const cloud = { enabled: true, adapterMac: "10:21:3E:4F:33:65", powerSource: "switched" };
  seqCase(
    "replace whole namespace, no-op redelivery, clear on null, absent leaves alone",
    JSON.stringify({ mode: "real", obd: { enabled: false, adapterMac: null, pollHz: 2 } }),
    [{ obd: cloud }, { obd: { ...cloud } }, { gnssAntBias: true }, { obd: null }, { obd: null }],
  );
  seqCase("obd appended last on a file without it", JSON.stringify({ simulation: { loop: false }, mode: "sim" }), [{ obd: { pollHz: 0.5 } }]);
  seqCase("obd key order change is a change (JSON.stringify compare)", JSON.stringify({ obd: { pin: "1234", enabled: true } }), [{ obd: { enabled: true, pin: "1234" } }]);
  seqCase("obd set together with timeSync", null, [{ timeSync: { offlineSources: false }, obd: { adapterMac: null } }]);
  write("cloud_config/obd.json", {
    jsSourceFile: "src/agent/cloud-config.ts (parseNamespaces :280-283, applyCloudConfig obd :399-438) + test/cloud-config.test.ts",
    cases,
  });
}

// ---------------------------------------------------------------------------
// hardware_block/peripherals.json
// ---------------------------------------------------------------------------
{
  const cases: unknown[] = [];
  const CM4 = { v: 2, platform: "rpi-cm4", encoder: "hw-h264", rails: false, statusLed: null, imu: "bno085", video: [{ key: "cam0", connector: "csi0", sensor: "imx290", codec: "h264", label: "Front" }] };
  const OBD = { adapter: "elm327", transport: "bt-spp" };
  const parse = (name: string, input: unknown): void => {
    const warnings: string[] = [];
    const block = hw.parseHardwareBlock(input, (m: string) => warnings.push(m));
    cases.push({ name, op: "parseHardwareBlock", input, expectedJson: block === null ? null : JSON.stringify(block), warnings });
  };
  parse("absent → no peripherals key", CM4);
  parse("{} → no peripherals key, no warning", { ...CM4, peripherals: {} });
  parse("null → no peripherals key, no warning", { ...CM4, peripherals: null });
  parse("valid obd → parsed verbatim", { ...CM4, peripherals: { obd: OBD } });
  parse("valid obd with extra fields → canonical {adapter, transport}", { ...CM4, peripherals: { obd: { transport: "bt-spp", adapter: "elm327", pin: "1234" } } });
  parse("invalid transport → one warning", { ...CM4, peripherals: { obd: { adapter: "elm327", transport: "ble" } } });
  parse("string obd → warning", { ...CM4, peripherals: { obd: "elm327" } });
  parse("null obd → warning", { ...CM4, peripherals: { obd: null } });
  parse("array obd → warning", { ...CM4, peripherals: { obd: [OBD] } });
  parse("unknown peripheral + valid obd → one warning, obd kept", { ...CM4, peripherals: { lidar: { adapter: "x" }, obd: OBD } });
  parse("two unknown peripherals → two warnings", { ...CM4, peripherals: { "9": 1, lidar: true } });
  parse("peripherals string → warning", { ...CM4, peripherals: "obd" });
  parse("peripherals array → warning", { ...CM4, peripherals: [OBD] });
  parse("peripherals number → warning", { ...CM4, peripherals: 5 });
  parse("bad encoder next to a valid peripheral → null, no warning", { ...CM4, encoder: "av1", peripherals: { obd: OBD } });
  parse("bad video next to a bad peripheral → null, no warning (video first)", { ...CM4, video: [1], peripherals: { obd: 1 } });
  parse("warning text uses JSON.stringify of the entry", { ...CM4, peripherals: { obd: { adapter: "stn", n: 1.5, s: "é" } } });
  const load = (name: string, text: string | null): void => {
    const p = tmp("hardware.json");
    if (text !== null) writeFileSync(p, text);
    const logs: string[] = [];
    const block = hw.loadHardwareBlock(p, (m: string) => logs.push(m.split(p).join("$PATH")));
    cases.push({ name, op: "loadHardwareBlock", input: text, expectedJson: block === null ? null : JSON.stringify(block), logs });
  };
  load("keeps a block whose only fault is a bad peripheral", JSON.stringify({ ...CM4, peripherals: { obd: { adapter: "stn" } } }));
  load("valid peripheral → no log", JSON.stringify({ ...CM4, peripherals: { obd: OBD } }));
  load("absent file → silent", null);
  load("not JSON → one log", "{nope");
  load("failed validation after a peripheral warning → two logs", JSON.stringify({ ...CM4, platform: "jetson", peripherals: { x: 1 } }));
  load("integer-like peripheral key order is JSON.parse order", '{"v":2,"platform":"rpi-cm5","rails":true,"statusLed":null,"imu":null,"video":[],"peripherals":{"zz":1,"10":2,"obd":{"transport":"bt-spp","adapter":"elm327"}}}');
  const eq = (name: string, a: unknown, b: unknown): void => {
    const pa = hw.parseHardwareBlock(a, () => {});
    const pb = hw.parseHardwareBlock(b, () => {});
    cases.push({ name, op: "hardwareBlocksEqual", input: [a, b], expectedJson: S(hw.hardwareBlocksEqual(pa, pb)) });
  };
  eq("a declared peripheral makes the blocks differ", { ...CM4, peripherals: { obd: OBD } }, CM4);
  eq("an empty peripherals object equals none", { ...CM4, peripherals: {} }, CM4);
  eq("an invalid peripheral equals none", { ...CM4, peripherals: { obd: 1 } }, CM4);
  write("hardware_block/peripherals.json", {
    jsSourceFile: "src/hardware/hardware-config.ts (parsePeripherals :104-128, parseHardwareBlock :133-188, loadHardwareBlock :196-217) + test/hardware-block.test.ts",
    cases,
  });
}

// ---------------------------------------------------------------------------
// commands/p5b-commands.json
// ---------------------------------------------------------------------------
{
  const cases: unknown[] = [];
  const T0 = 1_750_000_000_000;
  const msg = (over: Record<string, unknown> = {}): string =>
    JSON.stringify({ v: 1, requestId: "req-1", command: "reboot", ts: T0, expiresAt: T0 + 60_000, ...over });
  const parse = (name: string, payload: string): void => {
    cases.push({ name, op: "parseCommand", input: payload, expectedJson: S(cm.parseCommand(payload) ?? undefined) });
  };
  parse("obd_scan parses with no args", msg({ command: "obd_scan" }));
  parse("obd_forget ignores args", msg({ command: "obd_forget", args: { limit: 3 } }));
  parse("clear_sms parses", msg({ command: "clear_sms" }));
  parse("clear_sms limit 25", msg({ command: "clear_sms", args: { limit: 25 } }));
  parse("clear_sms limit 500 clamps", msg({ command: "clear_sms", args: { limit: 500 } }));
  parse("clear_sms no args → 10", msg({ command: "clear_sms", args: undefined }));
  parse("clear_sms args array → default", msg({ command: "clear_sms", args: [5] }));
  parse("clear_sms limit 0 → clamped", msg({ command: "clear_sms", args: { limit: 0 } }));
  parse("clear_sms limit 2.6 → rounded", msg({ command: "clear_sms", args: { limit: 2.6 } }));
  parse("clear_sms limit string → default", msg({ command: "clear_sms", args: { limit: "5" } }));
  parse("obd_scan expiresAt clamped to ts+120s", msg({ command: "obd_scan", expiresAt: T0 + 999_999 }));
  parse("unknown command still null", msg({ command: "obd_pair" }));
  for (const c of ["refresh", "read_sms", "clear_sms", "obd_scan", "obd_forget"]) {
    const calls: unknown[] = [];
    const res = await cm.executeCommand(c, async (cmd: string, args: string[]) => {
      calls.push([cmd, args]);
      return { stdout: "", stderr: "", exitCode: 0, failed: false, notFound: false };
    });
    cases.push({ name: `executeCommand ${c} never reaches the runner`, op: "executeCommand", input: c, expectedJson: S(res), calls });
  }
  write("commands/p5b-commands.json", {
    jsSourceFile: "src/agent/commands.ts (DeviceCommandName :79-87, parseCommand :160-179, executeCommand :257-275) + test/commands.test.ts",
    cases,
  });
}

// ---------------------------------------------------------------------------
// clock_trust/clock-trust.json — scripted traces through createClockTrust
// ---------------------------------------------------------------------------
{
  type Step =
    | { op: "advance"; ms: number }
    | { op: "tick" }
    | { op: "trusted" }
    | { op: "status" }
    | { op: "noteHeld" }
    | { op: "noteStep"; source: "gps" | "nitz" };
  const cases: unknown[] = [];
  const scenario = async (name: string, answers: string[], steps: Step[], graceMs?: number, startMono = 1_000): Promise<void> => {
    let mono = startMono;
    let probes = 0;
    const events: Array<{ level: string; msg: string }> = [];
    const trust = ct.createClockTrust({
      probe: async () => {
        probes += 1;
        const a = answers[Math.min(probes, answers.length) - 1]!;
        if (a.startsWith("throw:")) throw new Error(a.slice(6));
        return a;
      },
      ...(graceMs === undefined ? {} : { graceMs }),
      monoMs: () => mono,
      onEvent: (level: string, msg: string) => events.push({ level, msg }),
    });
    const trace: unknown[] = [];
    for (const s of steps) {
      const before = events.length;
      let result: unknown = null;
      if (s.op === "advance") mono += s.ms;
      else if (s.op === "tick") await trust.tick();
      else if (s.op === "trusted") result = trust.trusted();
      else if (s.op === "status") result = trust.status();
      else if (s.op === "noteHeld") trust.noteHeld();
      else if (s.op === "noteStep") trust.noteStep(s.source);
      trace.push({ step: s, result, events: events.slice(before), probes });
    }
    cases.push({ name, input: { answers, graceMs: graceMs ?? null, startMono, steps }, expected: trace });
  };
  const A = (ms: number): Step => ({ op: "advance", ms });
  const T: Step = { op: "tick" };
  const TR: Step = { op: "trusted" };
  const ST: Step = { op: "status" };
  const H: Step = { op: "noteHeld" };
  // the six it()s of test/clock-trust.test.ts, as traces
  await scenario("holds until NTP, then latches with one info event", ["unsynced", "unsynced", "ntp"], [TR, ST, H, H, T, A(5_000), T, TR, A(5_000), T, TR, ST, T, ST], 300_000);
  await scenario("rtc is not trust, a GPS step is", ["rtc"], [T, TR, { op: "noteStep", source: "gps" }, TR, ST]);
  await scenario("falls back after the grace window with one warn", ["unsynced"], [T, H, A(59_999), TR, A(1), TR, ST, A(100_000), TR, ST], 60_000);
  await scenario("NTP after a fallback clears the flag and says how long", ["unsynced", "ntp"], [T, A(60_000), TR, ST, A(30_000), T, ST, TR], 60_000);
  await scenario("a throwing probe counts toward the grace window", ["throw:timedatectl: boom"], [T, TR, A(1_000), T, TR, ST], 1_000, 0);
  cases.push({ name: "CLOCK_TRUST_DEFAULTS", input: null, expected: ct.CLOCK_TRUST_DEFAULTS });
  // extras: the default grace, NITZ, status() flipping the fallback, rounding
  await scenario("default 5 min grace: status() at 300 s flips fallback with the warn", ["unsynced"], [T, H, H, H, A(299_999), ST, A(1), ST, TR]);
  await scenario("nitz step while held", ["unsynced"], [T, H, A(12_345), { op: "noteStep", source: "nitz" }, ST, { op: "noteStep", source: "gps" }, ST]);
  await scenario("secs rounds like toFixed(0): 2.5 s → 3, 1.499 s → 1", ["unsynced", "ntp"], [A(2_500), T, T, ST]);
  await scenario("gps step after fallback: late-sync wording", ["unsynced"], [T, A(300_000), TR, A(45_500), { op: "noteStep", source: "gps" }, ST]);
  await scenario("throw then ntp", ["throw:spawn timedatectl ENOENT", "ntp"], [T, A(4_000), T, ST]);
  await scenario("held count keeps growing after trust", ["ntp"], [H, T, H, H, ST]);
  write("clock_trust/clock-trust.json", {
    jsSourceFile: "src/agent/clock-trust.ts (createClockTrust :84-212) + test/clock-trust.test.ts",
    cases,
  });
}

// ---------------------------------------------------------------------------
// settings/recording-settings.json: re-pin the module constants @ 09da38d
// (CONFIG_APPLY_CLASSES / CONFIG_NAMESPACE_KEYS gained `obd`). The file's
// cases are left to the H2 re-certification (their sources did not change).
// ---------------------------------------------------------------------------
{
  const p = join(OUT, "settings/recording-settings.json");
  const f = JSON.parse(readFileSync(p, "utf8"));
  const csm = await import(join(process.cwd(), "src/agent/config-sync.ts"));
  const rc = await import(join(process.cwd(), "src/recording/config.ts"));
  const cs2 = await import(join(process.cwd(), "src/agent/clock-step.ts"));
  const vp = await import(join(process.cwd(), "src/recording/video-presets.ts"));
  const next = {
    ...f,
    recordingDefaultsJson: JSON.stringify(rc.RECORDING_DEFAULTS),
    timeSyncDefaultsJson: JSON.stringify(cs2.TIME_SYNC_DEFAULTS),
    videoPresetsJson: JSON.stringify(vp.VIDEO_PRESETS),
    defaultVideoPreset: vp.DEFAULT_VIDEO_PRESET,
    configApplyClassesJson: JSON.stringify(csm.CONFIG_APPLY_CLASSES),
    configNamespaceKeys: csm.CONFIG_NAMESPACE_KEYS,
    constantsJsCommit: COMMIT,
  };
  const moved = ["recordingDefaultsJson", "timeSyncDefaultsJson", "videoPresetsJson", "defaultVideoPreset", "configApplyClassesJson", "configNamespaceKeys"]
    .filter((k) => JSON.stringify(f[k]) !== JSON.stringify((next as Record<string, unknown>)[k]));
  writeFileSync(p, JSON.stringify(next, null, 2) + "\n");
  process.stderr.write(`settings/recording-settings.json constants re-pinned; moved: ${moved.join(", ")}\n`);
}

rmSync(work, { recursive: true, force: true });
