# 6. The cloud contract

The agent is one end of a conversation. The other end is the xorgate backend, the web console, the
SDKs, the device simulator, and on the device itself the modem daemon and the CLI. All of them were
built against what the reference agent emits. So the shapes below are **frozen**: the Rust agent
emits exactly what the reference emits, and a device can be switched between the two agents with no
cloud change. The cloud cannot tell them apart, apart from the build identity strings.

A change to the contract therefore does not start in this repo. It starts in the backend (and the
reference agent, while it is still the reference), and is then ported here, with fixtures dumped
from the new reference.

## What is frozen

### MQTT topics

All topic strings come from one place:

```rust
// crates/xorgate-core/src/topics.rs:27-40
impl Topics {
    pub fn for_device(device_id: &str) -> Self {
        let base = format!("xorgate/devices/{device_id}");
        Self {
            telemetry: format!("{base}/telemetry"),
            status: format!("{base}/status"),
            config: format!("{base}/config"),
            config_reported: format!("{base}/config/reported"),
            command: format!("{base}/command"),
            command_response: format!("{base}/command/response"),
            sms: format!("{base}/sms"),
        }
    }
}
```

| Topic (under `xorgate/devices/<device-id>/`) | Direction | QoS, retain | Payload |
|---|---|---|---|
| `telemetry` | device → cloud | 0 | the telemetry payload, once per second |
| `status` | device → cloud | 0, not retained | `{v, deviceId, ts, online}`; also the Last-Will |
| `config` | cloud → device | 1, **retained** | the desired configuration, with a `rev` |
| `config/reported` | device → cloud | 1 | `{v, deviceId, ts, rev, origin, effective, status, changed?}`; the backend caps it at 32 KB, and the agent never drops an oversize report |
| `command` | cloud → device | 1, not retained | `{v, requestId, command, ts, expiresAt, args?}` |
| `command/response` | device → cloud | 1 | the ack: the command fields plus `status` (`accepted`, `rejected`, `failed`) and `reason?` |
| `sms` | device → cloud | 1 | `read_sms` / `clear_sms` results |

Once the cloud sends a tenant scope, telemetry moves to
`xorgate/orgs/<org>/ws/<workspace>/devices/<device-id>/telemetry` (`topics.rs:45-51`), with a
fallback to the unscoped topic if the scoped one does not carry traffic.

The MQTT client ID is the device ID, which is also the AWS IoT thing name. That is why two agents
must never run with the same identity: the broker disconnects the older session whenever the newer
one connects.

### The telemetry payload

```text
{ "v": 1, "deviceId", "ts", "seq",
  "metrics": { "<group>": { "<field>": { "value": <number>, "unit"?: "<unit>" } } },
  "media": { "kvs": {"value": bool}, "recording": {"value": bool}, "streams": [ … ] },
  "recording": { "enabled", "sessionId", "backlogBytes", "backlogSegments", "droppedSegments",
                 "uploading", "lastUploadAt", "bootDefer"?, "powerShed"?, "clockHold"?, "liveBlock"? } }
```

Rules the code enforces (types in `crates/xorgate-core/src/telemetry.rs`):

- **Metric leaves are numbers only**, `{value: number, unit?: string}`. A string or boolean leaf
  makes the cloud drop the **whole** payload.
- A group with no data is left out, never sent empty. `lte` is left out when the signal is unknown
  (CSQ 99). GPS `lat`/`lon` appear only with a fix. The ten IMU fields always appear together.
- Units are fixed strings: `°`, `m/s^2`, `km/h`, `dBm`, `asu`, `°C`, `%`, `V`, `mA`, `dBHz`, `s`, `m`.
- Values are rounded exactly as the reference rounded them (`xorgate_core::round`).
- `seq` starts at 0 when the process starts and increases by one per tick, even while publishing
  is held.

### The status message and the Last-Will

`{v:1, deviceId, ts, online}` (`telemetry.rs:409-420`). It is published with `online:true` on
connect and `online:false` on a graceful stop, and registered as the Last-Will with `online:false`
and the **connect-time** `ts`. The backend uses that timestamp: an `online:false` whose `ts` is more
than 30 s old is classified as an unexpected disconnect (the Last-Will), a recent one as a graceful
stop. A detail like "which timestamp goes in the will" is invisible in a code review and decisive
for the console. That is the kind of thing parity protects.

### Configuration namespaces

The `config` document carries these namespaces, each applied in a defined way:

```rust
// crates/xorgate-core/src/config_namespaces.rs:34-40 (trimmed)
pub const CONFIG_APPLY_CLASSES: &[(&str, ApplyClass)] = &[
    ("gnssAntBias", ApplyClass::ModemPoll),
    ("imuMount", ApplyClass::AgentPoll),
    ("recording", ApplyClass::AgentPoll),
    ("lteRecovery", ApplyClass::ModemPoll),
    ("timeSync", ApplyClass::AgentPoll),
    // … cellular (modem-poll), cameraMount, simulation, obd (agent-poll)
];
```

`agent-poll` namespaces are applied by the agent itself; `modem-poll` ones by the modem daemon, which
re-reads `settings.json` about every 30 s. Next to the namespaces, three envelope fields are handled
outside the revision guard: `scope`, `uploadBucket` and `hardware`.

Value rules:

- **The cloud rejects, the device clamps.** The cloud refuses out-of-range values. The device clamps
  whatever it receives into its own range (for example `timeSync.stepThresholdSecs` to 2–600,
  `crates/xorgate-core/src/settings.rs:509-520`). This only stays coherent while the device's range
  **contains** the cloud's, and a test checks that (below).
- **`settings.json` is an allowlist.** Parsing keeps only keys the agent knows, so the next save drops
  anything else. Key names are wire names and are never renamed: the modem daemon reads the file by
  exact key path.
- Some report blocks are reported-only (the cloud never sets them): `agent` (mode, build,
  capabilities, runtime), `telemetryScope`, `sim`, `uplink`, `apn`, and the camera-mount state. Status
  `detail` and `reason` texts are part of the contract, word for word.
- **One deliberate addition (D85, Phase 8):** `effective.agent.runtime` is `"rust"`, the last key of
  the `agent` block (`config_report.rs` `AGENT_RUNTIME`). The JS agent sends no such key. The backend
  mirrors it positive-only (`"rust"` sets `devices.agent = 'rust'`; an absent key leaves the column
  alone), and it stores `effective` as an opaque record, so the extra key passes its schema.

### Commands

`reboot`, `power_off`, `refresh`, `update`, `read_sms`, `clear_sms`, `obd_scan`, `obd_forget`
(`crates/xorgate-core/src/commands.rs:43-66`). The TTL is clamped to 120 s after the issue time. The
replay ledger `commands.json` keeps executed request IDs for an hour, at most 100. Acks are
`accepted`, `rejected` with a reason (`expired`, `duplicate`, `rate-limited`, …) or `failed`. The
report advertises `agent.capabilities` (`config_report.rs:879`) so the console knows which optional
commands this agent supports.

### Files on the device

These are shared with other programs, or with the other agent when a device is switched back, so
their formats are frozen too:

| File | Read by |
|---|---|
| `settings.json` | the modem daemon, the CLI, both agents |
| `config-sync.json`, `commands.json`, `recordings/counters.json`, `gnss-sky.json` | both agents (the agent's own bookkeeping) |
| `state.json`, `agent.pid` | the CLI, the device simulator host |
| `refresh-modem.flag` | the modem daemon |
| `/etc/xorgate/hardware.json`, `kvs.json` (`uploadBucket`), `/etc/xorgate/pipelines/` | both agents, provisioning |
| `recordings/` tree, S3 keys, video sidecars | the uploader, the cloud ingest |

### Privilege and behaviour

- The agent runs only its fixed set of exact `sudo -n` commands (reboot, power off, start the
  updater unit, the clock helper and the camera-rotation helper).
- It never reboots the device on its own, never restarts the modem daemon, and never opens the
  modem daemon's AT port.
- Every interval is measured on the monotonic clock; the wall clock only names and stamps things.
- Missing inputs fail open (no LED → assume healthy, no gate input → gate disarmed), except two
  rules that fail closed: an unreadable `settings.json` is never treated as "all settings removed",
  and network-provided time is trusted only under strict conditions.

## Why byte-exact JSON matters

Most consumers parse JSON by key, so why insist on identical bytes?

- **It is the only practical proof.** Parity is checked by diffing the two agents' outputs. If
  key order or number formatting were allowed to differ, every comparison would need a semantic
  normaliser, and the normaliser would become a place for real differences to hide.
- **Some readers are not JSON parsers.** The modem daemon greps `settings.json`. Operators and
  scripts grep log lines. The config plane detects local edits by hashing `settings.json`, so a
  rewrite with a different byte layout would look like an operator change and trigger a spurious
  `local` report.
- **Devices switch agents.** A device can be converted to the Rust agent and back, with all its
  files kept. Each agent must read what the other wrote and write what the other expects.
- **The schemas are strict where it matters.** A boolean in a metric leaf drops the payload, an
  absent key and a `null` mean different things, and the console compares `effective.agent.build`
  strings directly.

Chapter 3's [serde section](03-rust-essentials-in-this-codebase.md#serde-and-exact-json) shows how
this is done: struct field order is key order, `IndexMap` for dynamic maps, `JsNumber` for numbers,
and a deliberate choice of `null` versus omitted for every optional field.

## Where the contract is pinned

| Fixture | Pins | Test |
|---|---|---|
| `fixtures/config-contract.json` | namespace defaults, the video preset table, clamp ranges vs cloud bounds, apply classes. **Generated by the backend** (`npm run contract:generate` in the backend repo) and copied verbatim. Never edit it by hand: if the test fails, fix whichever side is wrong and regenerate. | `crates/xorgate-core/tests/config_contract.rs` |
| `fixtures/telemetry/payload-shapes.json` | telemetry, status and recording shapes, byte for byte | `crates/xorgate-core/tests/telemetry_fixtures.rs` |
| `fixtures/config_report/` | the reported document: `effective`, `status`, wording | `config_report_fixtures.rs` |
| `fixtures/commands/`, `fixtures/command_flow/` | command parsing, TTL, ledger, the full decision and its effects | `commands_fixtures.rs`, `command_flow_fixtures.rs` |
| `fixtures/settings/`, `fixtures/cloud_config/`, `fixtures/cli_set/` | `settings.json` parsing, clamps, cloud merge, `xorgate-rs set` | `settings_fixtures.rs`, `cloud_config_fixtures.rs`, `crates/xorgate-agent/tests/cli_set.rs` |
| `fixtures/sidecar/`, `fixtures/segments/`, `fixtures/upload_keys/` | sidecar JSON, segment names, S3 keys | `sidecar_fixtures.rs`, `segments_fixtures.rs`, `upload_keys_fixtures.rs` |
| `fixtures/js/number-format.json` | number printing, 5,141 cases | `js_fixtures.rs` |
| `fixtures/install_rs/bodies.json` | files `install-rs.sh` and `xorgate-rs update` both write | `install_rs_bodies.rs` + `scripts/tests/install-rs-bodies.sh` |

Unless a path is given, the tests are in `crates/xorgate-core/tests/`.

## How to check you did not break it

1. **Always:** `scripts/docker-build.sh test`. The fixture tests are fast and catch most shape and
   wording changes. When you touch a shape, also run `scripts/docker-build.sh test -p xorgate-core`
   on its own and read any failing diff carefully: a key that moved is a failure, not noise.
2. **When you change what goes on the wire or into a shared file:** run the replay rig, reference vs
   Rust, on a scenario that exercises it (chapter 5). The rig compares key order and number text, not
   only values.
3. **When you change the CLI:** the CLI matrix, which diffs both agents' CLI output and the
   resulting `settings.json`. It runs inside the rig container; the invocation is in
   [chapter 5](05-hardware-abstraction-and-testing.md#the-cli-matrix).
4. **When you change packaging or install:** `scripts/tests/dist.sh` (the `latest-rs.json` bytes) and
   `scripts/tests/install-rs-bodies.sh`.

## When the Rust agent differs on purpose

A few differences are intentional: the binary names itself `xorgate-rs` and reports its own build,
`state.json` says it was written by `xorgate-device-agent-rs@<version>`, replay mode is not
supported, and in some places a crash or race of the reference is not reproduced. Each such
difference is an entry in the **accepted-divergence register**, kept with the migration plan
(entries are numbered `D1`, `D2`, …). Each entry has a test that pins the Rust behaviour, and the
replay rig's tolerance table references it. A code comment that says `D29` or `T8` points at one of
these. Adding a divergence is a decision for the maintainers, not something to do in passing.
