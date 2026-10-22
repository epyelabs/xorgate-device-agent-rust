// Phase 4 WP3 review S2 — what `run(cmd, args, { timeoutMs })` (src/lib/exec.ts
// @89b5c64, execa 9.6.1 reject:false) resolves to when the child outlives the
// timeout, and when the force-kill lands (SIGTERM → 5 s → SIGKILL), plus the
// `applyRotationViaHelper` detail (boot-config.ts:291-292) built from it.
//   docker run --rm --platform linux/arm64 -e TZ=UTC -v <scratch js>:/js -w /js node:20-bookworm \
//     npx --no-install tsx .rig-drivers/probe-exec-timeout.mts
import { run } from "../src/lib/exec.ts";
const detail = (res: { stderr: string; stdout: string; exitCode: number | null }) =>
  res.stderr.trim() || res.stdout.trim() || `exit ${res.exitCode}`;
const cases: [string, string][] = [
  ["partial both", "echo partial-out; echo partial-err >&2; exec sleep 30"],
  ["stdout only", "echo only-out; exec sleep 30"],
  ["silent", "exec sleep 30"],
  ["TERM trap exits 7, a grandchild holds the pipes 3 s", "trap 'echo got-term >&2; exit 7' TERM; echo waiting; sleep 3 & wait"],
  ["ignores TERM: SIGKILL after the 5 s grace", "trap '' TERM; echo stubborn; exec sleep 30"],
];
const out = await Promise.all(cases.map(async ([name, script]) => {
  const t0 = Date.now();
  const res = await run("sh", ["-c", script], { timeoutMs: 300 });
  return { name, script, timeoutMs: 300, elapsedMs: Date.now() - t0, res, detail: detail(res) };
}));
console.log(JSON.stringify({
  jsSourceFile: "src/lib/exec.ts + src/hardware/boot-config.ts:285-292",
  jsCommit: "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  generatedBy: "plans/device-agent-js-to-rust/phase-4/evidence/wp3-tools/probe-exec-timeout.mts (docker node:20-bookworm linux/arm64, execa 9.6.1 from package-lock; DEFAULT_FORCE_KILL_TIMEOUT = 5000)",
  cases: out,
}, null, 2));
