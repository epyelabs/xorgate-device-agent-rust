#!/bin/sh
# Phase 7c C5 (and C10b): replay every *.imutx in DIR through BOTH lockstep
# runners and print counts-only verdicts (no bytes, no values):
#   (a) the Rust client:  xorgate-replay imu-lockstep   (crates/xorgate-core imu_lockstep)
#   (b) the Python oracle: tools/imu/oracle/oracle.py replay (the unmodified helper, real stack)
# Run on the homelab from the repo root of a PINNED tree:
#   sh tools/imu/oracle/c5.sh DIR            # DIR: the 0700 private transcript dir
# Env: AGENT (label, default p7c-c5), TARGET (cargo target dir in the
# xorgate-rs-p7-target volume, default /target/p7c-c5), CPUS (default 8).
# Exit 0 only when every transcript PASSES both directions.
set -eu
DIR=$(cd "${1:?usage: c5.sh DIR}" && pwd)
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
AGENT=${AGENT:-p7c-c5}
TARGET=${TARGET:-/target/p7c-c5}
CPUS=${CPUS:-8}
BUILD_IMAGE=xorgate-rs-build-amd64:1.99.0-6e1e227ae672
rc=0
echo "== (a) Rust client, lockstep ($DIR)"
docker run --rm --label "xorgate.agent=$AGENT" --network none --cpus "$CPUS" --memory 16g \
  -v "$REPO:/src:ro" -v "$DIR:/in:ro" \
  -v xorgate-rs-p7-cargo-registry:/usr/local/cargo/registry -v xorgate-rs-p7-cargo-git:/usr/local/cargo/git \
  -v xorgate-rs-p7-target:/target -e CARGO_TARGET_DIR="$TARGET" -e CARGO_NET_OFFLINE=true -w /src \
  "$BUILD_IMAGE" sh -c 'cargo build --locked -q -p xorgate-replay && "$CARGO_TARGET_DIR"/debug/xorgate-replay imu-lockstep /in/*.imutx' \
  || rc=1
echo "== (b) Python oracle, replay ($DIR)"
for f in "$DIR"/*.imutx; do
  OUT="$DIR" AGENT="$AGENT" sh "$REPO/tools/imu/oracle/run.sh" run /oracle/oracle.py replay "/out/$(basename "$f")" || rc=1
done
echo "== c5: $([ $rc = 0 ] && echo ALL PASS || echo FAIL)"
exit $rc
