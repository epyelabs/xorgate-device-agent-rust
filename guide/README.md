# The xorgate Rust device agent: a guide to the codebase

This guide is for developers who are new to Rust. It assumes you already know how to program:
async code and event loops, modules and packages, JSON, HTTP, MQTT and everyday Linux. It does not
assume you know ownership, `Result`, traits, Cargo or tokio. Those are explained as they show up in
this repository, using real code from it.

## What the agent is

`xorgate-rs` is the program that runs on an xorgate device: a Raspberry Pi Compute Module (CM4 or
CM5) in a vehicle, with an LTE modem, a GNSS receiver, an IMU, one or two cameras and an RGB status
LED. It runs as a systemd service and does five jobs:

1. **Telemetry.** Once a second it reads its sensors (the "collectors": IMU, GPS, LTE, system,
   optional OBD-II) and publishes one JSON payload over MQTT to AWS IoT Core.
2. **Configuration.** It receives the cloud's desired configuration (a retained MQTT message),
   writes it into `settings.json`, and reports back what is actually in force.
3. **Commands.** It accepts one-shot cloud commands (`reboot`, `update`, `read_sms`, …), acks them
   and runs them.
4. **Recording.** It records telemetry and camera video to disk, in segments, and uploads them to
   S3 when the device is online.
5. **Live video and housekeeping.** It supervises the live-video (KVS WebRTC) processes, sheds load
   when power or temperature is marginal, drives the status LED, steps the clock when there is no
   NTP, and keeps a `state.json` file that the CLI and other tools read.

The same binary is also the operator CLI: `xorgate-rs status`, `xorgate-rs set …`,
`xorgate-rs logs`, `xorgate-rs update`, `xorgate-rs manage` and so on.

## One idea to keep in mind: parity

The Rust agent is a rewrite and the successor to an earlier agent written in TypeScript (the
`device/xorgate-device-agent` repository). It keeps **byte-for-byte parity** with that agent at a
pinned commit, and that earlier agent is the **parity reference**. The previous agent is still the
production default on fielded devices; this agent runs alongside it until cutover. The two are
interchangeable on a device: a device can be converted to the Rust agent and back with no cloud
change (chapter 7). In practice parity means:

- every MQTT topic, JSON payload, file format and log line that something else reads must come out
  exactly as the reference produced it, down to key order and how numbers are printed;
- code comments often cite the reference's source, like `worker.ts:1245` or `` `state.ts:19` ``.
  Those are pointers into the reference repository at the pinned commit, not into this one;
- known quirks of the reference are sometimes reproduced on purpose, and the few places where the
  Rust agent deliberately differs are recorded and pinned by tests (an "accepted divergence"
  register).

So when you change something, the first question is not only "does it work?" but also "does it
still match the reference, or is this an approved difference?". Chapter 6 covers this.

## How to read this guide

Read chapters 1 to 4 in order the first time. After that, use chapters 5 to 9 as reference.

| # | File | What it covers |
|---|---|---|
| 1 | [Setup](01-setup.md) | The pinned toolchain, the Docker-only build, running tests, fmt, clippy and deny, editor setup |
| 2 | [Workspace tour](02-workspace-tour.md) | The five crates, how they depend on each other, where to start reading |
| 3 | [Rust essentials in this codebase](03-rust-essentials-in-this-codebase.md) | Ownership, `Result`, enums, traits, async with tokio, locks, serde, lifetimes, tests, macros, `unsafe`, each shown in this repo's code |
| 4 | [How the agent works](04-how-the-agent-works.md) | Startup, the worker and its planes, collectors, `state.json`, recording, media, power shedding, the LED, shutdown and the watchdog |
| 5 | [Hardware abstraction and testing](05-hardware-abstraction-and-testing.md) | The HAL seams and fakes, child processes, golden fixtures, the record/replay rig |
| 6 | [The cloud contract](06-the-cloud-contract.md) | What is frozen, why byte-exact JSON matters, where the contract fixtures live |
| 7 | [Build, package, deploy](07-build-package-deploy.md) | `pack.sh`, the CI workflows, `install-rs.sh`, the updater, the bench scripts |
| 8 | [Common tasks](08-common-tasks.md) | Step-by-step recipes with file pointers and the checks each one needs |
| 9 | [Gotchas and glossary](09-gotchas-and-glossary.md) | Real pitfalls found in this codebase, and the project's vocabulary |

## Conventions in this guide

- `path:line` references are to this repository at commit `c2377b1` (`main` when the guide was
  written). Lines drift as code changes; if a reference looks off, search for the quoted name.
- Excerpts are short and sometimes trimmed. A trimmed excerpt says so. Open the file for the rest.
- Placeholders look like `<device-id>`, `<bench-host>` or `<bucket-url>`. The guide contains no real
  device identifiers, addresses or credentials, and neither should anything you add to the repo.
- Code comments in the repo use short IDs such as `CF33`, `O5`, `P8`, `D29` or `T8`. They point into
  the migration plan's decision log, which lives outside this repo. The ones you will meet most are
  explained in the [glossary](09-gotchas-and-glossary.md#plan-ids-in-code-comments). You do not need
  the plan to work on the code.
