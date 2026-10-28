// Phase 5b CF78 (WP4): copied from plans/device-agent-js-to-rust/phase-2/evidence/wp5-tools/dump-wp5.ts so redump-all.sh
// re-runs it at any ref; only the JS root (cwd), the output dir ($OUT) and the jsCommit stamp
// ($JS_COMMIT_FULL) were changed.
// WP5 fixture dumper (phase-2 WP5). Run from a scratch copy of the JS repo @ 89b5c64:
//   cd <scratch>/js-89b5c64 && npx tsx <this file> <rust-repo>/fixtures
// Writes fixtures/s3/sign-put.json and fixtures/kvs_manifest/manifest.json.
import { mkdtempSync, writeFileSync, readFileSync, existsSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { encodeS3Key, signPut } from "./src/recording/s3-put.js";
import { loadManifest, persistUploadBucket } from "./src/media/manifest.js";

const out = resolve(process.argv[2] ?? "fixtures");
const JS_COMMIT = process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d";
const GEN = "npx tsx dump-wp5.ts in a git-archive scratch copy of the JS repo @ 89b5c64 (phase-2 WP5; script handed to WP7 via phase-2/evidence/wp5.md)";

// ---------- signPut ----------
const CREDS = { accessKeyId: "AKIDEXAMPLE", secretAccessKey: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", sessionToken: "TOKEN", expiration: 0 };
const LONG_TOKEN =
  "IQoJb3JpZ2luX2VjEJr//////////wEaCXVzLWVhc3QtMSJHMEUCIQDx+k3l9/abc=" +
  "FwoGZXIvYXdzEBYaDHhvcmdhdGUtZGV2LWNyZWRzIhEAAAAAAAAAAAAAAAAAAAAAAA==";
type In = { name: string; region: string; bucket: string; key: string; body: Buffer; bodyDesc: Record<string, unknown>; contentType: string; credentials: typeof CREDS; nowMs: number };
const b = (s: string) => ({ body: Buffer.from(s, "utf8"), bodyDesc: { bodyB64: Buffer.from(s, "utf8").toString("base64") } });
const base = { region: "us-east-1", bucket: "my-bucket", key: "telemetry/v1/dev/sess/1000-00000.jsonl.gz", contentType: "application/gzip", credentials: CREDS, nowMs: Date.UTC(2026, 6, 21, 12, 0, 0), ...b("hello") };
const THIRTY_MB = 30 * 1024 * 1024;
const signCases: In[] = [
  { name: "recording-s3.test.ts: builds a virtual-hosted request with all SigV4 headers", ...base },
  { name: "recording-s3.test.ts: is deterministic … sensitive to the body (other)", ...base, ...b("other") },
  { name: "sidecar json, dev bucket, 20261009T000000Z", ...base, bucket: "xorgate-dev-recordings", key: "media-sessions/v1/a73f7a36-0000-4000-8000-000000000000/cam0/019a0000-0000-7000-8000-000000000000/00000.json", contentType: "application/json", ...b('{"v":1,"segment":0}'), nowMs: Date.UTC(2026, 9, 9, 0, 0, 0) },
  { name: "key with spaces + unicode + emoji", ...base, key: "media sessions/v1/ünï©ødé/cam 0/😀 x/00000.json", nowMs: Date.UTC(2026, 9, 9, 13, 14, 15) },
  { name: "key with + = & ? # % ! ' ( ) * ~ ; , : @ $", ...base, key: "a+b=c/d&e?f#g%h/!'()*~/;,:@$/x y+z" },
  { name: "key with empty segments, leading slash, dot segments", ...base, key: "/lead//double/./dot/../up/" },
  { name: "empty body", ...base, ...b("") },
  { name: "30 MB body (content-length header only in the fixture)", ...base, body: Buffer.alloc(THIRTY_MB, 0x5a), bodyDesc: { bodyFill: { byte: 0x5a, len: THIRTY_MB } }, contentType: "video/mp4", key: "media-sessions/v1/dev/cam0/sess/00001.mp4" },
  { name: "realistic long session token", ...base, credentials: { ...CREDS, accessKeyId: "ASIAEXAMPLEEXAMPLE12", sessionToken: LONG_TOKEN } },
  { name: "leap day, ms truncated (2028-02-29T23:59:59.999Z)", ...base, nowMs: Date.UTC(2028, 1, 29, 23, 59, 59, 999) },
  { name: "leap day midnight (2024-02-29T00:00:00Z), eu-west-1, dotted bucket", ...base, region: "eu-west-1", bucket: "my.dotted.bucket", nowMs: Date.UTC(2024, 1, 29) },
  { name: "content-type with surrounding whitespace (trimmed only in the canonical form)", ...base, contentType: "  application/json; charset=utf-8 " },
  { name: "non-ascii body bytes", ...base, ...b("héllo 😀\n\u0000") },
  { name: "secret with symbols", ...base, credentials: { ...CREDS, secretAccessKey: "a/b+c=d!e~f" } },
  { name: "year-end rollover 2026-12-31T23:59:59Z", ...base, nowMs: Date.UTC(2026, 11, 31, 23, 59, 59) },
];
const cases: unknown[] = [];
for (const c of signCases) {
  const signed = signPut({ region: c.region, bucket: c.bucket, key: c.key, body: c.body, contentType: c.contentType, credentials: c.credentials, nowMs: c.nowMs });
  cases.push({
    name: c.name,
    op: "signPut",
    input: { region: c.region, bucket: c.bucket, key: c.key, ...c.bodyDesc, contentType: c.contentType, credentials: { accessKeyId: c.credentials.accessKeyId, secretAccessKey: c.credentials.secretAccessKey, sessionToken: c.credentials.sessionToken }, nowMs: c.nowMs },
    expected: { host: signed.host, path: signed.path, headers: Object.entries(signed.headers) },
  });
}
for (const k of [
  "a/b c/d+e*f", // recording-s3.test.ts: encodes reserved characters in keys but keeps slashes
  "", "/", "a//b/", "!'()*", "-_.~", "AZaz09", "ü/é/😀", "%", "%25", "a b+c=d&e?f#g", "\u007f\u0001", "  ", "..", "./x",
]) {
  cases.push({ name: `encodeS3Key ${JSON.stringify(k)}`, op: "encodeS3Key", input: k, expected: encodeS3Key(k) });
}
mkdirSync(join(out, "s3"), { recursive: true });
writeFileSync(join(out, "s3", "sign-put.json"), JSON.stringify({ jsSourceFile: "src/recording/s3-put.ts (signPut, encodeS3Key) + test/recording-s3.test.ts", jsCommit: JS_COMMIT, generatedBy: GEN, cases }, null, 2) + "\n");

// ---------- kvs manifest ----------
const dir = mkdtempSync(join(tmpdir(), "xg-wp5-"));
const valid = {
  deviceId: "dev-1",
  region: "us-east-1",
  credentialEndpoint: "abc.credentials.iot.us-east-1.amazonaws.com",
  roleAlias: "XorgateDeviceCreds",
  channels: [
    { streamKey: "cam0", channelName: "dev-1-cam0" },
    { streamKey: "cam1", channelName: "dev-1-cam1" },
  ],
};
type FileIn = { text?: string | null; b64?: string };
let n = 0;
function place(f: FileIn): string {
  const p = join(dir, `kvs-${n++}.json`);
  if (f.b64 !== undefined) writeFileSync(p, Buffer.from(f.b64, "base64"));
  else if (f.text !== null && f.text !== undefined) writeFileSync(p, f.text);
  return p;
}
const fileDesc = (f: FileIn) => (f.b64 !== undefined ? { inputB64: f.b64 } : { inputText: f.text ?? null });
const mcases: unknown[] = [];
function load(name: string, f: FileIn) {
  const p = place(f);
  mcases.push({ name, op: "loadManifest", ...fileDesc(f), expectedJson: JSON.stringify(loadManifest(p)) });
}
function persist(name: string, f: FileIn, bucket: string) {
  const p = place(f);
  const res = persistUploadBucket(p, bucket);
  const after = existsSync(p) ? readFileSync(p).toString("base64") : null;
  mcases.push({ name, op: "persistUploadBucket", ...fileDesc(f), bucket, expected: res, afterB64: after, afterLoadJson: JSON.stringify(loadManifest(p)) });
}
const J = JSON.stringify;
load("manifest.test.ts: parses a valid manifest", { text: J(valid) });
load("manifest.test.ts: returns null when the file is missing", { text: null });
load("manifest.test.ts: returns null on unparseable content", { text: "not json" });
load("manifest.test.ts: returns null when a required field is missing or empty [region empty]", { text: J({ ...valid, region: "" }) });
load("manifest.test.ts: returns null when a required field is missing or empty [credentialEndpoint undefined]", { text: J({ ...valid, credentialEndpoint: undefined }) });
load("manifest.test.ts: accepts an empty channel list", { text: J({ ...valid, channels: [] }) });
load("manifest.test.ts: drops malformed channel entries", { text: J({ ...valid, channels: [{ streamKey: "cam0", channelName: "dev-1-cam0" }, { streamKey: "cam1" }, "garbage"] }) });
// extras
load("extra: uploadBucket carried", { text: J({ ...valid, uploadBucket: "core-bucket" }) });
load("extra: empty uploadBucket omitted", { text: J({ ...valid, uploadBucket: "" }) });
load("extra: non-string uploadBucket omitted", { text: J({ ...valid, uploadBucket: 5 }) });
load("extra: channels missing", { text: J({ deviceId: "d", region: "r", credentialEndpoint: "c", roleAlias: "a" }) });
load("extra: channels is an object", { text: J({ ...valid, channels: { streamKey: "cam0", channelName: "x" } }) });
load("extra: channel extra fields stripped, null/number/array entries dropped", { text: J({ ...valid, channels: [null, 3, [], { streamKey: "cam0", channelName: "n", extra: 1 }, { streamKey: "", channelName: "n" }, { streamKey: 1, channelName: "n" }] }) });
load("extra: root array", { text: "[]" });
load("extra: root null", { text: "null" });
load("extra: root string", { text: '"x"' });
load("extra: root number", { text: "42" });
load("extra: deviceId number", { text: J({ ...valid, deviceId: 7 }) });
load("extra: roleAlias missing", { text: J({ ...valid, roleAlias: undefined }) });
load("extra: UTF-8 BOM prefix", { text: "﻿" + J(valid) });
load("extra: surrounding whitespace + newline", { text: "\n  " + J(valid, null, 2) + "\n\n" });
load("extra: duplicate keys (last wins)", { text: '{"deviceId":"a","region":"r","credentialEndpoint":"c","roleAlias":"x","region":"r2","channels":[]}' });
load("extra: invalid UTF-8 inside the bucket string (lossy decode)", { b64: Buffer.concat([Buffer.from('{"deviceId":"d","region":"r","credentialEndpoint":"c","roleAlias":"a","uploadBucket":"b'), Buffer.from([0xff, 0xfe]), Buffer.from('"}')]).toString("base64") });
load("extra: unicode + escapes in values", { text: '{"deviceId":"d\\u00e9v","region":"r","credentialEndpoint":"c","roleAlias":"a\\n","channels":[{"streamKey":"cäm","channelName":"\\ud83d\\ude00"}]}' });
load("extra: empty file", { text: "" });
load("extra: trailing comma", { text: '{"deviceId":"d",}' });

persist("manifest.test.ts: backfills a manifest without an uploadBucket", { text: J(valid) }, "core-bucket");
persist("manifest.test.ts: no-ops when the bucket already matches (retained redelivery)", { text: J({ ...valid, uploadBucket: "core-bucket" }) }, "core-bucket");
persist("manifest.test.ts: overwrites a different existing bucket (cloud is authoritative)", { text: J({ ...valid, uploadBucket: "old-bucket" }) }, "new-bucket");
persist("manifest.test.ts: preserves every other field verbatim, including unknown ones", { text: J({ ...valid, futureKey: { nested: true } }) }, "core-bucket");
persist("manifest.test.ts: returns no-manifest when kvs.json is absent or unparseable [absent]", { text: null }, "b");
persist("manifest.test.ts: returns no-manifest when kvs.json is absent or unparseable [not json]", { text: "not json" }, "b");
// extras
persist("extra: root array → no-manifest", { text: "[1]" }, "b");
persist("extra: root null → no-manifest", { text: "null" }, "b");
persist("extra: root string → no-manifest", { text: '"s"' }, "b");
persist("extra: incomplete manifest still backfilled (no validation)", { text: "{}" }, "b");
persist("extra: pretty-printed file is rewritten compact", { text: J(valid, null, 2) + "\n" }, "b");
persist("extra: uploadBucket in the middle keeps its position", { text: '{"deviceId":"d","uploadBucket":"old","region":"r"}' }, "new");
persist("extra: non-string uploadBucket overwritten", { text: '{"uploadBucket":5}' }, "b");
persist("extra: empty bucket string against absent key", { text: '{"a":1}' }, "");
persist("extra: number formatting of preserved fields", { text: '{"i":1,"f":1.0,"g":2.50,"big":1e21,"bigger":1.5e300,"small":1.5e-7,"tiny":0.000001,"neg":-0,"negf":-0.0,"u64":12345678901234567890,"i53":9007199254740993,"e":1E2,"ef":123456789012345680000,"third":0.30000000000000004}' }, "b");
persist("extra: string escaping of preserved fields", { text: '{"s":"\\u0001\\u001f\\b\\f\\n\\r\\t\\"\\\\/\\u007f\\u00e9\\u2028\\ud83d\\ude00"}' }, "b");
persist("extra: duplicate keys collapse (first position, last value)", { text: '{"a":1,"b":2,"a":3}' }, "x");
persist("extra: nested arrays/objects/booleans/null preserved", { text: '{"o":{"z":1,"a":[true,false,null,{"k":[]}]},"e":{}}' }, "b");
persist("extra: invalid UTF-8 elsewhere is rewritten as U+FFFD", { b64: Buffer.concat([Buffer.from('{"x":"'), Buffer.from([0xc3]), Buffer.from('"}')]).toString("base64") }, "b");
persist("extra: integer-like keys move first (JS property order)", { text: '{"b":1,"10":2,"2":3,"01":4,"4294967295":5,"4294967294":6,"-1":7}' }, "b");
persist("extra: BOM → no-manifest", { text: "﻿{}" }, "b");
mkdirSync(join(out, "kvs_manifest"), { recursive: true });
writeFileSync(join(out, "kvs_manifest", "manifest.json"), JSON.stringify({ jsSourceFile: "src/media/manifest.ts (loadManifest, persistUploadBucket) + test/manifest.test.ts", jsCommit: JS_COMMIT, generatedBy: GEN, cases: mcases }, null, 2) + "\n");
console.log(`sign-put: ${cases.length} cases, manifest: ${mcases.length} cases`);
