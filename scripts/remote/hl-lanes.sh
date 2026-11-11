#!/usr/bin/env bash
# hl.sh `lanes`: replay-rig lanes, SERIAL within one job, on the homelab
# (started by hl-job.sh, which exports TAG T SRC + the rig env: COMPOSE_FILE
# with the amd64/caps override, RIG_IMAGE, RIG_JS_CACHE, JS_REF,
# XORGATE_AGENT_LABEL).
#   hl-lanes.sh <items file>    lines: `name tape mode [purge]`, # comments
#     mode cal  = JS@$JS_REF vs JS@$JS_REF (--calibrate)
#          jsrs = JS@$JS_REF (a) vs the tree's xorgate-rs (b)
#          rsrs = the tree's xorgate-rs on BOTH sides
#          ab   = xorgate-rs-a (a) vs xorgate-rs-b (b) (import-bins <tag> a|b)
#     purge = delete the run dir after its summary (bench tapes: O8)
# Before EACH run (HL_PREFLIGHT=each; `none` = the caller did one): homelab
# 1-min load <= HL_MAX_LOAD (and, with HL_WAIT_OTHERS=1, no other agent's
# labelled container; HL_PEERS = tags not counted as others); 5-min steps, 60 min
# max, then the job stops (exit 3). Rows: out/q.tsv; untolerated paths (no
# values): out/untol/<name>.txt.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/remote/hl-lib.sh
. "$HERE/hl-lib.sh"
items=$1
: "${TAG:?}" "${T:?}" "${SRC:?}"
HL_PREFLIGHT=${HL_PREFLIGHT:-each}; HL_WAIT_OTHERS=${HL_WAIT_OTHERS:-0}; HL_PEERS=${HL_PEERS:-}
HL_RUN_SECS=${HL_RUN_SECS:-2700}
HL_PREFLIGHT_STEP=${HL_PREFLIGHT_STEP:-300}; HL_PREFLIGHT_MAX=${HL_PREFLIGHT_MAX:-3600}
: "${HL_HELPER_IMAGE:?}"   # the helper's amd64 image (hl-job.sh exports the script-tests one)
R=$SRC/tools/replay; BIN=$R/.cache/bin; Q=$T/out/q.tsv; LG=$T/logs/lanes
mkdir -p "$LG" "$T/out/untol" "$T/out/runs"
[ -f "$Q" ] || printf 'name\ttape\tmode\tstart\tsecs\trc\tverdict\tuntol\ttol\tids\tload_start\tload_end\tload_max\twaited\tcaps\tothers\tcopy\n' > "$Q"
load1() { cut -d' ' -f1 /proc/loadavg; }
others() {
  docker ps --filter label=xorgate.agent --format '{{.Label "xorgate.agent"}}:{{.Names}}' \
    | awk -F: -v me="$TAG" -v peers=",${HL_PEERS}," '$1 != me && index(peers, "," $1 ",") == 0' | tr '\n' ';'
}
modes=$(grep -vE '^[[:space:]]*(#|$)' "$items" | awk '{print $3}' | sort -u | tr '\n' ' ')
case " $modes " in
  *" ab "*)
    [ "$modes" = "ab " ] || { echo "an items file with ab lanes may hold ONLY ab lanes (the wrapper replaces xorgate-rs)"; exit 2; }
    if [ ! -x "$BIN/xorgate-rs-a" ] || [ ! -x "$BIN/xorgate-rs-b" ]; then echo "ab needs .cache/bin/xorgate-rs-a and -b (import-bins <tag> a|b)"; exit 2; fi
    # The lane.sh wrapper (phase-7e f-p2-round): the side is the agent's cert.
    cat > "$BIN/xorgate-rs" <<'Y'
#!/bin/bash
# hl-lanes.sh ab wrapper: side a = xorgate-rs-a, side b = xorgate-rs-b (CERT_PATH is …/a.crt or …/b.crt)
case "${CERT_PATH:-}" in
  */a.crt) exec -a /rig/cache/bin/xorgate-rs /rig/cache/bin/xorgate-rs-a "$@" ;;
  *)       exec -a /rig/cache/bin/xorgate-rs /rig/cache/bin/xorgate-rs-b "$@" ;;
esac
Y
    chmod +x "$BIN/xorgate-rs" ;;
  *) head -c 4 "$BIN/xorgate-rs" | grep -q ELF || { echo ".cache/bin/xorgate-rs is not an ELF binary (an ab wrapper left over?): import-bins again"; exit 2; } ;;
esac
while read -r name tape mode purge _; do
  case "$name" in ''|'#'*) continue ;; esac
  [[ $name =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || { echo "bad lane name $name"; exit 2; }
  case "$mode" in
    cal) args=(--calibrate) ;;
    jsrs) args=(--a js --b rust) ;;
    rsrs|ab) args=(--a rust --b rust) ;;
    *) echo "bad mode $mode"; exit 2 ;;
  esac
  tf=$R/.cache/tapes/$tape.ndjson.gz
  [ -f "$tf" ] || { echo "no tape $tf"; exit 2; }
  out=$T/out/runs/$name
  if [ -f "$out/diff.md" ] || grep -q "^$name	" "$Q"; then echo "$name: done already"; continue; fi
  waited=0
  while [ "$HL_PREFLIGHT" = each ]; do
    l=$(load1); o=$(others)
    if awk -v l="$l" -v m="$HL_MAX_LOAD" 'BEGIN { exit !(l <= m) }' && { [ "$HL_WAIT_OTHERS" != 1 ] || [ -z "$o" ]; }; then break; fi
    echo "$(date -u +%FT%TZ) $name not quiet: load1=$l others=[$o]" >> "$LG/q-wait.log"
    if [ "$waited" -ge "$HL_PREFLIGHT_MAX" ]; then printf '%s\t%s\t%s\tSKIPPED\twaited=%s\n' "$name" "$tape" "$mode" "$waited" >> "$Q"; exit 3; fi
    sleep "$HL_PREFLIGHT_STEP"; waited=$((waited + HL_PREFLIGHT_STEP))
  done
  export COMPOSE_PROJECT_NAME
  COMPOSE_PROJECT_NAME=hl_${TAG}_$(printf '%s' "$name" | tr '[:upper:].' '[:lower:]-' | tr -cd 'a-z0-9_-')
  o0=$(others); l0=$(load1); start=$(date -u +%FT%TZ); s=$(date +%s)
  # Sampler: max 1-min load every 15 s, and the rig container's real caps once.
  ( mx=$l0; capd=""
    while :; do
      sleep 15
      l=$(load1); mx=$(awk -v a="$mx" -v b="$l" 'BEGIN { print (b > a) ? b : a }')
      echo "$mx" > "$LG/$name.loadmax"
      if [ -z "$capd" ]; then
        c=$(docker ps -q --filter "label=com.docker.compose.project=$COMPOSE_PROJECT_NAME" --filter label=com.docker.compose.service=rig | head -1)
        [ -n "$c" ] && { docker inspect -f '{{.HostConfig.NanoCpus}}/{{.HostConfig.Memory}}' "$c" > "$LG/$name.caps"; capd=1; }
      fi
    done ) &
  smp=$!
  timeout --kill-after=60 "$HL_RUN_SECS" "$R/run-rig.sh" "$tf" --no-build --out "$out" "${args[@]}" > "$LG/$name.log" 2>&1
  rc=$?
  kill "$smp" 2>/dev/null; wait "$smp" 2>/dev/null
  # Linux host: the rig writes some run files as root with mode 0600, so
  # run-rig.sh's own `cp -R` into --out (as the ai user) stops part-way (the
  # diff itself ran as root inside the rig, on the complete run). Copy the
  # run again as root in a labelled container, hand it to the ai user and
  # delete the .cache/runs copy (which the ai user cannot delete either).
  run=$(grep -o '→ \.cache/runs/[^ ]*' "$LG/$name.log" | head -1 | sed 's#^→ \.cache/runs/##')
  copy=ok
  if grep -q "^cp: " "$LG/$name.log"; then copy=rescued; fi
  if [ -n "$run" ] && [ -d "$R/.cache/runs/$run" ]; then
    docker run --rm --platform linux/amd64 --label "xorgate.agent=$TAG" -v "$R/.cache/runs:/r" -v "$T/out/runs:/o" "$HL_HELPER_IMAGE" \
      sh -c 'mkdir -p "/o/$1" && cp -a "/r/$2/." "/o/$1/" && chown -R "$3" "/o/$1" && rm -rf "/r/$2"' _ "$name" "$run" "$(id -u):$(id -g)" \
      || copy=FAILED
  fi
  # run-rig.sh writes bins.sha256 after its copy; a stopped copy skipped it.
  [ -f "$out/bins.sha256" ] || { echo "# written by hl-lanes.sh (run-rig.sh's copy stopped): the lane's binaries"; cat "$T/out/lanes-bins.sha256"; } > "$out/bins.sha256" 2>/dev/null
  # The diff's own exit code, when run-rig.sh got to print it.
  drc=$(sed -n 's/^>> diff: .* (exit \([0-9]*\))$/\1/p' "$LG/$name.log" | tail -1)
  if [ -f "$out/diff.json" ]; then
    row=$(python3 -I "$HERE/hl-summ.py" row "$out")
    python3 -I "$HERE/hl-summ.py" untol "$out" > "$T/out/untol/$name.txt"
  else row=$(printf 'NO-DIFF\t-\t-\t-'); fi
  printf '%s\t%s\t%s\t%s\t%ss\trc=%s%s\t%s\t%s\t%s\t%s\t%ss\t%s\t%s\t%s\n' "$name" "$tape" "$mode" "$start" "$(( $(date +%s) - s ))" "$rc" "${drc:+/diff=$drc}" \
    "$row" "$l0" "$(load1)" "$(cat "$LG/$name.loadmax" 2>/dev/null || echo "$l0")" "$waited" "$(cat "$LG/$name.caps" 2>/dev/null || echo '?')" "[$o0]" "$copy" >> "$Q"
  tail -1 "$Q" | cut -f1-10
  if [ "${purge:-}" = purge ]; then rm -rf "${out:?}"; echo "$name: run dir purged (O8)"; fi
done < "$items"
echo LANES_DONE
