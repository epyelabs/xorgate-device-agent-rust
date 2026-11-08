# 8. Common tasks

Step-by-step recipes with file pointers. Line numbers are where things were when this guide was
written; search for the named item if they have moved.

## Before any change: the parity question

Ask first: **does the reference agent do this?**

- **Yes, and the Rust agent should match it.** Port the behaviour, and take the expected outputs
  from the reference (dumped fixtures, the replay rig), not from your reading of it.
- **No, it is new behaviour.** If it changes anything on the wire or in a shared file, it is a
  contract change. It starts in the backend (and the reference agent, while it is still the
  reference) and is ported here afterwards. If it is purely internal (a refactor, a performance fix)
  it must leave every output identical, and the replay rig is how you show that.
- **It deliberately differs.** That is an accepted divergence, decided by the maintainers and
  pinned by a test (chapter 6).

Then, for every change:

```sh
scripts/docker-build.sh fmt --all -- --check
scripts/docker-build.sh clippy --workspace --all-targets -- -D warnings
scripts/docker-build.sh test
```

Each recipe below adds the specific checks it needs.

## Add a telemetry field

Example: a new numeric field in the `lte` group that the modem daemon already writes into its
`telemetry.json`. The same steps apply to other groups.

1. **Confirm the contract.** The reference agent publishes this field, under this name, with this
   unit and rounding, at this position in the group. Metric leaves must be numbers.
2. **Read it.** Add the input field to `ModemTelemetry` (`crates/xorgate-core/src/types.rs:759`) and
   to its parser `ModemTelemetry::from_map` (`types.rs:825`).
3. **Declare it on the wire.** Add an `Option<MetricLeaf>` to `LteGroup`
   (`crates/xorgate-core/src/telemetry.rs:196-211`) **at the reference's position**: field order is
   key order. Use `#[serde(default, skip_serializing_if = "Option::is_none")]` so an absent value
   leaves the key out.
4. **Assemble it.** Map it in `lte_group` (`crates/xorgate-core/src/modem.rs:124-146`): filter
   non-finite values, attach the unit, round with `xorgate_core::round::round` if the reference
   rounds.
5. **Fixtures.** Three fixtures cover this path, with different provenance (check each file's
   `generatedBy`):
   - `fixtures/collectors/lte-scenarios.json`, the agent-side collector scenarios: re-dump it with
     its committed driver, `tools/replay/js/run-tsx.sh tools/replay/js/dump-lte-scenarios.mts …`
     (the driver's header gives its arguments);
   - `fixtures/modem/modem.json` and `fixtures/modem/telemetry-tolerance.json`, which pin
     `lte_group` itself, and `fixtures/telemetry/payload-shapes.json`, the full payload: their
     dump scripts were never committed, so write a driver in `tools/replay/js/` first and add it to
     `redump-all.sh`.

   Do not hand-edit expected values.
6. **The agent side** usually needs no change: `collectors/lte.rs` converts the typed group with
   `to_group`.

For the `system` group the assembly is `xorgate_core::system::system_group`
(`crates/xorgate-core/src/system.rs:397-428`) and the raw reads are in
`crates/xorgate-hal/src/system.rs`.

**Checks:** `scripts/docker-build.sh test -p xorgate-core --test imu_modem_fixtures`,
`--test telemetry_fixtures`, `scripts/docker-build.sh test -p xorgate-agent --test collectors`, then
a replay run, reference vs Rust, on a scenario with an `lte` track (`smoke` or `calibration`).

## Add a CLI subcommand

Use `crates/xorgate-agent/src/cli/recording.rs` (95 lines) as the template: a `main` that wires real
I/O, and a `run` that takes everything it touches as arguments, so a test can call it.

1. **Create `crates/xorgate-agent/src/cli/<name>.rs`:**

   ```rust
   // shape of crates/xorgate-agent/src/cli/recording.rs:18-26
   pub fn main(args: &[String]) -> i32 {
       run(args, &crate::env::runtime_home(), &mut StdConsole)
   }

   pub fn run(args: &[String], home: &Path, c: &mut dyn Console) -> i32 {
       // parse args, do the work, c.log(..) / c.error(..), return the exit code
   }
   ```

   `Console`, `StdConsole` and the test double `Captured` are in `cli/pipeline.rs:66-110`.
2. **Register it.** Add `pub mod <name>;` to `crates/xorgate-agent/src/cli.rs` (the list is
   alphabetical) and a match arm in `cli::main` (`cli.rs:124-157`):
   `Some("<name>") => <name>::main(&rest),`.
3. **Usage text.** Add its line to `usage()` (`cli.rs:65-115`). If it takes a flag with a value
   (`--limit 5`), add the flag to `VALUE_FLAGS` (`cli.rs:27-38`), otherwise the dispatcher may
   mistake the value for the subcommand.
4. **Rules for CLI code:** run tools through `xorgate_hal::exec::run`, never `Command::new`; write
   `settings.json` only through the shared writers (`config_plane::save_settings_js`,
   `crates/xorgate-core/src/cli_set.rs` for `set`) so the bytes stay identical; if it needs root,
   check and refuse with a clear message, as `update` does.
5. **Tests.** A unit test in the file with `Captured`, like `recording.rs:46-95`. For end-to-end
   behaviour, an integration test in `crates/xorgate-agent/tests/cli_<name>.rs` that runs the real
   binary (`env!("CARGO_BIN_EXE_xorgate-rs")`).

**Checks:** the three gates; the CLI matrix
([chapter 5](05-hardware-abstraction-and-testing.md#the-cli-matrix), runs in the rig container) if
the subcommand exists in the reference CLI (its output, exit code and any `settings.json` change must match). A verb that exists only in
the Rust agent is a divergence: agree on it first.

## Add a config setting with clamping

Example: a new numeric field in the existing `timeSync` namespace.

1. **Contract first.** The backend's config registry defines the field, its default and its bounds.
   Copy the regenerated `fixtures/config-contract.json` from the backend verbatim (never edit it by
   hand).
2. **Settings type.** Add the field to `TimeSyncSettings`
   (`crates/xorgate-core/src/settings.rs:494-506`) as an `Option`, in the order the reference parser
   inserts keys, with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
3. **Parser and clamp.** In `parse_time_sync_settings` (`settings.rs:509-520`):

   ```rust
   // crates/xorgate-core/src/settings.rs:514-517 (the existing pattern)
           step_threshold_secs: num(src.get("stepThresholdSecs"))
               .map(|t| clamp_round(t, 2.0, 600.0) as u32),
           max_steps_per_boot: num(src.get("maxStepsPerBoot"))
               .map(|m| clamp_round(m, 1.0, 20.0) as u32),
   ```

   `clamp_round` rounds like the reference, then clamps (`settings.rs:100-102`); `clamp` is the
   variant for decimal knobs. The device's range must **contain** the cloud's bounds: the cloud
   rejects, the device clamps.
4. **Default and resolution.** Add it to `TimeSyncConfig`, `TIME_SYNC_DEFAULTS` and
   `resolve_time_sync_config` (`settings.rs:522-553`).
5. **Use it** where the namespace is consumed. The clock stepper re-reads `timeSync` from
   `settings.json` on every pass (`crates/xorgate-agent/src/worker.rs:331-337`), so a cloud change
   applies live.
6. **`xorgate-rs set`.** Add the field to `set_time_sync` in `crates/xorgate-core/src/cli_set.rs:651`
   and to the supported-keys message there and in `USAGE` (`cli_set.rs:41`), worded as the
   reference.
7. **Contract test.** Extend `crates/xorgate-core/tests/config_contract.rs` (default, and
   clamp-contains-bounds, following the existing cases).

The cloud apply path (`xorgate_core::cloud_config`) and the report's `effective` block reuse the
namespace parser, so an existing namespace needs nothing more there. A **new namespace** touches
much more: `config_namespaces.rs` (apply class and key list), `AgentSettings` and
`parse_settings_object` (`settings.rs:858-901`), `CloudConfig`, `parse_namespaces` and `merge`
(`crates/xorgate-core/src/cloud_config.rs`), and the per-namespace cases in `config_report.rs`. Plan
that with the maintainers.

**Checks:** `scripts/docker-build.sh test -p xorgate-core --test config_contract`,
`--test settings_fixtures`, `--test cloud_config_fixtures`, `scripts/docker-build.sh test -p
xorgate-agent --test cli_set --test config_message --test config_sync`, the CLI matrix
([chapter 5](05-hardware-abstraction-and-testing.md#the-cli-matrix)),
and a replay run with a `config` track, which compares `settings.json` byte for byte and every
report.

## Handle a new cloud command

1. **Contract first:** the backend sends it, the reference agent handles it, and you have its exact
   ack wording and effects.
2. **The name.** In `crates/xorgate-core/src/commands.rs`: add the variant to `DeviceCommandName`
   (`:43-66`), to `ALL` (`:70-79`; the array length is part of its type, so update it), and to
   `as_str` (`:81-92`). If it runs in-process, add it to `is_in_process` (`:101-110`). The compiler
   now lists every `match` that must handle it.
3. **The decision.** In `decide_command` (`crates/xorgate-core/src/command_flow.rs:385`) produce its
   effects in the reference's order: usually `PersistLedger`, logs and events, the `Ack`, then the
   work. If the work is new, add an `Effect` variant (`command_flow.rs:273-332`).
4. **The execution.** Interpret the new effect in `Effects::run`
   (`crates/xorgate-agent/src/command_plane.rs:297`). Anything slow runs in the spawned half, never
   in the command task itself, so later commands are not delayed.
5. **Privilege.** If it needs `sudo`, stop and talk to the maintainers: a new grant means a sudoers
   file, `install-rs.sh`, `update_plan` and the bench all change together. The privileged argv can
   only come from `commands::plan` (`commands.rs:510`), by design.
6. **Capabilities.** If the console should offer it only to agents that support it, add it to
   `AGENT_CAPABILITIES` (`crates/xorgate-core/src/config_report.rs:879`).
7. **Fixtures.** Each file has its own driver:
   - `fixtures/command_flow/handle-command.json`: `tools/replay/js/dump-command-flow.mts`;
   - `fixtures/commands/p5b-commands.json`: `tools/replay/js/dump-p5b-spine.mts`;
   - `fixtures/commands/commands.json`: dumped by a script that was never committed, so extending it
     needs a new driver in `tools/replay/js/` first.

**Checks:** `scripts/docker-build.sh test -p xorgate-core --test commands_fixtures --test
command_flow_fixtures`, `scripts/docker-build.sh test -p xorgate-agent --test commands`, and a replay
run with a `commands` track (acks are compared in order, `sudo` calls as exact argv).

## Add a collector

1. **Core:** the pure part, in a new `crates/xorgate-core/src/<name>.rs` (add `pub mod <name>;` to
   `lib.rs`, alphabetically): parse raw input, assemble the `MetricGroup` with the reference's keys,
   order, units and rounding. If the group should be typed, add a struct to `telemetry.rs` and a field
   to `TelemetryMetrics` in the registry order.
2. **HAL:** the reader, in `crates/xorgate-hal/src/`. It must **fail open**: absent file, device or
   tool → `None`, never an error. If it is a new kind of input, report it to the tape through a new
   `TapeSink` method (see [Add a replay check](#add-a-replay-check)).
3. **Agent:** `crates/xorgate-agent/src/collectors/<name>.rs` implementing `Collector`
   (`collectors/mod.rs:47-55`): `group()`, `read()` for the live tick, `sample()` gated to once a
   second with `xorgate_core::collector_cache::poll_due`, stamping `updated_mono` only when the data
   changed. `collectors/lte.rs` is the smallest complete example.
4. **Register it** in `Collectors::new` (`collectors/mod.rs:142-180`) at the reference's position in
   the read order (imu → gps → lte → system → obd): that order is the key order of `metrics`. If it
   depends on hardware, gate it in `IoCapabilities::from_hardware` from the hardware block.
5. **Children:** a helper process goes through `xorgate_hal::child::spawn` behind a small trait (like
   `ImuIo`, `collectors/imu.rs:54`) so tests can substitute it.

**Checks:** fixture tests for the core part, a seam test in `crates/xorgate-agent/tests/collectors.rs`
style, the three gates, and a replay run with a synthetic track for the new input.

## Write a unit test and a fixture test

**Unit test**, in the file under test:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_an_empty_request_id() {
        let cmd = parse_command(br#"{"v":1,"requestId":"","command":"reboot","ts":1,"expiresAt":2}"#);
        assert_eq!(cmd, None);
    }
}
```

Async code: `#[tokio::test(start_paused = true)]` and `tokio::time::advance` or `sleep` to move the
virtual clock. Agent seam tests build a context with `tests/common/mod.rs::test_ctx` and fakes such
as `FakeLink` and `FakeHooks`, then assert the timeline (`crates/xorgate-agent/tests/plane.rs:136`).

**Fixture test:**

1. Write or extend a dump driver in `tools/replay/js/` that runs the reference's code on your inputs
   and prints the `{jsSourceFile, jsCommit, generatedBy, cases}` envelope. Run it with
   `tools/replay/js/run-tsx.sh <driver>` and save the output under `fixtures/<area>/<suite>.json`.
2. Add `crates/xorgate-core/tests/<area>_fixtures.rs`:

   ```rust
   mod common;

   #[derive(serde::Deserialize)]
   struct Case { name: String, input: serde_json::Value, expected: serde_json::Value }

   #[test]
   fn my_area_matches_the_reference() {
       let f = common::fixture::<Case>("<area>/<suite>.json");
       for c in &f.cases {
           let got = serde_json::to_value(my_function(&c.input)).unwrap();
           assert_eq!(got, c.expected, "{}", c.name);
       }
       assert_eq!(f.cases.len(), 12); // catches a truncated or regenerated-empty fixture
   }
   ```

   `common::fixture` checks the envelope and the reference commit for you
   (`crates/xorgate-core/tests/common/mod.rs:39-52`).

**Checks:** `scripts/docker-build.sh test -p xorgate-core --test <area>_fixtures`, and
`tools/replay/js/redump-all.sh <outdir>` with `redump-compare.py` if you changed a driver others
depend on.

## Add a replay check

Decide what is new:

- **An output field in telemetry, reports, acks or `state.json`.** The diff already compares every
  leaf and the key order. Nothing to add: run the rig and read the diff.
- **A new file that should be compared at the end of the run.** Add it to `SNAPSHOT_FILES`
  (`crates/xorgate-replay/src/play.rs:780`).
- **A new comparison or stream.** Add it to the diff engine (`diff_with`,
  `crates/xorgate-replay/src/diff.rs:2591`; `diff_p4.rs` and `media_diff.rs` hold the LED/SMS/clock
  and media streams), with unit tests next to it.
- **A new input kind.** Add a method with an empty default body to `TapeSink`
  (`crates/xorgate-hal/src/io.rs:55-130`), write it in `FileTape`
  (`crates/xorgate-hal/src/tape.rs:1227`), call it from the HAL reader at the point of reading, feed
  it to the agents in the player (`play.rs`), and give `synth` a track for it (`SCENARIOS`,
  `crates/xorgate-replay/src/synth.rs:71`).

Then:

1. `scripts/docker-build.sh test -p xorgate-replay`.
2. A **calibration** run (`run-rig.sh <tape> --calibrate`): reference vs reference must diff clean.
   If it does not, the rig or the new check is wrong.
3. A reference vs Rust run.
4. If a difference is genuinely expected, it needs a tolerance row with evidence from the reference
   side and a reviewer's agreement, cited in `basis`. Never widen a row to make a run pass.

**Checks:** all of the above, on a quiet Docker host, one rig run at a time.
