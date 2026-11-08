# 9. Gotchas and glossary

## Gotchas

Real pitfalls found while building this agent. Most have a test or a lint guarding them now; this
list tells you why the guard is there.

### glibc keeps freed upload buffers

The uploader reads each closed video fragment (about 7 MB) into one buffer and frees it after the
PUT. With glibc's default *dynamic* mmap threshold, the first such free raises the threshold, and
every later fragment buffer then comes from a per-thread arena and stays resident after it is freed.
The live heap stayed around 1 MB while the process's resident memory climbed from 30 to 80 MiB in
7 MiB steps over an hour on the bench.

The fix is one call, `xorgate_agent::alloc::tune()` (`crates/xorgate-agent/src/alloc.rs`), which
sets `M_MMAP_THRESHOLD` to 1 MiB explicitly. Setting it turns the dynamic adjustment off, so large
buffers are mapped and returned to the kernel on free. It must run **first thing in `main`, before
any thread exists** (`crates/xorgate-agent/src/main.rs:12`). `tests/alloc_tune.rs` and
`tests/main_calls_alloc_tune.rs` keep it that way. Do not move it after the tokio runtime is built.

### The watchdog ping lives in the main loop

systemd restarts the agent if it does not send `WATCHDOG=1` within 60 s. The ping is sent from the
worker's 1 s heartbeat branch (`crates/xorgate-agent/src/worker.rs:856-857`), **independently of
MQTT**. Two ways to break it:

- moving the ping into the publish loop, which only starts after the first successful connect: a
  device that boots without network would be restarted every minute;
- doing slow synchronous work inside a `select!` branch of the main loop, which delays every other
  branch, the heartbeat included. Hand work to a task instead.

`crates/xorgate-agent/tests/watchdog.rs` checks that pings continue while a connection attempt
hangs.

### Child processes must not inherit the notify variables

Every child is spawned through `xorgate_hal::child`, which strips `NOTIFY_SOCKET`, `WATCHDOG_PID`
and `WATCHDOG_USEC` (chapter 5). A child that kept them was found talking to systemd on a production
device. The agent's own environment keeps them.

### Raw spawning is a lint error

`clippy.toml` bans `Command::new` (std and tokio), the libc `fork`/`exec*`/`posix_spawn*`/`system`/
`popen` family and the nix equivalents. Use `xorgate_hal::child::command` / `std_command`, or
`xorgate_hal::exec::run`. The opt-out `#![allow(clippy::disallowed_methods)]` is for code that is not
the agent (tests that launch the binary, `build.rs`, the replay rig), always with a comment.

### GStreamer feature level and the Bookworm target

The `gstreamer` crate is built with feature `v1_22` only (`Cargo.toml:49`). A newer feature level
would compile on a newer system, but the binary would refer to symbols that GStreamer 1.22 does not
have and **fail to load** on a Bookworm device (the system links with full RELRO, so unresolved
symbols fail at start, not at first use). Related rules:

- build on Bookworm; the result also runs on Trixie, never the other way round (glibc);
- no musl static build: GStreamer loads plugins at run time;
- GStreamer elements from gst-plugins-rs (for example `isofmp4mux`) are not available on Bookworm.

### MSRV and dependency pins

- The toolchain is pinned to 1.99.0. Several dependencies need a recent compiler, and the
  distribution `rustc` is far too old. Bump it on purpose, in its own change.
- `cargo update` can raise a dependency's own minimum Rust version, or pull in a second version of a
  crate. Read `Cargo.lock` diffs; `cargo deny` warns on duplicates.
- Workspace path dependencies also carry a `version` (`Cargo.toml:20-25`), because `deny.toml`
  forbids wildcard requirements and treats a bare path dependency as one. Keep the two in step with
  `[workspace.package].version`.
- The MQTT library (`rumqttc` 0.25) brings an old `rustls-webpki` that has advisories. The agent
  never uses that code path (it builds its own rustls config), so `deny.toml` ignores those advisory
  IDs with reasons. Revisit them when `rumqttc` updates; do not add unexplained ignores.

### The device shares one power supply

On the devices, the Pi and the LTE modem share one power supply. Marginal supplies are a real field
problem: undervoltage crashes in vehicles were traced to it, and the load-shedding ladder exists
because of it. Two consequences for developers:

- **Never build on a device.** A full `cargo build` on the bench device's CPU browned it out. Build
  in Docker and copy the tarball (chapter 1).
- **CPU on the device is a power budget.** A change that adds steady CPU load (a busy loop, a faster
  poll, a heavier encoder setting) is a power change. Measure on the bench before it ships.

### Wall clock versus monotonic clock

The devices have no real-time clock battery. Their wall clock is corrected minutes into almost every
boot, by NTP, GPS or the modem. Any interval measured with the wall clock breaks when that happens:
a backward step once made a segment's age negative and stopped segment rotation for the rest of the
boot. Rule: intervals, timeouts and ages use `tokio::time::Instant` or `ctx.clock.mono_ms()`; the
wall clock (`ctx.now_ms()`) only stamps samples and names files. `xorgate-core` cannot read either
clock at all; it gets them as arguments.

### Exact JSON is easy to break by accident

- Field order in a serialised struct is the key order on the wire. Reordering fields, or inserting a
  new one "at the end" when the reference puts it in the middle, is a contract change.
- `HashMap` iteration order is random. Use `IndexMap` (or a struct) for anything serialised.
- An `f64` field serialised directly prints `3000.0`. Use `JsNumber` or `#[serde(with = "js_num")]`.
- `None` prints `null` unless the field has `skip_serializing_if = "Option::is_none"`, and the wire
  needs one or the other depending on the field.
- Rust's `f64::round` rounds halves away from zero; the reference's rounding rounds them up. Use
  `xorgate_core::round`.

### Log lines and messages are an interface

Operators and scripts grep the journal for lines like `[agent] cloud config applied: …`, and the
`state.json` events and report texts are shown in the console. Keep their wording, including the
`[agent]` and `[media <channel>]` prefixes, unless the change is agreed.

### Locks and callbacks

Never hold a `std::sync::Mutex` guard across an `.await`, and never call another component while
holding a lock if that component can call back. The recording manager and the capture supervisor
write down their lock order and callback rules in their module docs; read them before changing
either.

### Timers re-arm from the fire time

Use `crate::timer::SetInterval`, not `tokio::time::interval`. tokio's interval is fixed-rate, and the
resulting slightly short gaps changed which consumer refreshed a shared once-a-second cache, which
showed up as stale power data and missing samples.

### MQTT client details

- The maximum packet size is raised to 150,000 bytes. With the library's 10 KiB default, a large
  retained config resets the connection on every connect.
- The keepalive is clamped to 5–65,535 s, because the library rejects shorter values and the protocol
  field is 16 bits (`worker.rs:58-85`).
- The Last-Will is re-stamped with the connect time before every attempt; the backend reads that
  timestamp (chapter 6).

### One identity, one agent

Two processes connecting with the same device identity disconnect each other forever and fire each
other's Last-Will. That is why the Rust unit `Conflicts=` the reference agent's unit. The bench runs
both agents only because the Rust one uses a separate development identity there, and the bench's
installed unit has `Conflicts=` removed by `scripts/bench/install.sh`.

### The pinned update version

`agent.env` may pin `DEVICE_DIST_VERSION` to a reference-agent build tag. The Rust updater unit
overrides it to `latest` with a drop-in, but a hand-run `sudo xorgate-rs update` does not see the
drop-in: pass `--tag latest`.

### Children and shared memory

- A capture child killed with SIGKILL cannot clean up its `/dev/shm/shmpipe.*` segment (about 20 MB
  each). The capture supervisor reaps them after every child exit, and boot removes segments of dead
  owners (`crates/xorgate-hal/src/shm_reaper.rs`). Keep that if you change how children stop.
- The KVS master blocks when its stdout pipe fills (64 KB). Its output must always be drained, even
  when nobody reads the lines.
- A capture child that writes nothing for 45 s is killed and restarted (a known GStreamer stall at
  high resolutions).

### Reproduced quirks

Some behaviour of the reference is kept on purpose even though it is not ideal, because the cloud
was built against it. The best-known one: a video fragment's sidecar `startTs` is about 2.1 s late
(the encoder buffers one GOP before the fragment file appears), so replays show video about 2 s
behind telemetry. Fixing it is a planned change of its own, coordinated with the cloud, not a
drive-by fix.

### Build and tooling traps

- A build in a `git worktree` reports `unknown` as its commit (chapter 1). Harmless; packed builds
  carry the real one.
- Each checkout's target directory in the `xorgate-rs-target` volume grows to 10–20 GB. Delete it
  when you delete the checkout.
- A replay run on a busy machine can fail on CPU or RAM tolerances or tick pairing. That is not a
  verdict; re-run on a quiet host, one rig run at a time.
- Tapes recorded on a real device contain its location and SIM identity. Never commit one, never
  paste values from one into an issue or a summary.

## Glossary

| Term | Meaning |
|---|---|
| **Accepted divergence** | A deliberate, approved difference from the reference agent, numbered `D1`, `D2`, … in a register kept with the migration plan, pinned by a test and covered by a tolerance row. |
| **Apply class** | How a config namespace reaches the hardware: `agent-poll` (the agent applies it), `modem-poll` (the modem daemon applies it on its ~30 s poll), `agent-restart` (unused). |
| **Bench** | The maintainers' test device, on which the Rust agent runs next to a reference agent under a separate development identity. |
| **Boot defer / GPS gate** | The one-shot policy that holds video encoders back after boot on a weak supply (stage 1), optionally until GPS or LTE is up (stage 2). |
| **Bridge (shm bridge)** | The shared-memory video feed from a capture child to its KVS master (`shm/<stream-key>.sock`, fixed `I420 1280x720 15/1`). "Bridge-only" mode feeds live video without recording. |
| **Build channel** | Where a build came from: `dev`, `prod`, `bench`, `ci`, or `dev-local` for a binary without `build-info.json`. |
| **Capture child** | The supervised `gst-launch-1.0` process that owns one camera, writes MP4 fragments and feeds the bridge. |
| **Choke point** | The single place where live video is parked (`RecordingManager::apply_video`), so the published `liveBlock` always matches what happened. |
| **Clock trust** | The gate that holds live publishing and recording (at most 5 minutes) until the wall clock is trustworthy. |
| **Collector** | A source of one telemetry group: `imu`, `gps`, `lte`, `system`, `obd`. Implements the `Collector` trait. |
| **Config report** | The device's `…/config/reported` message: `effective` (what is in force), `status` (per namespace: `applied`, `adjusted` or `rejected`, with a detail or reason), `origin` (`boot`, `cloud` or `local`). |
| **Conversion** | Switching a device from the reference agent to the Rust agent with `install-rs.sh`; `--revert` switches back. |
| **Dist bucket** | The S3 bucket (one for dev, one for prod) that holds agent tarballs, their checksums, `latest-rs.json` and `install-rs.sh`. |
| **Effect** | A data description of one action ("persist the ledger", "ack accepted", "run this argv") returned by a pure decider in core and executed by the agent. |
| **Exclusive window** | A bench session during which the reference agent is stopped so the Rust agent can own the exclusive hardware (GNSS port, IMU, cameras). |
| **Fail open** | When an input is missing, behave as if all is well (no LED input → healthy, no gate input → gate off), rather than blocking. Two rules fail closed instead (chapter 6). |
| **Fixture (golden fixture)** | A JSON file of inputs and the reference agent's outputs, used as the expected result in a test. |
| **Fragment** | One roughly 60 s MP4 file of a video session (`NNNNN.mp4`), uploaded with its sidecar. |
| **Group, leaf** | A `metrics` group (`system`, `gps`, …) and one of its fields, always `{value: number, unit?}`. |
| **Hardware block** | `/etc/xorgate/hardware.json` (also sent by the cloud): platform, encoder, cameras, IMU, status LED, rails, peripherals such as OBD. |
| **Heartbeat** | Two things: the worker's 1 s tick (watchdog ping, LED tick), and the config plane's hourly report. |
| **Hooks** | `PhaseHooks` (`ctx.rs`): the worker's call sites into recording, LED, clock and camera code, implemented by `OpsHooks`. |
| **KVS, KVS master** | Amazon Kinesis Video Streams WebRTC, the live-video service; the master is the vendored `xorgate-kvs-webrtc` process, one per camera channel. |
| **Ledger** | `commands.json`: request IDs of executed commands, kept an hour, so a redelivered command is never run twice. |
| **liveBlock** | The telemetry field that says live video is blocked, why and since when; absent when live video is allowed. |
| **LWT (Last-Will)** | A message the broker publishes for a client that disappears without disconnecting: here `…/status {online:false}` with the connect-time timestamp. |
| **Namespace** | A top-level key of the config document and of `settings.json` (`recording`, `timeSync`, …). |
| **Offline mode** | `xorgate-rs run --offline`: no AWS, payloads printed to stdout. |
| **Parity reference** | The previous agent (TypeScript, the `xorgate-device-agent` repo) at a pinned commit. Code comments like `worker.ts:1245` point into it. |
| **Pipelines** | Files in `/etc/xorgate/pipelines/` describing each camera's GStreamer pipeline, rendered from the hardware block. |
| **Plane** | The agent's side of one MQTT conversation: the MQTT plane (connect, publish), the config plane, the command plane. |
| **Real mode, sim mode** | `MODE=real` reads hardware; `MODE=sim` publishes empty metrics (hardware-free). Replay mode is not supported by the Rust agent. |
| **Retained message** | An MQTT message the broker stores and delivers to every new subscriber; the desired config is one. |
| **Rev guard** | The rule that a config revision already applied is not applied again on redelivery. |
| **Rig** | The Docker set-up that runs the reference and the Rust agent on the same tape and diffs them. |
| **Scope** | The tenant (organisation, workspace) the cloud assigns; it moves telemetry to the scoped topic. |
| **Seq** | The telemetry sequence number: 0 at process start, +1 per tick. |
| **Session** | One recording run, named by a UUIDv7; telemetry segments and video fragments are grouped by session. |
| **Shed, rung** | Load shedding under undervoltage or heat: rung 1 parks live video, rung 2 also stops video recording (power only). |
| **Sidecar** | The JSON file next to each video fragment (`NNNNN.json`) with its timing and metadata; uploaded before the MP4. |
| **Sky state** | A GNSS verdict (clear, obstructed, no RF) derived from satellite signal reports. |
| **Stream key** | A camera's name in manifests and paths, for example `cam0`. |
| **Tape** | A recording of everything an agent read, used to replay those inputs to both agents. |
| **Tolerance row** | One allowed difference in `tools/replay/tolerances.toml`, with an ID (`T1`, …) and its evidence. |
| **TTFF** | Time to first fix: how long the GNSS receiver took to get a position. |
| **Watchdog** | systemd's liveness check: the agent must ping at least every 60 s or be restarted. |

## Plan IDs in code comments

Comments and test names refer to entries of the migration plan, which is kept outside this repo. You
do not need it to work on the code; the comment around an ID normally states the rule. The families:

| Looks like | Is |
|---|---|
| `D1` … `D41` | accepted divergences (see above) |
| `T1` … `T18`, `T7/D20`, `T8/O3` | rows of the replay tolerance table |
| `invariant 2`, `agent-anatomy §10` | the list of behaviours that must survive the rewrite, the basis of chapter 6 |
| `O1`, `P8`, `CF33`, `Q6`, `I4` | decision-log entries, decided questions and interface notes of the plan |
| `F-P3-MEM`, `F-P6-ENV`, `O-P6-1` | findings from testing on the bench, each with its fix |
| `review-wp3 #5`, `WP4`, `Phase 3` | code-review comments, work packages and stages of the port |

When you write new code, describe the rule in plain words. Add an ID only as a pointer next to that
description, never instead of it.
