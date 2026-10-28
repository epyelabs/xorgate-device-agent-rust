// Phase 4 WP4 tsx driver: runs the REAL `src/commands/update.ts` @ 89b5c64 —
// `parseEnvFile`, `serviceUser`, the three `install*Sudoers` and the
// bucket/version resolution + download-failure half of `updateCommand` — and
// records what it returns / writes / prints, for fixtures/update_plan/update.json.
//
// update.ts has no exports besides updateCommand and hard-codes absolute paths,
// so the driver loads a SCRATCH COPY of it (src/commands/.update-dump.ts) in which
// ONLY these edits are made (each asserted to hit exactly once):
//   - CONF_DIR, UNIT_DEST and the three *_SUDOERS_DEST constants are prefixed
//     with a temp root (so nothing outside the temp dir is read or written);
//   - one `export { … }` line is appended for the private functions.
// `visudo` and `curl` are fake scripts first on PATH; `process.getuid` and
// `process.exit` are stubbed (exit throws, so updateCommand stops at its first
// `fail`). updateCommand never gets past the download (curl always fails), so
// nothing is staged, swapped or installed.
//
//   cd <scratch>/js-89b5c64 && npx tsx <this file> > fixtures/update_plan/update.json
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync, existsSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = mkdtempSync(join(tmpdir(), "xg-wp4-update-"));
const bin = join(root, "bin");
mkdirSync(bin);
mkdirSync(join(root, "etc/xorgate"), { recursive: true });
mkdirSync(join(root, "etc/systemd/system/xorgate-agent.service.d"), { recursive: true });
mkdirSync(join(root, "etc/sudoers.d"), { recursive: true });
writeFileSync(join(bin, "visudo"), '#!/bin/sh\nexit "${XG_VISUDO_EXIT:-0}"\n');
writeFileSync(
  join(bin, "curl"),
  '#!/bin/sh\nprintf "%s\\n" "$@" > "$XG_CURL_ARGV"\nprintf "%s" "$XG_CURL_STDERR" >&2\nexit "${XG_CURL_EXIT:-22}"\n',
);
chmodSync(join(bin, "visudo"), 0o755);
chmodSync(join(bin, "curl"), 0o755);
process.env.PATH = `${bin}:${process.env.PATH}`;
process.env.XG_CURL_ARGV = join(root, "curl-argv");

let src = readFileSync("src/commands/update.ts", "utf8");
const edit = (from: string, to: string) => {
  const n = src.split(from).length - 1;
  if (n !== 1) throw new Error(`expected exactly one ${JSON.stringify(from)}, found ${n}`);
  src = src.replace(from, to);
};
const R = JSON.stringify(root);
edit('const CONF_DIR = "/etc/xorgate";', `const CONF_DIR = ${R} + "/etc/xorgate";`);
edit(
  'const UNIT_DEST = "/etc/systemd/system/xorgate-agent.service";',
  `const UNIT_DEST = ${R} + "/etc/systemd/system/xorgate-agent.service";`,
);
for (const d of ["021_xorgate-clock", "022_xorgate-update", "023_xorgate-camrotate"]) {
  edit(`"/etc/sudoers.d/${d}"`, `${R} + "/etc/sudoers.d/${d}"`);
}
src += "\nexport { parseEnvFile, serviceUser, installClockSudoers, installUpdateSudoers, installCamRotateSudoers };\n";
const dumpPath = join(process.cwd(), "src/commands/.update-dump.ts");
writeFileSync(dumpPath, src);
const m = await import(dumpPath);

// console capture
let out: string[] = [];
let err: string[] = [];
const realLog = console.log;
console.log = (...a: unknown[]) => void out.push(a.join(" "));
console.error = (...a: unknown[]) => void err.push(a.join(" "));
class Exit extends Error {
  constructor(public code: number) {
    super(`exit ${code}`);
  }
}
(process as any).exit = (c: number) => {
  throw new Exit(c);
};

const cases: unknown[] = [];

// --- parseEnvFile -----------------------------------------------------------
const envFile = join(root, "etc/xorgate/agent.env");
const envTexts: [string, string][] = [
  ["plain", "DEVICE_ID=abc\nDEVICE_DIST_BUCKET_URL=https://dist.example.com/b\n"],
  ["double quotes", 'A="quoted value"\n'],
  ["single quotes", "A='single'\n"],
  ["mismatched quotes strip independently", "A=\"x'\nB='y\nC=z\"\n"],
  ["lone quote", 'A="\nB=\'\'\n'],
  ["inner quotes kept", 'A="a"b"\n'],
  ["whitespace around key and =", "   KEY   =   spaced value   \n"],
  ["tabs", "\tT\t=\tv\t\n"],
  ["export prefix does not match", "export A=1\nB=2\n"],
  ["comments and blanks skipped", "# a comment\n\n   # indented\nA=1 # trailing comment kept\n"],
  ["= in value", "URL=https://x/y?a=b&c=d\n"],
  ["empty value", "A=\nB= \n"],
  ["CRLF lines with values are skipped, empty ones kept", "A=1\r\nB=\r\nC=3\n"],
  ["CR only line", "A=1\rB=2\n"],
  ["key rules", "1A=no\n_A=yes\nA-B=no\nAB9=yes\n=novalue\n"],
  ["duplicate keys keep first position, last value", "A=1\nB=2\nA=3\n"],
  ["no trailing newline", "A=1"],
  ["BOM before first key", "﻿A=1\n"],
  ["unicode whitespace", " A =　v \n"],
  ["line separator in value", "A=x y\nB=ok\n"],
  ["nbsp value trimmed then quotes", 'A=  "v"  \n'],
  ["value with spaces inside quotes", 'A="  padded  "\n'],
  ["empty file", ""],
];
for (const [name, text] of envTexts) {
  writeFileSync(envFile, text);
  cases.push({ name, op: "parseEnvFile", input: text, expected: m.parseEnvFile(envFile) });
}
rmSync(envFile, { force: true });
cases.push({ name: "missing file", op: "parseEnvFile", input: null, expected: m.parseEnvFile(envFile) });

// --- serviceUser ------------------------------------------------------------
const dropin = join(root, "etc/systemd/system/xorgate-agent.service.d/10-user.conf");
const userTexts: [string, string][] = [
  ["install.sh drop-in", "[Service]\nUser=pi\n"],
  ["spaces", "[Service]\n  User = vynci  \n"],
  ["Group line first", "[Service]\nGroup=pi\nUser=alice\n"],
  ["first User wins", "User=a\nUser=b\n"],
  ["root", "[Service]\nUser=root\n"],
  ["no User", "[Service]\nGroup=pi\n"],
  ["Username does not match", "Username=pi\n"],
  ["empty value then next line", "User=\npi\n"],
  ["commented", "#User=pi\n"],
  ["CR line start", "[Service]\rUser=cr\n"],
  ["value stops at whitespace", "User=pi extra\n"],
];
for (const [name, text] of userTexts) {
  writeFileSync(dropin, text);
  cases.push({ name, op: "serviceUser", input: text, expected: m.serviceUser() });
}
rmSync(dropin, { force: true });
cases.push({ name: "missing drop-in", op: "serviceUser", input: null, expected: m.serviceUser() });

// --- sudoers bodies + visudo refusal -----------------------------------------
const grants: [string, string, string][] = [
  ["installClockSudoers", "021_xorgate-clock", "clock"],
  ["installUpdateSudoers", "022_xorgate-update", "update"],
  ["installCamRotateSudoers", "023_xorgate-camrotate", "camrotate"],
];
for (const user of ["pi", "vynci", "root", null]) {
  for (const [fn, file, grant] of grants) {
    for (const visudoExit of [0, 1]) {
      if (user === null) rmSync(dropin, { force: true });
      else writeFileSync(dropin, `[Service]\nUser=${user}\n`);
      const dest = join(root, "etc/sudoers.d", file);
      rmSync(dest, { force: true });
      process.env.XG_VISUDO_EXIT = String(visudoExit);
      out = [];
      await m[fn]();
      cases.push({
        name: `${grant} grant, user ${user}, visudo exit ${visudoExit}`,
        op: "sudoers",
        input: { grant, user, visudoExit },
        expected: { body: existsSync(dest) ? readFileSync(dest, "utf8") : null, logs: out },
      });
    }
  }
}

// --- updateCommand: uid, resolution order, download failure ------------------
type R = {
  name: string;
  argv: string[];
  env?: Record<string, string>;
  agentEnv?: string | null;
  uid?: number;
  curlStderr?: string;
  curlExit?: number;
};
const resolution: R[] = [
  { name: "not root", argv: [], uid: 1000 },
  { name: "no bucket anywhere", argv: [], agentEnv: null },
  { name: "agent.env bucket, default latest", argv: [], agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist\n" },
  { name: "agent.env bucket trailing slash stripped once", argv: [], agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist//\n" },
  { name: "agent.env version pin", argv: [], agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist\nDEVICE_DIST_VERSION=v0.1.2\n" },
  { name: "env beats agent.env", argv: [], env: { DEVICE_DIST_BUCKET_URL: "https://env.example/d/", DEVICE_DIST_VERSION: "abc1234" }, agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist\nDEVICE_DIST_VERSION=v0.1.2\n" },
  { name: "--from beats env", argv: ["--from", "https://flag.example/x/"], env: { DEVICE_DIST_BUCKET_URL: "https://env.example/d" }, agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist\n" },
  { name: "--tag beats env + agent.env", argv: ["--tag", "deadbee"], env: { DEVICE_DIST_BUCKET_URL: "https://env.example/d", DEVICE_DIST_VERSION: "abc1234" }, agentEnv: "DEVICE_DIST_VERSION=v0.1.2\n" },
  { name: "empty env bucket is not nullish", argv: [], env: { DEVICE_DIST_BUCKET_URL: "" }, agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist\n" },
  { name: "empty env version is used as-is", argv: [], env: { DEVICE_DIST_VERSION: "" }, agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist\nDEVICE_DIST_VERSION=v9\n" },
  { name: "bucket of just a slash", argv: ["--from", "/"] },
  { name: "--from without a value falls through", argv: ["--from"], agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist\n" },
  { name: "--tag followed by another flag takes it as the value", argv: ["--tag", "--from", "https://flag.example/x"], agentEnv: "DEVICE_DIST_BUCKET_URL=https://file.example/dist\n" },
  { name: "quoted agent.env values", argv: [], agentEnv: "DEVICE_DIST_BUCKET_URL=\"https://file.example/q\"\nDEVICE_DIST_VERSION='v1'\n" },
  { name: "curl fails with empty stderr", argv: ["--from", "https://flag.example/x"], curlStderr: "", curlExit: 7 },
  { name: "curl stderr trimmed", argv: ["--from", "https://flag.example/x"], curlStderr: "  curl: (6) Could not resolve host: flag.example\n", curlExit: 6 },
];
for (const c of resolution) {
  delete process.env.DEVICE_DIST_BUCKET_URL;
  delete process.env.DEVICE_DIST_VERSION;
  for (const [k, v] of Object.entries(c.env ?? {})) process.env[k] = v;
  if (c.agentEnv == null) rmSync(envFile, { force: true });
  else writeFileSync(envFile, c.agentEnv);
  (process as any).getuid = () => c.uid ?? 0;
  process.env.XG_CURL_STDERR = c.curlStderr ?? "curl: (22) The requested URL returned error: 404";
  process.env.XG_CURL_EXIT = String(c.curlExit ?? 22);
  rmSync(process.env.XG_CURL_ARGV!, { force: true });
  out = [];
  err = [];
  let exit: number | null = null;
  try {
    await m.updateCommand(c.argv);
  } catch (e) {
    if (!(e instanceof Exit)) throw e;
    exit = e.code;
  }
  const curlArgv = existsSync(process.env.XG_CURL_ARGV!)
    ? readFileSync(process.env.XG_CURL_ARGV!, "utf8").replace(/\n$/, "").split("\n").map((a) => (a.includes("/xg-update-") ? "<tgz>" : a))
    : null;
  cases.push({
    name: c.name,
    op: "updateCommand",
    input: { argv: c.argv, env: c.env ?? {}, agentEnv: c.agentEnv ?? null, uid: c.uid ?? 0, curlStderr: process.env.XG_CURL_STDERR, curlExit: Number(process.env.XG_CURL_EXIT) },
    expected: { out, err, exit, curlArgv },
  });
}

rmSync(dumpPath, { force: true });
rmSync(root, { recursive: true, force: true });
// The temp root is an artefact of the re-rooting: strip it so paths read as on a device.
realLog(
  JSON.stringify(
    {
      jsSourceFile: "src/commands/update.ts",
      jsCommit: process.env.JS_COMMIT_FULL ?? "09da38dcc8b75c248625a3a0bee43b70e7def55d",
      generatedBy:
        "npx tsx tools/replay/js/dump-update-plan.mts (Phase 4 WP4) in a git-archive 89b5c64 scratch copy; REAL update.ts with only its absolute path constants re-rooted into a temp dir + an export line; fake visudo/curl on PATH",
      cases,
    },
    null,
    2,
  )
    .split(root)
    .join(""),
);
