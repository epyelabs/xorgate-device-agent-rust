#!/usr/bin/env bash
# shellcheck disable=SC2319,SC2015,SC2012  # test assertions: `[ … ]; expect $?` and `a && pass || failc`
# bash -n + shellcheck over every bench script AND the remote heredoc bodies
# they send (dumped through the ssh stub), run in a throwaway container:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/bench/tests/lint.sh
set -uo pipefail
# shellcheck source=scripts/bench/tests/_stubs.sh
source "$(dirname "$0")/_stubs.sh"
command -v shellcheck >/dev/null || { apt-get update -qq >/dev/null && apt-get install -y -qq shellcheck >/dev/null; } \
  || echo "NOTE: shellcheck not installable — bash -n only"
cd "$REPO" || exit 2
for f in scripts/bench/*.sh scripts/bench/tests/*.sh; do
  bash -n "$f" && pass "bash -n $f" || failc "bash -n $f"
done
for f in scripts/install-rs.sh scripts/tests/install-rs-*.sh; do bash -n "$f" && pass "bash -n $f" || failc "bash -n $f"; ! command -v shellcheck >/dev/null || { shellcheck -x "$f" >"$LOGS/sc-p5wp1.log" 2>&1 && pass "shellcheck -x $f" || { failc "shellcheck -x $f:"; cat "$LOGS/sc-p5wp1.log"; }; }; done  # Phase 5 WP1
if command -v shellcheck >/dev/null; then
  shellcheck -x scripts/bench/*.sh scripts/bench/tests/*.sh >"$LOGS/sc.log" 2>&1 && pass "shellcheck (Mac-side scripts + tests)" || { failc "shellcheck:"; cat "$LOGS/sc.log"; }
  fixture_bench; reset_units
  D=$(mktemp -d)
  for cmd in "dev-manifests.sh" "profile.sh shared" "profile.sh exclusive" "exclusive.sh begin" "exclusive.sh end" "exclusive.sh tape-off" \
             "exclusive.sh status" "gate-seam.sh on" "shm-audit.sh" "soak.sh" "filler.sh seed" "start.sh" "ntp-unsync.sh status" "status.sh"; do
    n=${cmd//[ \/]/_}
    # shellcheck disable=SC2086
    XORGATE_TEST_DUMP="$D/$n.sh" bash scripts/bench/$cmd >/dev/null 2>&1 || true
  done
  ls "$D"/*.sh >/dev/null 2>&1 || failc "no remote bodies dumped"
  # SC2034: the shared constants block is used by some bodies only.
  shellcheck -s bash -S warning -e SC2034 "$D"/*.sh >"$LOGS/sc-remote.log" 2>&1 && pass "shellcheck -S warning (remote bodies: $(ls "$D" | wc -l))" || { failc "shellcheck remote bodies:"; cat "$LOGS/sc-remote.log"; }
fi
# Phase 5 WP3: the dist scripts + their test (bash -n + shellcheck -x).
for f in scripts/dist-manifest.sh scripts/dist-check.sh scripts/dist-guard.sh scripts/tests/dist.sh; do bash -n "$f" && { ! command -v shellcheck >/dev/null || shellcheck -x "$f"; } >"$LOGS/sc-dist.log" 2>&1 && pass "bash -n + shellcheck $f" || { failc "lint $f:"; cat "$LOGS/sc-dist.log"; }; done
for f in tools/convert-e2e/*.sh tools/convert-e2e/shims/*; do bash -n "$f" && pass "bash -n $f" || failc "bash -n $f"; done; if command -v shellcheck >/dev/null; then shellcheck -x -s bash tools/convert-e2e/*.sh tools/convert-e2e/shims/* >"$LOGS/sc-convert-e2e.log" 2>&1 && pass "shellcheck (tools/convert-e2e)" || { failc "shellcheck tools/convert-e2e:"; cat "$LOGS/sc-convert-e2e.log"; }; fi  # Phase 5 WP2 (G1)
if command -v shellcheck >/dev/null; then D5=$(mktemp -d); for c in status apply revert; do XORGATE_TEST_DUMP="$D5/p5b_$c.sh" P5B_NO_COPY=1 bash scripts/bench/p5b-overlay.sh $c >/dev/null 2>&1 || true; done; shellcheck -s bash -S warning -e SC2034 "$D5"/*.sh >"$LOGS/sc-p5b.log" 2>&1 && pass "shellcheck -S warning (p5b-overlay remote bodies: $(ls "$D5" | wc -l))" || { failc "shellcheck p5b-overlay remote bodies:"; cat "$LOGS/sc-p5b.log"; }; fi  # Phase 5b WP4
f=tools/replay/shims/bluetoothctl; sh -n "$f" && { ! command -v shellcheck >/dev/null || shellcheck -s sh "$f"; } >"$LOGS/sc-p5b-shim.log" 2>&1 && pass "sh -n + shellcheck $f" || { failc "lint $f:"; cat "$LOGS/sc-p5b-shim.log"; }  # Phase 5b WP4
for f in tools/replay/tests/*.sh tools/capture-host/run-rig.sh; do bash -n "$f" && { ! command -v shellcheck >/dev/null || shellcheck -x "$f"; } >"$LOGS/sc-p7a.log" 2>&1 && pass "bash -n + shellcheck $f" || { failc "lint $f:"; cat "$LOGS/sc-p7a.log"; }; done  # Phase 7a WP3
for f in tools/replay/shims/xorgate-capture tools/replay/timing_truth.py tools/replay/tests/timing_truth_test.py tools/capture-host/timing_harness.py; do python3 -m py_compile "$f" 2>"$LOGS/py-p7a.log" && pass "py_compile $f" || { failc "py_compile $f:"; cat "$LOGS/py-p7a.log"; }; done  # Phase 7a WP3
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
