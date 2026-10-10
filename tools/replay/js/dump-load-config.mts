// WP1 tsx driver: dumps loadConfig() (src/agent/config.ts:197-308) over env/argv/settings
// cases into fixtures/agent_config/load-config.json. Run from the scratch copy of the JS
// repo at 89b5c64:  cd <scratch>/js-89b5c64 && npx tsx <this file> > out.json
import { mkdirSync, mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const KEYS = [
  "DEVICE_ID", "IOT_DATA_ENDPOINT", "CERT_PATH", "KEY_PATH", "CA_PATH", "MODE", "RATE_HZ",
  "MQTT_KEEPALIVE_SEC", "OFFLINE", "KVS_RUNTIME_BIN", "KVS_RUNTIME_LIB_DIR", "KVS_MANIFEST",
  "MEDIA_SOURCE", "MEDIA_TYPE", "KVS_PIPELINE_DIR", "KVS_GST_PIPELINE", "AWS_KVS_LOG_LEVEL",
  "KVS_IDLE_TEARDOWN_SECS", "KVS_CACERT_PATH", "HARDWARE_MANIFEST", "XORGATE_REPLAY_ASSETS",
  "XORGATE_REPLAY_SCENARIO", "XORGATE_HOME",
];

const { loadConfig } = await import(join(process.cwd(), "src/agent/config.ts"));

const creds = "/tmp/xg-wp1-creds";
mkdirSync(creds, { recursive: true });
for (const f of ["cert.crt", "private.key", "ca.pem"]) writeFileSync(join(creds, f), "x");
const online = {
  DEVICE_ID: "dev-1", IOT_DATA_ENDPOINT: "abc-ats.iot.us-east-1.amazonaws.com",
  CERT_PATH: join(creds, "cert.crt"), KEY_PATH: join(creds, "private.key"), CA_PATH: join(creds, "ca.pem"),
};

type Case = { name: string; argv: string[]; env: Record<string, string>; settings: string | null };
const cases: Case[] = [
  { name: "offline defaults (no env at all)", argv: ["--offline"], env: {}, settings: null },
  { name: "OFFLINE=1 env", argv: [], env: { OFFLINE: "1" }, settings: null },
  { name: "OFFLINE=true env + MODE=real", argv: [], env: { OFFLINE: "true", MODE: "real" }, settings: null },
  { name: "OFFLINE=yes is not truthy → DEVICE_ID required", argv: [], env: { OFFLINE: "yes" }, settings: null },
  { name: "online minimal (bench shape)", argv: [], env: { ...online, MODE: "real" }, settings: null },
  { name: "online, missing DEVICE_ID", argv: [], env: { ...online, DEVICE_ID: "" }, settings: null },
  { name: "online, unset DEVICE_ID", argv: [], env: { IOT_DATA_ENDPOINT: "e", CERT_PATH: online.CERT_PATH, KEY_PATH: online.KEY_PATH, CA_PATH: online.CA_PATH }, settings: null },
  { name: "online, empty endpoint", argv: [], env: { ...online, IOT_DATA_ENDPOINT: "" }, settings: null },
  { name: "mode precedence: settings.json beats MODE", argv: ["--offline"], env: { MODE: "real" }, settings: "{\"mode\":\"sim\"}" },
  { name: "mode precedence: --mode beats settings.json", argv: ["--offline", "--mode", "real"], env: { MODE: "sim" }, settings: "{\"mode\":\"sim\"}" },
  { name: "mode precedence: MODE when settings has none", argv: ["--offline"], env: { MODE: "replay" }, settings: "{\"gnssAntBias\":true}" },
  { name: "mode precedence: invalid settings mode ignored", argv: ["--offline"], env: { MODE: "real" }, settings: "{\"mode\":\"bogus\"}" },
  { name: "mode precedence: corrupt settings.json", argv: ["--offline"], env: { MODE: "real" }, settings: "{not json" },
  { name: "invalid MODE env", argv: ["--offline"], env: { MODE: "fast" }, settings: null },
  { name: "empty MODE env", argv: ["--offline"], env: { MODE: "" }, settings: null },
  { name: "--mode with no value is boolean → falls through to env", argv: ["--offline", "--mode"], env: { MODE: "real" }, settings: null },
  { name: "RATE_HZ 2 → interval 500", argv: ["--offline"], env: { RATE_HZ: "2" }, settings: null },
  { name: "RATE_HZ 3 → interval 333", argv: ["--offline"], env: { RATE_HZ: "3" }, settings: null },
  { name: "RATE_HZ 0.4 → interval 2500", argv: ["--offline"], env: { RATE_HZ: "0.4" }, settings: null },
  { name: "RATE_HZ 1e3 → interval 1", argv: ["--offline"], env: { RATE_HZ: "1e3" }, settings: null },
  { name: "RATE_HZ ' 2 ' (Number trims)", argv: ["--offline"], env: { RATE_HZ: " 2 " }, settings: null },
  { name: "RATE_HZ 0x2 (hex)", argv: ["--offline"], env: { RATE_HZ: "0x2" }, settings: null },
  { name: "RATE_HZ empty → 0 → error", argv: ["--offline"], env: { RATE_HZ: "" }, settings: null },
  { name: "RATE_HZ abc → NaN → error", argv: ["--offline"], env: { RATE_HZ: "abc" }, settings: null },
  { name: "RATE_HZ -1 → error", argv: ["--offline"], env: { RATE_HZ: "-1" }, settings: null },
  { name: "RATE_HZ Infinity → error", argv: ["--offline"], env: { RATE_HZ: "Infinity" }, settings: null },
  { name: "--rate flag beats RATE_HZ", argv: ["--offline", "--rate", "4"], env: { RATE_HZ: "2" }, settings: null },
  { name: "MQTT_KEEPALIVE_SEC 60.0 (CF3)", argv: ["--offline"], env: { MQTT_KEEPALIVE_SEC: "60.0" }, settings: null },
  { name: "MQTT_KEEPALIVE_SEC 30", argv: ["--offline"], env: { MQTT_KEEPALIVE_SEC: "30" }, settings: null },
  { name: "MQTT_KEEPALIVE_SEC 0.5 is accepted by the TS", argv: ["--offline"], env: { MQTT_KEEPALIVE_SEC: "0.5" }, settings: null },
  { name: "MQTT_KEEPALIVE_SEC 0 → error", argv: ["--offline"], env: { MQTT_KEEPALIVE_SEC: "0" }, settings: null },
  { name: "MQTT_KEEPALIVE_SEC x → error", argv: ["--offline"], env: { MQTT_KEEPALIVE_SEC: "x" }, settings: null },
  { name: "relative paths resolve against the cwd", argv: ["--offline"], env: { CERT_PATH: "a/../b/cert.crt", KVS_MANIFEST: "./etc/kvs.json", HARDWARE_MANIFEST: "hw.json", KVS_PIPELINE_DIR: "/etc/xorgate//pipelines/" }, settings: null },
  { name: "KVS_RUNTIME_BIN moves the default CA bundle", argv: ["--offline"], env: { KVS_RUNTIME_BIN: "/opt/x/media/bin/kvs" }, settings: null },
  { name: "KVS_CACERT_PATH + KVS_GST_PIPELINE set", argv: ["--offline"], env: { KVS_CACERT_PATH: "/etc/ssl/kvs.pem", KVS_GST_PIPELINE: "p.gst" }, settings: null },
  { name: "KVS_GST_PIPELINE empty is unset", argv: ["--offline"], env: { KVS_GST_PIPELINE: "" }, settings: null },
  { name: "replay knobs", argv: ["--offline"], env: { XORGATE_REPLAY_ASSETS: "/assets/v2", XORGATE_REPLAY_SCENARIO: "demo-01" }, settings: null },
  { name: "flag overrides for identity + media", argv: ["--offline", "--device-id", "flag-dev", "--endpoint", "flag-ep", "--media-manifest", "/m/kvs.json", "--hardware", "/h.json", "--media-source", "devicesrc"], env: { DEVICE_ID: "env-dev" }, settings: null },
  { name: "a flag value that starts with -- is consumed", argv: ["--offline", "--device-id", "--endpoint", "x"], env: {}, settings: null },
  { name: "positional args are ignored", argv: ["run", "--offline", "stray"], env: {}, settings: null },
  { name: "--offline=1 is not the boolean flag", argv: ["--offline=1"], env: {}, settings: null },
  { name: "OFFLINE empty string", argv: [], env: { ...online, OFFLINE: "" }, settings: null },
];

const out: unknown[] = [];
// A neutral cwd for path.resolve (the fixture feeds the same one to Rust).
process.chdir("/usr");
const cwd = process.cwd();
for (const c of cases) {
  const home = mkdtempSync(join(tmpdir(), "xg-home-"));
  for (const k of KEYS) delete process.env[k];
  process.env.XORGATE_HOME = home;
  for (const [k, v] of Object.entries(c.env)) process.env[k] = v;
  if (c.settings !== null) writeFileSync(join(home, "settings.json"), c.settings);
  let expected: unknown;
  try {
    const cfg = loadConfig(c.argv);
    const { creds: _creds, media, ...rest } = cfg;
    const { streamGuard: _sg, ...mediaRest } = media;
    expected = { ok: { ...rest, media: mediaRest } };
  } catch (err) {
    expected = { error: (err as Error).message };
  }
  out.push({ name: c.name, input: { argv: c.argv, env: c.env, settingsJson: c.settings, cwd }, expected });
  rmSync(home, { recursive: true, force: true });
}
rmSync(creds, { recursive: true, force: true });
console.log(JSON.stringify({
  jsSourceFile: "src/agent/config.ts (loadConfig :197-308, parseArgs :173-191) + src/daemon/runtime.ts",
  jsCommit: "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  generatedBy: "npx tsx dump-load-config.mts in a git-archive scratch copy of 89b5c64 (phase-2 WP1; script handed to WP7 via evidence/wp1.md)",
  cases: out,
}, null, 2));
