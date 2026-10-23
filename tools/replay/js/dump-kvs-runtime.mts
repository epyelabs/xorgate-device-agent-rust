// Phase 3 WP5 — dump the KVS runtime process contract from the REAL
// src/hardware/kvs-webrtc.ts: `streamChannel` argv + child env (observed by
// the child itself: the "runtime" is a sh script that prints its argv and
// env, so execa's env merge and the LD_LIBRARY_PATH join are what a real
// master sees), and `detectRuntime` on this linux/arm64 container.
//
//   tools/replay/js/run-tsx.sh dump-kvs-runtime.mts > fixtures/media_ladder/kvs-runtime.json
import { writeFileSync, mkdirSync, chmodSync, rmSync } from "node:fs";
import { detectRuntime, streamChannel } from "../src/hardware/kvs-webrtc.ts";

const KEYS = [
  "AWS_DEFAULT_REGION",
  "AWS_IOT_CORE_CREDENTIAL_ENDPOINT",
  "AWS_IOT_CORE_CERT",
  "AWS_IOT_CORE_PRIVATE_KEY",
  "AWS_IOT_CORE_ROLE_ALIAS",
  "AWS_IOT_CORE_THING_NAME",
  "AWS_KVS_CACERT_PATH",
  "AWS_KVS_LOG_LEVEL",
  "LD_LIBRARY_PATH",
  "KVS_GST_PIPELINE",
  "KVS_IDLE_TEARDOWN_SECS",
];
const dir = "/tmp/xg-kvs-runtime";
rmSync(dir, { recursive: true, force: true });
mkdirSync(dir, { recursive: true });
const bin = `${dir}/fake-master`;
writeFileSync(
  bin,
  "#!/bin/sh\nfor a in \"$@\"; do printf 'ARG=%s\\n' \"$a\"; done\n" +
    KEYS.map((k) => `if [ -n "\${${k}+x}" ]; then printf 'ENV ${k}=%s\\n' "$${k}"; fi`).join("\n") +
    "\nprintf 'stderr line\\n' >&2\nexit 0\n",
);
chmodSync(bin, 0o755);

const base = {
  region: "us-east-1",
  credentialEndpoint: "c1abcdefgh.credentials.iot.us-east-1.amazonaws.com",
  roleAlias: "xorgate-kvs-role-alias",
  thingName: "a73f7a36-dev",
  certPath: "/etc/xorgate/cert.crt",
  keyPath: "/etc/xorgate/private.key",
  caCertPath: "/opt/xorgate-rs/media/cert.pem",
  libDir: "/opt/xorgate-rs/media/lib",
};
const CASES: { name: string; inheritedLd: string | null; mediaType: string; source: string; env: Record<string, unknown> }[] = [
  { name: "defaults: no pipeline, no inherited LD_LIBRARY_PATH", inheritedLd: null, mediaType: "video-only", source: "testsrc", env: { ...base, logLevel: "3", idleTeardownSecs: "30" } },
  { name: "per-channel pipeline + inherited LD_LIBRARY_PATH appended", inheritedLd: "/usr/lib/extra", mediaType: "video-only", source: "testsrc", env: { ...base, pipelinePath: "/var/lib/xorgate-rs/etc/pipelines/cam0.gst", logLevel: "3", idleTeardownSecs: "30" } },
  { name: "empty inherited LD_LIBRARY_PATH is dropped; idle teardown 0", inheritedLd: "", mediaType: "audio-video", source: "devicesrc", env: { ...base, logLevel: "1", idleTeardownSecs: "0" } },
  { name: "empty libDir + inherited LD: inherited only", inheritedLd: "/x/lib", mediaType: "video-only", source: "testsrc", env: { ...base, libDir: "", logLevel: "", idleTeardownSecs: "" } },
  { name: "empty libDir, nothing inherited: LD_LIBRARY_PATH set to empty", inheritedLd: null, mediaType: "video-only", source: "testsrc", env: { ...base, libDir: "", logLevel: "3", idleTeardownSecs: "30" } },
  { name: "logLevel/idleTeardownSecs undefined: AWS_KVS_LOG_LEVEL defaults to 3, KVS_IDLE_TEARDOWN_SECS unset; empty pipelinePath unset", inheritedLd: null, mediaType: "video-only", source: "testsrc", env: { ...base, pipelinePath: "" } },
];
const cases: unknown[] = [];
for (const c of CASES) {
  if (c.inheritedLd === null) delete process.env.LD_LIBRARY_PATH;
  else process.env.LD_LIBRARY_PATH = c.inheritedLd;
  const stdout: string[] = [];
  const stderr: string[] = [];
  const h = streamChannel("a73f7a36-dev-cam0", c.mediaType, c.source, bin, c.env as any, {
    onStdout: (l) => stdout.push(l),
    onStderr: (l) => stderr.push(l),
  });
  const res = await h.done;
  const argv = stdout.filter((l) => l.startsWith("ARG=")).map((l) => l.slice(4));
  const env: Record<string, string> = {};
  for (const l of stdout.filter((l) => l.startsWith("ENV "))) {
    const kv = l.slice(4);
    const i = kv.indexOf("=");
    env[kv.slice(0, i)] = kv.slice(i + 1);
  }
  cases.push({
    name: c.name,
    op: "streamChannel",
    input: { channel: "a73f7a36-dev-cam0", mediaType: c.mediaType, source: c.source, inheritedLd: c.inheritedLd, env: c.env },
    expected: { argv, env, stderr, exitCode: res.exitCode },
  });
}
delete process.env.LD_LIBRARY_PATH;

mkdirSync(`${dir}/adir`, { recursive: true });
for (const [name, path, envBin] of [
  ["binary present", bin, undefined],
  ["binary missing", `${dir}/nope`, undefined],
  ["path is a directory", `${dir}/adir`, undefined],
  ["binary present, KVS_RUNTIME_BIN set", bin, bin],
] as const) {
  if (envBin) process.env.KVS_RUNTIME_BIN = envBin;
  else delete process.env.KVS_RUNTIME_BIN;
  cases.push({ name: `detectRuntime: ${name}`, op: "detectRuntime", input: { binaryPath: path, kvsRuntimeBinEnv: envBin ?? null, platform: process.platform, arch: process.arch }, expected: await detectRuntime(path) });
}
process.stdout.write(
  JSON.stringify(
    {
      jsSourceFile: "src/hardware/kvs-webrtc.ts (detectRuntime :56-77, streamChannel :85-109)",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy: "plans/device-agent-js-to-rust/phase-3/evidence/wp5-tools/dump-kvs-runtime.mts via tools/replay/js/run-tsx.sh (node:20-bookworm linux/arm64; no gst-launch-1.0 in the container)",
      cases,
    },
    null,
    2,
  ) + "\n",
);
