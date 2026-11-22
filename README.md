# xorgate-device-agent-rust

The Rust xorgate device agent, the successor to the TypeScript agent (`device/xorgate-device-agent`).
The migration plan and its progress live in `plans/device-agent-js-to-rust/` in the `xorgate`
monorepo (`README.md` = the plan, `PROGRESS.md` = where it stands).

New to the codebase or to Rust? Start with the [guide](guide/README.md).

Ships one aarch64 binary, `xorgate-rs`, that speaks the **identical** cloud contract (MQTT topics,
payload shapes, settings namespaces, reports, S3 layouts) and reuses the **same on-device files**
(`/etc/xorgate`, `/var/lib/xorgate`, pipelines, settings.json, config-sync.json), so a device can be
flipped between the JS and the Rust agent with no cloud change. The JS agent is still the
production default on fielded devices; the Rust agent runs alongside it until cutover.

**Status:** the whole agent is ported: telemetry from every collector, the config and command
planes, on-device recording and upload, live video supervision, power shedding, the LED, the clock
stepper, the SMS reader, the CLI and the self-updater. Replay mode for the device simulator is
not supported and stays with the JS agent. See
[What the agent is](guide/README.md#what-the-agent-is).

## Coexistence

| | JS agent | Rust agent |
|---|---|---|
| binary | `node /opt/xorgate/dist/worker.js` | `/opt/xorgate-rs/bin/xorgate-rs run` |
| CLI wrapper | `/usr/local/bin/xorgate` | `/usr/local/bin/xorgate-rs` |
| unit | `xorgate-agent.service` | `xorgate-agent-rs.service` (`Conflicts=xorgate-agent.service`, `Type=notify` + watchdog) |
| artifact | `xorgate-agent-<sha>.tar.gz` | `xorgate-agent-rs-<sha>.tar.gz` + `latest-rs.json` in the SAME dist buckets |
| identity / files | `/etc/xorgate/agent.env`, `/var/lib/xorgate` | the same (shipped unit); the bench scripts can also run the Rust agent on a separate development identity, with its own state directory, through a bench-only drop-in |

The two agents never share an IoT identity (a duplicate clientId kicks the other and fires its LWT).

## Install

A device is provisioned the standard way (JS agent) and then **converted** with
`scripts/install-rs.sh`, which ships at the root of every tarball (`/opt/xorgate-rs/install-rs.sh`)
and as the bucket key `install-rs.sh` + `install-rs.sh.sha256`. The full runbook is
[`docs/convert.md`](docs/convert.md): prerequisites, offline `--tarball`, revert, updates, exit
codes, troubleshooting.

```
# convert (on the device, as the provisioning user): fetch, verify, run
B=$(sudo sed -n 's/^DEVICE_DIST_BUCKET_URL=//p' /etc/xorgate/agent.env); B=${B%/}
curl -fsSLO "$B/install-rs.sh" -O "$B/install-rs.sh.sha256" && sha256sum -c install-rs.sh.sha256 && sudo bash install-rs.sh

sudo /opt/xorgate-rs/install-rs.sh --status            # state=converted|converting|reverted|not-converted
sudo systemctl start xorgate-updater-rs                # update to xorgate-agent-rs-latest (what the cloud `update` does)
sudo /opt/xorgate-rs/install-rs.sh --revert            # back to the JS agent; Rust files kept, inert
sudo /opt/xorgate-rs/install-rs.sh --revert --purge    # ... and delete everything the conversion installed
```

The conversion writes only Rust-owned paths (`/opt/xorgate-rs`, `/usr/local/bin/xorgate-rs`, the
`-rs` units and drop-ins, sudoers `024`, the record in `/var/lib/xorgate-rs-install`). It disables
the JS unit and starts the Rust one, whose `Conflicts=` stops the JS agent. `/etc/xorgate`,
`/var/lib/xorgate`, the JS install and the modem daemon are untouched.

## Layout

| Path | What |
|---|---|
| `crates/xorgate-core/` | pure logic: payload types and exact JSON, parsers, every policy and decision; no I/O |
| `crates/xorgate-hal/` | Linux edges: `/proc` and `/sys` readers, tool runs, serial and AT ports, GPIO, atomic writes, child processes, the replay tape |
| `crates/xorgate-cloud/` | the MQTT 5 client over mutual TLS, IoT credentials, SigV4 and the S3 PUT |
| `crates/xorgate-agent/` | the `xorgate-rs` binary: the worker, its planes and subsystems, and every CLI subcommand |
| `crates/xorgate-replay/` | the record/replay test harness (never shipped) |
| `fixtures/` | golden test vectors dumped from the JS agent, plus the backend's `config-contract.json` |
| `systemd/`, `scripts/`, `.github/` | units; build, pack, install and bench scripts; CI and deploy workflows |
| `tools/` | the replay rig and the end-to-end rigs (convert, update, OBD, SMS) |
| `media/`, `python/`, `modem/`, `system/` | vendored pieces shipped in the tarball: the KVS WebRTC runtime, the IMU/OBD helpers, the modem daemon, the sudo helpers |
| `docs/convert.md`, `guide/` | the conversion runbook; the codebase guide |

Details: [guide/02-workspace-tour.md](guide/02-workspace-tour.md).

## Build

- **Always in Docker.** `scripts/docker-build.sh` runs the CI recipe locally: `linux/arm64`
  `debian:bookworm` (glibc 2.36 + GStreamer 1.22, the fleet's) with the toolchain pinned by
  `rust-toolchain.toml`. Native on Apple Silicon, emulated (slow) on x86. No host Rust toolchain
  needed. The binary lands at `target/docker/release/xorgate-rs`; `scripts/docker-build.sh pack`
  also packs a tarball.
- **Never build on a device.** Build in Docker, then install the tarball on the bench with
  `scripts/bench/install.sh target/dist/xorgate-agent-rs-<version>.tar.gz`.
- **CI:** `ci.yml` is manual only. `device-rs-deploy-dev.yml` runs on a push to `main` that touches
  shipped paths and publishes to the dev dist bucket; `device-rs-deploy-prod.yml` runs on a published
  GitHub Release. Never dispatch the deploy workflows by hand.

GStreamer is linked with gstreamer-rs feature `v1_22` **only**: a binary built with a newer feature
level fails to load on Bookworm. Build on Bookworm, run on Bookworm or Trixie, never the reverse.
musl is not an option (GStreamer loads plugins at run time).

Details: [guide/01-setup.md](guide/01-setup.md), [guide/07-build-package-deploy.md](guide/07-build-package-deploy.md).

## Run

```
xorgate-rs -v                 # 0.2.0 (<sha>, <channel>) from <install root>/build-info.json
xorgate-rs run                # the worker (reads the agent.env keys from the environment)
xorgate-rs run --offline      # no AWS: prints each would-be payload to stdout once per second
XORGATE_LOG=debug xorgate-rs run   # tracing filter (default: info, rumqttc=warn)
```

Environment (same keys as `/etc/xorgate/agent.env`): `DEVICE_ID`, `IOT_DATA_ENDPOINT`, `CERT_PATH`,
`KEY_PATH`, `CA_PATH`, `MODE`, `RATE_HZ` (1), `MQTT_KEEPALIVE_SEC` (60), `XORGATE_HOME`
(`~/.xorgate`; the unit sets `/var/lib/xorgate`), `HARDWARE_MANIFEST` (`/etc/xorgate/hardware.json`).

## Checks

```
scripts/docker-build.sh fmt --all -- --check
scripts/docker-build.sh clippy --workspace --all-targets -- -D warnings
scripts/docker-build.sh test
```

`cargo deny check` is not in the build image: run it in `scripts/docker-build.sh shell` after
`cargo install --locked cargo-deny`. It runs only in the manual `ci.yml` and locally. `deny.toml`
ignores six RustSec advisories by ID, each with its reason: five for `rustls-webpki` 0.102 and one
for the unmaintained `rustls-pemfile`. All six come only through rumqttc 0.25.1, through a code path
the agent never runs (xorgate-cloud builds its own rustls 0.23 config). Revisit when rumqttc moves to
`rustls-webpki` 0.103.

Details: [guide/01-setup.md](guide/01-setup.md).
