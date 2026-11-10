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
#   scripts/docker-build.sh image-tag       # print the build image tag and exit
#   scripts/docker-build.sh dockerfile      # print the Dockerfile text it builds from
#
# x86_64 (Phase 7e, the simulator image's agent): put `--platform linux/amd64`
# first, e.g. `scripts/docker-build.sh --platform linux/amd64 test`. Build amd64
# on an x86_64 host (the homelab): on a Mac it runs under emulation (slow). The
# amd64 build image is Dockerfile.build with its ONE `FROM --platform=linux/arm64`
# line rewritten (Dockerfile.build itself is never edited for it, so the default
# arm64 image tag — a hash of that file — stays what every checkout already has).
# It gets its own image name (xorgate-rs-build-amd64:<toolchain>-<hash of the
# rewritten file>), target sub-directory (/target/<checkout>-amd64) and output
# dir (target/docker-amd64/release/), so the two never collide.
# (XORGATE_DOCKER_TARGET_DIR overrides the target sub-directory for BOTH: set
# it per platform, or the two builds share and thrash one target dir.)
#
# Output: target/docker/release/xorgate-rs (a Linux aarch64 binary). The build
# itself runs in a Docker volume (CARGO_TARGET_DIR=/target/<checkout>) — not in the
# bind-mounted repo — because a host `cargo build` on macOS would otherwise
# clobber/fight the Linux artifacts in target/release, and because named
# volumes are much faster than virtiofs for cargo's thousands of small files.
# Only the final binary is copied back into the repo tree.
#
# Each checkout builds into its OWN sub-directory of the shared volume,
# /target/$(basename <repo dir>) — e.g. /target/xorgate-device-agent-rust for
# the main checkout, /target/xorgate-device-agent-rust-wp3 for a worktree —
# because parallel worktrees sharing one target dir leaked each other's stale
# artifacts into test builds (PROGRESS decision D1.2). Override with
# XORGATE_DOCKER_TARGET_DIR=/target/<name> (must live under /target).
#
# Caveat: in a `git worktree` checkout the repo's `.git` is a FILE pointing at the
# main checkout's path, which does not exist inside the container, so
# crates/xorgate-agent/build.rs cannot read the sha and bakes `unknown` — the
# `--version` fallback then prints `(unknown, dev-local)`. Harmless: the
# build-info.json pack.sh stamps (from the host's git) is what the installed
# binary reports. A plain clone and CI (a normal checkout) bake the real sha.
#
# Caches (named Docker volumes, survive between runs; `--clean` removes them):
#   xorgate-rs-cargo-registry  /usr/local/cargo/registry
#   xorgate-rs-cargo-git       /usr/local/cargo/git
#   xorgate-rs-target          /target  (one sub-directory per checkout, see above)
# The image is tagged by the toolchain channel + a hash of Dockerfile.build, so
# bumping rust-toolchain.toml or the apt list rebuilds it automatically.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PLATFORM=linux/arm64
if [ "${1:-}" = "--platform" ]; then
  PLATFORM="${2:-}"
  shift 2 || { echo "--platform needs a value" >&2; exit 2; }
fi
case "$PLATFORM" in
  linux/arm64) IMAGE_NAME=xorgate-rs-build; SUFFIX=""; OUT_DIR=target/docker/release ;;
  linux/amd64) IMAGE_NAME=xorgate-rs-build-amd64; SUFFIX="-amd64"; OUT_DIR=target/docker-amd64/release ;;
  *) echo "--platform must be linux/arm64 (default) or linux/amd64 (got '$PLATFORM')" >&2; exit 2 ;;
esac
TARGET_DIR="${XORGATE_DOCKER_TARGET_DIR:-/target/$(basename "$REPO")$SUFFIX}"
# Refused before anything is built (review-wp2 S7).
if [ "${1:-}" = pack ] && [ -n "$SUFFIX" ]; then
  echo "pack is aarch64-only (the device tarball)" >&2
  exit 2
fi
case "$TARGET_DIR" in
  /target/?*) ;;
  *) echo "XORGATE_DOCKER_TARGET_DIR must be /target/<name> (got $TARGET_DIR)" >&2; exit 2 ;;
esac

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
# The Dockerfile text the image is built from: Dockerfile.build verbatim for
# arm64; for amd64 the same text with its one arm64 FROM line rewritten.
dockerfile_text() {
  if [ -z "$SUFFIX" ]; then
    cat "$REPO/Dockerfile.build"
  else
    local n
    n=$(grep -c '^FROM --platform=linux/arm64 ' "$REPO/Dockerfile.build" || true)
    [ "$n" = 1 ] || { echo "Dockerfile.build: expected exactly one 'FROM --platform=linux/arm64' line (found $n)" >&2; return 1; }
    sed 's#^FROM --platform=linux/arm64 #FROM --platform=linux/amd64 #' "$REPO/Dockerfile.build"
  fi
}
DF_TEXT=$(dockerfile_text)
sha12() { if command -v shasum >/dev/null 2>&1; then shasum -a 256 | cut -c1-12; else sha256sum | cut -c1-12; fi; }
# arm64 hashes the FILE itself (the tag every checkout has, by construction);
# amd64 hashes the rewritten text.
if [ -z "$SUFFIX" ]; then
  DF_HASH=$(sha12 < "$REPO/Dockerfile.build")
else
  DF_HASH=$(printf '%s\n' "$DF_TEXT" | sha12)
fi
IMAGE="$IMAGE_NAME:${TOOLCHAIN}-${DF_HASH}"
if [ "${1:-}" = "image-tag" ]; then
  echo "$IMAGE"
  exit 0
fi
if [ "${1:-}" = "dockerfile" ]; then
  printf '%s\n' "$DF_TEXT"
  exit 0
fi

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo ">> building image $IMAGE (toolchain $TOOLCHAIN, $PLATFORM)" >&2
  t0=$(date +%s)
  if [ -z "$SUFFIX" ]; then
    docker build --platform "$PLATFORM" -f "$REPO/Dockerfile.build" \
      --build-arg "RUST_TOOLCHAIN=$TOOLCHAIN" -t "$IMAGE" "$REPO"
  else
    printf '%s\n' "$DF_TEXT" | docker build --platform "$PLATFORM" -f - \
      --build-arg "RUST_TOOLCHAIN=$TOOLCHAIN" -t "$IMAGE" "$REPO"
  fi
  echo ">> image built in $(( $(date +%s) - t0 )) s" >&2
fi

run_in() {
  docker run --rm --platform "$PLATFORM" \
    ${DOCKER_TTY:+-it} \
    -v "$REPO:/src" \
    -v xorgate-rs-cargo-registry:/usr/local/cargo/registry \
    -v xorgate-rs-cargo-git:/usr/local/cargo/git \
    -v xorgate-rs-target:/target \
    -e CARGO_TARGET_DIR="$TARGET_DIR" \
    -e CARGO_TERM_COLOR=always \
    -w /src \
    "$IMAGE" "$@"
}

copy_out() {
  # The volume is only visible inside a container: copy the binary into the repo.
  mkdir -p "$REPO/$OUT_DIR"
  run_in sh -c "cp '$TARGET_DIR/release/xorgate-rs' /src/$OUT_DIR/xorgate-rs && ls -l '$TARGET_DIR/release/xorgate-rs'"
  echo ">> binary: $REPO/$OUT_DIR/xorgate-rs ($(wc -c < "$REPO/$OUT_DIR/xorgate-rs" | tr -d ' ') bytes)" >&2
}

MODE="${1:-build}"
[ $# -gt 0 ] && shift
t0=$(date +%s)
echo ">> CARGO_TARGET_DIR=$TARGET_DIR (volume xorgate-rs-target)" >&2
case "$MODE" in
  build)
    run_in cargo build --release "$@"
    copy_out
    ;;
  pack)
    run_in cargo build --release "$@"
    copy_out
    "$REPO/scripts/pack.sh" --channel bench --bin "$REPO/$OUT_DIR/xorgate-rs"
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
