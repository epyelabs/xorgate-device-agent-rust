#!/usr/bin/env bash
# Phase 7a WP0: the capture-host harnesses. Builds the agent (dev profile) in
# the CI build image (Dockerfile.build, Bookworm + GStreamer 1.22 headers) and
# runs the harnesses in the replay-rig image (tools/replay/Dockerfile.rig: the
# REAL gst-launch-1.0 1.22 + base/good/bad/ugly). Mac Docker only; never on
# the bench.
#
#   tools/capture-host/run-rig.sh build
#   tools/capture-host/run-rig.sh reference            # re-record the gst-launch fixtures
#   tools/capture-host/run-rig.sh transcripts [OUTDIR] # host transcripts + compare vs fixtures
#   tools/capture-host/run-rig.sh dot [OUTDIR] [dot_equiv.py run args...]
#
# Env: XORGATE_AGENT_LABEL (container label, default capture-host),
#      RIG_TARGET_DIR (cargo target dir in the xorgate-rs-target volume,
#      default /target/<branch-slug>), RIG_IMAGE.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
LABEL="${XORGATE_AGENT_LABEL:-capture-host}"
BRANCH=$(git -C "$REPO" rev-parse --abbrev-ref HEAD 2>/dev/null | tr '/' '-' || echo local)
TARGET_DIR="${RIG_TARGET_DIR:-/target/$BRANCH}"
RIG_IMAGE="${RIG_IMAGE:-xorgate-replay-rig:node20-bookworm-gst}"
TOOLCHAIN=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO/rust-toolchain.toml" | head -n1)
DF_HASH=$(shasum -a 256 "$REPO/Dockerfile.build" 2>/dev/null | cut -c1-12 || sha256sum "$REPO/Dockerfile.build" | cut -c1-12)
BUILD_IMAGE="xorgate-rs-build:${TOOLCHAIN}-${DF_HASH}"
CACHE="$HERE/.cache"
FIXTURES="$REPO/fixtures/capture-host/gst-launch-1.22.0-debian-12.13"

rig() {
  docker run --rm --label "xorgate.agent=$LABEL" --platform linux/arm64 \
    -v "$REPO:/repo" -w /repo "$RIG_IMAGE" "$@"
}

cmd="${1:-}"
shift || true
case "$cmd" in
  build)
    mkdir -p "$CACHE/bin"
    docker run --rm --label "xorgate.agent=$LABEL" --platform linux/arm64 \
      -v "$REPO:/src" \
      -v xorgate-rs-cargo-registry:/usr/local/cargo/registry \
      -v xorgate-rs-cargo-git:/usr/local/cargo/git \
      -v xorgate-rs-target:/target \
      -e CARGO_TARGET_DIR="$TARGET_DIR" -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}" -w /src "$BUILD_IMAGE" \
      sh -c "cargo build -p xorgate-agent && cp $TARGET_DIR/debug/xorgate-rs /src/tools/capture-host/.cache/bin/"
    ;;
  reference)
    rig python3 tools/capture-host/transcript.py record --program /usr/bin/gst-launch-1.0 \
      --outdir "/repo/${FIXTURES#"$REPO"/}"
    ;;
  transcripts)
    out="${1:-$CACHE/host}"
    rig python3 tools/capture-host/transcript.py record \
      --program /repo/tools/capture-host/.cache/bin/xorgate-rs --arg0 xorgate-capture --prefix __capture-host \
      --outdir "/repo/${out#"$REPO"/}"
    python3 "$HERE/transcript.py" compare "$FIXTURES" "$out"
    ;;
  dot)
    out="${1:-$CACHE/dot}"
    shift || true
    rig python3 tools/capture-host/dot_equiv.py run \
      --host /repo/tools/capture-host/.cache/bin/xorgate-rs --outdir "/repo/${out#"$REPO"/}" "$@"
    ;;
  *)
    sed -n '2,16p' "$0" >&2
    exit 2
    ;;
esac
