#!/usr/bin/env bash
# Phase 6 P3a lanes: cross-agent recording takeover on a NON-empty state
# (plans/device-agent-js-to-rust/phase-6/evidence/P3a-takeover.md §2).
#
#   tools/replay/p3a-lanes.sh synth              # the four p6-takeover-* tapes → .cache/tapes
#   tools/replay/p3a-lanes.sh matrix             # print the planned lanes L1–L16
#   tools/replay/p3a-lanes.sh run <lane>...      # run lanes SERIALLY
#   tools/replay/p3a-lanes.sh run-matrix         # run L1–L16 serially
#
# <lane> = <id>:<tape>:<dir>:<sig>
#   tape  p6-takeover-open | -vbacklog | -tbacklog | -reap | -inflight  (no .ndjson.gz)
#   dir   jr = side A JS→Rust, side B JS→JS (the control)
#         rj = side A Rust→JS, side B Rust→Rust
#         jj / rr = the same-agent calibration of a lane (on demand only)
#   sig   term (systemctl stop: SIGTERM to the whole unit, SIGKILL after 90 s)
#         kill (SIGKILL to the whole unit)
#
# Per lane:
#   1. Quiet-host preflight (handbook §3): no other `xorgate.agent`-labelled
#      container, macOS 1-min load ≤ 8. Polled every 5 min for up to 60 min,
#      else the lane is SKIPPED.
#   2. run-rig.sh --no-build with the handoff flags; the full run goes to
#      $P3A_WORK/runs/<id>.
#   3. One TSV line to $P3A_OUT/q.tsv: lane, tape, dir, sig, start, secs, rc,
#      verdict (PASS / FAIL / INVALID), load (start / max / end). An INVALID
#      lane (inherited state not as designed, or a cut near a rotation) is a
#      tape fix + re-run, never a lane's final word.
#   4. The small artifacts are copied to $P3A_OUT/<id>/: diff.md/json,
#      play.json, handoff.json, per side meta.json, handoff-state.json and
#      the stderr tail. No mp4, no full logs.
# The whole runner holds the Mac awake (`caffeinate -dims -w $$`).
#
# Env:
#   P3A_OUT              evidence dir (required)
#   P3A_WORK             scratch for full runs (required)
#   XORGATE_AGENT_LABEL  default p6-takeover
#   RIG_IMAGE            default xorgate-replay-rig:p6-takeover
#   RIG_JS_CACHE         the shared JS build cache (absolute)
#   JS_REF               default 09da38d
#   P3A_LOAD_MAX         default 8
set -u
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
export XORGATE_AGENT_LABEL="${XORGATE_AGENT_LABEL:-p6-takeover}"
export RIG_IMAGE="${RIG_IMAGE:-xorgate-replay-rig:p6-takeover}"
export JS_REF="${JS_REF:-09da38d}"
LOAD_MAX="${P3A_LOAD_MAX:-8}"
TAPES="p6-takeover-open p6-takeover-vbacklog p6-takeover-tbacklog p6-takeover-reap p6-takeover-inflight p6-takeover-vbacklog-503tail"

matrix() {
  local n=1 t d s
  for t in p6-takeover-open p6-takeover-vbacklog p6-takeover-tbacklog; do
    for d in jr rj; do for s in term kill; do echo "L$n:$t:$d:$s"; n=$((n+1)); done; done
  done
  echo "L13:p6-takeover-reap:jr:kill"
  echo "L14:p6-takeover-reap:rj:kill"
  echo "L15:p6-takeover-inflight:jr:kill"
  echo "L16:p6-takeover-inflight:rj:kill"
}

load1() { sysctl -n vm.loadavg 2>/dev/null | awk '{print $2}' || awk '{print $1}' /proc/loadavg; }
others() {
  docker ps --filter label=xorgate.agent --format '{{.Label "xorgate.agent"}}' \
    | grep -vx "$XORGATE_AGENT_LABEL" | sort -u | tr '\n' ' '
}

synth() {
  cd "$HERE" || exit 1
  mkdir -p .cache/tapes .cache/runs
  local t p="xorgate-replay-p3a-synth"
  for t in $TAPES; do
    docker compose -p "$p" run --rm -T --no-deps rig synth "$t" -o "/rig/out/$t.ndjson.gz" --fixtures /rig/fixtures >/dev/null \
      || { echo "synth $t FAILED" >&2; docker compose -p "$p" down -v --remove-orphans >/dev/null 2>&1; exit 1; }
    mv ".cache/runs/$t.ndjson.gz" .cache/tapes/
    shasum -a 256 ".cache/tapes/$t.ndjson.gz"
  done
  docker compose -p "$p" down -v --remove-orphans >/dev/null 2>&1
}

flags() { # <dir> <sig> → run-rig.sh args
  case "$1" in
    jr) echo "--a js --b js -- --handoff-signal $2 --then-a rust --then-b js" ;;
    rj) echo "--a rust --b rust -- --handoff-signal $2 --then-a js --then-b rust" ;;
    jj) echo "--a js --b js -- --handoff-signal $2 --then-a js --then-b js" ;;
    rr) echo "--a rust --b rust -- --handoff-signal $2 --then-a rust --then-b rust" ;;
    *) echo "bad dir $1" >&2; return 1 ;;
  esac
}

run_lane() { # <id>:<tape>:<dir>:<sig>
  local id tape dir sig
  IFS=: read -r id tape dir sig <<<"$1"
  local args; args=$(flags "$dir" "$sig") || return 2
  local tapef="$HERE/.cache/tapes/$tape.ndjson.gz"
  [ -f "$tapef" ] || { echo "no tape $tapef (run: $0 synth)" >&2; return 2; }
  local waited=0 L O
  while :; do
    L=$(load1); O=$(others)
    if [ -z "$O" ] && awk -v l="$L" -v m="$LOAD_MAX" 'BEGIN{exit !(l<=m)}'; then break; fi
    if [ "$waited" -ge 3600 ]; then
      printf '%s\t%s\t%s\t%s\tSKIPPED\tload=%s others=%s waited=%ss\n' "$id" "$tape" "$dir" "$sig" "$L" "$O" "$waited" >> "$P3A_OUT/q.tsv"
      return 3
    fi
    echo "$(date -u +%FT%TZ) $id wait: load=$L others=[$O]" >> "$P3A_OUT/q-wait.log"
    sleep 300; waited=$((waited+300))
  done
  local out="$P3A_WORK/runs/$id" log="$P3A_WORK/logs/$id.log"
  mkdir -p "$P3A_WORK/logs" "$P3A_OUT/$id"
  rm -rf "$out"
  local L0; L0=$(load1)
  local s; s=$(date +%s)
  local start; start=$(date -u +%FT%TZ)
  # load sampler (every 15 s) for the run's max
  ( while :; do load1 >> "$P3A_WORK/logs/$id.load"; sleep 15; done ) &
  local sampler=$!
  # shellcheck disable=SC2086  # $args is a flag list by construction
  "$HERE/run-rig.sh" "$tapef" --no-build --out "$out" $args > "$log" 2>&1
  local rc=$?
  kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null
  local L1; L1=$(load1)
  local Lmax; Lmax=$(sort -n "$P3A_WORK/logs/$id.load" 2>/dev/null | tail -1)
  local v; v=$(grep -m1 'verdict' "$out/diff.md" 2>/dev/null | sed 's/- verdict: //')
  printf '%s\t%s\t%s\t%s\t%s\t%ss\trc=%s\t%s\tload1 start=%s max=%s end=%s\twaited=%ss\n' \
    "$id" "$tape" "$dir" "$sig" "$start" "$(( $(date +%s) - s ))" "$rc" "${v:-no diff}" "$L0" "${Lmax:-?}" "$L1" "$waited" >> "$P3A_OUT/q.tsv"
  local f
  for f in diff.md diff.json play.json handoff.json; do [ -f "$out/$f" ] && cp "$out/$f" "$P3A_OUT/$id/"; done
  for f in a b; do
    mkdir -p "$P3A_OUT/$id/$f"
    for g in meta.json handoff-state.json; do [ -f "$out/$f/$g" ] && cp "$out/$f/$g" "$P3A_OUT/$id/$f/"; done
    [ -f "$out/$f/stderr.log" ] && grep -a '^\[rig\]' "$out/$f/stderr.log" > "$P3A_OUT/$id/$f/rig-lines.txt"
  done
  return "$rc"
}

case "${1:-}" in
  synth) synth ;;
  matrix) matrix ;;
  run|run-matrix)
    : "${P3A_OUT:?set P3A_OUT}" "${P3A_WORK:?set P3A_WORK}"
    mkdir -p "$P3A_OUT" "$P3A_WORK"
    caffeinate -dims -w $$ &
    LANES=()
    if [ "$1" = run-matrix ]; then
      while read -r l; do LANES+=("$l"); done < <(matrix)
    else
      shift; LANES=("$@")
    fi
    for l in "${LANES[@]}"; do
      run_lane "$l"; rc=$?
      [ "$rc" -eq 3 ] && { echo "stopped at $l (preflight timeout)" >> "$P3A_OUT/q.tsv"; exit 3; }
    done
    echo "lanes done $(date -u +%FT%TZ)" >> "$P3A_OUT/q.tsv" ;;
  *) sed -n '2,40p' "$0"; exit 2 ;;
esac
