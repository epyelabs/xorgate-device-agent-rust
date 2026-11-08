# 1. Setup

This chapter gets you from a fresh clone to a green test run. Everything builds inside Docker. You
do not need Rust installed on your machine unless you want editor support (see the end of this
chapter).

## What you need

- **Docker.** On an Apple Silicon Mac, Docker Desktop runs the `linux/arm64` build container
  natively. On an x86 machine it runs under emulation and is much slower, but it works.
- **git**, and about 30 GB of free disk for the build caches (see [Disk hygiene](#disk-hygiene)).
- For the record/replay rig only: a checkout of the reference agent repository next to this one
  (`../xorgate-device-agent`). Chapter 5 explains why.

## The toolchain is pinned

`rust-toolchain.toml` pins the exact compiler:

```toml
# rust-toolchain.toml:4-7
[toolchain]
channel = "1.99.0"
components = ["rustfmt", "clippy"]
profile = "minimal"
```

`rustup` (the Rust toolchain installer) reads this file and uses exactly that version for every
`cargo` command in the repo. The workspace manifest also declares it as the minimum supported Rust
version (`rust-version = "1.99"` in `Cargo.toml`).

Why pin it, and why so new:

- **The distribution compilers are too old.** Debian Bookworm ships rustc 1.63 and Trixie 1.85.
  Several dependencies need newer: gstreamer-rs 0.25 needs at least 1.92 and ratatui 0.30 needs
  1.88. So the build always uses rustup, never `apt install rustc`.
- **Reproducibility.** CI, your machine and any reviewer compile with the same compiler, so a lint
  or a codegen change never appears "by itself".
- **Bump it deliberately.** Changing the channel is a reviewed change: the Docker image is tagged
  with the channel, so a bump rebuilds the image, and CI picks the new version from the same file.

## Why the build only happens in Docker

The agent links **GStreamer 1.22** and **glibc 2.36** dynamically, the versions on the devices
(Raspberry Pi OS Bookworm, aarch64). A binary built against newer libraries will not load on a
device. So the build must happen on Bookworm, on aarch64, and the simplest way to guarantee that is
a `debian:bookworm` container for `linux/arm64`. That is `Dockerfile.build`, and it is the same
recipe CI uses (an arm64 runner with `container: debian:bookworm`).

Two consequences:

- **Build on Bookworm, run on Bookworm or Trixie, never the reverse.** glibc is backward compatible,
  not forward compatible.
- **musl (fully static linking) is not an option**, because GStreamer loads its plugins with
  `dlopen` at run time.

**Never build on a device.** It is tempting, because the device is aarch64 Bookworm. Do not. The Pi
and its LTE modem share one power supply, and a full `cargo build` on the CPU once browned out the
bench device. A device also has only 4 GB of RAM and is running real work. The top-level `README.md`
and `scripts/bench/install.sh --from-bench-build` still describe an older on-device build path.
Treat that path as retired. Build in Docker and copy the result.

## Build and test commands

All of these go through `scripts/docker-build.sh`, which builds the image the first time (a few
minutes), then runs `cargo` in the container with your checkout mounted at `/src`:

```sh
scripts/docker-build.sh                      # cargo build --release → target/docker/release/xorgate-rs
scripts/docker-build.sh test                 # cargo test (the whole workspace)
scripts/docker-build.sh test -p xorgate-core # one crate
scripts/docker-build.sh test -p xorgate-core --test telemetry_fixtures   # one integration-test file
scripts/docker-build.sh test -p xorgate-agent state::tests              # tests whose path matches
scripts/docker-build.sh clippy --workspace --all-targets -- -D warnings  # the lint gate
scripts/docker-build.sh fmt --all -- --check # formatting gate
scripts/docker-build.sh fmt --all            # reformat in place
scripts/docker-build.sh shell                # an interactive shell in the build container
scripts/docker-build.sh pack                 # release build, then scripts/pack.sh --channel bench
scripts/docker-build.sh --clean              # delete the cargo and target cache volumes
```

Any first argument that is not `build`, `pack`, `shell` or `--clean` is passed to `cargo` as is, so
`scripts/docker-build.sh check` or `scripts/docker-build.sh doc` work too.

The three gates CI runs on every build of `main` are the same three you should run before you hand
in a change:

```sh
scripts/docker-build.sh fmt --all -- --check
scripts/docker-build.sh clippy --workspace --all-targets -- -D warnings
scripts/docker-build.sh test
```

`-D warnings` turns every clippy warning into an error. clippy is Rust's linter. Besides its normal
lints, this repo configures one project rule in `clippy.toml`: raw process spawning is banned
(chapter 5 explains why).

### cargo deny

`cargo deny check` checks every dependency against `deny.toml`: licenses must be permissive, no
known security advisories, no yanked crates, no unknown registries or git sources. `cargo-deny` is
not in the build image. Run it in the container shell (the install is lost when the shell exits):

```sh
scripts/docker-build.sh shell
cargo install --locked cargo-deny && cargo deny check
```

or install it on your host if you have rustup there. `deny.toml` ignores six advisories by ID, all
from an old `rustls-webpki` that only the MQTT library pulls in through a code path the agent never
uses. Each ignore has a written reason. Do not add an ignore without one.

### Script test suites

Some checks are shell scripts under `scripts/tests/`. Each one says in its header how to run it,
always in a throwaway container because several of them write to system paths. For example:

```sh
docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/tests/unit-sections.sh
docker run --rm -v "$PWD:/src:ro" -w /src debian:bookworm bash scripts/tests/install-rs-unit.sh
```

`unit-ordering.sh` and `install-rs-systemd.sh` need a real systemd as PID 1 in a privileged
container. The recipe is in each file's header. Run these when you touch `systemd/` or
`scripts/install-rs.sh`.

## Running the agent on your machine

The agent has an offline mode that needs no AWS credentials and no hardware. It prints each
telemetry payload it would publish, one JSON line per second:

```sh
scripts/docker-build.sh shell
XORGATE_HOME=/tmp/xg HARDWARE_MANIFEST=/nonexistent \
  cargo run -p xorgate-agent --bin xorgate-rs -- run --offline
```

Stop it with Ctrl-C. Look at `/tmp/xg/state.json` while it runs. `XORGATE_LOG=debug` raises the log
level (the default filter is `info` with the MQTT library at `warn`). `xorgate-rs --help` lists the
other commands.

## Where things land

- `target/docker/release/xorgate-rs` is the only build output copied back into your checkout.
- The real build directory lives in a Docker volume, `xorgate-rs-target`, one sub-directory per
  checkout (`/target/<checkout dir name>`). Volumes are much faster than a bind mount for cargo's
  many small files, and keeping Linux artifacts out of your checkout stops them from clashing with
  anything your host tools build.
- `xorgate-rs-cargo-registry` and `xorgate-rs-cargo-git` cache downloaded crates between runs.

### Worktrees

If you use `git worktree`, the worktree's `.git` is a file pointing at the main checkout, which does
not exist inside the container. The build script then cannot read the commit and bakes `unknown`
into the binary, so `xorgate-rs -v` prints `(unknown, dev-local)`. That is harmless. `pack.sh`
stamps the real commit into `build-info.json` from the host, and that is what an installed agent
reports.

### Disk hygiene

Each checkout's target directory grows to 10–20 GB. With several worktrees this adds up fast; one
incident filled a laptop disk. After you delete a worktree, delete its target directory too:

```sh
docker run --rm -v xorgate-rs-target:/target alpine rm -rf /target/<checkout dir name>
docker system df          # check the volume sizes
```

## Editor setup (rust-analyzer)

rust-analyzer is the Rust language server: completion, go-to-definition, inline types and errors.
The repo ships no editor configuration. Two workable setups:

1. **Dev container (closest to CI).** Open the repo in a container built from `Dockerfile.build`
   (for example with your editor's "dev containers" feature) and add the component with
   `rustup component add rust-analyzer`. Everything resolves, including the GStreamer bindings.
2. **Host toolchain.** Install rustup on your machine. Thanks to `rust-toolchain.toml` it fetches
   1.99.0 by itself. Then install your editor's rust-analyzer extension. On macOS, `xorgate-core`
   (pure Rust) works fully; the crates that use Linux-only APIs and GStreamer show errors that have
   nothing to do with your change. Use the Docker commands above as the source of truth.

Either way, consider setting rust-analyzer's check command to `clippy`, so you see the project lints
as you type.
