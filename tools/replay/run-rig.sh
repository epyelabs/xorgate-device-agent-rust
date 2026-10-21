#!/usr/bin/env bash
# Run the record/replay rig on one tape and diff the two agents (C3).
#
#   tools/replay/run-rig.sh <tape> [options] [-- extra `xorgate-replay play` flags]
#
#   --calibrate        A = B = js (C3a: two JS instances must diff clean)
#   --a js|rust        agent A (default js)
#   --b js|rust        agent B (default rust)
#   --out DIR          copy the run + diff.md there (default
#                      plans/…/phase-3/evidence/replay-<UTC ts>/; EVIDENCE_ROOT overrides)
#   --no-build         skip build-bins.sh (use the binaries in .cache/bin)
#
# Compose project: per run, `xorgate-replay-<ts>-<a>-<b>` (override with
# COMPOSE_PROJECT_NAME); the cleanup tears down ONLY that project, never another
# run's (or another agent's) broker.
#
# Steps: gen-certs.sh → js-build.sh → build-bins.sh → mosquitto up → `play`
# (real time; the tape's length + settle) → `diff` → copy → compose down -v
# (on every exit path, errors and signals included).
# Exit code = the diff's (non-zero on any UNTOLERATED mismatch).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
EVIDENCE_ROOT="${EVIDENCE_ROOT:-$(cd "$REPO/../../plans/device-agent-js-to-rust/phase-3/evidence" 2>/dev/null && pwd || echo "$HERE/.cache/evidence")}"

usage() { sed -n '2,17p' "$0"; exit 2; }
[ $# -ge 1 ] || usage
TAPE=""; A=js; B=rust; OUT=""; BUILD=1; EXTRA=()
while [ $# -gt 0 ]; do
  case "$1" in
    --calibrate) A=js; B=js; shift ;;
    --a) A="$2"; shift 2 ;;
    --b) B="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --no-build) BUILD=0; shift ;;
    -h|--help) usage ;;
    --) shift; EXTRA=("$@"); break ;;
    -*) echo "unknown option $1" >&2; usage ;;
    *) TAPE="$1"; shift ;;
  esac
done
[ -f "$TAPE" ] || { echo "no tape: $TAPE" >&2; exit 2; }

TS=$(date -u +%Y%m%dT%H%M%SZ)
RUN="run-$TS-$A-$B"
[ -n "$OUT" ] || OUT="$EVIDENCE_ROOT/replay-$TS"
PROJECT="${COMPOSE_PROJECT_NAME:-xorgate-replay-$(printf '%s' "$TS-$A-$B" | tr '[:upper:]' '[:lower:]')}"
dc() { docker compose -p "$PROJECT" "$@"; }

"$HERE/gen-certs.sh"
"$HERE/js-build.sh"
[ "$BUILD" -eq 0 ] || "$HERE/build-bins.sh"

mkdir -p "$HERE/.cache/tapes" "$HERE/.cache/runs"
DEST_TAPE="$HERE/.cache/tapes/$(basename "$TAPE")"
[ "$(cd "$(dirname "$TAPE")" && pwd)/$(basename "$TAPE")" = "$DEST_TAPE" ] || cp "$TAPE" "$DEST_TAPE"
export RIG_TAPES="$HERE/.cache/tapes" RIG_RUNS="$HERE/.cache/runs"

cd "$HERE"
# shellcheck disable=SC2317,SC2329  # invoked by the EXIT trap
# `-v`: the mosquitto image declares two anonymous volumes (/mosquitto/data,
# /mosquitto/log); without it every run leaked both (F-RIG-1, Phase 4 gate).
cleanup() { dc down -v --remove-orphans >/dev/null 2>&1 || true; }
trap 'rc=$?; cleanup; exit $rc' EXIT
# A signal ends the script through `exit`, so the EXIT trap (and the teardown)
# runs on Ctrl-C / kill / Ctrl-\ too (QUIT does not run an EXIT trap by itself).
# The long `dc run` steps run in the background + `wait`, so a TERM is acted on
# at once (bash defers traps while a FOREGROUND child runs): the handler
# forwards it to that `docker compose run` (which stops its container), waits
# for it, then exits through the EXIT trap. Review-gate-fixes F-R4.
CHILD=""
# shellcheck disable=SC2317,SC2329  # invoked by the signal traps
on_signal() {
  if [ -n "$CHILD" ]; then
    kill -TERM "$CHILD" 2>/dev/null || true
    wait "$CHILD" 2>/dev/null || true
  fi
  exit "$1"
}
trap 'on_signal 130' INT
trap 'on_signal 143' TERM
trap 'on_signal 131' QUIT
# run_dc <compose args…>: `docker compose -p $PROJECT …` as a waited background
# child; returns its exit status.
run_dc() {
  docker compose -p "$PROJECT" "$@" &
  CHILD=$!
  local rc=0
  wait "$CHILD" || rc=$?
  CHILD=""
  return "$rc"
}
echo ">> compose project $PROJECT"
dc build --quiet rig
dc up -d mosquitto
echo ">> play $(basename "$TAPE") A=$A B=$B → .cache/runs/$RUN (real time)"
run_dc run --rm -T rig play --tape "/rig/tapes/$(basename "$TAPE")" --out "/rig/out/$RUN" \
  --a "$A" --b "$B" --js-dir "/rig/cache/js-89b5c64" --rs-bin /rig/cache/bin/xorgate-rs ${EXTRA[@]+"${EXTRA[@]}"}
RC=0
run_dc run --rm -T rig diff "/rig/out/$RUN" --tolerances /rig/tools/tolerances.toml || RC=$?
mkdir -p "$OUT"
cp -R "$HERE/.cache/runs/$RUN/." "$OUT/"
echo ">> diff: $OUT/diff.md (exit $RC)"
exit $RC
