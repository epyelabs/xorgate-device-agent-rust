// Phase 5b WP1 tsx driver: re-dumps fixtures/config_report/config-report.json
// against the REAL `src/agent/config-sync.ts` @ 09da38d.
//
//   1. Every existing case (Phase 1 WP4, @ 89b5c64) is re-evaluated through
//      the 09da38d function its `op` names, on its recorded input; the
//      expectation is replaced by what 09da38d returns. The cases whose
//      expectation moved are listed on stderr (X3: `capabilities` grew to
//      4 entries; `CONFIG_APPLY_CLASSES` gained `obd`).
//   2. The Phase 5b cases are appended ("[p5b] …"): `effective.obd`, the
//      `obd` namespace verdicts (`obdStatus`, the revert marker read off
//      settings.json — CF74), the clock-trust `timeSync` details, the report
//      carrying `obd`, and `unheldNamespaces` (new op).
//
// Usage (inside the shared JS copy, cwd = its root):
//   npx --no-install tsx .rig-drivers/dump-p5b-spine-config-report.mts <in.json> > <out.json>
import { mkdtempSync, readFileSync, writeFileSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const cs = await import(join(process.cwd(), "src/agent/config-sync.ts"));
const cc = await import(join(process.cwd(), "src/agent/cloud-config.ts"));
const st = await import(join(process.cwd(), "src/daemon/settings.ts"));
const bi = await import(join(process.cwd(), "src/lib/build-info.ts"));

const IN = process.argv[2];
if (!IN) throw new Error("usage: dump-p5b-spine-config-report.mts <in.json>");
const fixture = JSON.parse(readFileSync(IN, "utf8"));
const work = mkdtempSync(join(tmpdir(), "xg-p5b-cr-"));
let n = 0;
const tmp = (name: string): string => join(work, `${n++}-${name}`);

const U = (v: unknown): unknown =>
  v && typeof v === "object" && !Array.isArray(v) && (v as Record<string, unknown>).$undefined === true
    ? undefined
    : v;
const S = (v: unknown): string | null => (v === undefined ? null : JSON.stringify(v));

/** settings inputs go through loadSettings (parser order), as the worker reads them. */
function settingsOf(v: unknown): Record<string, unknown> {
  const p = tmp("settings.json");
  writeFileSync(p, JSON.stringify(v ?? {}));
  return st.loadSettings(p);
}
function desiredFrom(payload: unknown) {
  if (typeof payload !== "string") return null;
  const parsed = cc.parseCloudMessage(payload);
  if (parsed === null) throw new Error("fixture message did not parse");
  return { rev: parsed.config.rev ?? 0, raw: parsed.raw, parsed: parsed.config };
}

async function run(op: string, i: Record<string, unknown>): Promise<string | null> {
  switch (op) {
    case "buildEffective":
      return S(
        cs.buildEffective(
          settingsOf(i.settings),
          i.modem,
          U(i.mode),
          U(i.cameraMount),
          U(i.telemetryScope),
          U(i.obd),
        ),
      );
    case "buildStatus":
      return S(
        cs.buildStatus(
          desiredFrom(i.desiredPayload),
          i.effective,
          i.modem,
          i.at,
          U(i.hints) ?? {},
          U(i.cameraMount),
          i.settings === undefined || U(i.settings) === undefined ? undefined : settingsOf(i.settings),
        ),
      );
    case "buildConfigReport": {
      const input: Record<string, unknown> = { ...i };
      delete input.desiredPayload;
      delete input.build;
      input.desired = desiredFrom(i.desiredPayload);
      input.settings = settingsOf(i.settings);
      for (const k of Object.keys(input)) if (U(input[k]) === undefined) delete input[k];
      return S(cs.buildConfigReport(input));
    }
    case "parseSimTelemetry":
      return S(cs.parseSimTelemetry(U(i.raw)));
    case "parseUplinkTelemetry":
      return S(cs.parseUplinkTelemetry(U(i.raw)));
    case "parseApnTelemetry":
      return S(cs.parseApnTelemetry(U(i.raw)));
    case "shouldApplyRev":
      return S(cs.shouldApplyRev(U(i.rev), i.lastAppliedRev));
    case "namespacesOf":
      return S(cs.namespacesOf(settingsOf(i.settings)));
    case "diffNamespaces":
      return S(cs.diffNamespaces(i.base, i.next));
    case "readModemBias": {
      const p = tmp("telemetry.json");
      if (typeof i.text === "string") writeFileSync(p, i.text);
      return S(await cs.readModemBias(i.now, p));
    }
    case "loadConfigSyncState": {
      const p = tmp("config-sync.json");
      if (typeof i.text === "string") writeFileSync(p, i.text);
      return S(cs.loadConfigSyncState(p));
    }
    case "ensureConfigSyncBaseline": {
      const sp = tmp("config-sync.json");
      const fp = tmp("settings.json");
      writeFileSync(sp, JSON.stringify(i.state));
      st.saveSettings(i.settings, fp);
      const hash = cs.readSettingsHash(fp);
      if (hash !== i.settingsHash) throw new Error(`settingsHash mismatch: ${hash} != ${i.settingsHash}`);
      const before = readFileSync(sp, "utf8");
      const next = cs.ensureConfigSyncBaseline(sp, fp);
      const save = readFileSync(sp, "utf8") !== before;
      return `{"state":${JSON.stringify(next)},"save":${save}}`;
    }
    case "CONFIG_APPLY_CLASSES":
      return S(cs.CONFIG_APPLY_CLASSES);
    case "unheldNamespaces": {
      const d = desiredFrom(i.desiredPayload);
      return S(cs.unheldNamespaces(d!.parsed, settingsOf(i.settings), U(i.baseline)));
    }
    default:
      throw new Error(`unknown op ${op}`);
  }
}

// ---------------------------------------------------------------------------
// 1. re-evaluate every existing case
// ---------------------------------------------------------------------------
const build = bi.loadBuildInfo();
const moved: string[] = [];
const cases: Array<Record<string, unknown>> = [];
for (const c of fixture.cases as Array<Record<string, unknown>>) {
  const got = await run(c.op as string, c.input as Record<string, unknown>);
  if (got !== (c.expectedJson ?? null)) moved.push(`${c.op}: ${c.name}`);
  cases.push({ ...c, expectedJson: got });
}

// ---------------------------------------------------------------------------
// 2. Phase 5b cases
// ---------------------------------------------------------------------------
const UNDEF = { $undefined: true };
const FRESH = { value: true, fresh: true };
const NO_MODEM = { value: null, fresh: false, detail: "modem telemetry unavailable" };
const MAC = "AA:BB:CC:DD:EE:FF";
/** A collector report in the key order of `collectors/obd.ts:712-731`. */
function report(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    enabled: true,
    adapterMac: MAC,
    pin: "1234",
    pollHz: 1,
    fields: [],
    powerSource: "switched",
    status: "live",
    link: 2,
    source: "settings",
    adapter: { mac: MAC },
    ...over,
  };
}
function add(name: string, op: string, input: Record<string, unknown>, source = "src/agent/config-sync.ts @ 09da38d"): void {
  cases.push({ name: `[p5b] ${name}`, source, op, input, expectedJson: null });
}
const msg = (o: Record<string, unknown>): string => JSON.stringify({ v: 1, rev: 7, ...o });
const OBD = { enabled: true, adapterMac: MAC };

add("buildEffective carries the collector's report as effective.obd, after apn", "buildEffective", {
  settings: { obd: OBD },
  modem: FRESH,
  mode: "real",
  cameraMount: UNDEF,
  telemetryScope: UNDEF,
  obd: report(),
});
add("buildEffective without a collector report has no obd key, even with the namespace held", "buildEffective", {
  settings: { obd: OBD },
  modem: NO_MODEM,
  mode: UNDEF,
  cameraMount: UNDEF,
  telemetryScope: UNDEF,
  obd: null,
});
add("buildEffective: agent.capabilities on every device (sim mode, no obd)", "buildEffective", {
  settings: {},
  modem: NO_MODEM,
  mode: "sim",
  cameraMount: UNDEF,
  telemetryScope: UNDEF,
  obd: UNDEF,
});

const statusCase = (name: string, desired: unknown, settings: unknown, obd: unknown, hints: unknown = UNDEF): void => {
  const effective: Record<string, unknown> = {};
  if (obd !== null) effective.obd = obd;
  add(name, "buildStatus", {
    desiredPayload: msg({ obd: desired }),
    effective,
    modem: FRESH,
    at: 42,
    hints,
    cameraMount: UNDEF,
    settings,
  });
};
statusCase("obd without a collector report → rejected (no peripherals.obd)", OBD, { obd: OBD }, null);
statusCase("obd not held in settings.json yet → adjusted", OBD, {}, report());
statusCase("obd held with a different value → adjusted (changed locally?)", OBD, { obd: { enabled: false } }, report());
statusCase("obd held, collector not caught up (settings source) → adjusted next tick", { ...OBD, pollHz: 2 }, { obd: { ...OBD, pollHz: 2 } }, report());
statusCase("obd held, collector still on the env seam → adjusted env text", OBD, { obd: OBD }, report({ source: "env", adapterMac: null, enabled: false }));
statusCase("obd applied + forgotten adapter → applied with the forget detail", OBD, { obd: OBD }, report({ status: "not_configured", link: 0 }));
statusCase("obd applied + adapter_not_found → not reachable detail", OBD, { obd: OBD }, report({ status: "adapter_not_found", link: 0 }));
statusCase("obd applied + backoff → not reachable detail", OBD, { obd: OBD }, report({ status: "backoff", link: 0 }));
statusCase("obd applied + parked → parked detail", OBD, { obd: OBD }, report({ status: "parked", link: 0 }));
statusCase("obd applied + live → applied, no detail", OBD, { obd: OBD }, report());
statusCase(
  "obd enabled without an adapter → applied, nothing polled",
  { enabled: true },
  { obd: { enabled: true } },
  report({ adapterMac: null, adapter: { mac: null }, status: "not_configured", link: 0 }),
);
statusCase(
  "obd disabled → applied, no detail",
  { enabled: false },
  { obd: { enabled: false } },
  report({ enabled: false, adapterMac: null, adapter: { mac: null }, status: "disabled", link: 0 }),
);
statusCase(
  "obd adapterMac null held → applied (defaults fill the rest)",
  { adapterMac: null },
  { obd: { adapterMac: null } },
  report({ enabled: false, adapterMac: null, adapter: { mac: null }, status: "disabled", link: 0 }),
);
statusCase("obd report missing a namespace key → adjusted", OBD, { obd: OBD }, (() => {
  const r = report();
  delete r.powerSource;
  return r;
})());
statusCase("obd revert marker {} while settings.json still holds obd → adjusted (CF74)", {}, { obd: OBD }, report());
statusCase("obd revert marker {} with settings.json clear → applied even though effective.obd exists (CF74)", {}, {}, report());
statusCase("obd revert marker {} without settings param semantics: no settings → applied", {}, UNDEF, report());
statusCase("obd clamped pollHz → adjusted (normalized)", { ...OBD, pollHz: 5 }, { obd: { ...OBD, pollHz: 2 } }, report({ pollHz: 2 }));
statusCase("obd all-invalid → rejected no valid fields", { adapterMac: "zz", pin: "12" }, {}, report());
statusCase("obd lowercase MAC is normalized → adjusted", { adapterMac: "aa:bb:cc:dd:ee:ff" }, { obd: { adapterMac: MAC } }, report());
add("obd delivered as an array → rejected not a valid object", "buildStatus", {
  desiredPayload: msg({ obd: [1] }),
  effective: { obd: report() },
  modem: FRESH,
  at: 42,
  hints: UNDEF,
  cameraMount: UNDEF,
  settings: { obd: OBD },
});

const tsCase = (name: string, hints: unknown): void =>
  add(name, "buildStatus", {
    desiredPayload: msg({ timeSync: { offlineSources: true } }),
    effective: { timeSync: { offlineSources: true } },
    modem: FRESH,
    at: 9,
    hints,
    cameraMount: UNDEF,
  });
tsCase("timeSync while the clock trust gate holds → held detail", { clockTrust: "held", clockStepperArmed: true });
tsCase("timeSync in clock-trust fallback → fallback detail", { clockTrust: "fallback", clockStepperArmed: true });
tsCase("timeSync held wins over a stood-down stepper", { clockTrust: "held", clockStepperArmed: false });
tsCase("timeSync trusted + stood-down stepper → the stepper detail", { clockTrust: "trusted", clockStepperArmed: false });
tsCase("timeSync trusted + armed stepper → applied, no detail", { clockTrust: "trusted", clockStepperArmed: true });

add("buildConfigReport carries effective.obd + status.obd + 4 capabilities", "buildConfigReport", {
  deviceId: "dev-1",
  ts: 11,
  origin: "boot",
  lastAppliedRev: 7,
  settings: { mode: "real", obd: OBD },
  modem: FRESH,
  mode: "real",
  desiredPayload: msg({ obd: OBD }),
  hints: { clockTrust: "trusted" },
  obd: report(),
  build: UNDEF,
});
add("buildConfigReport without the peripheral: no effective.obd, obd rejected", "buildConfigReport", {
  deviceId: "dev-1",
  ts: 12,
  origin: "cloud",
  lastAppliedRev: 7,
  settings: { obd: OBD },
  modem: NO_MODEM,
  mode: "real",
  desiredPayload: msg({ obd: OBD }),
  build: UNDEF,
});

add("namespacesOf includes obd last", "namespacesOf", { settings: { gnssAntBias: true, obd: OBD } });

const unheld = (name: string, payload: Record<string, unknown>, settings: unknown, baseline: unknown): void =>
  add(name, "unheldNamespaces", { desiredPayload: msg(payload), settings, baseline }, "src/agent/config-sync.ts:323-347 @ 09da38d");
unheld("never fires before a baseline exists", { obd: OBD }, {}, UNDEF);
unheld("obd carried, absent from settings.json AND baseline → adopted", { gnssAntBias: true, obd: OBD }, { gnssAntBias: true }, { gnssAntBias: true });
unheld("obd present in the baseline (deleted locally) → not adopted", { obd: OBD }, {}, { obd: OBD });
unheld("obd present in settings.json → not adopted", { obd: OBD }, { obd: { enabled: false } }, {});
unheld("null (the revert marker parse) is not carried", { obd: {} }, {}, {});
unheld("several never-held keys come back in registry order", { obd: OBD, timeSync: { offlineSources: false }, recording: { telemetry: { enabled: true } } }, {}, {});
unheld("an empty baseline object still counts as a baseline", { lteRecovery: { deadZoneProfile: "balanced" } }, {}, {});

for (const c of cases) {
  if (c.expectedJson === null && String(c.name).startsWith("[p5b]")) {
    c.expectedJson = await run(c.op as string, c.input as Record<string, unknown>);
  }
}

const out = {
  ...fixture,
  jsSourceFile:
    "test/config-sync.test.ts + test/config-sync-cellular.test.ts + test/camera-mount.test.ts (Phase 1 cases re-evaluated) + src/agent/config-sync.ts @ 09da38d ([p5b] cases)",
  jsCommit: "09da38dcc8b75c248625a3a0bee43b70e7def55d",
  generatedBy:
    "tools/replay/js/dump-p5b-spine-config-report.mts (Phase 5b WP1): every Phase 1 case (originally dumped by the Phase 1 WP4 scratchpad driver @ 89b5c64) re-evaluated through the real 09da38d function its op names, on its recorded input (settings via loadSettings; files via a tmp dir), plus the [p5b] cases",
  build,
  cases,
};
process.stdout.write(JSON.stringify(out, null, 2) + "\n");
process.stderr.write(`cases ${cases.length}; moved vs input fixture: ${moved.length}\n${moved.join("\n")}\n`);
rmSync(work, { recursive: true, force: true });
