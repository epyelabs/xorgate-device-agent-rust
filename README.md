# xorgate-device-agent-rust

The Rust rewrite of the xorgate device agent (`device/xorgate-device-agent`, TypeScript/Node).
Plan, research and phase gates: `plans/device-agent-js-to-rust/` in the `xorgate` monorepo
(`README.md` = the plan, `PROGRESS.md` = where we are, `phase-0-toolchain-spike.md` = this phase).

Ships one aarch64 binary, `xorgate-rs`, that speaks the **identical** cloud contract (MQTT topics,
payload shapes, settings namespaces, reports, S3 layouts) and reuses the **same on-device files**
(`/etc/xorgate`, `/var/lib/xorgate`, pipelines, settings.json, config-sync.json), so a device can be
flipped between the JS and the Rust agent with no cloud change. The JS agent stays the production
default until the Rust agent passes the parity gate.

**Phase 0 (this tree):** toolchain + skeleton. The binary boots, links GStreamer 1.22 dynamically,
connects to IoT Core over mTLS (MQTT 5), publishes status/LWT and a byte-shape-identical telemetry
payload carrying only the `system` group at 1 Hz, subscribes to the retained config + command topics
(logged, not acted on), writes `agent.pid` + `state.json`, and stops gracefully on SIGTERM.
Nothing else of the agent is ported yet.

## Coexistence (plan README §6)

| | JS agent (today) | Rust agent |
|---|---|---|
| binary | `node /opt/xorgate/dist/worker.js` | `/opt/xorgate-rs/bin/xorgate-rs run` |
| CLI wrapper | `/usr/local/bin/xorgate` | `/usr/local/bin/xorgate-rs` |
| unit | `xorgate-agent.service` | `xorgate-agent-rs.service` (`Conflicts=xorgate-agent.service`, `Type=notify` + watchdog) |
| artifact | `xorgate-agent-<sha>.tar.gz` | `xorgate-agent-rs-<sha>.tar.gz` + `latest-rs.json` in the SAME dist buckets |
| identity / files | `/etc/xorgate/agent.env`, `/var/lib/xorgate` | the same (shipped unit); the **bench** runs the Rust agent on the parked DEV identity with its own `StateDirectory=xorgate-rs` via a bench-only drop-in while the JS agent keeps running on prod |

The two agents never share an IoT identity (a duplicate clientId kicks the other and fires its LWT).

## Layout

```
Cargo.toml                 workspace; [profile.release] lto="fat", codegen-units=1, strip, line tables
rust-toolchain.toml        pinned stable (1.99.0); rustfmt + clippy
deny.toml                  cargo-deny: permissive licenses only, RustSec advisories, no yanked crates
crates/xorgate-core/       PURE (no I/O, no tokio, no clocks): payload types + serde, system parsers,
                           topics, JS-exact rounding. Tested against fixtures/.
crates/xorgate-hal/        Linux edges: procfs/sysfs readers, statvfs, bounded `vcgencmd` runs,
                           atomic JSON writer (port of atomicWriteJson).
crates/xorgate-cloud/      rumqttc 0.25 (v5) + rustls mTLS wrapper: LWT re-stamped per connect,
                           resubscribe on CONNACK, exponential backoff with jitter, state watch,
                           bounded graceful shutdown, mqtt.js offline queue + resend; Amazon Root
                           CA 1 bundled for no-CA callers (the agent requires CA_PATH, like the TS).
crates/xorgate-agent/      the `xorgate-rs` binary: clap CLI, env (agent.env keys), tracing
                           (journald under systemd, fmt otherwise), pidfile + state.json, worker.
fixtures/                  JSON test vectors extracted from the TS vitest suites, each tagged with
                           {jsSourceFile, jsCommit}. The JS repo is never modified.
systemd/ scripts/ .github/ unit, build-info/pack/docker-build/bench helpers, CI (phase-0/ci branch)
```

## Build

- **CI** (`.github/workflows/`): `ubuntu-24.04-arm` runner inside a `debian:bookworm` container
  (glibc 2.36 + GStreamer 1.22 = the fleet's), rustup pinned by `rust-toolchain.toml`,
  `cargo test` + `clippy -D warnings` + `cargo build --release`, then `scripts/pack.sh` →
  `xorgate-agent-rs-<sha>.tar.gz` uploaded to the dev dist bucket on push to `main`
  (prod on a GitHub Release).
- **Mac**: no Rust toolchain needed — `scripts/docker-build.sh` runs the same Bookworm container
  under `--platform linux/arm64` (slow but exact).
- **Bench (CM5, native)**: `rsync -a --exclude target --exclude .git ./ pi@<bench>:~/src/xorgate-device-agent-rust/`
  then on the Pi `cargo build -j3 --release` (rustup under `~/.cargo`; apt `build-essential
  pkg-config libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libssl-dev`). First build
  ≈ 10 min; `-j3` keeps the 4 GB CM5 out of swap. Then `scripts/bench/install.sh --from-bench-build`.

GStreamer is linked with gstreamer-rs feature `v1_22` **only** — a binary built with a newer
feature level fails to load on Bookworm (full RELRO). glibc: build on Bookworm, runs on Trixie,
never the reverse. musl is not an option (GStreamer dlopens plugins).

## Run

```
xorgate-rs -v                 # 0.0.1 (<sha>, <channel>) from <install root>/build-info.json
xorgate-rs run                # the worker (reads the agent.env keys from the environment)
xorgate-rs run --offline      # no AWS: prints each would-be payload to stdout once per second
XORGATE_LOG=debug xorgate-rs run   # tracing filter (default: info, rumqttc=warn)
```

Environment (same keys as `/etc/xorgate/agent.env`): `DEVICE_ID`, `IOT_DATA_ENDPOINT`, `CERT_PATH`,
`KEY_PATH`, `CA_PATH`, `MODE`, `RATE_HZ` (1), `MQTT_KEEPALIVE_SEC` (60), `XORGATE_HOME`
(`~/.xorgate`; the unit sets `/var/lib/xorgate`), `HARDWARE_MANIFEST` (`/etc/xorgate/hardware.json`).

## Checks

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo deny check            # needs `cargo install cargo-deny`
```

`cargo deny` (blocking in CI) ignores five RustSec advisories by id in
`deny.toml`: RUSTSEC-2025-0134 (rustls-pemfile unmaintained) and
RUSTSEC-2026-0049/0098/0099/0104 (rustls-webpki 0.102.8). All five come only
through rumqttc 0.25.1, whose `use-rustls` feature hard-depends on
`rustls-webpki ^0.102` + `rustls-pemfile ^2` (no feature or `[patch]` avoids
them). The agent never runs that code path: xorgate-cloud builds its own
rustls 0.23 `ClientConfig` (webpki 0.103.x, aws-lc-rs) and passes it as
`TlsConfiguration::Rustls`. Revisit (drop the ignores, expect one
`rustls-webpki` in `Cargo.lock`) when rumqttc releases on webpki 0.103.
