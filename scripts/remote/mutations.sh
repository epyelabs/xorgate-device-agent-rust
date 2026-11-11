#!/usr/bin/env bash
# Mutation check of scripts/tests/hl.sh (review F9): each mutant changes ONE
# line of the helper's dangerous logic; the suite must go RED for every one.
# m1-m10 are the review's ten (re-expressed on the fixed code); m11-m20 pin the
# review fixes F1-F8. Run in a throwaway container like the suite itself:
#   docker run --rm -v "$PWD:/src:ro" -w /src debian:bookworm bash -c \
#     'apt-get update -qq && apt-get install -y -qq git python3 shellcheck procps >/dev/null; bash scripts/remote/mutations.sh'
#   MUTANTS="m4 m5" bash scripts/remote/mutations.sh    # a subset
# Exit 0 only when the baseline is GREEN and every mutant is RED.
set -uo pipefail
SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
# id % file % exact text (must occur once) % replacement % what it breaks  ('%' separates: texts hold '|')
TABLE=$(cat <<'EOF'
m1%hl-job.sh%ids=$(docker ps -aq --filter "label=xorgate.agent=$TAG")%ids=$(docker ps -aq --filter "label=xorgate.agent")%clean: container filter without =<tag>
m2%hl-job.sh%'BEGIN { exit !(u + c <= t) }'%'BEGIN { exit !(c <= t) }'%budget ignores held CPUs
m3%hl-job.sh%grep "^hl_${TAG}_" || true); do tmux kill-session%grep "^hl_${TAG}" || true); do tmux kill-session%clean: session prefix without _
m4%hl-job.sh%rm -f "$SHARED/res/${TAG}_"*%rm -f "$SHARED/res/${TAG}"*%clean: reservation prefix without _
m5%hl-job.sh%sort -u | grep "^hl_${TAG}_" || true); do%sort -u | grep "^hl_${TAG}" || true); do%clean: compose project prefix without _
m6%hl-job.sh%*) command tmux has-session -t "=hl_$(basename "$1")" 2>/dev/null ;;%*) true ;;%res_sum counts stale reservations
m7%hl-job.sh%cmd_clean() {\n  tagdir "$1"%cmd_clean() {\n  TAG=$1; T=$ROOT/$TAG; SRC=$T/src%clean without the .hl-tag check
m8%hl-job.sh%"$img" rm -rf "/target/$TAG"%"$img" rm -rf "/target/"%clean: rm -rf /target/
m9%hl-job.sh%[ $((avail - used_m - gb)) -ge%[ $((avail - gb)) -ge%memory guard ignores held memory
m10%hl-job.sh%  if [ "$fp" != "$last" ]; then%  if false; then%the touch never happens
m11%hl.sh%{ (cd "$w" && git check-ignore --no-index --stdin < "$tmp/extra.txt") || [ $? -eq 1 ]; }%(cd "$w" || exit 2; git check-ignore --no-index --stdin < "$tmp/extra.txt"; true)%F1: the pre-fix check-ignore (silent abort)
m12%hl-job.sh%  tag_verified || refuse%  true || refuse%F1: start ignores an unverified sync
m13%hl-job.sh%      total_c=$(awk -v a="$total_c" -v b="$HL_CAP_TOTAL_CPUS" 'BEGIN { print a + b }')%      :%F2: a malformed reservation counts as 0
m14%hl-job.sh%  echo "75" > "$T/logs/$NAME.rc"%  true%F5: a refusal writes no rc
m15%hl-job.sh%  OUTD=$T/out/lanes/${fp:0:12}-$binsha%  OUTD=$T/out/lanes/one%F6a: lane results not per tree
m16%hl-job.sh%  [ "$bfp" = "$fp" ] || die%  true || die%F6b: lanes take another tree's bins
m17%hl-job.sh% || { echo "== touch FAILED: target.fp not updated"; return 1; }% || true%F7: tree recorded although the touch failed
m18%hl-job.sh%  res_lock || refuse%  true || refuse%F3: no reservation lock
m19%hl.sh%if [ -n "$live" ] && [ "$3" != 1 ]; then%if false; then%F8: sync while a job of the tag runs
m20%hl-job.sh%kill -TERM -- "-$pg" 2>/dev/null%true%F4: stop does not TERM the rig's process group
EOF
)
setup() { # <dir>: the suite's inputs only
  mkdir -p "$1/scripts/tests" "$1/tools/replay"
  cp -R "$SRC/scripts/remote" "$1/scripts/remote"
  cp "$SRC/scripts/tests/hl.sh" "$1/scripts/tests/hl.sh"
  cp "$SRC/tools/replay/build-bins.sh" "$1/tools/replay/build-bins.sh"
}
run_suite() { # <dir> -> rc; first FAIL line in $W/first
  (cd "$1" && bash scripts/tests/hl.sh) > "$W/out" 2>&1
  local rc=$?
  grep -m1 '^FAIL' "$W/out" > "$W/first" || true
  return $rc
}
setup "$W/base"
if run_suite "$W/base"; then echo "baseline: GREEN ($(grep -c '^PASS' "$W/out") checks)"
else echo "baseline: RED — fix the suite first"; head -5 "$W/first"; exit 1; fi
red=0; total=0; bad=0
while IFS='%' read -r id file old new what; do
  [ -n "$id" ] || continue
  case " ${MUTANTS:-$id} " in *" $id "*) ;; *) continue ;; esac
  total=$((total + 1))
  d=$W/$id; setup "$d"
  if ! python3 -I - "$d/scripts/remote/$file" "$old" "$new" <<'PY'
import sys
p, old, new = sys.argv[1], sys.argv[2].replace("\\n", "\n"), sys.argv[3].replace("\\n", "\n")
s = open(p).read()
n = s.count(old)
if n != 1:
    sys.exit(f"mutation text found {n} times (need exactly 1)")
open(p, "w").write(s.replace(old, new))
PY
  then echo "$id NOT APPLIED ($what)"; bad=$((bad + 1)); continue; fi
  if run_suite "$d"; then echo "$id SURVIVES ($what)"; bad=$((bad + 1))
  else red=$((red + 1)); echo "$id RED ($what) — first: $(cat "$W/first")"; fi
  rm -rf "$d"
done <<<"$TABLE"
echo "== mutations: $red/$total RED"
[ "$bad" -eq 0 ] && [ "$red" -eq "$total" ]
