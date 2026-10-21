#!/usr/bin/env bash
# shellcheck disable=SC2015  # test assertions: `a && pass || failc`
# CF68 (Phase 5 WP1): the bash copies of the 024 sudoers body, the
# /usr/local/bin/xorgate-rs wrapper and 10-dist.conf (in scripts/install-rs.sh,
# and the 024 body in the bench scripts/bench/_lib.sh) must equal the golden
# fixtures/install_rs/bodies.json, which crates/xorgate-core/tests/
# install_rs_bodies.rs ties to xorgate_core::update_plan. So neither bash copy
# can drift from what `xorgate-rs update` writes. Needs bash 4 + sed only:
#   docker run --rm -v "$PWD:/src" -w /src debian:bookworm bash scripts/tests/install-rs-bodies.sh
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
GOLDEN="$REPO/fixtures/install_rs/bodies.json"
fail=0
pass() { echo "PASS $*"; }
failc() { echo "FAIL $*"; fail=1; }

# The value of `"<key>": "<json string>"` (one per line) decoded; the golden
# uses only the escapes \n and \" (anything else is refused, not guessed).
golden() {
  local raw rest
  raw=$(sed -n "s/^[[:space:]]*\"$1\": \"\(.*\)\",\{0,1\}\$/\1/p" "$GOLDEN")
  [ "$(printf '%s\n' "$raw" | wc -l)" = 1 ] && [ -n "$raw" ] || { echo "golden: no single \"$1\" line" >&2; return 1; }
  rest=${raw//\\n/}; rest=${rest//\\\"/}
  case $rest in *\\*) printf 'golden: "%s" uses an escape other than \\n or \\"\n' "$1" >&2; return 1 ;; esac
  raw=${raw//\\\"/\"}
  printf '%s' "${raw//\\n/$'\n'}"
}
# Byte compare of two strings (trailing newlines included: the X sentinel).
same() { # <label> <want> <got>
  if [ "$2" = "$3" ]; then pass "$1"; else
    failc "$1"; diff <(printf '%s' "$2") <(printf '%s' "$3") | sed 's/^/    /'
  fi
}

# install-rs.sh guards main(): sourcing it only defines the functions.
# shellcheck source=scripts/install-rs.sh
source "$REPO/scripts/install-rs.sh"
set +e

W=$(golden wrapper; echo X) || failc "golden wrapper unreadable"
same "install-rs.sh rs_wrapper_body == golden wrapper (update_plan::WRAPPER_BODY)" "$W" "$(rs_wrapper_body; echo X)"
D=$(golden distDropin; echo X) || failc "golden distDropin unreadable"
same "install-rs.sh rs_dist_dropin_body == golden distDropin (§8 10-dist.conf)" "$D" "$(rs_dist_dropin_body; echo X)"

# The bench copy lives inside the bench_remote_lib heredoc: take the function
# text out of scripts/bench/_lib.sh and define it here.
LIBFN=$(sed -n '/^update_sudoers_body() {$/,/^}$/p' "$REPO/scripts/bench/_lib.sh")
[ -n "$LIBFN" ] && eval "$LIBFN" && pass "scripts/bench/_lib.sh update_sudoers_body extracted" || failc "scripts/bench/_lib.sh update_sudoers_body not found"

for u in pi operator; do
  G=$(golden "$u"; echo X) || failc "golden updaterSudoers.$u unreadable"
  same "install-rs.sh rs_update_sudoers_body $u == golden (update_plan::updater_sudoers_body)" "$G" "$(rs_update_sudoers_body "$u"; echo X)"
  same "_lib.sh update_sudoers_body $u == golden (update_plan::updater_sudoers_body)" "$G" "$(update_sudoers_body "$u"; echo X)"
done

echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
