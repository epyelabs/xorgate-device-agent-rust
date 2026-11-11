#!/usr/bin/env bash
# Build the Rust agent (`xorgate-rs`) and the harness (`xorgate-replay`) for the
# rig in the CI build image (linux/arm64 bookworm, Dockerfile.build) and copy
# them to tools/replay/.cache/bin. Never builds on the bench (orchestrator rule:
# a cargo build browns the bench out).
#
#   tools/replay/build-bins.sh            # release (what ships)
#   PROFILE=dev tools/replay/build-bins.sh
#   RIG_TARGET_DIR=/target/p2-wp7         # private cargo target dir in the volume
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
PROFILE="${PROFILE:-release}"
TARGET_DIR="${RIG_TARGET_DIR:-/target/rig-$(basename "$REPO")}"
TOOLCHAIN=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO/rust-toolchain.toml" | head -n1)
DF_HASH=$(shasum -a 256 "$REPO/Dockerfile.build" 2>/dev/null | cut -c1-12 || sha256sum "$REPO/Dockerfile.build" | cut -c1-12)
IMAGE="xorgate-rs-build:${TOOLCHAIN}-${DF_HASH}"
docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "build image $IMAGE missing: run scripts/docker-build.sh once" >&2; exit 1; }
mkdir -p "$HERE/.cache/bin"
OUTDIR=release; [ "$PROFILE" = dev ] && OUTDIR=debug
docker run --rm --label "xorgate.agent=${XORGATE_AGENT_LABEL:-rig}" --platform linux/arm64 \
  -v "$REPO:/src" \
  -v xorgate-rs-cargo-registry:/usr/local/cargo/registry \
  -v xorgate-rs-cargo-git:/usr/local/cargo/git \
  -v xorgate-rs-target:/target \
  -e CARGO_TARGET_DIR="$TARGET_DIR" -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}" -w /src "$IMAGE" \
  sh -c "cargo build --locked --profile $PROFILE -p xorgate-agent -p xorgate-replay && cp $TARGET_DIR/$OUTDIR/xorgate-rs $TARGET_DIR/$OUTDIR/xorgate-replay /src/tools/replay/.cache/bin/ \
    && cc -shared -fPIC -O2 -Wall -Wextra -o /src/tools/replay/.cache/bin/rig-statvfs.so /src/tools/replay/statvfs-shim/rig_statvfs.c -ldl"
# (Phase 5b WP5 item 4: rig-statvfs.so = the deterministic root filesystem both
# agents' disk_usage reads, preloaded by the player; statvfs-shim/test.sh.)
ls -l "$HERE/.cache/bin"
# F-P2-BUILDREV (review-P2): the binary reports `unknown` as its build, so the
# log records the tree it was built from next to the binaries' sha256.
dirty=""; git -C "$REPO" diff --quiet HEAD -- 2>/dev/null || dirty=" (+ uncommitted changes)"
echo "built from git $(git -C "$REPO" rev-parse HEAD)$dirty"
( cd "$HERE/.cache/bin" && { shasum -a 256 xorgate-rs xorgate-replay rig-statvfs.so 2>/dev/null || sha256sum xorgate-rs xorgate-replay rig-statvfs.so; } )
