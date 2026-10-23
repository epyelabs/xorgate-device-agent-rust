#!/usr/bin/env bash
# H4 (phase-5b-js-catchup.md §11): the 78 JS python tests against the
# VENDORED tree (rs:python/ = git archive 09da38d python), in
# xorgate-obd-py:bookworm (Python 3.11 + ELM327-emulator 4.0.0). Must report
# 78 tests, 0 failures, 0 skipped (the 14 emulator tests run over a pty).
#
#   tools/obd-e2e/h4-python.sh [log]      # default log: target/obd-e2e/H4-python.log
#
# The tree is mounted read-only and copied to a scratch dir inside the
# container (the tests and the emulator write elm.log / __pycache__ / audit
# files; nothing may land in the vendored tree, whose SHA256SUMS pack.sh checks).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
IMAGE=xorgate-obd-py:bookworm
LOG=${1:-$REPO/target/obd-e2e/H4-python.log}
mkdir -p "$(dirname "$LOG")"
docker build --platform linux/arm64 -q -t "$IMAGE" "$HERE" >/dev/null
docker run --rm --platform linux/arm64 --label xorgate.agent="${XORGATE_AGENT_LABEL:-p5b-obd}" \
  -v "$REPO/python:/vendored:ro" "$IMAGE" sh -c '
    set -e
    python3 --version; cat /opt/elm-venv/PIP-FREEZE.txt
    (cd /vendored && sha256sum --quiet -c SHA256SUMS) && echo "vendored tree matches SHA256SUMS"
    mkdir -p /work && cp -R /vendored /work/python && cd /work
    python3 -m unittest discover -s python/tests -v 2>&1' | tee "$LOG"
summary=$(grep -E '^(Ran [0-9]+ tests|OK|FAILED)' "$LOG" | tr '\n' ' ')
echo "== H4 summary: $summary"
grep -qE '^Ran 78 tests' "$LOG" || { echo "H4 FAIL: expected 78 tests"; exit 1; }
# unittest prints `OK (skipped=N)` and `... skipped '<reason>'` per test; a bare
# `OK` = 0 failures AND 0 skipped (a test NAME may contain "skipped").
grep -qE '^OK$' "$LOG" || { echo "H4 FAIL: not a bare OK (failures or skips)"; exit 1; }
if grep -qE "\.\.\. skipped " "$LOG"; then echo "H4 FAIL: skipped tests present"; exit 1; fi
echo "H4 PASS: 78 tests, 0 failures, 0 skipped"
