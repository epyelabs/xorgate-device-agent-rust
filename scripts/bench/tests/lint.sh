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
             "exclusive.sh status" "gate-seam.sh on" "shm-audit.sh" "soak.sh" "filler.sh seed" "start.sh" "ntp-unsync.sh status" "status.sh" \
             "drive-blackbox-ctl.sh install" "drive-blackbox-ctl.sh status" "drive-blackbox-ctl.sh uninstall" "drive-blackbox-ctl.sh summary latest"; do
    n=${cmd//[ \/]/_}
    # shellcheck disable=SC2086
    XORGATE_TEST_DUMP="$D/$n.sh" bash scripts/bench/$cmd >/dev/null 2>&1 || true
  done
  ls "$D"/*.sh >/dev/null 2>&1 || failc "no remote bodies dumped"
  # SC2034: the shared constants block is used by some bodies only.
  shellcheck -s bash -S warning -e SC2034 "$D"/*.sh >"$LOGS/sc-remote.log" 2>&1 && pass "shellcheck -S warning (remote bodies: $(ls "$D" | wc -l))" || { failc "shellcheck remote bodies:"; cat "$LOGS/sc-remote.log"; }
fi
# Phase 5 WP3: the dist scripts + their test (bash -n + shellcheck -x).
for f in scripts/dist-manifest.sh scripts/dist-check.sh scripts/dist-guard.sh scripts/tests/dist.sh scripts/tests/unit-sections.sh scripts/tests/unit-ordering.sh scripts/tests/clippy-ban-probe.sh; do bash -n "$f" && { ! command -v shellcheck >/dev/null || shellcheck -x "$f"; } >"$LOGS/sc-dist.log" 2>&1 && pass "bash -n + shellcheck $f" || { failc "lint $f:"; cat "$LOGS/sc-dist.log"; }; done
for f in tools/convert-e2e/*.sh tools/convert-e2e/shims/*; do bash -n "$f" && pass "bash -n $f" || failc "bash -n $f"; done; if command -v shellcheck >/dev/null; then shellcheck -x -s bash tools/convert-e2e/*.sh tools/convert-e2e/shims/* >"$LOGS/sc-convert-e2e.log" 2>&1 && pass "shellcheck (tools/convert-e2e)" || { failc "shellcheck tools/convert-e2e:"; cat "$LOGS/sc-convert-e2e.log"; }; fi  # Phase 5 WP2 (G1)
if command -v shellcheck >/dev/null; then D5=$(mktemp -d); for c in status apply revert; do XORGATE_TEST_DUMP="$D5/p5b_$c.sh" P5B_NO_COPY=1 bash scripts/bench/p5b-overlay.sh $c >/dev/null 2>&1 || true; done; shellcheck -s bash -S warning -e SC2034 "$D5"/*.sh >"$LOGS/sc-p5b.log" 2>&1 && pass "shellcheck -S warning (p5b-overlay remote bodies: $(ls "$D5" | wc -l))" || { failc "shellcheck p5b-overlay remote bodies:"; cat "$LOGS/sc-p5b.log"; }; fi  # Phase 5b WP4
f=tools/replay/shims/bluetoothctl; sh -n "$f" && { ! command -v shellcheck >/dev/null || shellcheck -s sh "$f"; } >"$LOGS/sc-p5b-shim.log" 2>&1 && pass "sh -n + shellcheck $f" || { failc "lint $f:"; cat "$LOGS/sc-p5b-shim.log"; }  # Phase 5b WP4
# Phase 7e WP2: the simulator image's scripts (bash -n + shellcheck -x).
# sim/entrypoint.sh keeps the JS entrypoint's lines byte for byte (its diff vs
# the JS file is the three [7e] changes only), so the findings that shape
# carries are waived for it alone: SC2155 (`export X="$(…)"`), SC2015 + SC2317
# (the `forward` trap handler; SC2329 is shellcheck >= 0.10's name for it).
for f in sim/entrypoint.sh sim/build-image.sh scripts/docker-build.sh scripts/tests/sim-image.sh; do
  ex=(); [ "$f" != sim/entrypoint.sh ] || ex=(-e "SC2155,SC2015,SC2317,SC2329")
  bash -n "$f" && { ! command -v shellcheck >/dev/null || shellcheck -x "${ex[@]}" "$f"; } >"$LOGS/sc-p7e.log" 2>&1 && pass "bash -n + shellcheck $f" || { failc "lint $f:"; cat "$LOGS/sc-p7e.log"; }
done
# Phase 6 drive blackbox: the python bench tools parse (ast only: py_compile
# would write __pycache__ into the mounted repo).
for f in scripts/bench/*.py; do python3 -c 'import ast, sys; ast.parse(open(sys.argv[1]).read(), sys.argv[1])' "$f" && pass "python3 ast.parse $f" || failc "python3 ast.parse $f"; done
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
