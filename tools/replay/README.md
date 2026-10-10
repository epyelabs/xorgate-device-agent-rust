# Record/replay harness (Phase 2, WP7)

Parity evidence for the Rust port: the JS agent @ `89b5c64` and the Rust agent
run side by side on the **same recorded inputs**, and everything they publish
and write is diffed under a tolerance table whose every row has an id. Design:
`plans/device-agent-js-to-rust/phase-2-cloud-plane.md` §Record/replay
(README §5.3). Acceptance check C3:

- **C3a calibration** — two JS instances on one tape diff clean;
- **C3b parity** — JS vs Rust on the ≥10 min bench tape: zero UNTOLERATED rows.

## Quick start (Mac, Docker Desktop)

```sh
# once: the CI build image (scripts/docker-build.sh builds it)
tools/replay/gen-certs.sh          # rig CA + broker/agent/player certs (.cache/certs)
tools/replay/js-build.sh           # git archive 89b5c64 → npm ci → npm run build (.cache/js-89b5c64)
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
per-kind counts, closed spans, NMEA gaps, wall steps and truncation.

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
(refresh, rate-limited refresh, expired, duplicate requestId), `cli`.

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
  ticks. When the gap exceeds 800 ms the roles flip and the one lead tick in
  between is logged as `skip`. While the broker path is cut (payloads are
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
`inputs-mqtt.ndjson` (what the player published), and per agent `a/`, `b/`:
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

- ONE run at a time per Docker host (fixed compose project, fixed rig
  deviceIds), on a QUIET Docker VM: `cpu_usage`/`ram_usage` are the VM's live
  values (T3), and concurrent cargo builds in the same VM swing them by far
  more than T3 allows and perturb the agents' tick latency.

- `/proc/device-tree/chosen/power/max_current` is a fixed path in both agents
  and cannot be redirected in a container: `psu_max_current` is absent on both
  sides; the tape's `dt` is replayed only by a Rust-only `ReplayHal` run.
- `cpu_temp` (no thermal zone in the Docker VM) is absent on both sides;
  `cpu_usage`, `ram_usage`, `disk_usage` are the rig host's live values (T3).
- `rpicam-hello` is absent (camera reconcile = "cannot enumerate"); camera
  mount, LED, recording and media are Phase 3/4 and tolerated as T6.

## CLI matrix (C11)

`cli-matrix.sh` runs both CLIs (`node dist/cli.js` @ 89b5c64, `xorgate-rs`; or
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
i2cdetect. Also:

`cargo test -p xorgate-replay` covers the tape round trip, synth determinism,
the gate state machine, the rewrite rules and the diff/tolerance engine; a
smoke test reads `fixtures/replay/smoke.ndjson.gz`. The JS-vs-Rust run is the
gate runner's C3 in the Docker rig on the Mac.
