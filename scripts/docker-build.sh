#!/usr/bin/env bash
# Build the Rust agent for the CM5 from a Mac (or any Docker host) using the
# CI recipe: linux/arm64 debian:bookworm + apt GStreamer 1.22 + rustup pinned
# by rust-toolchain.toml (Dockerfile.build). On Apple Silicon the container
# runs natively (no QEMU); on x86 Docker it runs under emulation (slow).
#
#   scripts/docker-build.sh                 # cargo build --release
#   scripts/docker-build.sh test            # cargo test --workspace
#   scripts/docker-build.sh clippy --workspace --all-targets -- -D warnings
#   scripts/docker-build.sh shell           # interactive bash in the build container
#   scripts/docker-build.sh pack            # build --release, then scripts/pack.sh --channel bench
#
# Output: target/docker/release/xorgate-rs (a Linux aarch64 binary). The build
# itself runs in a Docker volume (CARGO_TARGET_DIR=/target) — not in the
# bind-mounted repo — because a host `cargo build` on macOS would otherwise
# clobber/fight the Linux artifacts in target/release, and because named
# volumes are much faster than virtiofs for cargo's thousands of small files.
# Only the final binary is copied back into the repo tree.
#
# Caches (named Docker volumes, survive between runs; `--clean` removes them):
#   xorgate-rs-cargo-registry  /usr/local/cargo/registry
#   xorgate-rs-cargo-git       /usr/local/cargo/git
#   xorgate-rs-target          /target
# The image is tagged by the toolchain channel + a hash of Dockerfile.build, so
# bumping rust-toolchain.toml or the apt list rebuilds it automatically.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PLATFORM=linux/arm64
IMAGE_NAME=xorgate-rs-build

if [ "${1:-}" = "--clean" ]; then
  docker volume rm -f xorgate-rs-cargo-registry xorgate-rs-cargo-git xorgate-rs-target >/dev/null
  echo "removed cargo/target volumes"
  exit 0
fi

# Toolchain channel: rust-toolchain.toml wins; fall back to stable so the image
# can be validated before the skeleton lands (the orchestrator's Phase 0 note).
TOOLCHAIN=stable
if [ -f "$REPO/rust-toolchain.toml" ]; then
  TOOLCHAIN=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO/rust-toolchain.toml" | head -n1)
  [ -n "$TOOLCHAIN" ] || TOOLCHAIN=stable
fi
if command -v shasum >/dev/null 2>&1; then
  DF_HASH=$(shasum -a 256 "$REPO/Dockerfile.build" | cut -c1-12)
else
  DF_HASH=$(sha256sum "$REPO/Dockerfile.build" | cut -c1-12)
fi
IMAGE="$IMAGE_NAME:${TOOLCHAIN}-${DF_HASH}"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo ">> building image $IMAGE (toolchain $TOOLCHAIN, $PLATFORM)" >&2
  t0=$(date +%s)
  docker build --platform "$PLATFORM" -f "$REPO/Dockerfile.build" \
    --build-arg "RUST_TOOLCHAIN=$TOOLCHAIN" -t "$IMAGE" "$REPO"
  echo ">> image built in $(( $(date +%s) - t0 )) s" >&2
fi

run_in() {
  docker run --rm --platform "$PLATFORM" \
    ${DOCKER_TTY:+-it} \
    -v "$REPO:/src" \
    -v xorgate-rs-cargo-registry:/usr/local/cargo/registry \
    -v xorgate-rs-cargo-git:/usr/local/cargo/git \
    -v xorgate-rs-target:/target \
    -e CARGO_TARGET_DIR=/target \
    -e CARGO_TERM_COLOR=always \
    -w /src \
    "$IMAGE" "$@"
}

copy_out() {
  # The volume is only visible inside a container: copy the binary into the repo.
  mkdir -p "$REPO/target/docker/release"
  run_in sh -c 'cp /target/release/xorgate-rs /src/target/docker/release/xorgate-rs && ls -l /target/release/xorgate-rs'
  echo ">> binary: $REPO/target/docker/release/xorgate-rs ($(wc -c < "$REPO/target/docker/release/xorgate-rs" | tr -d ' ') bytes)" >&2
}

MODE="${1:-build}"
[ $# -gt 0 ] && shift
t0=$(date +%s)
case "$MODE" in
  build)
    run_in cargo build --release "$@"
    copy_out
    ;;
  pack)
    run_in cargo build --release "$@"
    copy_out
    "$REPO/scripts/pack.sh" --channel bench --bin "$REPO/target/docker/release/xorgate-rs"
    ;;
  shell)
    DOCKER_TTY=1 run_in bash
    ;;
  *)
    # Any other cargo subcommand (test, clippy, fmt, check, …) verbatim.
    run_in cargo "$MODE" "$@"
    ;;
esac
echo ">> $MODE finished in $(( $(date +%s) - t0 )) s" >&2
