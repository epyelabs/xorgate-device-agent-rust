#!/usr/bin/env bash
# H12 (phase-5b-js-catchup.md §11): OBD Docker e2e, Rust only. Builds the
# aarch64 debug binary in the build image (CARGO_TARGET_DIR=/target/p5b-obd),
# then runs h12-case.sh in xorgate-obd-py:bookworm. Log: target/obd-e2e/H12-obd-e2e.log
#
#   tools/obd-e2e/run-h12.sh            # RUN_S=150 (agent run time, s), RUNS=1 (repeats)
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
WORK="$REPO/target/obd-e2e"
LABEL="${XORGATE_AGENT_LABEL:-p5b-obd}"
mkdir -p "$WORK/in"
docker run --rm --platform linux/arm64 --label xorgate.agent="$LABEL" -v "$REPO:/src" \
  -v xorgate-rs-cargo-registry:/usr/local/cargo/registry -v xorgate-rs-cargo-git:/usr/local/cargo/git \
  -v xorgate-rs-target:/target -e CARGO_TARGET_DIR=/target/p5b-obd -e CARGO_BUILD_JOBS=3 -w /src \
  xorgate-rs-build:1.99.0-c813d06bc873 sh -c 'cargo build -p xorgate-agent 2>&1 | tail -2 && cp /target/p5b-obd/debug/xorgate-rs /src/target/obd-e2e/in/xorgate-rs'
docker build --platform linux/arm64 -q -t xorgate-obd-py:bookworm "$HERE" >/dev/null
# RUNS=N repeats the case (review-wp2 M2: the shutdown must be deterministic).
: > "$WORK/H12-obd-e2e.log"
rc=0
for i in $(seq 1 "${RUNS:-1}"); do
  echo "=== H12 run $i/${RUNS:-1}" | tee -a "$WORK/H12-obd-e2e.log"
  docker run --rm --platform linux/arm64 --label xorgate.agent="$LABEL" -e RUN_S="${RUN_S:-150}" \
    -v "$WORK/in:/in:ro" -v "$REPO/python:/vendored:ro" -v "$HERE:/e2e:ro" \
    xorgate-obd-py:bookworm bash /e2e/h12-case.sh 2>&1 | tee -a "$WORK/H12-obd-e2e.log"
  [ "${PIPESTATUS[0]}" = 0 ] || rc=1
done
exit "$rc"
