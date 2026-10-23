// WP1 (Phase 4) dump driver: runs the REAL `ledCommand` (src/commands/led.ts
// @ 89b5c64) over the real `LedDriver` + `probeHost` (src/hardware/led.ts),
// with only the two host seams faked:
//   - src/lib/platform.js  isRaspberryPi() → host.pi (isMock() stays REAL: env + platform)
//   - src/lib/exec.js      commandExists()/run() → a fake host: `pinctrl` records
//                          argv + virtual ms, `gpioinfo gpiochip0` answers host.gpioinfo
// and vitest fake timers (virtual time), so `led test` (≈ 60 s) runs instantly
// and every pinctrl call carries its exact virtual timestamp.
//
// Run (scratch copy ONLY, never the JS repo):
//   cp zz-wp1-dump-led-cli.test.ts <rust>/tools/replay/.cache/js-89b5c64/test/
//   docker run --rm --platform linux/arm64 -e TZ=UTC -v <that dir>:/js -w /js node:20-bookworm \
//     npx --no-install vitest run test/zz-wp1-dump-led-cli.test.ts
// Output: test/zz-wp1-led-cli.json → fixtures/led_cli/led-cli.json in the Rust repo.
import { writeFileSync } from "node:fs";
import { afterAll, it, vi } from "vitest";

const host = vi.hoisted(() => ({
  pi: true,
  pinctrl: true,
  gpioinfo: null as string | null, // null = gpioinfo not installed
  gpioinfoFails: false,
  pinctrlFails: false,
  t0: 0,
  calls: [] as { t: number; argv: string[] }[],
}));

vi.mock("../src/lib/platform.js", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../src/lib/platform.js")>()),
  isRaspberryPi: () => host.pi,
}));

vi.mock("../src/lib/exec.js", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../src/lib/exec.js")>()),
  commandExists: async (cmd: string) =>
    cmd === "pinctrl" ? host.pinctrl : cmd === "gpioinfo" ? host.gpioinfo !== null : false,
  run: async (cmd: string, args: string[] = []) => {
    host.calls.push({ t: Date.now() - host.t0, argv: [cmd, ...args] });
    if (cmd === "gpioinfo") {
      return host.gpioinfoFails
        ? { stdout: "", stderr: "boom", exitCode: 1, failed: true, notFound: false }
        : { stdout: host.gpioinfo ?? "", stderr: "", exitCode: 0, failed: false, notFound: false };
    }
    const failed = cmd === "pinctrl" && host.pinctrlFails;
    return { stdout: "", stderr: "", exitCode: failed ? 1 : 0, failed, notFound: false };
  },
}));

const { ledCommand } = await import("../src/commands/led.js");

const FREE = [
  '\tline  12:      "GPIO12"       unused   input  active-high ',
  '\tline  16:      "GPIO16"       unused   input  active-high ',
  '\tline  21:      "GPIO21"       unused   input  active-high ',
].join("\n");
const FOREIGN = [
  '\tline  12:      "GPIO12"       unused   input  active-high ',
  '\tline  16:      "GPIO16"       "sdmode"  output  active-high [used]',
  '\tline  21:      "GPIO21"       unused   input  active-high ',
].join("\n");
// libgpiod 1.6 on the bench while the Rust agent holds the lines (CF50).
const OWN = [
  '\tline  12:      unnamed  "xorgate-rs"  output  active-high [used]',
  '\tline  16:      unnamed  "xorgate-rs"  output  active-high [used]',
  '\tline  21:      unnamed  "xorgate-rs"  output  active-high [used]',
].join("\n");

type Case = {
  name: string;
  args: string[];
  host?: Partial<typeof host>;
  env?: Record<string, string | undefined>;
  /** Emit SIGINT after this many virtual ms instead of running to completion. */
  signalAtMs?: number;
};

const cases: Case[] = [
  { name: "no args → usage", args: [] },
  { name: "help → usage", args: ["help"] },
  { name: "unknown colour", args: ["nope"] },
  { name: "unknown pattern", args: ["red", "nope"] },
  { name: "flags are ignored when picking positionals", args: ["--x", "off", "-q"] },
  { name: "led red (solid, latched)", args: ["red"] },
  { name: "led off", args: ["off"] },
  { name: "led white flash2", args: ["white", "flash2"] },
  { name: "led red blink-fast, SIGINT after the first step", args: ["red", "blink-fast"], signalAtMs: 100 },
  { name: "led purple heartbeat, SIGINT mid-cycle", args: ["purple", "heartbeat"], signalAtMs: 400 },
  { name: "led boot", args: ["boot"] },
  { name: "led exit-state, SERVICE_RESULT unset", args: ["exit-state"], env: { SERVICE_RESULT: undefined } },
  { name: "led exit-state, SERVICE_RESULT=success", args: ["exit-state"], env: { SERVICE_RESULT: "success" } },
  { name: "led exit-state, SERVICE_RESULT=signal", args: ["exit-state"], env: { SERVICE_RESULT: "signal" } },
  { name: "led exit-state, SERVICE_RESULT=exit-code", args: ["exit-state"], env: { SERVICE_RESULT: "exit-code" } },
  { name: "led exit-state, SERVICE_RESULT empty", args: ["exit-state"], env: { SERVICE_RESULT: "" } },
  { name: "led test (walk the ladder)", args: ["test"] },
  { name: "inert: mock mode", args: ["red"], env: { XORGATE_MOCK: "1" } },
  { name: "inert: not a Raspberry Pi", args: ["boot"], host: { pi: false } },
  { name: "inert: pinctrl missing", args: ["exit-state"], host: { pinctrl: false }, env: { SERVICE_RESULT: "signal" } },
  { name: "gpioinfo all free", args: ["green"], host: { gpioinfo: FREE } },
  { name: "gpioinfo fails → busy check skipped", args: ["green"], host: { gpioinfo: FOREIGN, gpioinfoFails: true } },
  { name: "inert: foreign consumer holds GPIO16", args: ["green"], host: { gpioinfo: FOREIGN } },
  { name: "own consumer xorgate-rs holds all three (TS refuses; Rust CF50 does not)", args: ["green"], host: { gpioinfo: OWN } },
  { name: "pinctrl write fails → blanked, logged once", args: ["cyan"], host: { pinctrlFails: true } },
  { name: "valid colour, pattern off is not a pattern", args: ["blue", "off"] },
];

const out: unknown[] = [];

it("dumps", async () => {
  vi.useFakeTimers();
  for (const c of cases) {
    Object.assign(host, {
      pi: true,
      pinctrl: true,
      gpioinfo: null,
      gpioinfoFails: false,
      pinctrlFails: false,
      ...c.host,
    });
    host.calls = [];
    const savedEnv: Record<string, string | undefined> = {};
    const env = { XORGATE_MOCK: "0", ...c.env };
    for (const [k, v] of Object.entries(env)) {
      savedEnv[k] = process.env[k];
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
    const lines: { fd: 1 | 2; text: string }[] = [];
    const logSpy = vi.spyOn(console, "log").mockImplementation((...a: unknown[]) => {
      lines.push({ fd: 1, text: a.map(String).join(" ") });
    });
    const errSpy = vi.spyOn(console, "error").mockImplementation((...a: unknown[]) => {
      lines.push({ fd: 2, text: a.map(String).join(" ") });
    });
    let exitCalled: number | null = null;
    const exitSpy = vi.spyOn(process, "exit").mockImplementation(((code?: number) => {
      exitCalled = code ?? 0;
    }) as never);
    const before = new Map(
      (["SIGINT", "SIGTERM", "SIGHUP"] as const).map((s) => [s, process.listeners(s)]),
    );
    process.exitCode = undefined;
    host.t0 = Date.now();

    const p = ledCommand(c.args);
    if (c.signalAtMs !== undefined) {
      await vi.advanceTimersByTimeAsync(c.signalAtMs);
      process.emit("SIGINT", "SIGINT");
      await vi.advanceTimersByTimeAsync(10);
    } else {
      await vi.runAllTimersAsync();
    }
    await p;
    const endMs = Date.now() - host.t0;

    for (const [s, ls] of before) {
      for (const l of process.listeners(s)) if (!ls.includes(l)) process.off(s, l);
    }
    logSpy.mockRestore();
    errSpy.mockRestore();
    exitSpy.mockRestore();
    for (const [k, v] of Object.entries(savedEnv)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
    out.push({
      name: c.name,
      input: { args: c.args, host: c.host ?? {}, env: c.env ?? {}, signalAtMs: c.signalAtMs ?? null },
      expected: {
        lines,
        exitCode: exitCalled ?? (process.exitCode as number | undefined) ?? 0,
        processExitCalled: exitCalled !== null,
        calls: host.calls,
        endMs,
      },
    });
    process.exitCode = undefined;
  }
  vi.useRealTimers();
});

afterAll(() => {
  writeFileSync(
    "test/zz-wp1-led-cli.json",
    JSON.stringify(
      {
        jsSourceFile: "src/commands/led.ts",
        jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
        generatedBy:
          "plans/device-agent-js-to-rust/phase-4/evidence/wp1-tools/zz-wp1-dump-led-cli.test.ts (vitest, fake timers) in a git-archive scratch copy of 89b5c64: the REAL ledCommand + LedDriver + probeHost; only isRaspberryPi and exec.run/commandExists faked (pinctrl records argv + virtual ms; gpioinfo answers host.gpioinfo); console.log → fd 1, console.error → fd 2; process.exit mocked (recorded, not taken)",
        cases: out,
      },
      null,
      1,
    ) + "\n",
  );
});
