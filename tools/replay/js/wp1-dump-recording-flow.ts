/**
 * WP1 (Phase 3) driver: dumps fixtures/recording_flow/manager.json by running
 * the REAL RecordingManager @ 89b5c64 (real VideoCaptureSupervisor over a fake
 * spawn, real BootDeferPolicy over temp budget/telemetry files, real
 * LoadShedPolicy) through scenario scripts that mirror the 26 RecordingManager
 * it()s of test/recording-video-capture.test.ts. Deterministic: one fake clock
 * (wall + mono) feeds both policies and Date.now; shed ticks and settings
 * polls are driven explicitly (timers set to 1 h).
 *
 *   cd <scratch js copy> && npx tsx wp1-dump-recording-flow.ts <scenarios.json> <out.json>
 */
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [scenFile, outFile] = process.argv.slice(2);
const scenarios = JSON.parse(readFileSync(scenFile, "utf8"));

const clock = { wall: 1_760_000_000_000, mono: 5_000_000 };
const realNow = Date.now;
Date.now = () => clock.wall;

const settle = async (m: any) => {
  for (let i = 0; i < 20; i++) {
    await m.videoTransition;
    await new Promise((r) => setImmediate(r));
  }
};

async function runScenario(sc: any) {
  const home = mkdtempSync(join(tmpdir(), "wp1-flow-"));
  process.env.XORGATE_HOME = home;
  // Fresh module graph per scenario is not needed: settings/paths read XORGATE_HOME at call time.
  const { saveSettings } = await import("./src/daemon/settings.js");
  const { RecordingManager } = await import("./src/recording/manager.js");
  const { BootDeferPolicy, LoadShedPolicy } = await import("./src/recording/boot-policy.js");

  const camNames: Record<string, string> = {};
  const cams = (sc.cameras ?? ["cam0"]).map((k: string) => {
    camNames[`/base/${k}@1a`] = k;
    return { streamKey: k, cameraName: `/base/${k}@1a` };
  });
  const spawns: { cam: string; record: boolean; stopped: string | null }[] = [];
  const spawn = (_cmd: string, args: string[]) => {
    let resolveDone: (r: any) => void = () => {};
    const done = new Promise<any>((res) => (resolveDone = res));
    const joined = args.join(" ");
    const name = Object.keys(camNames).find((n) => joined.includes(`camera-name=${n}`));
    const entry = { cam: name ? camNames[name] : "?", record: joined.includes("splitmuxsink"), stopped: null as string | null };
    spawns.push(entry);
    return {
      done,
      stop: (sig: string = "SIGINT") => {
        if (entry.stopped === null) entry.stopped = sig;
        resolveDone({ stdout: "", stderr: "", exitCode: 0, failed: false, notFound: false });
      },
    };
  };

  const budgetPath = join(home, "max_current");
  if (sc.budgetMa !== null && sc.budgetMa !== undefined) {
    const b = Buffer.alloc(4);
    b.writeUInt32BE(sc.budgetMa, 0);
    writeFileSync(budgetPath, b);
  }
  const telemetryFile = join(home, "telemetry.json");
  let gnss: any = sc.gnss ?? null;
  let reading: any = sc.reading ?? null;
  const events: { level: string; msg: string }[] = [];
  const captureEvents: string[] = [];
  const liveShed: boolean[] = [];
  const disabled: string[][] = [];
  let sampleCalls = 0;

  if (sc.settings) saveSettings({ recording: sc.settings });
  const m: any = new RecordingManager({
    deviceId: "dev-1",
    sampleGroups: async () => ({}),
    timeSource: { current: () => "ntp", refresh: async () => "ntp" },
    root: join(home, "recordings"),
    pollMs: 3_600_000,
    shedSampleMs: 3_600_000,
    bootPolicy: new BootDeferPolicy({
      telemetryFile,
      budgetPath,
      now: () => clock.wall,
      monoMs: () => clock.mono,
      ...(sc.gnssWired === false ? {} : { gnss: () => gnss }),
    }),
    shedPolicy: new LoadShedPolicy({ monoMs: () => clock.mono }),
    powerSample:
      sc.powerSample === false
        ? undefined
        : async () => {
            sampleCalls++;
            clock.mono += 1000;
            clock.wall += 1000;
            return reading;
          },
    onEvent: (level: string, msg: string) => {
      if (msg.startsWith("video capture ") || msg.startsWith("video bridge ")) captureEvents.push(msg);
      else events.push({ level, msg });
    },
    ...(sc.video === false
      ? {}
      : {
          video: {
            mediaEnabled: sc.mediaEnabled !== false,
            cameras: () => cams,
            shmDir: join(home, "shm"),
            spawn,
            stableMs: 3_600_000,
            sweepPollMs: 3_600_000,
            stallMs: 3_600_000,
            setLiveShed: (s: boolean) => liveShed.push(s),
            setDisabledCameras: (keys: string[]) => disabled.push([...keys]),
          },
        }),
  });

  let ev0 = 0, ls0 = 0, dc0 = 0, sp0 = 0;
  const out: any[] = [];
  for (const step of sc.steps) {
    const extra: any = {};
    switch (step.op) {
      case "start":
        await m.start();
        break;
      case "settings":
        saveSettings({ recording: step.recording });
        m.reapply();
        break;
      case "reapply":
        m.reapply();
        break;
      case "telemetry": {
        if (step.value === null) rmSync(telemetryFile, { force: true });
        else {
          const text = JSON.stringify({ timestamp: new Date(clock.wall).toISOString(), ...step.value });
          writeFileSync(telemetryFile, text);
          extra.wrote = text;
        }
        break;
      }
      case "gnss":
        gnss = step.value;
        break;
      case "reading":
        reading = step.value;
        break;
      case "advance":
        clock.mono += step.ms;
        clock.wall += step.ms;
        break;
      case "shedTicks":
        for (let i = 0; i < step.n; i++) {
          await m.shedTick();
          await settle(m);
        }
        break;
      default:
        throw new Error(`unknown op ${step.op}`);
    }
    await settle(m);
    const st = m.status();
    const status: any = { enabled: st.enabled, session: st.sessionId !== null };
    if ("bootDefer" in st) status.bootDefer = st.bootDefer;
    if ("powerShed" in st) status.powerShed = st.powerShed;
    if ("liveBlock" in st) status.liveBlock = st.liveBlock;
    out.push({
      ...step,
      ...extra,
      expected: {
        events: events.slice(ev0),
        liveShed: liveShed.slice(ls0),
        disabled: disabled.slice(dc0),
        spawns: spawns.slice(sp0).map((s) => ({ cam: s.cam, record: s.record })),
        live: spawns.filter((s) => s.stopped === null).map((s) => s.cam).sort(),
        status,
        videoKey: m.videoKey,
        clock: { wall: clock.wall, mono: clock.mono },
        sampleCalls,
      },
    });
    ev0 = events.length; ls0 = liveShed.length; dc0 = disabled.length; sp0 = spawns.length;
  }
  m.stop();
  await settle(m);
  rmSync(home, { recursive: true, force: true });
  const { steps: _s, ...rest } = sc;
  return { ...rest, steps: out, captureEvents };
}

const cases: any[] = [];
for (const sc of scenarios) {
  clock.wall = 1_760_000_000_000;
  clock.mono = 5_000_000;
  cases.push(await runScenario(sc));
}
Date.now = realNow;
writeFileSync(
  outFile,
  JSON.stringify(
    {
      jsSourceFile: "src/recording/manager.ts",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy:
        "tsx wp1-dump-recording-flow.ts (phase-3/evidence/wp1-tools) in a scratch `git archive 89b5c64` copy: the real RecordingManager + VideoCaptureSupervisor (fake spawn) + BootDeferPolicy/LoadShedPolicy on one fake clock, scenarios mirroring test/recording-video-capture.test.ts RecordingManager it()s; capture-supervisor events (`video capture …`, `video bridge …`) are WP4's and listed separately",
      cases,
    },
    null,
    1,
  ) + "\n",
);
console.log(`wrote ${cases.length} cases`);
