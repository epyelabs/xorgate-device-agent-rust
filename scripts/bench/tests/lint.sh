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
if command -v shellcheck >/dev/null; then
  shellcheck -x scripts/bench/*.sh scripts/bench/tests/*.sh >"$LOGS/sc.log" 2>&1 && pass "shellcheck (Mac-side scripts + tests)" || { failc "shellcheck:"; cat "$LOGS/sc.log"; }
  fixture_bench; reset_units
  D=$(mktemp -d)
  for cmd in "dev-manifests.sh" "profile.sh shared" "profile.sh exclusive" "exclusive.sh begin" "exclusive.sh end" "exclusive.sh tape-off" \
             "exclusive.sh status" "gate-seam.sh on" "shm-audit.sh" "soak.sh" "filler.sh seed" "start.sh"; do
    n=${cmd//[ \/]/_}
    # shellcheck disable=SC2086
    XORGATE_TEST_DUMP="$D/$n.sh" bash scripts/bench/$cmd >/dev/null 2>&1 || true
  done
  ls "$D"/*.sh >/dev/null 2>&1 || failc "no remote bodies dumped"
  # SC2034: the shared constants block is used by some bodies only.
  shellcheck -s bash -S warning -e SC2034 "$D"/*.sh >"$LOGS/sc-remote.log" 2>&1 && pass "shellcheck -S warning (remote bodies: $(ls "$D" | wc -l))" || { failc "shellcheck remote bodies:"; cat "$LOGS/sc-remote.log"; }
fi
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
