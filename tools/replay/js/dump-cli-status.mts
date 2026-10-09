// WP6 tsx driver: runs the REAL `statusCommand` (src/commands/status.ts:28-120
// over manager.ts inspect :105-120) over pidfile × state.json scenarios and
// records the stdout lines + exit code. Run from a scratch copy of 89b5c64,
// non-root, stdout NOT a TTY (no ANSI):
//   cd <scratch>/js-89b5c64 && npx tsx <this file> > fixtures/cli_set/status-render.json
// The live pid is this process; "verified" records name this script in
// `worker` (ps sees it in the command line), "reused" ones name a path ps
// never shows. Normalised in the output: this pid → <PID>, the build line →
// <BUILD>, the scratch home → <HOME>. Date.now is fixed.
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";

const NOW = 2_000_000_000_000;
Date.now = () => NOW;
const { statusCommand } = await import(join(process.cwd(), "src/commands/status.ts"));
const { formatBuildInfo } = await import(join(process.cwd(), "src/lib/build-info.ts"));
const me = basename(process.argv[1]);
const DEAD = 2147483000;

const rec = (o: Record<string, unknown> = {}) => ({ pid: process.pid, startedAt: NOW - 3_725_000, deviceId: "dev-1", mode: "real", rateHz: 1, offline: false, worker: `/x/${me}`, argv: [], nodeVersion: "v20", ...o });
const st = (o: Record<string, unknown> = {}) => ({ v: 1, writtenBy: "test", deviceId: "dev-1", mode: "real", rateHz: 1, endpoint: "abc-ats.iot.us-east-1.amazonaws.com", offline: false, pid: process.pid, startedAt: NOW - 3_725_000, lastWriteAt: NOW - 400, connection: "connected", connectionSince: NOW - 3_600_000, seq: 3144, published: 3145, publishErrors: 0, lastPublishAt: NOW - 300, lastError: null, latest: null, recentEvents: [], ...o });

type Case = { name: string; pidfile?: unknown; state?: unknown; stateText?: string; identity: "verified" | "reused" | "n/a"; denied?: boolean };
const cases: Case[] = [
  { name: "stopped (no pidfile)", identity: "n/a" },
  { name: "unreadable home (EACCES)", identity: "n/a", denied: true },
  { name: "stale: process gone", pidfile: rec({ pid: DEAD }), identity: "n/a" },
  { name: "stale: pid recycled", pidfile: rec({ worker: "/nonexistent/zz-not-here.js" }), identity: "reused" },
  { name: "running, no state snapshot", pidfile: rec(), identity: "verified" },
  { name: "running, corrupt state.json", pidfile: rec(), stateText: "{nope", identity: "verified" },
  { name: "running, state failing the structural gate", pidfile: rec(), state: { ...st(), v: 2 }, identity: "verified" },
  { name: "running, JS-agent shape (recording on, video, last error)", pidfile: rec(), identity: "verified", state: st({ recording: { enabled: true, sessionId: "019f95d1-aaaa-bbbb", backlogBytes: 5_368_709_120, backlogSegments: 12, droppedSegments: 2, uploading: false, lastUploadAt: null }, latest: { media: { kvs: false, streams: [{ streamKey: "cam0", recording: false }, { streamKey: "cam1", recording: true }, { streamKey: "cam2" }] } }, lastError: "read ECONNRESET" }) },
  { name: "running, Rust-agent shape (no recording block, never published)", pidfile: rec({ argv: ["run"], worker: `/x/${me}` }), identity: "verified", state: st({ connection: "reconnecting", connectionSince: NOW - 9_500, lastPublishAt: null, published: 0, seq: 0 }) },
  { name: "running, offline 2 Hz, stale state", pidfile: rec({ offline: true, rateHz: 2, mode: "sim", startedAt: NOW - 200_000_000 }), identity: "verified", state: st({ endpoint: "offline", lastWriteAt: NOW - 4_000, connection: "connected", connectionSince: NOW - 45_000, lastPublishAt: NOW - 4_000 }) },
  { name: "running, recording off with backlog", pidfile: rec({ startedAt: NOW - 59_000 }), identity: "verified", state: st({ recording: { enabled: false, sessionId: null, backlogBytes: 1536, backlogSegments: 3, droppedSegments: 0 } }) },
  { name: "running, recording off, dropped only", pidfile: rec({ startedAt: NOW - 5_000 }), identity: "verified", state: st({ recording: { enabled: false, backlogBytes: 0, backlogSegments: 0, droppedSegments: 4 } }) },
  { name: "running, recording on, no session, tiny backlog", pidfile: rec(), identity: "verified", state: st({ recording: { enabled: true, sessionId: null, backlogBytes: 512, backlogSegments: 1, droppedSegments: 0 } }) },
  { name: "running, odd values (missing endpoint/errors, 0 rateHz)", pidfile: rec({ rateHz: 0, startedAt: NOW + 5_000 }), identity: "verified", state: (() => { const s: Record<string, unknown> = st({ connectionSince: NOW + 10, lastPublishAt: NOW - 90_000_000 }); delete s.endpoint; delete s.publishErrors; return s; })() },
  { name: "running, fractional rate + float seq", pidfile: rec({ rateHz: 0.5 }), identity: "verified", state: st({ seq: 1.5, lastWriteAt: NOW - 6_000, lastPublishAt: NOW - 7_250 }) },
];

const out: unknown[] = [];
for (const c of cases) {
  const home = mkdtempSync(join(tmpdir(), "xg-wp6-status-"));
  process.env.XORGATE_HOME = home;
  if (c.pidfile) writeFileSync(join(home, "agent.pid"), JSON.stringify(c.pidfile));
  if (c.state) writeFileSync(join(home, "state.json"), JSON.stringify(c.state));
  if (c.stateText) writeFileSync(join(home, "state.json"), c.stateText);
  if (c.denied) chmodSync(home, 0o000);
  const lines: string[] = [];
  const ol = console.log;
  console.log = (...a: unknown[]) => lines.push(a.join(" "));
  process.exitCode = undefined;
  try {
    await statusCommand();
  } finally {
    console.log = ol;
  }
  const norm = (s: string) => s.split(home).join("<HOME>").split(formatBuildInfo()).join("<BUILD>").split(`pid ${process.pid}`).join("pid <PID>");
  out.push({
    name: c.name,
    input: { pidfile: c.pidfile ?? null, state: c.state ?? null, stateText: c.stateText ?? null, identity: c.identity, homeDenied: !!c.denied, now: NOW },
    expected: { lines: lines.map(norm), exitCode: process.exitCode ?? 0 },
  });
  process.exitCode = undefined;
  if (c.denied) chmodSync(home, 0o700);
  rmSync(home, { recursive: true, force: true });
}
process.stdout.write(JSON.stringify({
  jsSourceFile: "src/commands/status.ts:28-120 (print) over src/daemon/manager.ts inspect :105-120, src/lib/format.ts (uptimeLabel, ageLabel, formatBytes)",
  jsCommit: "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  generatedBy: "npx tsx dump-cli-status.mts in a git-archive scratch copy of 89b5c64, non-root, stdout not a TTY (phase-2 WP6; script in evidence/wp6-tools, handed to WP7)",
  cases: out,
}, null, 2) + "\n");
