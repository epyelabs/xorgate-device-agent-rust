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
    // … cellular (modem-poll), cameraMount, simulation, obd (agent-poll),
    // cameraSensors (agent-poll; Rust-only, last)
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
- **A status `detail` or `reason` is at most 500 characters** (JavaScript string length). That is
  the backend's schema bound, and a longer one makes it drop the whole report. `build_status` ends
  by fitting every entry (`fit_status_text`). What gives way first is the long quoted part, not
  the sentence: a word over 40 (then 28) characters (a path, a section name, a token) is clipped
  with `…` inside. Only when that is not enough is a text cut, at a sentence end, else at a clause
  (`: `, `, `, ` — `), never inside a word, and closed with ` …`. The camera namespaces, whose
  reason joins one sentence per port, share the room fairly (`join_status_parts`): the first
  port's sentence stays whole when that still leaves the others what they need or 150 characters,
  otherwise every port gets an equal share, so the last port's advice is not what pays. With so
  many ports that a share would be under 60 characters, the rest are named (`; and cam5, cam6`).
  The report as a whole must stay
  under 32 768 bytes; `cameraSensors` holds at most 16 ports and no entry over 512 bytes.
- Some report blocks are reported-only (the cloud never sets them): `agent` (mode, build,
  capabilities, runtime), `telemetryScope`, `sim`, `uplink`, `apn`, the camera-mount state and the
  per-port camera report. Status
  `detail` and `reason` texts are part of the contract, word for word.
- **One deliberate addition (D85, Phase 8):** `effective.agent.runtime` is `"rust"`, the last key of
  the `agent` block (`config_report.rs` `AGENT_RUNTIME`). The JS agent sends no such key. The backend
  mirrors it positive-only (`"rust"` sets `devices.agent = 'rust'`; an absent key leaves the column
  alone), and it stores `effective` as an opaque record, so the extra key passes its schema.
- **A second deliberate addition (support-other-camera-models plan, Phase 0):** `effective.cameras`,
  the per-port camera report, the last key of `effective` (after `obd`). One entry per camera of the
  device model, keyed by stream key: what is declared, what `config.txt` says, what the kernel bound
  and libcamera listed, whether capture delivers, and one `verdict` with a sentence
  (`crates/xorgate-core/src/camera_truth.rs`). The JS agent sends no such key. It is reported-only
  and is **never** a namespace key: the console would compute "Device differs" for a reported block
  that sits under one. The same value is written to `state.json` as `cameras` (its last key) for
  `xorgate-rs status`. It exists in real mode, online, with a hardware block that lists cameras;
  not in sim or replay mode, not with `--offline`. It is re-reported when its summary changes, not
  on every step of the capture back-off (`crates/xorgate-agent/src/ops/camera_truth.rs`).

  The words on the wire today:

  | Field | Values |
  |---|---|
  | `verdict` | `ok`, `mismatch`, `reboot-required`, `not-detected`, `no-frames`, `empty`, `unchecked`, `requirements-missing`, `unknown-sensor` |
  | `capture.state` | `streaming`, `starting`, `failing`, `absent`, `off` |
  | `declared.source` | `config`, `boot-config`, `model` |
  | `bound.identity` | `chip-id`, `weak`, `none`, or `null` |

  **A reader must tolerate a value it does not know in any of these, and a key it does not know
  anywhere in the block.** A later phase may add verdicts and fields. `detail` is for people: show
  it, never parse it. `capture.lastError` is plain text (terminal colour codes are stripped).
  `declared`, `bootConfig` and every field of `bound` may be `null`.

  What Phase 2 of that plan added to the block (the key set and key order did not change):
  - `declared.source` `config`: the port is **managed**, its sensor comes from the `cameraSensors`
    setting (`declared.sensor` is the setting's id, `none` for a port declared empty). A port
    without a setting reports as before (`boot-config` or `model`); there is no `unmanaged` verdict.
  - `unknown-sensor`: the setting names a sensor this build's catalog lacks. Nothing is written.
  - `requirements-missing`: the declared sensor needs an OS release the device does not run, or
    packages that `dpkg-query` does not show installed in the catalog's versions, and the camera is
    not listed. The agent itself installs nothing (it is unprivileged); the sentence names the
    packages and says what does: "run `sudo xorgate-rs update` on the device (the console's Update
    button does the same), or `sudo xorgate-rs camera requirements --install`" (see "The extra
    packages of a sensor" in `guide/07`). A package on hold counts as installed (dpkg shows it
    as `hi`).
  - `reboot-required` also covers a camera line that differs from the one the device booted with
    (an option, the rotation, a line removed or added), on a managed and on an unmanaged port.
    A real fault outranks it: when the capture is failing or nothing answers and the pending
    change keeps the overlay, the verdict is the fault's (`no-frames`, `not-detected`) and the
    sentence ends "…also waits for a reboot".
  - When the running video preset is one the sensor cannot run (the catalog's
    `unsupportedPresets`), the sentence of an `ok` or `no-frames` port ends with the catalog's reason.
- **A third deliberate addition (the same plan, Phase 2): the namespace `cameraSensors`** and the
  capability `camera.sensors`. Neither exists in the JS agent, and the cloud never stores or
  publishes the namespace for a JS device.
  - **Shape:** `{ "cameras": { "<streamKey>": { "sensor": "<catalog id>" | "none", "options"?: { "<key>": <number> } } } }`.
    An ABSENT port key is unmanaged: the agent leaves that port's camera line exactly as it found it.
  - **Retained message:** key absent = keep the local value; `{}` = drop it; a value = replace whole.
  - **Held as delivered.** Unlike the other namespaces, an entry is not normalised: a sensor id or an
    option this build's catalog lacks, an unknown key and an entry of the wrong type are kept, so
    the agent can refuse that one port with a sentence and hand the value back unchanged in a local
    fold-back. Only a key that is not a stream key is dropped.
  - **What it does:** the camera reconciler makes each managed port's `dtoverlay=` line the declared
    sensor's line through `xorgate-camcfg set` (or `remove` for `none`), one write per change, none
    when the line already says it. It never reboots. See `guide/04`.
  - **An option the entry does not name is "as found".** On a line of the declared sensor the agent
    keeps whatever the line has for it, so the value in force is in the port's report
    (`effective.cameras.<key>.bootConfig.line`), not in the setting. A reader that shows options
    should take an un-named option from that line (no token = the overlay's default).
  - **Reported:** `effective.cameraSensors` is the namespace as held plus `reconciled: false` (not
    looked at yet, or the pass looked at an older value) or `rebootRequired`; `status.cameraSensors` is `applied`, `adjusted` (saved but
    not in force: "saved; the device has not reconciled its boot config yet", or "written to the boot
    config — REBOOT REQUIRED to apply: cam0 imx708") or `rejected` ("cam0: <why>; …", naming every
    port that could not be applied). A local `xorgate-rs set cameraSensors.…` folds back as
    `changed.cameraSensors`: the whole namespace, or `null` when it was removed.
  - **Each namespace says only what is its own.** One pass reports both `cameraSensors` (the
    sensor's line) and `cameraMount` (the rotation), and the wording keeps them apart:
    - "written to the boot config" is said only for a line THE AGENT wrote in this boot. A line
      that differs from the booted one and that something else changed (a hand edit, an
      installer) reads "the camera line was changed outside the agent since this boot (not
      written by this setting) — REBOOT REQUIRED to apply: cam0 imx335".
    - A change of the rotation alone is `cameraMount`'s pending reboot. `cameraSensors` stays
      `applied`: the sensor setting did not change.
    - `cameraMount` on a managed port reports the rotation of the camera that RUNS there,
      whichever sensor it is. A sensor swap that waits for its reboot does not make the mount
      "REBOOT REQUIRED" when the running camera already shows the mount asked and the new line
      gives it too. When the new line waits for a reboot and nothing runs on the port, the
      pending entry reads "cam0 0° (no camera runs on the port until then)".
    - When no camera answers on a managed port and nothing waits for a reboot (a wrong
      declaration after its reboot), `cameraMount` is `adjusted` with "no camera answers on cam0,
      so the rotation in force cannot be read — look at the camera sensor setting (cameraSensors)
      and the camera report of the port; nothing here waits for a reboot". It claims no write and
      asks for no reboot.
    - A refused sensor entry does not turn `cameraMount` `rejected` with the sensor's sentence.
      The mount is judged by what is there: `applied` with its values when the running camera
      shows the mount asked; "the camera sensor setting of this port is not applied (see
      cameraSensors), so its camera line is left as it is" only when a mount change is actually
      pending on that line.
  - **The capability** is in `effective.agent.capabilities`, last. The backend refuses the namespace
    for an agent that does not advertise it and **withdraws a stored value on the first report whose
    list lacks it**. So the list is one compile-time constant (`AGENT_CAPABILITIES`), emitted whole in
    every report that has an `agent` block. Never build it conditionally. The provisioner sends the
    same string in its claim body (`capabilities: ["camera.sensors"]`); without it the device starts
    unmanaged.

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
