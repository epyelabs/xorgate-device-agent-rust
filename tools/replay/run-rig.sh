#!/usr/bin/env bash
# Run the record/replay rig on one tape and diff the two agents (C3).
#
#   tools/replay/run-rig.sh <tape> [options] [-- extra `xorgate-replay play` flags]
#
#   --calibrate        A = B = js (C3a: two JS instances must diff clean)
#   --a js|js@REF|rust agent A (default js)
#   --b js|js@REF|rust agent B (default rust)
#                      `js` = the JS build @ $JS_REF; `js@<ref>` pins that side
#                      (Phase 5b dual ref: `--a js@89b5c64 --b js@09da38d`)
#   --out DIR          copy the run + diff.md there (default
#                      plans/…/phase-3/evidence/replay-<UTC ts>/; EVIDENCE_ROOT overrides)
#   --no-build         skip build-bins.sh (use the binaries in .cache/bin)
#
# Compose project: per run, `xorgate-replay-<ts>-<a>-<b>` (override with
# COMPOSE_PROJECT_NAME); the cleanup tears down ONLY that project, never another
# run's (or another agent's) broker.
#
# Phase 5b env:
#   JS_REF=<sha>          the JS reference for a bare `js` side (default
#                         09da38d since the H7 flip; 89b5c64 stays buildable for archaeology)
#   RIG_JS_CACHE=<abs>    the dir holding the ONE shared built copy per ref
#                         (`js-<sha>/`, js-build.sh); default tools/replay/.cache.
#                         Mounted read-only at /rig/jscache.
#   XORGATE_AGENT_LABEL   the `xorgate.agent` container label (default `rig`):
#                         every container of the run carries it (hygiene filter)
#
# Steps: gen-certs.sh → js-build.sh → build-bins.sh → hash .cache/bin →
# mosquitto up → `play` (real time; the tape's length + settle) → `diff` → copy
# (+ `bins.sha256`: the sha256 of xorgate-replay, xorgate-rs and any
# xorgate-rs-* as found at play start) → compose down -v (on every exit path,
# errors and signals included).
# Exit code = the diff's (non-zero on any UNTOLERATED mismatch).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
EVIDENCE_ROOT="${EVIDENCE_ROOT:-$(cd "$REPO/../../plans/device-agent-js-to-rust/phase-3/evidence" 2>/dev/null && pwd || echo "$HERE/.cache/evidence")}"

usage() { sed -n '2,33p' "$0"; exit 2; }
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

JS_REF="${JS_REF:-09da38d}"
RIG_JS_CACHE="${RIG_JS_CACHE:-$HERE/.cache}"
case "$RIG_JS_CACHE" in /*) ;; *) echo "RIG_JS_CACHE must be absolute: $RIG_JS_CACHE" >&2; exit 2 ;; esac
export RIG_JS_CACHE
export XORGATE_AGENT_LABEL="${XORGATE_AGENT_LABEL:-rig}"
# side <spec> → "<kind> <ref>" (ref empty for rust)
side() {
  case "$1" in
    rust) echo "rust -" ;;
    js) echo "js $JS_REF" ;;
    js@?*) echo "js ${1#js@}" ;;
    *) echo "bad agent spec: $1 (js | js@<ref> | rust)" >&2; exit 2 ;;
  esac
}
read -r A_KIND A_REF <<<"$(side "$A")"
read -r B_KIND B_REF <<<"$(side "$B")"
tag() { if [ "$1" = js ]; then printf 'js%s' "$2"; else printf rust; fi; }
A_TAG=$(tag "$A_KIND" "$A_REF"); B_TAG=$(tag "$B_KIND" "$B_REF")

TS=$(date -u +%Y%m%dT%H%M%SZ)
RUN="run-$TS-$A_TAG-$B_TAG"
[ -n "$OUT" ] || OUT="$EVIDENCE_ROOT/replay-$TS"
PROJECT="${COMPOSE_PROJECT_NAME:-xorgate-replay-$(printf '%s' "$TS-$A_TAG-$B_TAG" | tr '[:upper:]' '[:lower:]')}"
dc() { docker compose -p "$PROJECT" "$@"; }

"$HERE/gen-certs.sh"
JS_DIR_ARGS=()
for s in "a $A_KIND $A_REF" "b $B_KIND $B_REF"; do
  read -r l k r <<<"$s"
  [ "$k" = js ] || continue
  JS_SHA="$r" "$HERE/js-build.sh"
  JS_DIR_ARGS+=(--js-dir-"$l" "/rig/jscache/js-$r")
done
[ "$BUILD" -eq 0 ] || "$HERE/build-bins.sh"
# Per-run binary record (F-P2-ROUND review F5): the sha256 of every binary the
# run can use, hashed BEFORE `play` and written to the run's own `bins.sha256`
# (the agent side(s): `xorgate-rs`, plus any `xorgate-rs-*` it may exec as a
# per-side wrapper; the harness: `xorgate-replay`, used for play AND diff).
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
BINS_SHA=$(
  cd "$HERE/.cache/bin"
  echo "# binaries in tools/replay/.cache/bin at play start ($(date -u +%FT%TZ)); repo HEAD $(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
  for f in xorgate-replay xorgate-rs xorgate-rs-* rig-statvfs.so; do
    [ -f "$f" ] && sha256 "$f"
  done
  true
)
for f in xorgate-replay xorgate-rs; do
  printf '%s\n' "$BINS_SHA" | grep -q "  $f\$" || { echo "no $f in $HERE/.cache/bin" >&2; exit 2; }
done

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
echo ">> play $(basename "$TAPE") A=$A_TAG B=$B_TAG → .cache/runs/$RUN (real time)"
run_dc run --rm -T rig play --tape "/rig/tapes/$(basename "$TAPE")" --out "/rig/out/$RUN" \
  --a "$A_KIND" --b "$B_KIND" --js-dir "/rig/jscache/js-$JS_REF" ${JS_DIR_ARGS[@]+"${JS_DIR_ARGS[@]}"} \
  --rs-bin /rig/cache/bin/xorgate-rs ${EXTRA[@]+"${EXTRA[@]}"}
RC=0
run_dc run --rm -T rig diff "/rig/out/$RUN" --tolerances /rig/tools/tolerances.toml || RC=$?
mkdir -p "$OUT"
cp -R "$HERE/.cache/runs/$RUN/." "$OUT/"
printf '%s\n' "$BINS_SHA" > "$OUT/bins.sha256"
echo ">> diff: $OUT/diff.md (exit $RC)"
exit $RC
