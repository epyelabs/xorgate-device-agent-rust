# 5. Hardware abstraction and testing

The agent talks to a modem, a GNSS receiver, an IMU, cameras, GPIO pins, `sudo` and AWS. None of
that exists on a laptop or in CI. This chapter shows how the code is cut so that almost everything
can be tested without hardware, and how parity with the reference agent is proven.

There are four layers of testing, from fastest to most realistic:

| Layer | What | Where | Runs in |
|---|---|---|---|
| Unit tests | one function or type | `#[cfg(test)] mod tests` in each file | `cargo test` |
| Golden-fixture tests | core logic against inputs and outputs dumped from the reference | `crates/*/tests/*_fixtures.rs` + `fixtures/` | `cargo test` |
| Seam tests | a real subsystem with fake edges (fake MQTT link, fake sudo, fake source, paused clock) | `crates/xorgate-agent/tests/*.rs` | `cargo test` |
| Record/replay | the real reference agent and the real Rust agent fed the same recorded inputs, outputs diffed | `crates/xorgate-replay`, `tools/replay/` | Docker rig |

On top of that, the bench device and field drives validate on real hardware. That is the
maintainers' job and not part of the day-to-day loop.

## The seams

Two techniques keep hardware out of tests.

**1. Traits at every edge that a test needs to control.** The production type talks to the real
thing; a test passes a fake. The important ones:

| Trait | Defined in | Real implementation | Test fake |
|---|---|---|---|
| `TapeSink` (record/replay hook) | `crates/xorgate-hal/src/io.rs:55` | `NullTape`, `FileTape` | `FakeTape` in `crates/xorgate-agent/tests/common/mod.rs` |
| `PrivilegedRunner` (`sudo -n …`) | `crates/xorgate-hal/src/privileged.rs:136` | `SudoRunner`, `TapedRunner` | `RecordingRunner` (same file) |
| `NmeaOpener` (GNSS tty) | `crates/xorgate-hal/src/nmea_tty.rs:107` | `TtyOpener` | test openers in `tests/collectors.rs` |
| `ImuIo` (IMU detect + helper) | `crates/xorgate-agent/src/collectors/imu.rs:54` | `RealImuIo` | fakes in `tests/collectors.rs` |
| `PinWriter` (LED pins) | `crates/xorgate-hal/src/led_driver.rs:80` | gpiocdev / `pinctrl` writers | `tests/led_driver.rs` |
| `AtTransport` (modem AT port) | `crates/xorgate-hal/src/at_port.rs:215` | the serial port | a pty and `tests/support/fake_modem.rs` |
| `MqttTransport` | `crates/xorgate-agent/src/mqtt_plane.rs:43` | `RealTransport`, `OfflineTransport` | `FakeLink` in `tests/common/mod.rs` |
| `ObjectPut` (S3 PUT) | `crates/xorgate-cloud/src/s3.rs:48` | `S3Put` | fakes in `tests/recording_upload.rs` |
| `TelemetrySource`, `PhaseHooks`, `ConfigPlane`, `CommandPlane` | `crates/xorgate-agent/src/ctx.rs` | collectors, `OpsHooks`, the planes | `FakeSource`, `FakeHooks`, `FakeConfig`, `FakeCommands` |
| `RecordingBackend` and its component traits | `crates/xorgate-agent/src/recording/manager.rs:128-294` | `recording::Backend` | `tests/recording_support/mod.rs` |
| `MasterBackend`, `CaptureBackend` | `media/supervisor.rs:76`, `recording/capture/backend.rs:41` | real child processes | fake children in the media and capture tests |

Most fakes record what was asked of them on a shared **timeline** (`Vec<String>`), so a test can
assert the exact order of calls. Order is part of parity: for example, on connect the reference
publishes `online:true` before it subscribes. `crates/xorgate-agent/tests/plane.rs:136-158` checks
exactly that.

**2. The same environment knobs the reference read.** Many readers are plain functions over a
path, and the path comes from configuration: `GPS_NMEA_PORT`, `LTE_TELEMETRY_FILE`,
`XORGATE_PYTHON_DIR`, `HARDWARE_MANIFEST`, `KVS_MANIFEST`, `XORGATE_HOME`
(`crates/xorgate-agent/src/env.rs:1-19`). Tools (`vcgencmd`, `i2cdetect`, `timedatectl`, `sudo`,
`rpicam-hello`) are found on `PATH`. A test or the replay rig points these at a pty, a temp file, a
fake Python script or a directory of shell shims. Because the Rust agent honours exactly the same
knobs as the reference, one rig can drive both.

`xorgate_hal::test_support::write_executable` writes such a shim safely from a multi-threaded test
(the module doc at `crates/xorgate-hal/src/test_support.rs:1-17` explains the `ETXTBSY` race it
avoids). It is only compiled with the `test-support` feature.

## Child processes go through one helper

The agent runs many child processes: the IMU and OBD Python helpers, one `gst-launch-1.0` per
camera, one KVS master per camera, `vcgencmd`, `sudo`, `curl` in the updater. All of them are
created through `xorgate-hal`:

- `xorgate_hal::child::spawn` (`crates/xorgate-hal/src/child.rs:214-321`) for supervised
  long-running children: own process group (so the whole tree can be signalled), killed if the
  handle is dropped, stdout and stderr drained line by line, the exit delivered last. Signals are
  sent only by the task that owns the child and has just checked it is not yet reaped, so a
  recycled PID can never be hit.
- `xorgate_hal::exec::run` (`crates/xorgate-hal/src/exec.rs:23`) for short runs with a timeout,
  returning stdout, stderr and the exit code, never an error.
- Both build their command with `xorgate_hal::child::command` / `std_command`:

```rust
// crates/xorgate-hal/src/child.rs:193-198
#[allow(clippy::disallowed_methods)] // the one sanctioned `Command::new`
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    strip_notify_env(&mut cmd, |k| std::env::var_os(k).is_some());
    cmd
}
```

**Why.** The agent runs under systemd with `Type=notify` and a watchdog. systemd puts
`NOTIFY_SOCKET`, `WATCHDOG_PID` and `WATCHDOG_USEC` into the agent's environment. A child that
inherited them could talk to systemd as if it were the agent. On a production device the OBD helper
did exactly that, producing a stream of "reception only permitted for main PID" errors. The fix is to
strip those three variables from every child (`NOTIFY_ENV_KEYS`, `child.rs:177`) while the agent
keeps them for itself.

**How it is enforced.** A rule that relies on everyone remembering it does not last, so
`clippy.toml` bans every other way to start a process:

```toml
# clippy.toml:9-12 (trimmed)
disallowed-methods = [
  { path = "std::process::Command::new", reason = "use xorgate_hal::child::std_command (strips NOTIFY_SOCKET/WATCHDOG_PID/WATCHDOG_USEC, F-P6-ENV)" },
  { path = "tokio::process::Command::new", reason = "use xorgate_hal::child::command (strips NOTIFY_SOCKET/WATCHDOG_PID/WATCHDOG_USEC, F-P6-ENV)" },
  { path = "libc::fork", reason = "spawn through xorgate_hal::child (F-P6-ENV)" },
  # … every exec*, posix_spawn*, system, popen, and the nix fork/exec family
]
```

With `clippy -D warnings` in CI, a raw `Command::new` anywhere in agent code fails the build. Code
that is not the agent (tests that start the real binary, `build.rs`, the replay rig) opts out at the
top of the file with `#![allow(clippy::disallowed_methods)]` and a comment saying why. Do not add
that opt-out to agent code. `scripts/tests/clippy-ban-probe.sh` proves that each banned path really
is flagged. `crates/xorgate-hal/tests/child_env.rs` and `crates/xorgate-agent/tests/child_env.rs`
prove that children do not see the variables.

## Golden fixtures

A golden fixture is a recorded input together with the output the reference agent produced for it.
`fixtures/` has one folder per core module (`fixtures/power/`, `fixtures/nmea/`,
`fixtures/settings/`, …). Every file has the same envelope:

```json
{
  "jsSourceFile": "test/power.test.ts",
  "jsCommit": "89b5c6407ef88e20f7168bd767d22a7be04ebcb6",
  "generatedBy": "…",
  "cases": [
    { "name": "splits current and sticky nibbles", "op": "parseThrottled",
      "input": "throttled=0x50005\n", "expected": { "current": 5, "sticky": 5 } }
  ]
}
```

(`fixtures/power/power.json`, reformatted and trimmed.) The tags record which reference file and
commit the cases came from. The shared loader refuses a fixture whose commit is not one of the two
pinned reference commits, or that has no cases (`crates/xorgate-core/tests/common/mod.rs:39-52`).

A fixture test reads the file, runs the Rust function on each input and compares with the expected
output, usually as `serde_json::Value`, so a difference in shape fails too:

```rust
// crates/xorgate-core/tests/cameras_fixtures.rs:20-36 (trimmed)
#[test]
fn hardware_block_test_ts_camera_enumeration() {
    let f = common::fixture::<Case>("cameras/enumeration.json");
    for c in &f.cases {
        let platform = Platform::parse(c.input["platform"].as_str().unwrap()).unwrap();
        let got: Value = match c.op.as_str() {
            "parseCameraList" => serde_json::to_value(parse_camera_list(
                c.input["output"].as_str().unwrap(),
                platform,
            ))
            .unwrap(),
            // … "connectorForPath"
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
```

Serialisation is tested the same way: `crates/xorgate-core/tests/telemetry_fixtures.rs` parses each
payload in `fixtures/telemetry/payload-shapes.json` and requires that re-serialising gives the
**identical string**.

**Where fixtures come from.** Fixtures are dumped from the reference agent, never written by hand.
Most drivers are in `tools/replay/js/`. `run-tsx.sh <driver>` runs one driver against a scratch copy
of the reference at the pinned commit (made with `git archive`, so the reference repository is never
modified), and `redump-all.sh` re-runs every committed driver, with `redump-compare.py` to compare
against the committed files.

Not every fixture has a committed driver. Some early ones were dumped by scripts that are not in
this repo, for example `fixtures/telemetry/payload-shapes.json`, `fixtures/modem/modem.json`,
`fixtures/modem/telemetry-tolerance.json` and `fixtures/commands/commands.json`. **Check the file's
`generatedBy` field:** if it names a `tools/replay/js/` driver, re-run that driver; if it says the
script was not committed, changing that fixture means writing a new driver here first.

If a fixture test fails, the Rust code is wrong, not the fixture. Editing a fixture to make a test
pass defeats its purpose.

Two special fixtures:

- `fixtures/config-contract.json` is generated by the **backend** from its config registry, not by
  the reference agent. See chapter 6.
- `fixtures/replay/smoke.ndjson.gz` is the only committed tape (below).

## The record/replay rig

Fixtures test functions. The rig tests the whole agent: the real reference agent and the real Rust
agent run side by side, fed the **same recorded inputs**, and everything each one publishes or
writes is compared field by field.

### Tapes

A **tape** is a recording of everything that came into an agent: NMEA lines, IMU lines, every
version of the modem daemon's file, `vcgencmd` outputs, inbound MQTT messages, connection drops,
CLI invocations, child-process lines and exits, `sudo` results, and a clock sample every 10 s.

- Format: gzip-compressed NDJSON. The first line is a header (`"k":"meta"`, with the device's
  hardware block and starting files). Every other line is
  `{"t": <ms since start, monotonic>, "w": <wall ms>, "k": <kind>, …}`
  (`crates/xorgate-hal/src/tape.rs:1-24`).
- Recording: start the Rust agent with `XORGATE_RS_TAPE=<path>`. Every HAL reader reports what it
  just read through `TapeSink`, at the point of reading. Without a tape the sink is `NullTape` and
  each hook costs one empty call.
- Synthetic tapes: `xorgate-replay synth <scenario>` builds a deterministic tape from fixtures;
  `xorgate-replay synth --list` lists the scenarios (`crates/xorgate-replay/src/synth.rs:71`).

**Real tapes are never committed.** A tape from a real device contains its location and SIM
identity. They stay on the machine that pulled them (`scripts/bench/tape-pull.sh`) and are
gitignored. Summaries of real tapes must not quote GPS or IMU values either.

### The rig and the diff

`tools/replay/docker-compose.yml` starts a TLS MQTT broker (mosquitto) and a `rig` container. Inside
it, `xorgate-replay play` starts both agents as its children, each with its own device ID, and feeds
the tape through the [knobs above](#the-seams): a pty per agent for NMEA, a rewritten LTE file, a
fake IMU helper, PATH shims, and a TCP proxy in front of the broker to reproduce connection drops.
It shifts embedded timestamps to "now" and holds input changes until both agents have taken the same
1 Hz tick, so that the two outputs pair up.

`xorgate-replay diff <run>` then compares:

| Stream | How |
|---|---|
| telemetry | tick by tick: every leaf, the key **order**, numbers as JSON text |
| config reports | in order, after each quiet period |
| acks, status, SMS replies | in order |
| files at the end | `settings.json` byte-exact; the other state files as JSON |
| `state.json` and its events | at the end, and the set of events seen |
| `sudo` calls | the exact argv sequence |
| recordings, sidecars, S3 PUTs, child processes | (media runs) |

Every difference is either covered by a row of the tolerance table, and the report names the row,
or it is **UNTOLERATED** and the diff exits 1.

### The tolerance table, and why it is never widened

`tools/replay/tolerances.toml` lists the only differences allowed between the two agents. Each row
has an `id`, a `title`, a `basis` (the evidence), a `kind` and the paths it covers:

```toml
# tools/replay/tolerances.toml:52-59
[[rule]]
id = "T2"
title = "seq compared as deltas; deviceId and requestId normalised to rig ids"
basis = "phase file T2"
kind = "deltas"
streams = ["telemetry", "state"]
paths = ["seq", "latest.seq"]
normalize_ids = true
```

Typical rows exclude timestamps (two processes never stamp the same millisecond, but key order is
still checked), compare host measurements such as CPU and RAM loosely (the two agents run on the
same busy container), or import an entry of the accepted-divergence register (`T7`), each with the
test that pins it.

The table is the definition of "same behaviour". Widening a row, or adding one, to turn a red run
green would hide exactly the regressions the rig exists to catch. So the rules are:

- a new or wider row needs evidence from the reference side and a reviewer's agreement, cited in
  `basis`;
- when a run fails, find the root cause. Either the Rust agent is wrong (fix it), or the rig is
  wrong (fix the rig, and prove it with a calibration run), or the host was too loaded (see below);
- a host measurement (`T3/host`) may explain a CPU or RAM difference only when the rig's own
  sampling shows the host moved by that much. It never widens what an agent may report.

### Running a replay

You need Docker, the build image (`scripts/docker-build.sh` once), and a checkout of the reference
agent repository at `../xorgate-device-agent` (or set `JS_REPO=<path>`). Then, from the repo root
(`tools/replay/README.md` has the full guide):

```sh
tools/replay/gen-certs.sh      # throwaway CA and certs for the rig broker
tools/replay/js-build.sh       # build the reference at the pinned commit into tools/replay/.cache
tools/replay/build-bins.sh     # xorgate-rs + xorgate-replay for linux/arm64 → tools/replay/.cache/bin

# a synthetic tape
docker compose -f tools/replay/docker-compose.yml run --rm -T rig \
  synth calibration -o /rig/out/calibration.ndjson.gz --fixtures /rig/fixtures
mkdir -p tools/replay/.cache/tapes
mv tools/replay/.cache/runs/calibration.ndjson.gz tools/replay/.cache/tapes/

tools/replay/run-rig.sh tools/replay/.cache/tapes/calibration.ndjson.gz --calibrate --out <dir>  # reference vs reference
tools/replay/run-rig.sh tools/replay/.cache/tapes/calibration.ndjson.gz --out <dir>              # reference vs Rust
```

Read `<dir>/diff.md`. The exit code is the diff's. Things to know:

- **Calibrate first.** Two reference instances on the same tape must diff clean. If they do not, the
  rig has a problem, and a reference-vs-Rust result means nothing yet.
- **It runs in real time**: the tape's length plus 20 s. `--speed` exists for debugging only;
  timers behave differently, so a faster run is not evidence.
- **One run at a time per Docker host, on a quiet host.** CPU and RAM values come from the rig
  container, and a parallel build swings them past the tolerance and delays ticks. A failure on a
  loaded machine is not a verdict: re-run it when the host is quiet.
- Without `--out`, `run-rig.sh` copies the run into the migration plan's evidence folder if that
  exists next to the repo, else into `tools/replay/.cache/evidence`.

### Cheaper checks that need no reference build

- `scripts/docker-build.sh test -p xorgate-replay` tests the tape format, synthesis, the gating
  logic and the diff and tolerance engine.
- **The CI smoke replay**, `tools/replay/ci-smoke.sh <xorgate-rs> <xorgate-replay> [out dir]
  [smoke|media]`, replays the committed smoke tape Rust-vs-Rust through a local TLS broker, with no
  Docker inside. CI runs it as a step of `ci.yml`, inside its Bookworm container. It must run as
  root in a **throwaway container only**, because it edits `/etc/hosts`. The build container of
  `scripts/docker-build.sh` is one (it is removed when the shell exits):

  ```sh
  scripts/docker-build.sh shell
  apt-get update && apt-get install -y --no-install-recommends mosquitto openssl python3 procps
  cargo build -p xorgate-agent -p xorgate-replay
  tools/replay/ci-smoke.sh "$CARGO_TARGET_DIR/debug/xorgate-rs" "$CARGO_TARGET_DIR/debug/xorgate-replay" /tmp/smoke
  ```

  Inside that container `CARGO_TARGET_DIR` is `/target/<checkout dir name>`, so the debug binaries
  are under `$CARGO_TARGET_DIR/debug/`. The 60 s tape plays in real time, and the script exits
  non-zero on any untolerated difference. The `media` mode also needs the GStreamer tools and plugins and a
  `/dev/shm` larger than that container gets by default; leave it to CI.

### The CLI matrix

`tools/replay/cli-matrix.sh` runs the same list of CLI invocations through both agents' CLIs and
diffs stdout, stderr, the exit code and the resulting `settings.json` after every step. It needs the
reference build, so do the three set-up steps of [Running a replay](#running-a-replay) first. It runs
**inside the rig container**, and the compose project must be torn down with `down -v` afterwards,
or the broker's volumes leak:

```sh
docker compose -p cli-matrix -f tools/replay/docker-compose.yml run --rm -T --entrypoint bash rig \
  /rig/tools/cli-matrix.sh --out /rig/out/cli-matrix-<ts>
docker compose -p cli-matrix -f tools/replay/docker-compose.yml down -v --remove-orphans
```

The results land in `tools/replay/.cache/runs/cli-matrix-<ts>/`. `--a` and `--b` choose the sides
(default: reference vs Rust), and `--fresh` runs the per-case list in `tools/replay/cli/cases.txt`
instead of the cumulative one. The "CLI matrix" section of `tools/replay/README.md` has the rest.

## Other end-to-end rigs

| Rig | Tests |
|---|---|
| `tools/convert-e2e/` | `install-rs.sh` converting a simulated reference-provisioned device and back (see its README) |
| `tools/update-e2e/` | the `xorgate-rs update` rail against a local fake dist bucket |
| `tools/obd-e2e/` | the OBD-II helper against an emulator |
| `tools/sms-fake-modem/` | the SMS reader against a fake modem on a pty |

Each one documents its command in its header or README. All of them run in Docker, and none touches
a real bucket or device.
