#!/usr/bin/env bash
# C11 CLI matrix: run the SAME invocation list through two CLIs in scratch
# homes and diff stdout / stderr (ANSI stripped), exit code and the resulting
# settings.json bytes after every step. Runs INSIDE the rig container:
#
#   docker compose -f tools/replay/docker-compose.yml run --rm -T --entrypoint bash rig \
#     /rig/tools/cli-matrix.sh [--a js|rust] [--b js|rust] [--out /rig/out/cli-matrix-<ts>]
#
# Default A=js (`node dist/cli.js` @ 89b5c64), B=rust (`xorgate-rs`). Each side
# runs the list top to bottom in ONE home (settings accumulate like on a
# device). Steps run as the unprivileged `node` user unless marked `root`
# (root against a node-owned home = the "you don't need sudo" note). Rows
# tagged `expect:` differ BY DESIGN (O9 logs, D20/T8 build identity, Phase 4
# wifi) and are reported separately. Exit 1 on any other difference.
set -uo pipefail
A=js; B=rust; OUT="/rig/out/cli-matrix-$(date -u +%Y%m%dT%H%M%SZ)"
while [ $# -gt 0 ]; do
  case "$1" in --a) A=$2; shift 2 ;; --b) B=$2; shift 2 ;; --out) OUT=$2; shift 2 ;; *) echo "unknown $1" >&2; exit 2 ;; esac
done
JS_CLI="node /rig/cache/js-89b5c64/dist/cli.js"
RS_CLI="/rig/cache/bin/xorgate-rs"
cli_for() { case "$1" in js) echo "$JS_CLI" ;; rust) echo "$RS_CLI" ;; esac; }

# who | expect | argv…   (expect: "-" = must match)
CASES=$(cat <<'LIST'
node|-|set
node|-|set nonsense 1
node|-|set media off
node|-|set mode sim --no-restart
node|-|set mode bogus
node|-|set gps-antenna active
node|-|set gps-antenna sideways
node|-|set gps-fix-reset-policy veto-deadzone
node|-|set deadzone-profile aggressive
node|-|set deadzone-profile wild
node|-|set recording.telemetry.enabled on
node|-|set recording.telemetry.rateHz 10
node|-|set recording.telemetry.rateHz 999
node|-|set recording.telemetry.rateHz abc
node|-|set recording.video.enabled off
node|-|set recording.video.segmentSeconds 5
node|-|set recording.video.cameras.cam0 off
node|-|set recording.upload.mode auto
node|-|set recording.retention.maxBytes 1000000
node|-|set recording.nope.field 1
node|-|set timeSync.stepThresholdSecs 15
node|-|set timeSync.stepThresholdSecs -3
node|-|set timeSync.offlineSources false
node|-|set timeSync.maxStepsPerBoot 2
node|-|set timeSync.bogus 1
node|-|set cameraMount.cam0.rotationDeg 180
node|-|set cameraMount.cam1.rotationDeg 90
node|-|set apn synth.apn --no-wait
node|-|set apn "" --no-wait
root|-|set timeSync.stepThresholdSecs 12
node|-|status
node|expect:O9 logs falls back to journalctl without agent.log|logs -n 5
node|expect:T8/D20 build identity|-v
node|expect:T8 set wifi is Phase 4 (CF22)|set wifi on
LIST
)

run_side() { # label kind
  local label=$1 cli home i=0
  cli=$(cli_for "$2")
  home=/tmp/cli-matrix/$label
  rm -rf "$home"; mkdir -p "$home" "$OUT/$label"; chown node:node "$home"
  while IFS='|' read -r who expect argv; do
    [ -n "$who" ] || continue
    i=$((i + 1))
    local pre=(env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=/tmp TZ=UTC NO_COLOR=1 XORGATE_HOME="$home" LTE_TELEMETRY_FILE=/nonexistent/telemetry.json)
    local cmd="$cli $argv"
    if [ "$who" = node ]; then
      setpriv --reuid=1000 --regid=1000 --init-groups "${pre[@]}" bash -c "$cmd" >"$OUT/$label/$i.out" 2>"$OUT/$label/$i.err" </dev/null
    else
      "${pre[@]}" bash -c "$cmd" >"$OUT/$label/$i.out" 2>"$OUT/$label/$i.err" </dev/null
    fi
    echo $? > "$OUT/$label/$i.code"
    cp "$home/settings.json" "$OUT/$label/$i.settings" 2>/dev/null || echo "<absent>" > "$OUT/$label/$i.settings"
    printf '%s\n' "$expect|$who|$argv" > "$OUT/$label/$i.case"
  done <<<"$CASES"
}

strip() { sed -e 's/\x1b\[[0-9;]*[A-Za-z]//g' "$1"; }
run_side a "$A"
run_side b "$B"
n=$(find "$OUT/a" -name "*.case" | wc -l); fail=0; expected=0
{
  echo "# CLI matrix — A ($A) vs B ($B)"; echo
  echo "| # | who | argv | exit A/B | stdout | stderr | settings.json | verdict |"
  echo "|---|---|---|---|---|---|---|---|"
} > "$OUT/cli-matrix.md"
for i in $(seq 1 "$n"); do
  IFS='|' read -r expect who argv < "$OUT/a/$i.case"
  ca=$(cat "$OUT/a/$i.code"); cb=$(cat "$OUT/b/$i.code")
  so=same; se=same; st=same
  diff -q <(strip "$OUT/a/$i.out") <(strip "$OUT/b/$i.out") >/dev/null || so=DIFF
  diff -q <(strip "$OUT/a/$i.err") <(strip "$OUT/b/$i.err") >/dev/null || se=DIFF
  cmp -s "$OUT/a/$i.settings" "$OUT/b/$i.settings" || st=DIFF
  verdict=ok
  if [ "$ca" != "$cb" ] || [ $so = DIFF ] || [ $se = DIFF ] || [ $st = DIFF ]; then
    if [ "$expect" = "-" ]; then verdict=FAIL; fail=$((fail + 1)); else verdict="by design (${expect#expect:})"; expected=$((expected + 1)); fi
  fi
  echo "| $i | $who | \`$argv\` | $ca/$cb | $so | $se | $st | $verdict |" >> "$OUT/cli-matrix.md"
done
{
  echo; echo "**$((n - fail - expected)) identical, $expected different by design, $fail FAIL** (of $n)."
  echo; echo "Per-step files: \`$OUT/{a,b}/<n>.{out,err,code,settings}\`."
} >> "$OUT/cli-matrix.md"
cat "$OUT/cli-matrix.md"
[ "$fail" -eq 0 ]
