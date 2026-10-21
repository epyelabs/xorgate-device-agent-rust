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
  ["partial both", "echo partial-out; echo partial-err >&2; sleep 30"],
  ["stdout only", "echo only-out; sleep 30"],
  ["silent", "sleep 30"],
  ["ignores TERM", "trap '' TERM; echo stubborn; sleep 30"],
  ["TERM trap prints", "trap 'echo got-term >&2; exit 7' TERM; echo waiting; sleep 30 & wait"],
];
const out = [];
for (const [name, script] of cases) {
  const t0 = Date.now();
  const res = await run("sh", ["-c", script], { timeoutMs: 300 });
  out.push({ name, elapsedMs: Date.now() - t0, res, detail: detail(res) });
}
console.log(JSON.stringify(out, null, 2));
