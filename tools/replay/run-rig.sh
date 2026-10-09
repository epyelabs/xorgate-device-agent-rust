#!/usr/bin/env bash
# Run the record/replay rig on one tape and diff the two agents (C3).
#
#   tools/replay/run-rig.sh <tape> [options] [-- extra `xorgate-replay play` flags]
#
#   --calibrate        A = B = js (C3a: two JS instances must diff clean)
#   --a js|rust        agent A (default js)
#   --b js|rust        agent B (default rust)
#   --out DIR          copy the run + diff.md there (default
#                      plans/…/phase-2/evidence/replay-<UTC ts>/)
#   --no-build         skip build-bins.sh (use the binaries in .cache/bin)
#
# Steps: gen-certs.sh → js-build.sh → build-bins.sh → mosquitto up → `play`
# (real time; the tape's length + settle) → `diff` → copy → compose down.
# Exit code = the diff's (non-zero on any UNTOLERATED mismatch).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
EVIDENCE_ROOT="${EVIDENCE_ROOT:-$(cd "$REPO/../../plans/device-agent-js-to-rust/phase-2/evidence" 2>/dev/null && pwd || echo "$HERE/.cache/evidence")}"

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

"$HERE/gen-certs.sh"
"$HERE/js-build.sh"
[ "$BUILD" -eq 0 ] || "$HERE/build-bins.sh"

mkdir -p "$HERE/.cache/tapes" "$HERE/.cache/runs"
cp "$TAPE" "$HERE/.cache/tapes/$(basename "$TAPE")"
export RIG_TAPES="$HERE/.cache/tapes" RIG_RUNS="$HERE/.cache/runs"

cd "$HERE"
cleanup() { docker compose down --remove-orphans >/dev/null 2>&1 || true; }
trap cleanup EXIT
docker compose build --quiet rig
docker compose up -d mosquitto
echo ">> play $(basename "$TAPE") A=$A B=$B → .cache/runs/$RUN (real time)"
docker compose run --rm -T rig play --tape "/rig/tapes/$(basename "$TAPE")" --out "/rig/out/$RUN" \
  --a "$A" --b "$B" --js-dir "/rig/cache/js-89b5c64" --rs-bin /rig/cache/bin/xorgate-rs "${EXTRA[@]}"
set +e
docker compose run --rm -T rig diff "/rig/out/$RUN" --tolerances /rig/tools/tolerances.toml
RC=$?
set -e
mkdir -p "$OUT"
cp -R "$HERE/.cache/runs/$RUN/." "$OUT/"
echo ">> diff: $OUT/diff.md (exit $RC)"
exit $RC
