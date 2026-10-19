// Phase 4 WP5: the TS `manage` screens of the 3 `test/monitor.test.tsx` it()s
// (ink-testing-library, 80 columns, no colour) printed as text, for the
// behaviour/screen comparison against the ratatui port
// (crates/xorgate-agent/tests/manage_tui.rs dumps the Rust frames with
// WP5_DUMP_FRAMES=1). Evidence only — not a fixture (screens differ by
// layout engine; Q10 changes the control texts).
//
//   FORCE_COLOR=0 tools/replay/js/run-tsx.sh tools/replay/js/dump-monitor-frames.mts
import { createElement } from "react";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
process.env.XORGATE_HOME = mkdtempSync(join(tmpdir(), "xg-mon-"));
process.env.XORGATE_MOCK = "1";
const { render } = await import("ink-testing-library");
const { MonitorApp } = await import(join(process.cwd(), "src/monitor/MonitorApp.tsx"));
const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));
const now = Date.now();
const record = { pid: 4242, startedAt: now - 1000, deviceId: "dev-abc-123", mode: "sim", rateHz: 1, offline: true, worker: "/x/dist/worker.js", argv: ["--offline"], nodeVersion: "v20" };
const snapshot = { v: 1, writtenBy: "test", deviceId: "dev-abc-123", mode: "sim", rateHz: 1, endpoint: "offline", offline: true, pid: 4242, startedAt: now, lastWriteAt: now, connection: "connected", connectionSince: now, seq: 12, published: 12, publishErrors: 0, lastPublishAt: now, lastError: null,
  latest: { v: 1, deviceId: "dev-abc-123", ts: now, seq: 12, metrics: {
    imu: { quat_r: { value: 0.92 }, quat_i: { value: 0.33 }, quat_j: { value: 0.07 }, quat_k: { value: 0.19 }, euler_roll: { value: 40.1, unit: "°" }, euler_pitch: { value: -0.3, unit: "°" }, euler_yaw: { value: 23.6, unit: "°" }, accel_x: { value: 0.01, unit: "m/s^2" }, accel_y: { value: -0.02, unit: "m/s^2" }, accel_z: { value: 0.03, unit: "m/s^2" } },
    gps: { lat: { value: 37.7749 }, lon: { value: -122.4194 }, alt: { value: 20, unit: "m" }, speed: { value: 0, unit: "km/h" } },
    lte: { rssi: { value: -67, unit: "dBm" }, quality: { value: 18, unit: "asu" } },
    system: { cpu_usage: { value: 31, unit: "%" }, cpu_temp: { value: 54, unit: "°C" }, ram_usage: { value: 47, unit: "%" }, disk_usage: { value: 38, unit: "%" } } },
    media: { kvs: { value: false }, recording: { value: false }, streams: [] } },
  recentEvents: [{ ts: now, level: "info", msg: "connected to offline" }] };
const running = { kind: "running", record, identity: "verified", state: snapshot, stateFresh: true };
const show = (title: string, f: string | undefined) => console.log(`----- ${title} -----\n${f ?? ""}\n`);

let r = render(createElement(MonitorApp, { agentArgs: ["--offline"], inspector: async () => running }));
await delay(150); show("it:87 first frame", r.lastFrame());
r.stdin.write("j"); r.stdin.write("j"); r.stdin.write("j"); await delay(80); show("it:87 after jjj (GPS)", r.lastFrame());
r.unmount();

r = render(createElement(MonitorApp, { agentArgs: ["--offline"], inspector: async () => running }));
await delay(150);
for (let i = 0; i < 6; i++) r.stdin.write("j");
await delay(80); show("it:122 Settings", r.lastFrame());
r.stdin.write("\r"); await delay(60); show("it:122 operating", r.lastFrame());
r.stdin.write(" "); await delay(60); show("it:122 space (real)", r.lastFrame());
r.stdin.write("j"); await delay(60); show("it:122 j (wraps; wifi not present)", r.lastFrame());
r.unmount();

r = render(createElement(MonitorApp, { agentArgs: [], inspector: async () => ({ kind: "stopped" }) }));
await delay(150); show("it:167 not running", r.lastFrame());
r.unmount();
process.exit(0);
