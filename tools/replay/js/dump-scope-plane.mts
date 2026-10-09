// WP1 tsx driver: the scope-wiring scenario of the Rust plane test
// (crates/xorgate-agent/tests/plane.rs), run on the REAL TelemetryScope with the
// worker's noteScopeChange wording (worker.ts:739-779, copied verbatim below).
// Timeline (ms from 0): applyScope(WS_A) at 0; cycles every 3300 ms: connect at t,
// one publish at t+1000 (only until the first fallback), close at t+1300; after the
// fallback: connect once more and tick every 1000 ms until the re-probe fires.
import { join } from "node:path";
const { TelemetryScope } = await import(join(process.cwd(), "src/agent/telemetry-scope.ts"));

const DEVICE = "dev-scope";
const WS_A = { organizationId: "org-1", workspaceId: "ws-a" };
const s = new TelemetryScope({ deviceId: DEVICE, unscopedTopic: `xorgate/devices/${DEVICE}/telemetry` });
const events: { level: string; msg: string; log: string; at: number }[] = [];
function noteScopeChange(change: any, now: number): void {
  if (change.kind === "scoped") {
    const how = change.reason === "reprobe" ? " (re-probe after fallback)" : "";
    events.push({ at: now, level: "info", log: `[agent] telemetry topic -> ${change.topic}${how}`,
      msg: `telemetry topic scoped to ws ${change.scope.workspaceId}${how}` });
  } else {
    const mins = Math.round(change.reprobeInMs / 60_000);
    events.push({ at: now, level: "warn",
      log: `[agent] telemetry topic FALLBACK -> ${change.topic}: the broker refused ` +
        `ws ${change.scope.workspaceId} on ${change.cycles} consecutive connections. ` +
        `Publishing on the unscoped topic (still ingested); re-probing in ~${mins} min.`,
      msg: `telemetry topic fallback to unscoped: ws ${change.scope.workspaceId} refused ` +
        `(${change.cycles} cycles); re-probe in ~${mins} min` });
  }
}
let c = s.applyScope(WS_A, 0); if (c) noteScopeChange(c, 0);
const topics: string[] = [s.topic];
let t = 0; let cycles = 0; let fell = false;
while (!fell && cycles < 1000) {
  s.noteConnect(t);
  s.noteTick(t + 1000); s.notePublish(); topics.push(s.topic);
  const ch = s.noteClose(t + 1300);
  if (ch) { noteScopeChange(ch, t + 1300); fell = true; }
  t += 3300; cycles++;
}
const fallbackCycle = cycles;
topics.push(s.topic);
s.noteConnect(t);
let ticks = 0; let reprobed = false;
while (!reprobed && ticks < 10_000) {
  ticks++;
  const ch = s.noteTick(t + ticks * 1000);
  if (ch) { noteScopeChange(ch, t + ticks * 1000); reprobed = true; }
}
topics.push(s.topic);
console.log(JSON.stringify({
  jsSourceFile: "src/agent/telemetry-scope.ts (TelemetryScope) + src/worker.ts:739-779 (noteScopeChange wording)",
  jsCommit: "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  generatedBy: "npx tsx dump-scope-plane.mts in a git-archive scratch copy of 89b5c64 (phase-2 WP1)",
  cases: [{ name: "badCycle loop through the plane → fallback → timed re-probe",
    input: { deviceId: DEVICE, scope: WS_A, cyclePeriodMs: 3300, closeAfterMs: 1300, publishAfterMs: 1000, tickMs: 1000 },
    expected: { events, fallbackAfterCycles: fallbackCycle, reprobeAfterTicks: ticks, topicsAfterScope: topics[0], topicAfterReprobe: topics[topics.length - 1] } }],
}, null, 2));
