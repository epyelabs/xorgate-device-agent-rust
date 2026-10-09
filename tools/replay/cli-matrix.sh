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
# tagged `expect:<rule>/<row>` differ BY DESIGN and cite a row of
# tolerances.toml (T8 logs-journal / cli-banner / set-wifi / set-mode-running;
# the script refuses an id the table lacks); for those only the settings.json
# bytes must still match. Exit 1 on any other difference.
#
#   --fresh   instead of the cumulative list below, run tools/replay/cli/cases.txt
#             through cli/e2e-set-run.sh (WP6's per-case runner: every case from
#             cli/seed.json in a fresh home; @root / @running tags) for both sides.
set -uo pipefail
A=js; B=rust; OUT="/rig/out/cli-matrix-$(date -u +%Y%m%dT%H%M%SZ)"; FRESH=0
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TOL="$HERE/tolerances.toml"
while [ $# -gt 0 ]; do
  case "$1" in --a) A=$2; shift 2 ;; --b) B=$2; shift 2 ;; --out) OUT=$2; shift 2 ;; --fresh) FRESH=1; shift ;; *) echo "unknown $1" >&2; exit 2 ;; esac
done
# An `expect:` id must be a row of the tolerance table (never a silent widening).
known_row() { # T8/<row>
  local row=${1#*/}
  [ "${1%%/*}" = T8 ] && grep -q "^  id = \"$row\"" "$TOL"
}
JS_CLI="node /rig/cache/js-89b5c64/dist/cli.js"
RS_CLI="${RS_CLI:-/rig/cache/bin/xorgate-rs}"   # override: a branch build (e.g. WP6 before merge)
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
node|expect:T8/logs-journal|logs -n 5
node|expect:T8/cli-banner|-v
node|expect:T8/cli-banner|help
node|expect:T8/cli-banner|
node|expect:T8/cli-banner|frobnicate
node|expect:T8/set-wifi|set wifi on
node|expect:T8/cli-systemd-verbs|start
node|expect:T8/cli-systemd-verbs|stop
node|expect:T8/cli-systemd-verbs|restart
node|expect:T8/cli-later-verbs|update
node|expect:T8/cli-later-verbs|pipeline
node|expect:T8/cli-later-verbs|recording
node|expect:T8/cli-later-verbs|led
node|expect:T8/cli-later-verbs|sms
node|expect:T8/cli-later-verbs|manage
node|expect:T8/cli-later-verbs|monitor
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
    local cmd="timeout 20 $cli $argv"
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
if [ "$FRESH" -eq 1 ]; then
  # WP6's per-case layout <n>/{case,out,err,code,settings} → this script's flat one.
  for side in a b; do
    kind=$A; [ $side = b ] && kind=$B
    # Homes on the container's own fs: the bind-mounted Mac dir (virtiofs)
    # emulates ownership and once lost a just-renamed settings.json to `cp`.
    RUNNER=$(cli_for "$kind") "$HERE/cli/e2e-set-run.sh" "/tmp/cli-fresh/$side" >/dev/null 2>&1
    rm -rf "$OUT/$side.cases"; cp -R "/tmp/cli-fresh/$side" "$OUT/$side.cases"
    mkdir -p "$OUT/$side"
    for d in "/tmp/cli-fresh/$side"/*/; do
      k=$(basename "$d")
      for f in out err code settings; do cp "$d/$f" "$OUT/$side/$k.$f"; done
      # case line is expect|who|argv-after-set
      IFS='|' read -r e w argv < "$d/case"; printf '%s|%s|set %s\n' "$e" "$w" "$argv" > "$OUT/$side/$k.case"
    done
  done
else
  run_side a "$A"
  run_side b "$B"
fi
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
    id=${expect#expect:}
    if [ "$expect" = "-" ]; then verdict=FAIL; fail=$((fail + 1))
    elif ! known_row "$id"; then verdict="FAIL (expect id $id not in tolerances.toml)"; fail=$((fail + 1))
    elif [ $st = DIFF ]; then verdict="FAIL ($id allows output/exit, not settings.json)"; fail=$((fail + 1))
    else verdict="by design ($id)"; expected=$((expected + 1)); fi
  fi
  echo "| $i | $who | \`$argv\` | $ca/$cb | $so | $se | $st | $verdict |" >> "$OUT/cli-matrix.md"
done
{
  echo; echo "**$((n - fail - expected)) identical, $expected different by design, $fail FAIL** (of $n)."
  echo; echo "Per-step files: \`$OUT/{a,b}/<n>.{out,err,code,settings}\`."
} >> "$OUT/cli-matrix.md"
cat "$OUT/cli-matrix.md"
[ "$fail" -eq 0 ]
