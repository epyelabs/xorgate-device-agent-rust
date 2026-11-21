# Record/replay harness (Phase 2 WP7; Phase 3 WP6 media additions)

Parity evidence for the Rust port: the JS agent @ `09da38d` (the default since
the Phase 5b H7 flip; `89b5c64` stays buildable via `JS_REF` / `js@89b5c64`
for archaeology, it gates nothing) and the Rust agent
run side by side on the **same recorded inputs**, and everything they publish
and write is diffed under a tolerance table whose every row has an id. Design:
`plans/device-agent-js-to-rust/phase-2-cloud-plane.md` §Record/replay
(README §5.3). Acceptance check C3:

- **C3a calibration** — two JS instances on one tape diff clean;
- **C3b parity** — JS vs Rust on the ≥10 min bench tape: zero UNTOLERATED rows.

Phase 3 (`phase-3-recording-media.md` §Record/replay, acceptance check D3)
adds recording + media: real `gst-launch-1.0` capture on `videotestsrc`, a fake
KVS master, a fake IoT credentials endpoint and a SigV4-verifying fake S3 in
the player, eight synthetic media scenarios and the media streams of the diff
— see **Phase 3: recording + media** below.

## Quick start (Mac, Docker Desktop)

```sh
# once: the CI build image (scripts/docker-build.sh builds it)
tools/replay/gen-certs.sh          # rig CA + broker/agent/player certs (.cache/certs)
tools/replay/js-build.sh           # git archive $JS_SHA (default 09da38d) → npm ci → npm run build (.cache/js-<sha>)
tools/replay/build-bins.sh         # xorgate-rs + xorgate-replay (linux/arm64) → .cache/bin

# a tape: synthetic …
docker compose -f tools/replay/docker-compose.yml run --rm -T rig \
  synth calibration -o /rig/out/calibration.ndjson.gz --fixtures /rig/fixtures
mv tools/replay/.cache/runs/calibration.ndjson.gz tools/replay/.cache/tapes/
# … or the bench window tape: scripts/bench/tape-pull.sh

tools/replay/run-rig.sh tools/replay/.cache/tapes/calibration.ndjson.gz --calibrate   # C3a (JS vs JS)
tools/replay/run-rig.sh <bench tape>                                                    # C3b (JS vs Rust)
```

`run-rig.sh` = certs → JS build → binaries → `docker compose up mosquitto` →
`xorgate-replay play` (REAL TIME: tape length + 20 s settle) → `xorgate-replay
diff` → copies the run + `diff.md`/`diff.json` to
`plans/…/phase-2/evidence/replay-<UTC ts>/` (or `--out`). Exit code = the diff's.
`--speed` other than 1 exists for debugging only: timers differ, so a faster
replay is NOT parity evidence.

Never build on the bench (orchestrator rule: a cargo build browned the bench
out); everything is built in the CI image and copied.

## Layout

| Path | What |
|---|---|
| `crates/xorgate-hal/src/tape.rs` | tape format, `FileTape` gz writer (the Rust agent's `TapeSink` file impl), reader |
| `crates/xorgate-replay/` | `xorgate-replay` binary: `inspect`, `synth`, `play`, `diff` |
| `tools/replay/docker-compose.yml`, `Dockerfile.rig` | `mosquitto` (TLS, MQTT 5, client certs required) + `rig` (node 20 bookworm arm64 + GStreamer runtime libs) |
| `mosquitto.conf`, `gen-certs.sh` | broker + throwaway PKI (SANs `broker`, `mosquitto`, `localhost`) |
| `js-build.sh`, `build-bins.sh`, `run-rig.sh` | build + run drivers |
| `shims/` | `vcgencmd`, `i2cdetect`, `timedatectl`, `sudo`, `rpicam-hello` (PATH shims) |
| `fake-python/bno085_read.py` | IMU helper stand-in: relays the tape's IMU lines from the player |
| `tolerances.toml` | T1–T9 (T7 = Phase 1 accepted divergences D1–D20, each with its pinning test) |
| `js/run-tsx.sh` + `js/*.mts` | white-box tsx drivers against the scratch JS copy (WP2/WP3/WP4 add theirs) |
| `.cache/` (gitignored) | JS build, binaries, certs, tapes, runs |

## Tape format

NDJSON, gzip (`.ndjson.gz`). Line 1:
`{"v":1,"k":"meta","deviceId","build","startWall","hardware","kvs":{"channels"},"env":{…redacted},"home"?,"source"?,"synthetic"?}`.
Then `{"t":<mono ms since start>,"w":<wall ms>,"k":<kind>,…}`:

| `k` | payload | written |
|---|---|---|
| `nmea` | `line` (split on `\n`, `\r` kept) | tty reader, before parsing |
| `imu` / `imu_spawn` / `imu_exit` | `line` / `argv` / `code` | Python child |
| `i2cdetect` | `stdout` or `notFound:true` | detection |
| `lte_file` | `text` / `textB64` / `absent:true` | every read reported, written on change |
| `exec` | `argv`, `stdout`, `code` | every run reported, written on change per argv |
| `dt` | `max_current` hex / `absent:true` | once |
| `mqtt_in` | `topic`, `payload` / `payloadB64`, `retain`, `qos` | every inbound publish incl. retained redeliveries |
| `conn` | `state` `connected`/`closed` | MQTT plane |
| `cli` | `argv` | `xorgate-rs set …` while taping — in the SIDE FILE `<tape>.cli.ndjson` (see below) |
| `clock` | `wall`, `mono` | every 10 s |
| `proc` | `path`, `text` | optional, Rust-only replay |

The agent writes a tape when `XORGATE_RS_TAPE=<path>` is set (WP1 wires
`FileTape` behind its `TapeSink`). The writer sync-flushes every 2 s and on
`conn`/`cli`/`clock`, so a tape cut by SIGKILL or the C7 reboot reads back up
to the last flush (`inspect` says `TRUNCATED`). `meta.home` (optional) carries
`settings.json`/`config-sync.json`/`commands.json` at start so the rig starts
both agents from the bench's state. **Tapes are never committed** (bench
location + SIM identity; O8) except `fixtures/replay/smoke.ndjson.gz`, which is
synthetic.

**CLI side file.** `xorgate-rs set …` with `XORGATE_RS_TAPE` in its env
appends `{"w":<wall ms>,"k":"cli","argv":["set",…]}` to `<tape>.cli.ndjson`
(WP6 `cli/set.rs`, wp6.md A6): the gz tape has one writer, the agent process.
`Tape::read` merges the side file when it exists: each line gets the tape's
mono `t` of the nearest earlier record plus the wall delta and is inserted in
`t` order (tape order wins ties; a cut last line is ignored). `tape-pull.sh`
copies the side file along; during the window the CLI must be run with the
same `XORGATE_RS_TAPE` (`exclusive.sh begin` prints the command line).

`xorgate-replay inspect <tape> [--dump [--kind nmea]…]` prints the header,
per-kind counts, closed spans, NMEA gaps, wall steps and truncation, and
`events (input side):` — what the tape exercises, per kind: NMEA sentence
types + GGA quality, IMU line types, helper exit codes, i2cdetect, LTE
present/absent, exec argv (+ non-zero exit), `config:` valid / malformed /
retain / `+uploadBucket` / `+hardware valid|INVALID` / `rev-not-newer` (stale
or redelivered rev, by tape order), `command:<name>` / expired / duplicate
requestId, conn, cli. Run it on `<run>/b/agent-tape.ndjson.gz` too to see what
the Rust agent actually received.

## Synthetic tapes

`xorgate-replay synth --list`. Deterministic (fixed start wall
`1785353400000`, the `now` of `fixtures/modem/telemetry-tolerance.json`; no
randomness; byte-reproducible gzip), labelled in `meta.synthetic`, built from
the fixtures (NMEA shapes + position of `fixtures/nmea`, `telemetry-tolerance`
raw docs, BNO085 Q-point IMU samples, real `pmic_read_adc` layout). Scenarios:
`smoke` (60 s), `calibration` (240 s), `full` (10 min, every track),
`nmea-fix`, `nmea-nofix`, `nmea-silence` (35 s gap → C12 reopen), `lte-seq`
(15 s absent span), `lte-fixtures`, `imu` (helper exit code 4 + respawn),
`config` (rev 1 → rev 2 + retained redelivery after a 20 s cut), `commands`
(refresh, rate-limited refresh, expired, duplicate requestId), `cli`,
`config-envelope` (rev 3 with a valid `hardware` block + `uploadBucket`
published DURING a cut, so it arrives as a retained delivery; then the same
envelope at rev 4, an invalid `hardware` block at rev 5, a non-JSON payload, a
`v:2` payload, a stale rev 2, rev 5 again), `sms-sim` (meta env `MODE=sim`:
the player starts both agents in sim mode, so `read_sms` answers from the
fixture sweep; telemetry is the O7 sim source, not comparable).

`calibration` and `full` also carry the `config-envelope` track (all live, from
75 % of the run, after every older config record) and `commands-sms` (two
`read_sms` at 45 %; real mode, so the sweep is the O4 reader-unavailable
error). These tracks are APPENDED: every record of the older tracks is
unchanged (only `meta.synthetic` lists the extra tracks), so earlier runs stay
comparable record for record.

The player takes `MODE` from the tape meta `env.MODE` (`sim`/`replay`/`real`,
default `real`).

## The rig (`xorgate-replay play`)

Both agents run inside the `rig` container as children of the player, with
distinct deviceIds (`…000a`, `…000b`), `TZ=UTC`, `MODE=real`, and every input
redirected through the knobs the TS already reads (the Rust agent must honour
the same — common brief rule 6):

| Knob | Rig value |
|---|---|
| `GPS_NMEA_PORT` | a pty per agent, raw mode (no echo / CR-NL translation); the player keeps the slave open |
| `LTE_TELEMETRY_FILE` | a file per agent, rewritten atomically or removed |
| `XORGATE_PYTHON_DIR` | `fake-python/` (relays lines from `$RIG_IMU_SOCK`, exits on `__EXIT__ <code>`) |
| `PATH` | per-agent copy of `shims/` (+ system dirs); `i2cdetect` left out when the tape says notFound; `rpicam-hello` ABSENT unless `--with-rpicam` |
| `IOT_DATA_ENDPOINT=broker` | `broker` → 127.0.0.1 in the rig: a TCP proxy to `mosquitto:8883` |
| `CA_PATH`/`CERT_PATH`/`KEY_PATH` | rig CA + per-agent client cert |
| `KVS_MANIFEST` / `HARDWARE_MANIFEST` / `KVS_PIPELINE_DIR` / `XORGATE_HOME` | scratch (`kvs.json` channels from the tape, `[]` for synth; hardware from `meta.hardware`) |
| `KVS_RUNTIME_BIN` | a missing path (media self-gates) |

- **Time shift.** Embedded wall times are shifted by the whole seconds between
  the tape's `startWall` and the replay start (`rewrite.rs`): LTE-file ISO
  strings, `read_at`, `cclk`; NMEA GGA/RMC/GLL/GNS/ZDA time + date (checksum
  recomputed); command `ts`/`expiresAt`. Relative times are preserved (an
  expired command stays expired). `--no-shift` disables it.
- **Retained config.** Config publishes go to the broker retained (as the
  cloud does); a tape `mqtt_in` with `retain:true` and the payload the broker
  already holds is a redelivery and is NOT republished — the broker redelivers
  on the agent's resubscribe by itself.
- **Connection spans.** A tape `conn closed` … `connected` span drops every
  proxied agent connection and refuses new ones until it ends. The phase file
  says "docker pause of the broker"; the proxy has the same effect on the
  agents while the capture client keeps recording and the rig needs no docker
  socket.
- **Hold gate (pairing).** Each agent ticks at 1 Hz on its own phase (and
  Node's `setInterval` drifts ~1 ms/tick); the TS also stamps `ts` AFTER
  `await source.read()` (`worker.ts:1192-1194`), so `ts = tick + read
  latency`. An input changing between the two agents' reads would make every
  stream-fed field differ by one sample, so the player never changes an input
  between the two ticks of a pair: it estimates each agent's next tick from
  the lower envelope of `ts − k·1000` over its last 8 payloads and holds
  releases from `lead tick − 150 ms` (`--guard-ms`; a line released just
  before must be HANDLED by Node before its read) until BOTH payloads of the
  pair are observed (timeout 2 s); a pair whose first tick is already
  observed is held for its natural partner. Inputs are delayed by less than one pair gap and
  never reordered. Pairs go to `pairs.ndjson`; the diff compares exactly those
  ticks. When the gap exceeds the flip gap (`--flip-gap-ms`, default 500 ms =
  always the nearest alignment; 800 before series 5) the roles flip and the
  one lead tick in between is logged as `skip`. While the broker path is cut (payloads are
  queued, ticks unobservable) the gate holds on PREDICTED ticks (the
  envelope extrapolated through the cut); no gating while an agent has been
  silent > 2.5 s outside a cut. Values stamped in a cut ±5 s are still
  tolerated as T8 `rig-cut-ungated`. `--stagger-ms N` starts B N ms after A, which
  forces distinct phases in a JS-vs-JS calibration.
- **NMEA only while both ports are open.** A line is written to the ptys only
  while BOTH agents' processes hold their slave open (`/proc/<pid>/fd`, cached
  200 ms). The TS opens the port lazily on its first tick (and reopens after
  30 s of silence) at its own phase; a line seen by one agent only, or queued
  before an open and read late, would shift that agent's TTFF clock (which
  starts at the first quality-0 GGA). If one agent has not opened its port
  5 s after the other did, the other is fed alone so a never-opened port shows
  in the diff.
- **Prefill.** `lte_file`/`exec`/`i2cdetect`/`dt` events in the tape's first
  second are applied before the agents start (those files existed before the
  recording agent did).
- **Stop.** After the tape + `--settle-s` (20): SIGTERM to both at once, 30 s
  grace, then SIGKILL of the process group (logged in `stderr.log`).

Run directory (`<run>/`): `play.json` (start/end wall, shift, closed spans,
input transitions, wall steps), `pairs.ndjson`, `player.log`,
`inputs-mqtt.ndjson` (what the player published), `releases.ndjson` (every
tape record's ACTUAL release wall ms — `diff` builds every input instant (T4
windows, the cut residual, report/input-edge rows, D23) from it and refuses a
run without it), and per agent `a/`, `b/`:
`meta.json`, `mqtt.ndjson` (every publish, with the publisher's own `qos`
and `retain`: the capture subscribes `xorgate/#` at QoS 2 with
Retain-As-Published and retain-handling 2 = no retained replay, so each line is
a live publish), `files.ndjson` (every snapshot
file on change, 1 s poll; the first poll logs absent files as `text: null`),
`initial.json` (the seeded files before the agents start), `final/<key>` (raw
end-of-run copies), `final.json`, `sudo.log`, `cli.log`, `stdout.log`,
`stderr.log`.

Snapshot files (`play.rs` `SNAPSHOT_FILES`: every file either agent can write
in Phase 2 scope, from the TS writers). Keys: home files bare —
`settings.json`, `config-sync.json`, `commands.json`, `gnss-sky.json`,
`state.json`, `agent.pid`, `refresh-modem.flag`; manifests `etc/kvs.json`
(uploadBucket backfill), `etc/hardware.json` (persisted cloud block); plus
every `etc/pipelines/<f>` present. `initial.json` / `final.json` are
`{"w": wall ms, "files": {key: {"present": true, "len", "b64" (raw bytes),
"text" (verbatim, null if not UTF-8)} | {"present": false}}}`.

## The diff (`xorgate-replay diff <run>`)

| Stream | Compared |
|---|---|
| telemetry | player pairs (fallback: nearest `ts` within ±500 ms); every leaf; key ORDER; numbers as JSON text (T9) |
| report | quiet periods (10 s); equal counts → pairwise in order, else ±1 (T5) and the last report of each side exact |
| ack, status, sms | in order |
| files at end | settings.json byte-exact; config-sync / commands / gnss-sky / agent.pid as JSON |
| state | state.json at end (recentEvents compared as `events`) |
| events | the multiset of `level: msg` of every state.json event seen in the 1 s snapshots |
| sudo | the argv sequence (`sudo.log`) |

`host.ndjson` (written by the player every 50 ms) is the rig's own
`ram_usage` reading (same formula as `hardware/system.ts:85-98`): a
`ram_usage` difference beyond T3's ±1 is cited as `T3/host` only when this
series moved by at least the excess between the two agents' `ts` (±250 ms);
otherwise it stays UNTOLERATED. It explains host noise; it never widens what
an agent may report.

Every mismatch is listed with both values and the id of the row that covers it
(`T1` … `T9`, `T7/D1`, `T8/O3`, …) or `UNTOLERATED`; exit 1 on any UNTOLERATED.
Rows are explained in `tolerances.toml`. Editing rule: T1–T9 come from the
phase file, T7 is imported verbatim from `phase-1/evidence/accepted-divergences.md`;
a new or wider row needs an O-decision or a reviewer's OK cited in `basis`.

## Limits of the rig (know these when reading a diff)

- ONE run at a time per Docker host (fixed rig deviceIds; the compose project
  is per run — `xorgate-replay-<ts>-<a>-<b>` or `$COMPOSE_PROJECT_NAME` — so
  `run-rig.sh`'s cleanup only removes its own containers), on a QUIET Docker VM: `cpu_usage`/`ram_usage` are the VM's live
  values (T3), and concurrent cargo builds in the same VM swing them by far
  more than T3 allows and perturb the agents' tick latency.

- `/proc/device-tree/chosen/power/max_current` is a fixed path in both agents
  and cannot be redirected in a container: `psu_max_current` is absent on both
  sides; the tape's `dt` is replayed only by a Rust-only `ReplayHal` run.
- `cpu_temp` (no thermal zone in the Docker VM) is absent on both sides;
  `cpu_usage`, `ram_usage`, `disk_usage` are the rig host's live values (T3).
- `rpicam-hello` is absent (camera reconcile = "cannot enumerate"); camera
  mount, LED, recording and media were Phase 3/4 (T6, DELETED in Phase 4 —
  see the Phase 4 section: Pi tapes give each agent its own device tree).

## CLI matrix (C11)

`cli-matrix.sh` runs both CLIs (`node dist/cli.js` @ `$JS_REF`, default 09da38d, `xorgate-rs`; or
`RS_CLI=<path>` for a branch build) inside the rig container and diffs stdout,
stderr (ANSI stripped), exit code and settings.json bytes:

```sh
cd tools/replay
docker compose run --rm -T --no-deps --entrypoint bash rig /rig/tools/cli-matrix.sh            # cumulative list (one home, 34 steps)
docker compose run --rm -T --no-deps --entrypoint bash rig /rig/tools/cli-matrix.sh --fresh    # cli/cases.txt, a fresh home per case (36)
```

`--fresh` drives `cli/e2e-set-run.sh` (WP6's per-case runner, also usable
outside the rig with `RUNNER=…`) over `cli/cases.txt`, every case seeded from
`cli/seed.json`; tags `@root` (sudo note) and `@running` (a decoy live worker +
pidfile, the `set mode` restart path). A by-design difference is tagged
`expect:T8/<row>` and must name a row of `tolerances.toml` (`set-wifi`,
`cli-banner`, `logs-journal`, `set-mode-running`); settings.json must match
even then.

## CI

`tools/replay/ci-smoke.sh <xorgate-rs> <xorgate-replay>` runs in the CI job
(`.github/workflows/ci.yml`, step "Replay smoke tape"): there is no in-process
`ReplayHal` seam (the collectors read the OS), and the CI container has no
Docker, so it replays `fixtures/replay/smoke.ndjson.gz` Rust-vs-Rust through
the same OS-boundary knobs with a local TLS mosquitto, diffs under
`tolerances.toml`, and checks that the telemetry carried gps/imu/lte/system
and that the agent's own re-recorded tape holds nmea/imu/lte_file/exec/
i2cdetect. A second step runs `ci-smoke.sh … media` (Phase 3, see
"Limits (Phase 3)"). Also:

`cargo test -p xorgate-replay` covers the tape round trip, synth determinism,
the gate state machine, the rewrite rules and the diff/tolerance engine; a
smoke test reads `fixtures/replay/smoke.ndjson.gz`. The JS-vs-Rust run is the
gate runner's C3 in the Docker rig on the Mac.

## Phase 3: recording + media (WP6)

### Quick start

```sh
tools/replay/gen-certs.sh            # regenerates a Phase 2 cert dir (adds the `aws` server cert)
docker compose -f tools/replay/docker-compose.yml run --rm -T --no-deps rig \
  synth media -o /rig/out/media.ndjson.gz --fixtures /rig/fixtures
mv tools/replay/.cache/runs/media.ndjson.gz tools/replay/.cache/tapes/
tools/replay/run-rig.sh tools/replay/.cache/tapes/media.ndjson.gz --calibrate   # D3a (JS vs JS)
tools/replay/run-rig.sh tools/replay/.cache/tapes/media.ndjson.gz               # D3b (JS vs Rust)
```

The rig image now carries `gstreamer1.0-tools` + `-plugins-{base,good,bad,ugly}`
and `ca-certificates`; the compose network is **internal** (no egress: no
agent request can reach real AWS or KVS), `init: true` (tini reaps the gst
shim's tee) and `shm_size: 512m` (four 20 MiB shmsink bridges do not fit
Docker's 64 MiB default). Media runs are heavy (two agents × two x264
encoders): ONE rig run at a time on a QUIET Docker VM (WP6 schedules rig runs
in Phase 3; other WPs run unit/integration tests only).

### New tape kinds (`xorgate-hal::tape`, Rust agent only)

| `k` | payload | written |
|---|---|---|
| `child_spawn` | `role` (`capture`/`kvs`), `key`, `argv`, `env` (the master's 11 contract keys only), `pid`, `nice` | after spawn |
| `child_line` | `role`, `key`, `fd`, `line` (capture: only `New clock:` / `Got EOS` / `ERROR` lines) or `count` (kvs: lines per 10 s) | every drained line (the writer filters) |
| `child_signal` | `role`, `key`, `pid`, `sig` | every agent-sent signal |
| `child_exit` | `role`, `key`, `pid`, `code`, `signal`, `external` | waiter |
| `fs_out` | `rel` (under the home), `op` `create`/`close`/`sidecar`/`delete`/`gz`, `size` | recorder, sweep, disk manager, uploader |
| `s3_put` | `key`, `contentType`, `size`, `sha256`, `status` / `error` | uploader, when a PUT settles |
| `shm` | `names` | every 60 s + at each capture exit |

`rpicam-hello --list-cameras` reuses `exec`, the UV / thermal series reuse
`exec` (`vcgencmd`), settings come through `mqtt_in`, the D12 gate seam
reuses `lte_file`. Meta: `kvs.upload` (the manifest had the upload identity)
and, on synthetic tapes only, `rig` (`s3Faults`, `filler`).

### What the player adds (`media.rs`, `aws.rs`)

| Input | Rig value |
|---|---|
| `kvs.json` | tape channels + (when the tape has channels / `kvs.upload` / Phase 3 output records) a COMPLETE manifest with the rig's fake-AWS identity: region `ca-central-1`, credentialEndpoint `c0rig00000000.credentials.iot.ca-central-1.amazonaws.com`, roleAlias `xorgate-rig-role-alias`, uploadBucket `xorgate-rig-media`, deviceId = the rig id. Phase 2 tapes keep the `{"channels": …}` stub |
| `gst-launch-1.0` (PATH) | `shims/gst-launch-1.0`: logs argv verbatim to `<work>/shim.ndjson`, replaces ONLY `libcamerasrc camera-name=<cam>` with `videotestsrc is-live=true pattern=ball`, tees stdout/stderr (so the rig sees `New clock:` / `handling interrupt.` / `Got EOS` for the JS agent too; a Rust side's capture also counts as `int` by its own tape's `child_signal` record when it proves delivery — written after the `kill(2)` returned with `sig` 2 and `errno` 0, stamped before the child's death, and no `child_exit` for that pid (it died with its agent): at the agent's stop the child can be killed by the agent's exit before gst prints the line) and `exec`s the real binary — same pid, real x264enc + splitmuxsink + shmsink, so the ~2.1 s sidecar GOP lag is reproduced (D8 rig half) |
| `KVS_RUNTIME_BIN` | `<work>/kvs/xorgate-kvs-webrtc` = `media/xorgate-kvs-webrtc` (fake master: logs argv + contract env, a line every 5 s, exit 0 on SIGINT, the tape's code on an `external` exit) when the tape has channels; else a missing path (Phase 2) |
| `rpicam-hello` (PATH) | present when the tape answers `--list-cameras` (`exec`), or with `--with-rpicam` |
| TLS trust | rig CA → the container's system store (`update-ca-certificates`, Rust's platform verifier) and `NODE_EXTRA_CA_CERTS` (Node) — no agent knob (P5) |
| fake IoT creds | `:443`, SNI `*.credentials.iot.ca-central-1.amazonaws.com` → mTLS (rig CA); thing name must be a rig deviceId; fixed creds, 1 h expiry |
| fake S3 | `:443`, `<bucket>.s3.ca-central-1.amazonaws.com` → SigV4 recomputed independently with the fixed rig secret (bad signature = 403 + a FAIL row); bodies stored (`<run>/<label>/s3/<key>`, mp4 as size + sha256), every PUT logged in order (`s3.ndjson`); `meta.rig.s3Faults` windows answer 503 |
| filler (D9) | `meta.rig.filler {mib, fragmentMib, ageDays, key, trimLastBytes?}` → a 7-day-old closed session of SPARSE fragments + finalized sidecars seeded into both homes (`trimLastBytes` shortens the newest fragment: `rec-disk` uses 40 KiB so its 10th eviction is not decided by the open-file bytes at a rotation, see `synth.rs`) |
| external exits | tape `child_exit {external: true}` at tape time: kvs → the fake exits with the code; capture → the key's live gst is killed with the tape's signal (SIGKILL when it exited with a code) |

Run dir additions per agent: `shim.ndjson`, `procs.ndjson` (pid gone /
external), `recordings.json` (every file seen under `recordings/`, 1 s poll:
first/last/size/gone), `final/recordings/**` (json + telemetry bodies),
`final/recordings.mp4.json`, `s3.ndjson`, `s3/**`; per run `shm.ndjson`
(`/dev/shm/shmpipe.*`, 5 s), `aws.ndjson` (creds requests, TLS errors,
unrouted PUTs).

### Synthetic media scenarios (D3)

| scenario | s | exercises |
|---|---|---|
| `media` | 60 | both cameras record (720p15-1mbps, 10 s fragments), masters, uploads — the ≤60 s calibration tape |
| `rec-boot-defer` | 150 | `bootDefer`/`gpsGate` `always`: video held (LTE `connecting`, no fix), released at 80 s (LTE connected at 70 s + fix) |
| `rec-shed-uv` | 180 | rail 5.10 V < `railLowVolts` 5.2 → rung 1 (live parked) → rung 2 (bridge-only), restore from 110 s (5.30 V ≥ `railRestoreVolts` 5.25) |
| `rec-shed-thermal` | 150 | firmware soft-temp bit `0x8` 20→110 s: rung 1 only, recording continues |
| `rec-disk` | 120 | 600 MiB filler + `maxBytes` 512 MiB, uploads off: oldest-first eviction, counters. `meta.home` seeds `settings.json` with the scenario's recording namespace, so both agents BOOT with uploads off (without it the boot defaults upload the filler until the rev-1 config lands at 400 ms, and how much depends on each agent's boot speed) |
| `rec-upload-outage` | 180 | MQTT cut 50→90 s (uploader pause), S3 503 window 100→130 s (backoff), resume |
| `rec-cameras` | 150 | `cameras.cam1:false` at 50 s (capture + master parked), `true` at 100 s |
| `rec-preset` | 150 | cam0 capture SIGKILL (external) at 35 s, cam1 master exit 1 at 45 s, preset → `1080p15-2mbps` at 70 s (`stopAndWait`) |

Every scenario carries the CM5 dual-imx290 hardware block (no IMU), channels
cam0/cam1 and the bench's `rpicam-hello --list-cameras` listing (fixtures).
`rpicam-hello` and the PSU device tree cannot be faked for the SoC
temperature (`/sys/class/thermal` is not redirectable in a container), so
thermal shedding is driven by the firmware bit `0x8` of `get_throttled`.

### The media diff (`media_diff.rs`)

Normalisation: the agent's work dir → `<work>` (also in the end files, so the
`etc/pipelines/*` renders compare byte-exact), deviceId → `{id}`, session
uuids → `<sid:video:<key>#n>` / `<sid:telemetry#n>` (order of first
appearance), telemetry file-name ms stamps → `<ms>`.

| stream | exact | tolerated |
|---|---|---|
| `children` | per (role, key): spawn count, argv, kvs env, how each child ended (`int`/`eos`/`kill`/`external`/`exit:<code>`); per side: cam0→cam1 stagger ≥ 4 s | spawn instant ±1 s (T10) |
| `session` | `session.json` | `sessionId`, `bootId`, `anchorTs`, `anchorMono` (T11) |
| `sidecar` | key order + every field | `startTs` ±300 ms vs the agent's OWN capture PLAYING (`New clock:`) time, `startMono` deltas ±300 ms, `durationMs` ±150 ms, `sizeBytes` (T12); diff.md prints the D8 rig figure (fragment-0 `startTs` − PLAYING, median per side) |
| `mp4` | per side: seq contiguity, no 0-byte closed fragment | fragment count per session ±1 (T12) |
| `rec-telemetry` | decompressed header lines (minus T1/T11), sample key order, per segment each group's leaf paths + number shapes; per telemetry session the number of samples WITHOUT the agent-sampled `system` group | segment count ±1, file-name stamps (T13); `system`-less samples ±2 per session (`T13/system-presence`, calibrated on 10 JS-vs-JS runs); gz bytes never compared (D24) |
| `s3` | per side: every signature valid, sidecar before its mp4; A vs B: key multiset + content types of the successful PUTs | the last fragment of a session on one side only, failed-attempt counts (T14) |
| `file:counters.json` | as JSON | — |
| `evictions` | the ordered list of fragments deleted without a successful PUT | — |
| `shm` | orphan `shmpipe.<pid>.*` of dead gst children at the end, per side | — |

T6 was SHRUNK to the Phase 4 gaps (LED, camera mount, clock stepper) — and is
DELETED by Phase 4 WP6 (CF54): nothing phase-owned remains. In Phase 3: the
payload's `media`/`recording` blocks, `bootDefer`/`powerShed`/`liveBlock`, the
recording/hardware/uploader events and the recording latch details are
compared now, so a missing recording/media event FAILS. T1 adds
`recording.backlogBytes` and `liveBlock.since`; T11 excludes the payload's
`recording.sessionId`; T8 `s3-error-text` folds ONLY WP3's unmapped transport
texts (`fold_texts`, node side: unknown issuer / self-signed, SAN mismatch,
corrupt PEM body, key/cert mismatch, the empty multi-address `AggregateError`;
Rust side: the raw `error_chain` fallback). The agent's own `S3 PUT failed:
HTTP …` / timeout / creds texts (`own_prefixes`) and every transport text WP3
maps to node's wording (`mapped_prefixes`: `getaddrinfo …`, `connect E…`,
`socket hang up`, the PEM `no start line` / DECODER `unsupported` …) are exact.

### Phase 3 tsx drivers (`js/`)

WP1–WP5's fixture dumpers, copied verbatim from `phase-3/evidence/wp<N>-tools/`
and run through `js/run-tsx.sh <driver> [args]` against the scratch JS copy:
`wp1-dump-recording-flow.ts` (+ `wp1-scenarios.json`, `wp1-rate-quirk.ts`),
`dump-recorder.mts`, `dump-disk.mts` (WP2), `dump-upload-keys.mts`,
`dump-upload-step.mts` (WP3), `dump-capture-ladder.mts` (WP4),
`dump-media-ladder.mts`, `dump-kvs-runtime.mts`, `dump-hardware-reconcile.mts`,
`dump-stream-guard.mts` (WP5). The fixture each one writes is listed in
`js/run-tsx.sh`.

Rig-only refinements found by the media calibration (JS-vs-JS, synth `media`):
telemetry segment bodies are compared decompressed AND normalised (header
`deviceId` → `{id}`, `sessionId` → its label); the kvs env's per-agent
`/rig/certs/<label>.crt|.key` → `<cert>`/`<key>`; T4 `transition` covers array
paths (`media.streams.<i>.state`); a live-read T3 metric's int-vs-float shape
inside segments is T3 (an integral value prints as an int in both runtimes);
T3/host also accepts a host sample pair from each agent's read window
[ts − 150, ts + 250] ms (the run's own encoders swing the host ram ~3 points
per 50 ms); T10 `flash`: a kvs master SIGINT-stopped within 100 ms of its
spawn (the capture-spawn bounce racing the master's restart inside one agent,
worker.ts:455 → media.ts:51-59) may be absent on the other side, dropped from
both sides only when that equalises the spawn counts; T8 `end-snapshot`:
state.json's shutdown-time `recording.backlogSegments` ±3 (each camera's final
EOS fragment + the telemetry segment land before or after the last write —
every tick's value is still exact in the telemetry stream). All eight synth
scenarios calibrate clean JS-vs-JS (`phase-3/evidence/replay-20261017T22*`).
The CI smoke (`ci-smoke.sh`, Rust-vs-Rust) additionally widens the
tick-drift per-side period bound to [995, 1015] ms for shared runners; the
rig bound stays [999.5, 1010].

The `exec` record carries `stderr` when the caller reports one
(`TapeSink::exec_with_stderr`, e.g. rpicam-hello's zero-camera `No cameras
available!`); the player writes `<state>/exec/<key>.err` and the shims print it
on stderr.

### Limits (Phase 3)

- Sample VALUES inside telemetry segments are not compared pairwise (each
  recorder ticks on its own timer, ungated); the live `telemetry` stream
  compares the same collector values under the hold gate.
- A capture child that exited with a CODE on the device is re-enacted as a
  signal kill (gst cannot be told to exit with a code); both agents get the same.
- CI: besides the Phase 2 smoke, `ci-smoke.sh … media` replays the (58 s cut of the)
  synthetic `media` scenario Rust-vs-Rust in the CI container (no Docker:
  real gst-launch-1.0 behind the shim from the apt `gstreamer1.0-*` packages,
  the fake masters, the fake IoT creds + SigV4-checked fake S3 on
  127.0.0.1:443). The job is root in a throwaway `debian:bookworm` container,
  so the player's `update-ca-certificates` + `/etc/hosts` edits are safe there
  (never run it as root on a workstation); the container gets
  `--shm-size=512m` (2 agents × 2 shmsink bridges). Besides the diff it
  checks both sides PUT mp4 + telemetry objects with valid signatures, spawned
  gst pipelines, and that the Rust agent's tape holds the Phase 3 kinds. It is
  a plumbing check, not parity: JS-vs-Rust stays the Docker rig's job.
  `T13/system-presence` (±2 per session) applies unchanged — its earlier
  smoke-only lift went with the P-WP6-1 timer fix. Media mode starts both agents together (`--stagger-ms 0`:
  both are SIGTERMed together, so a start stagger shortens B's last fragment
  past T12) and cuts the tape to 58 s (the scenario's last LTE rewrite at
  59.5 s races the 60 s telemetry rotation).


## Phase 4: LED, SMS, clock stepper, camera mount, remote rail (WP6)

Plan: `phase-4-operations.md` §8. Same doctrine: both agents on the same
inputs, every difference cited by a row of `tolerances.toml` or UNTOLERATED.

### New tape kinds (`xorgate-hal::tape`, I9 — recorded by the Rust agent)

| `k` | payload | written |
|---|---|---|
| `at_tx` / `at_rx` | `port`, `line` (no `\r`) / `hex` (raw chunk) | `AtSession` write / read |
| `gpio` | `pins`, `high`, `via` (`gpiocdev`/`pinctrl`) | every `PinWriter::write` |
| `sudo` | `argv` (`["sudo","-n",…]`), `code`, `stdout`?, `stderr`?, `notFound`? | every privileged run, when it settles |
| `ntp_probe` | `synced` | the clock stepper's NTP probe |

The player replays `ntp_probe` (the `timedatectl` shim's NTPSynchronized,
`<state>/ntp`, default `yes`), `sudo` (the shim's answer per helper, applied up
front — the record is the answer the device got) and `at_tx`/`at_rx` (the fake
modem's transcript mode). `gpio` is compared through the pinctrl shim, not
replayed.

### What the player adds (`src/p4.rs`)

| Input | Rig value |
|---|---|
| A Raspberry Pi of its own (`meta.rig.pi`) | each agent runs in a PRIVATE mount namespace: `unshare -m --propagation private -- pi-ns.sh <work>/pi <agent…>` binds `/boot/firmware` (its own `config.txt`) and `/sys/firmware/devicetree/base` = `/proc/device-tree` (`model` "Raspberry Pi Compute Module 5 Rev 1.0", camera `rotation` bytes). `exec` keeps the pid. `meta.rig.pi.dtWrites [{t, rel, hex}]` rewrites DT files at tape time (the simulated reboot). Needs `cap_add: SYS_ADMIN` + AppArmor unconfined (compose) |
| `pinctrl`, `gpioinfo`, `dtoverlay` (PATH, Pi tapes only) | `pinctrl` logs `<wall ms> <argv>` to `<run>/<label>/led.log`; `gpioinfo` / `dtoverlay` answer from `exec` records (`gpioinfo` default: 54 unused lines) |
| `XORGATE_LED_WRITER=pinctrl` | both agents (Q6: the JS agent always drives pinctrl; the Rust agent's runtime choice), so both hit one shim |
| SMS port (`meta.rig.modem` or a transcript) | a pty per agent, `XORGATE_SMS_PORT=<work>/sms-tty`; a thread answers with the FakeModem semantics of `test/sms-modem.test.ts:46-111` @89b5c64 (`ATE0`, `CMGF?/=`, `CPMS?`, `CPMS="m"`, `CMGRO=i`, else `ERROR`; `\r\n<line>\r\n` framing), or the recorded `at_rx` bytes per `at_tx`; every command → `<run>/<label>/at.ndjson` (`forbidden` / `fault` flagged) |
| `sudo` | logs as before and answers from the tape; `xorgate-camrotate` EXECUTES the real vendored helper (`/rig/system/xorgate-camrotate`, else the JS build's byte-identical copy; `--camrotate`) with a clean env, inside the agent's namespace — the ONE executing shim mode |

### Synthetic scenarios

| scenario | s | exercises (gate rows) |
|---|---|---|
| `p4-led-ladder` | 180 | starting → gps-wait → healthy → network (cut 40→60 s) → flash2 on a REAL config change (75.7 s; the 85.7 s redelivery must not flash) → undervoltage via the Phase 3 rail seam (100→140 s; `liveBlock.reason` first, CF57) → healthy (E2, E3) |
| `p4-sms` | 60 | FakeModem with the bench inbox (SM 7/40 unread, ME 23/23) + a 2-part UCS2 concat + a stored SUBMIT, text mode at start (→ PDU → restored); read_sms limit 10, a reuse inside 5 s, limit 3 later (E2, E5) |
| `p4-clock-gps` | 120 | NTP unsynced; GPS +600 s with a fix → one gps step; NTP synced at 90 s → stood down (E2, E7) |
| `p4-clock-nitz` | 120 | no GPS fix; NITZ +600 s unregistered (reg 0, fail-closed) then reg 5 → one nitz step; NTP at 100 s (E2, E7) |
| `p4-clock-cap` | 700 | `maxStepsPerBoot: 1` (seeded settings.json); implausible NITZ 80/01/06 → warn + 5 min hold-off; GPS +600 s from 200 s → step 1/1; the cap warning after the next hold-off (E2, E7) |
| `p4-cam-mount` | 300 | cameraMount cam0 0→180 (real helper edits the agent's config.txt → pending-reboot, rebootRequired), DT 180 at 100 s → applied (60 s poll), →0 at 170.7 s, DT 0 at 230 s (E2, E12) |
| `p4-update-cmd` | 60 | cloud update / reboot / power_off → sudo argv (T8/O3 for the updater unit name) (E2, E10, E16) |

The clock tapes move the SOURCES +600 s (the container clock cannot move); the
bench recipe (§9 E8) moves the clock −600 s instead. The SMS store contents
are `fixtures/replay/p4-sms-modem.json`, dumped by
`js/dump-sms-modem.mts` with the TS test's PDU builder (no hand-typed PDU).

### The Phase 4 diff (`src/diff_p4.rs`)

| stream | exact | tolerated |
|---|---|---|
| `led` | the ordered `[led] a → b (colour pattern)` lines (stdout+stderr); the multiset of the other `[led] …` lines; the run count; per run, the pin groups (one `apply()` = its rise + fall writes, `+12,16 -21`) of the common prefix | T15: per-step interval ±50 ms; a run's start ±1150 ms; its length where the two durations differ ≤ 2300 ms (each agent changes rung on its own 1 s heartbeat) |
| `at` | the command sequence; the modem end state | — (a forbidden command — `CMGR=`, `CMGL`, `CMGD`, `CMGW`, `CMSS`, `CMGS`, 2-arg `CPMS` — or a text-mode `CMGRO` FAILS on either side) |
| `sudo` | argv sequence | T16: `xorgate-setclock <epoch>` compared as the offset from the call time ±1 s (each agent ticks on its own 30 s phase) |

A run starts at every pin group that drives all three channels (each new
colour/pattern starts from "levels unknown", `led.ts` `stop()`); a group is one
`apply()` (a lone rise, then a fall on other pins, < 60 ms apart).

T6 is DELETED (CF54) and T8/O4 too (CF48: the Rust real-mode reader answers
like the TS): a JS-only `[clock] …` / `camera mount: …` event, a report's
`effective.cameraMount` / `clockStepperArmed`, the stood-down latch detail or a
set-clock / camrotate sudo argv on one side only FAILS.

C11 (`cli-matrix.sh`): every Phase 4 verb is real — `set wifi`, `pipeline`,
`recording` expect equality; `update` / `led` use `map:T8/cli-q9-names` (the
Rust side's `xorgate-rs` → `xorgate`, Q9 local hints, then exact) and `sms`
`map:T8/cli-sms-duration`; `manage` / `monitor` without a tty are
`T8/cli-tui-no-tty` (the TUI is compared on TestBackend frames + bench E15).

`ci-smoke.sh` keeps `$OUT`'s diff + small logs in `$CI_SMOKE_KEEP` on every
exit (the load-only media flake) — plus each agent's own tape and the small
files of its `final/` + `s3/` trees (no mp4), so a recorded-value row can be
timed against that agent's NMEA reads afterwards, plus the tape itself
(`tape.ndjson.gz`, which `diff` uses when play.json's recorded path is gone),
so a kept run re-diffs completely — and the player now waits up to the capture
stop grace (10 s) for an exited agent's process group before its SIGKILL, so a
capture's `ended` (int vs kill) is the agent's doing, not a race with the rig.

## Phase 5b: JS catch-up to `09da38d` (WP4)

- **Refs.** `JS_REF` / `JS_SHA` default to `09da38d` (H7 flip). Name a side `js@<sha>`
  (`run-rig.sh <tape> --a js@89b5c64 --b js@09da38d`) for a per-side build (`--js-dir-a/-b`);
  `89b5c64` stays buildable for archaeology and gates nothing. Shared builds:
  `RIG_JS_CACHE=<abs .cache>`; label runs with `XORGATE_AGENT_LABEL`.
- **Fixture re-dump (H2, CF78).** `js/redump-all.sh <outdir>` re-runs every driver of `js/` (and the
  Phase 1/2 generators copied from the plan evidence: `dump-wp6`, `dump-m1`, `dump-lte-scenarios`,
  `dump-imu-scenarios`, `dump-wp5`, `dump-settings-proto`) at `$JS_SHA`; every driver stamps
  `jsCommit` from `$JS_COMMIT_FULL`. `js/redump-compare.py <outdir> fixtures` → BYTE / STAMP / DIFF.
  Fixtures from Phase 1 test tables with no generator keep the `89b5c64` stamp (their TS tests
  pass at `09da38d`; list in `phase-5b/evidence/delta-inventory.md`).
- **OBD (`src/p5b.rs`).** `meta.rig.obd` starts the ELM327-emulator 4.0.0 (`obd-emulator.py`, venv in
  `Dockerfile.rig`) on a pty per agent (`XORGATE_OBD_SERIAL`), builds a per-side python dir (REAL
  `obd_read.py` / `obd_scan.py` / `obd/`) and the `bluetoothctl` shim's state (pair/trust/connect
  REFUSED). Event paths normalise the side's pty to `{obd-serial}`.
- **FakeModem `allowClear`** (`meta.rig.modem.allowClear`): `AT+CMGD=1,4` is answered only on the
  p5b-sms-clear tape; anywhere else it is FORBIDDEN and fails the run.
- **Clock-trust hold (X5).** A held agent publishes nothing; `held_alive` counts any publish as a
  tick, and the post-hold settle pairs the resume (tolerances.toml "POST-HOLD SETTLE").
- **Tapes** (`synth p5b-*`): sms-clear, led, clock-hold, clock-fallback, unheld, obd-emulator,
  obd-crank, obd-badproto, obd-config (cloud `obd: null` rejection), obd-cmds, obd-cmds-noperiph.
- **Bench overlay** (`scripts/bench/p5b-overlay.sh apply|revert|status`): OBD emulator + FakeModem
  under the shared profile only; CF76 keeps `peripherals.obd` and `XORGATE_OBD_*` out of the env.

## Phase 6 P3a: cross-agent takeover (handoff mode)

Plan: `plans/device-agent-js-to-rust/phase-6/evidence/P3a-takeover.md` §Design.

- **Flags** (`xorgate-replay play`, after `--` in `run-rig.sh`):
  - `--handoff-signal term|kill --then-a <kind> --then-b <kind>`: at the tape's `meta.rig.handoff.atMs`, the player stops BOTH agents the way systemd stops a unit (`handoff.rs`). Both units keep the default `KillMode=control-group` and the 90 s `TimeoutStopSec`. The signal goes to the whole cgroup-equivalent member set: the `/proc` descendants plus their process groups.
    - `term`: SIGTERM to every member, then SIGKILL after 90 s.
    - `kill`: SIGKILL at once.
  - After the stop, the player snapshots each side's `recordings/` (`<side>/handoff-state.json`, small files in `<side>/handoff/recordings/`). It then restarts side A as `--then-a` and side B as `--then-b` on the SAME homes, 2 s after the slower stop. Defaults: `--handoff-at-ms`, `--handoff-gap-ms 2000`, `--handoff-stop-timeout-ms 90000`.
- **Tape time is frozen across the pause:** `t0` and the fake S3's fault origin move by it. Both the hold gate and the watch gate restart as at a boot.
- **Logs:** later incarnations append to `stdout.log`/`stderr.log` after a `[rig] handoff: incarnation N (<kind>)` marker. A Rust incarnation N ≥ 1 writes `agent-tape.N.ndjson.gz`. `meta.json` carries `incarnations [{fromW, kind}]`; its `kind` is the last incarnation's.
- **Refused combinations:** a tape with per-attempt external exits, a Pi namespace or OBD.
- **Diff** (`takeover.rs`; exact, no tolerance rows):
  - The `takeover` stream: `state` / `edge` (an INVALID lane, not a verdict), `fate` (put×N / disk / gone per inherited file; no double upload, no lost segment), `recovery-lines`, `disk-end`.
  - Kind-directional rules use the kind of the incarnation that produced each item (R2). Each item is a session, a tick, a report, or the events of each era.
  - A child the handoff signalled ends as `handoff:<sig>` (R1).
- **Tapes:** `synth p6-takeover-open | -vbacklog | -tbacklog | -reap | -inflight`.
- **In-flight PUT** (`meta.rig.s3Holds [{fromMs, toMs, holdMs, suffix}]`, `aws.rs` `Hold`): a PUT whose key ends `suffix` and whose headers arrive in the window is held `holdMs` before its body is read.
  - If the client died during the hold (the body read fails, or the post-body probe sees EOF), the PUT is ABORTED: never stored, never answered, logged `s3-held` / `s3-aborted` in `aws.ndjson`.
  - A live client is answered normally.
  - The sidecar re-PUT that follows is the TS's documented idempotent retry (`takeover/sidecar-reput`).
- **Lanes:** `p3a-lanes.sh synth | matrix | run <id>:<tape>:<jr|rj|jj|rr>:<term|kill> | run-matrix`. Lanes run serially; before each one, a quiet-host preflight checks that no other `xorgate.agent` container runs and that the 1-min load is ≤ 8. The runner holds the Mac awake with `caffeinate -dims`.

## Phase 7c WP3: the Rust IMU reader lane (`XORGATE_IMU_BACKEND=rust`)

Plan: `plans/device-agent-js-to-rust/phase-7c-rust-imu.md` §Design 6 (R6: the relay lane is the plan of record); gate rows C6/C7.

- **Default lanes are unchanged:** with the knob unset a Rust side spawns `python3 <XORGATE_PYTHON_DIR>/bno085_read.py --address 0x4a`, i.e. `fake-python/bno085_read.py`.
- **The rust-backend lane:** `run-rig.sh <tape> --a js --b rust -- --env-b XORGATE_IMU_BACKEND=rust`. The player then sets that side's `XORGATE_IMU_READER_BIN` to ITS OWN binary (`/rig/cache/bin/xorgate-replay`; an explicit `--env-b XORGATE_IMU_READER_BIN=…` wins), and `XORGATE_IMU_SOC_CHECK=off` (the agent's R9 allow-list runs `rust` only on a BCM2712 device tree, which the rig lacks), whose hidden `__imu-reader` subcommand (`src/imu_relay.rs`) is the relay: the same socket protocol as the fake helper (`$RIG_IMU_SOCK`, the tape's lines verbatim, `__EXIT__ <code>`, SIGINT → 0).
- **The relay asserts its own argv** (`["xorgate-imu", "__imu-reader", "--address", "0x4a"]`). On any mismatch, e.g. the agent dropped `arg0`, it prints `{"type": "error", "error": "init failed: rig argv [...]"}` and exits 3: the collector parks, and the park line fails the run. It is an ELF subcommand, not a script, because the kernel's `#!` handling replaces `argv[0]` with the script path, so a script relay could never see `xorgate-imu`.
- **What it proves:** knob → `ChildSpec` (program, `arg0`, args) → spawn → supervision (Redetect/Park). It does NOT exercise the SHTP codec or client: those rest on C4 (oracle differential), C5 (W0 lockstep) and C10b (bench reverse lockstep).
- **The IMU knobs are refused on a JS side** (and on a JS handoff incarnation).
- **C7 tapes:** the IMU-bearing synthetic tapes `smoke`, `calibration`, `full`, `imu`.

## Camera report (support-other-camera-models, Phase 0): Rust-only, lanes on camera tapes differ by construction

Since branch `camera/phase0-truth` the Rust agent does things on the camera path that the JS agent
does not, on purpose (accepted divergences **D89–D94** and **D105**, in the register
`plans/done/device-agent-js-to-rust/phase-1/evidence/accepted-divergences.md`). Orchestrator ruling
(provisional, 2026-11-21): the plan does not gate on JS-vs-Rust lanes for tapes with a camera
hardware block. **No tolerance matcher was built and no tolerance row was widened**, so on those
tapes a JS-vs-Rust lane reports the differences below as UNTOLERATED. That is the expected reading,
not a regression. A Rust-vs-Rust lane with one binary on both sides is unaffected; an `ab` lane
across this change differs the same way.

What differs, where the agent runs in real mode, online, with a hardware block that lists cameras:

| # | Stream | Difference | Row |
|---|---|---|---|
| 1 | `report` | `effective.cameras` on the Rust side of every report | D89 |
| 2 | `report` (count) | extra Rust-side reports: one when capture first settles after a boot report that carried `off` / `starting`, one per real change of a port (hold 20 s, doubling to 10 min) | D89 |
| 3 | `state` | `cameras`, the last key of the Rust state.json | D90 |
| 4 | child processes, events | only on a tape with a camera-less port and stale pipeline files: the JS side spawns capture children from the stale name, the Rust side does not, and logs one `[video-capture] <key>: no camera enumerated on its port …` line | D91 |
| 5 | end snapshot `etc/pipelines/<key>.gst.direct` | only on a tape with a sensor change | D92 |
| 6 | events | the `hardware: … using it anyway` warning, only where config.txt and the model disagree or a variant name is listed | D93 |
| 7 | `exec` | extra `rpicam-hello --list-cameras` runs (15 s, 30 s, 60 s, then every 5 min), only while a port is held AND the rig host's `/sys/bus/i2c/devices` shows a driver bound on it (never on the rig as it is) | D94 |
| 8 | events | the hold line `<key>: no camera enumerated on <csiN> (expected <sensor>) …` names config.txt's sensor instead of the model's, only on a tape whose config.txt line and model disagree for a camera-less port | D105 |

Affected synthetic scenarios (rows 1–3; the others need inputs no synthetic tape has): every one
that carries the `media-hw` track: `media`, `rec-boot-defer`, `rec-shed-uv`, `rec-shed-thermal`,
`rec-disk`, `rec-upload-outage`, `rec-cameras`, `rec-preset`, `p6-takeover-open`,
`p6-takeover-vbacklog`, `p6-takeover-vbacklog-503tail`, `p6-takeover-tbacklog`,
`p6-takeover-inflight`, `p6-takeover-reap`, `p4-led-ladder`, `p5b-led`; `p4-cam-mount` if its
hardware block lists cameras; and every bench tape. Not affected: `smoke`, `calibration`, `full`
(their hardware block has `video: []`), the nmea / sms / clock / obd scenarios, and everything in sim
or replay mode (no camera report there at all).

If these lanes are ever gated again, what would be needed (none of it exists):

- a presence-only row for a Rust-only SUBTREE (`effective.cameras`, state `cameras`): the JS side
  lacks the path, the Rust side holds an object. `rust_only_value` (D85) wants one exact value;
- a report-count rule for a Rust-side report that differs from that side's previous report only
  under `effective.cameras` (the `report-echo` row is the model);
- for a scenario that pins verdicts: the new inputs are not on the tape. config.txt comes from
  `pi-ns.sh` (Pi tapes only) and `/sys/bus/i2c/devices` is the rig host's, so `bound.driver` reads
  `false` and, off Pi tapes, `bootConfig` reads `null`. `pi-ns.sh` would have to bind a fake
  `/sys/bus/i2c/devices`.

## Camera-line owner (support-other-camera-models, Phase 1): `p4-cam-mount` is a Rust-only scenario now, and the rig cannot run it faithfully yet

Since branch `camera/phase1b-line-owner` the Rust agent changes the camera lines of config.txt through
`system/xorgate-camcfg` and no longer calls `xorgate-camrotate` (accepted divergences **D95–D104**).
On every tape that changes `cameraMount` (today: `p4-cam-mount`) the two agents differ BY DESIGN:

| # | Stream | JS side | Rust side | Row |
|---|---|---|---|---|
| 1 | `sudo` | `sudo -n /usr/local/bin/xorgate-camrotate imx290 cam0 180` | `sudo -n /usr/local/bin/xorgate-camcfg rotate cam0 imx290 180` | D95 |
| 2 | `exec` | `dtoverlay -h imx290` before each write | none for a catalog overlay | D96 |
| 3 | end snapshot `config.txt` | `…,cam0,4lane` after 180 → 0 (the token is dropped) | `…,cam0,4lane,rotation=0` | D98 |

**No tolerance matcher was built and no tolerance row was widened** (`tolerances.toml` still says a
camrotate sudo argv on one side only FAILS, which is now the expected reading of a JS-vs-Rust lane on
this tape). `p4-cam-mount` is to be run **Rust-vs-Rust** (`rsrs`, or `ab` across a change).

**The rig as it is does NOT exercise the Rust write path of this tape.** Nothing under `tools/replay/`
or in `xorgate-replay` was changed for it (a harness change lapses every tape's eligibility and needs
the orchestrator's go). What a Rust side does on the rig today, and what would make it faithful:

1. The agent first checks that the helper is installed (`/usr/local/bin/xorgate-camcfg` and
   `/usr/local/bin/xorgate-camcfg.overlays`). The rig image has neither, so the Rust side reports
   `cameraMount` `rejected`: "could not edit …: the xorgate-camcfg helper is not installed on this
   device yet — run `sudo xorgate-rs update` once more", runs NO sudo and leaves config.txt alone.
   That is the update-window behaviour, pinned by `tests/camera_mount.rs`
   `the_update_window_is_a_clear_status_not_a_loop`; it is not a rig failure.
2. To run the write path: (a) `pi-ns.sh` binds `/rig/system/xorgate-camcfg` and
   `/rig/system/xorgate-camcfg.overlays` at those two paths (the repo's `system/` is already mounted read-only at
   `/rig/system`); (b) `shims/sudo` gets a second executing mode beside `RIG_CAMROTATE`: for
   `xorgate-camcfg`, `exec env -i PATH=… "$RIG_CAMCFG" "$@"` (no test seam is needed: inside the
   agent's namespace `/boot/firmware/config.txt` and `/proc/device-tree/model` are the tape's own,
   and `env -i` leaves `SUDO_UID` unset, which the helper does not need); (c) `p4.rs`
   `prepare_agent` passes `RIG_CAMCFG` as it passes `RIG_CAMROTATE`.
3. The B7 gate is satisfied on the rig as it is: the tape's `rpicam-hello` listing names `imx290` on
   both ports and config.txt declares `imx290`, so the port's verdict is `ok` (the rig host's
   `/sys/bus/i2c/devices` is not consulted when cameras can be listed).

The helper itself, and the reconciler over the REAL helper, are covered off the rig:
`crates/xorgate-agent/tests/camcfg_helper.rs` and `tests/camera_mount.rs` (the TS golden of
`reconcileCameraMount`, pass by pass, with every by-design difference named in
`fixtures/camera_mount/rust-divergences.json`).

