// Phase 3 WP3 — dump the uploader's S3 key mappers (`telemetry-source.ts:45-51`,
// `video-source.ts:51-56,174-178`, sidecar key `:162`) over stray/valid names.
// Run via tools/replay/js/run-tsx.sh <this> > fixtures/upload_keys/upload-keys.json
import { createTelemetrySource } from "../src/recording/telemetry-source.ts";
import { createVideoSource } from "../src/recording/video-source.ts";

const S1 = "0198aaaa-0000-7000-8000-000000000001";
const UP = "0198AAAA-0000-7000-8000-000000000001";
const tel = createTelemetrySource("dev-1", "/nonexistent/telemetry");
const vid = createVideoSource("dev-1", "/nonexistent/video", { isSessionActive: () => false });

const telemetryNames = [
  `${S1}/seg-1753000000000-00003.jsonl.gz`,
  `${S1}/seg-1000-00000.jsonl.gz`,
  `${S1}/seg-1000-0001.jsonl.gz`,
  `${S1}/seg-1753000000000-123456.jsonl.gz`,
  `${S1}/seg-01-00002.jsonl.gz`,
  `${UP}/seg-1-00002.jsonl.gz`,
  "any session/seg-1-2.jsonl.gz",
  "sess/whatever.jsonl.gz",
  "no-slash",
  "/seg-1-2.jsonl.gz",
  "sess/",
  "",
  "/",
  "a/seg-1-2.jsonl.gz/extra",
  "sess/seg-1-2.jsonl.gz.tmp",
  "sess/seg--2.jsonl.gz",
  "sess/seg-1-.jsonl.gz",
  "sess/seg-١-2.jsonl.gz",
  "sess/seg-1-2.jsonl.gz\n",
  "sess/SEG-1-2.jsonl.gz",
  "sess/seg-1-2.jsonl",
  "sess/seg-1-2.JSONL.GZ",
  "sess/seg-1-2xjsonl.gz",
  "sess/seg-1-2.jsonlxgz",
  "sess/seg-1-2-3.jsonl.gz",
  "sess/seg-1.jsonl.gz",
  "sess/ seg-1-2.jsonl.gz",
];

const videoNames = [
  `cam0/${S1}/00000.mp4`,
  `cam1/${S1}/00042.mp4`,
  `cam0/${S1}/123456.mp4`,
  `cam0/${S1}/0000.mp4`,
  `cam0/${UP}/00000.mp4`,
  `cam0/0198aaaa000070008000000000000001/00000.mp4`,
  `cam0/0198aaaa-0000-7000-8000-00000000000g/00000.mp4`,
  `cam0/${S1}/00000.json`,
  `bad key/${S1}/00000.mp4`,
  `cam-0_A/${S1}/00000.mp4`,
  `cam.0/${S1}/00000.mp4`,
  `cam0/${S1}/00000.mp4/x`,
  `cam0/${S1}`,
  `/${S1}/00000.mp4`,
  `cam0//00000.mp4`,
  `cam0/${S1}/evil.mp4`,
  `cam0/${S1}/00000.MP4`,
  `cam0/${S1}/00000.mp4\n`,
  `cam0/${S1}\n/00000.mp4`,
  `cam0\n/${S1}/00000.mp4`,
  `cam0/${S1}/٠٠٠٠٠.mp4`,
  `cåm/${S1}/00000.mp4`,
  `cam0/${S1}/00000xmp4`,
  `cam0/${S1}/.mp4`,
  "cam0/not-a-uuid/00000.mp4",
  "no-slashes",
  "",
];

const cases = [
  ...telemetryNames.map((name) => ({
    name: `telemetry ${JSON.stringify(name)}`,
    op: "telemetry",
    input: { deviceId: "dev-1", name },
    expected: tel.s3KeyFor({ path: "/x", name }),
  })),
  ...videoNames.map((name) => {
    const key = vid.s3KeyFor({ path: "/x", name });
    return {
      name: `video ${JSON.stringify(name)}`,
      op: "video",
      input: { deviceId: "dev-1", name },
      // `video-source.ts:162`: the sidecar key next to a parsed mp4 key.
      expected: { key, sidecarKey: key === null ? null : key.replace(/\.mp4$/, ".json") },
    };
  }),
];

console.log(
  JSON.stringify(
    {
      jsSourceFile: "src/recording/telemetry-source.ts + src/recording/video-source.ts (s3KeyFor)",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy:
        "plans/device-agent-js-to-rust/phase-3/evidence/wp3-tools/dump-upload-keys.mts via tools/replay/js/run-tsx.sh (scratch git-archive copy @ 89b5c64)",
      cases,
    },
    null,
    2,
  ),
);
