#!/usr/bin/env bash
# Mutation check of scripts/tests/hl.sh: each mutant changes ONE place of the
# helper's dangerous logic (an exact text that must occur exactly once); the
# suite must go RED for every one.
#   m1-m20   the review's ten (re-expressed on the fixed code) + one per F1-F8 fix;
#   n1-n10   the fix-check reviewer's mutants (re-expressed where the code moved);
#   m21-m37  one per fix-check fix (M1-M4, H2, S1-S3, S5, S7, S9) + M5 (tmux env bleed).
# Run it in a throwaway Linux container like the suite itself (it needs GNU
# userland, procps and, for m21, tmux; the helper's script-tests image has all):
#   docker run --rm -v "$PWD:/src:ro" -w /src <script-tests image> bash scripts/remote/mutations.sh
#   MUTANTS="m4 n6" bash scripts/remote/mutations.sh    # a subset
# It parses under bash 3.2 (`bash -n` on the Mac) but runs only on Linux.
# Exit 0 only when the baseline is GREEN and every mutant is RED; a mutant
# listed in EXPECTED_SURVIVORS is reported with its reason instead.
set -uo pipefail
case "$(uname -s)" in Linux) ;; *) echo "mutations.sh runs on Linux only (a throwaway container: see the header)"; exit 2 ;; esac
SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
# Mutants that cannot go red BY DESIGN, with the reason (reported, not failed).
EXPECTED_SURVIVORS="n1"
why_survivor() {
  case "$1" in
    n1) echo "equivalent since fix-check S1: wait and status accept an rc only with the CURRENT run id (<name>.rcid = <name>.runid), so an old .rc left by the missing pre-guard rm can no longer be reported" ;;
  esac
}
# id <TAB> file (under scripts/remote) <TAB> exact text (\n = newline) <TAB> replacement <TAB> what it breaks
# (TAB-separated: the texts hold '|', '%', '~' and '@')
table() {
  cat <<'EOF'
m1	hl-job.sh	ids=$(docker ps -aq --filter "label=xorgate.agent=$TAG" --filter label=xorgate.hl=1)	ids=$(docker ps -aq --filter "label=xorgate.agent" --filter label=xorgate.hl=1)	clean: container filter without =<tag>
m2	hl-job.sh	'BEGIN { exit !(u + c <= t) }'	'BEGIN { exit !(c <= t) }'	budget ignores held CPUs
m3	hl-job.sh	grep "^hl_${TAG}_" || true); do tmux kill-session	grep "^hl_${TAG}" || true); do tmux kill-session	clean: session prefix without _
m4	hl-job.sh	rm -f "$SHARED/res/${TAG}_"*	rm -f "$SHARED/res/${TAG}"*	clean: reservation prefix without _
m5	hl-job.sh	sort -u | grep "^hl_${TAG}_" || true); do	sort -u | grep "^hl_${TAG}" || true); do	clean: compose project prefix without _
m6	hl-job.sh	*) command tmux has-session -t "=hl_$(basename "$1")" 2>/dev/null ;;	*) true ;;	res_sum counts stale reservations
m7	hl-job.sh	cmd_clean() {\n  tagdir "$1"	cmd_clean() {\n  TAG=$1; T=$ROOT/$TAG; SRC=$T/src	clean without the .hl-tag check
m8	hl-job.sh	"$img" rm -rf "/target/$TAG"	"$img" rm -rf "/target/"	clean: rm -rf /target/
m9	hl-job.sh	[ $((avail - used_m - gb)) -ge	[ $((avail - gb)) -ge	memory guard ignores held memory
m10	hl-job.sh	  if [ "$fp" != "$last" ]; then	  if false; then	the touch never happens
m11	hl.sh	{ (cd "$w" && git check-ignore --no-index --stdin < "$tmp/extra.txt") || [ $? -eq 1 ]; }	(cd "$w" || exit 2; git check-ignore --no-index --stdin < "$tmp/extra.txt"; true)	F1: the pre-fix check-ignore (silent abort)
m12	hl-job.sh	  tag_verified || refuse "the last sync of $TAG did not complete and verify (checked under the lock)	  true || refuse "the last sync of $TAG did not complete and verify (checked under the lock)	F1/M4-P1: start skips the verified check under the lock
m13	hl-job.sh	      total_c=$(awk -v a="$total_c" -v b="$HL_CAP_TOTAL_CPUS" 'BEGIN { print a + b }')	      :	F2: a malformed reservation counts as 0
m14	hl-job.sh	  echo "75" > "$T/logs/$NAME.rc"	  true	F5: a refusal writes no rc
m15	hl-job.sh	  OUTD=$T/out/lanes/${fp:0:12}-$binsha	  OUTD=$T/out/lanes/one	F6a: lane results not per tree
m16	hl-job.sh	  [ "$bfp" = "$fp" ] || die	  true || die	F6b: lanes take another tree's bins
m17	hl-job.sh	 || { echo "== touch FAILED: target.fp not updated"; return 1; }	 || true	F7: tree recorded although the touch failed
m18	hl-job.sh	  res_lock || refuse	  true || refuse	F3: no reservation lock
m19	hl.sh	if [ -n "$live" ] && [ "$3" != 1 ]; then	if false; then	F8: sync while a job of the tag runs
m20	hl-job.sh	kill -TERM -- "-$pg" 2>/dev/null	true	F4: stop does not TERM the rig's process group
n1	hl-job.sh	rm -f "$T/logs/$NAME.rc" "$T/logs/$NAME.end" "$T/logs/$NAME.refused" "$T/logs/$NAME.stopped" "$T/logs/$NAME.rcid"	rm -f "$T/logs/$NAME.refused"	F5: a STARTED job leaves the previous .rc visible until it ends
n2	hl.sh	rm -f "$T/verified.fp" "$T/source.txt.new" "$T/ignored.txt"	rm -f "$T/source.txt.new" "$T/ignored.txt"	F1: sync prep keeps the old verified marker
n3	hl-job.sh	grep -qx "tree_fp=$fp" "$T/source.txt.new" || die	true || die	F1: verify-src installs a record whose tree_fp != the manifest
n4	hl-job.sh	head -c 4 xorgate-rs | grep -q ELF && sha256sum xorgate-rs	true	F6: lanes dir key ignores xorgate-rs
n5	hl-job.sh	[ "$bfp" = "$(cat "$ROOT/$from/verified.fp" 2>/dev/null)" ] || die	true || die	F6: import-bins takes bins of the source's OLD tree
n6	hl-job.sh	if (q == root) print pg[p]	print pg[p]	F4/M3: stop TERMs EVERY timeout process group on the host
n7	hl-job.sh	filt=(--filter "label=xorgate.agent=$TAG" --filter label=xorgate.hl=1)	filt=(--filter "label=xorgate.agent" --filter label=xorgate.hl=1)	F4/M3: stop removes every tag's containers
n8	hl-job.sh	sessions=$(command tmux ls -F '#{session_name}' 2>/dev/null | grep "^hl_${TAG}_" || true)	sessions=$(command tmux ls -F '#{session_name}' 2>/dev/null | grep "^hl_${TAG}" || true)	F4/M3: stop <tag> also kills hl_<tag>1_* / hl_<tag>-x_* sessions
n9	hl.sh	for a in "$@"; do q="$q $(printf '%q' "$a")"; done	for a in "$@"; do q="$q $a"; done	F12: run args reach the remote shell unquoted
n10	hl-job.sh	(\ pid:[0-9]+)?$ ]]; }	(\ pid:[0-9]+)? ]]; }	F2 (control): reservation regex loses its end anchor
m21	hl-job.sh	 > $(printf '%q' "$T/logs/$NAME.log") 2>&1" 9>&-; then	 > $(printf '%q' "$T/logs/$NAME.log") 2>&1"; then	M1: the tmux server inherits the reservation lock fd
m22	hl-job.sh	    rm -f -- "${SRC:?}/$f" || die "could not remove stale $f from $TAG/src (root-owned?): sync FAILED"\n    [ ! -e "$SRC/$f" ] || die "stale $f is still in $TAG/src: sync FAILED"	    rm -f -- "${SRC:?}/$f" || true	M4/P2: a prune that fails is ignored
m23	hl-job.sh	  [ -z "$extra" ] || die	  true || die	M4/P2: verify-src accepts extra files
m24	hl.sh	flock -w 60 9 || { echo "hl: could not take the reservation lock: refusing to sync" >&2; exit 3; }	true	M4/P1: sync prep without the reservation lock
m25	hl-job.sh	  if [ "$kind" = lto ] && [ $((n_cargo + n_lto)) -gt 0 ]; then	  if false; then	H2: a fat-LTO build beside other cargo jobs
m26	hl-job.sh	      [ "$sfp" = "$(cat "$ROOT/$from/verified.fp" 2>/dev/null)" ] || die "ab lane	      true || die "ab lane	M2: ab side from the source's OLD tree
m27	hl-job.sh	 "$B/BUILT_FROM" \\n       "$B/xorgate-rs-a" "$B/xorgate-rs-b" "$B/BUILT_FROM.a" "$B/BUILT_FROM.b"; then	 "$B/BUILT_FROM"; then	M2: build-bins keeps superseded side bins
m28	hl-lib.sh	[[ ${m%g} =~ ^[0-9]+$ ]] && [ "${m%g}" -gt 0 ] && {	[[ ${m%g} =~ ^[0-9]+$ ]] && {	S2: --memory 0g (no limit) admitted
m29	hl-job.sh	  [ -z "$want" ] || filt+=(--filter "label=xorgate.hl.job=$want")	  true	S3: stop <tag> <name> removes the tag's other jobs' containers
m30	hl-lanes.sh	      sleep "$HL_SAMPLE_SECS"\n      sample_overlap "$name"	      sleep "$HL_SAMPLE_SECS"	S9: overlap sampled only at the run's start/end
m31	hl.sh	      if [ -z "$rid" ] || [ "$rcid" = "$rid" ]; then	      if true; then	S1: wait accepts an rc of another run
m32	hl.sh	then set_state refused-local "$*"; fi; exit 2; }	then :; fi; exit 2; }	S1: a run refused on the Mac is not recorded
m33	clock-guard.py	    if INDIRECT_WORD.match(w[0]):	    if False:	S5: indirect exec through a variable not refused
m34	hl-job.sh	  ids=$(docker ps -aq --filter "label=xorgate.agent=$TAG" --filter label=xorgate.hl=1)	  ids=$(docker ps -aq --filter "label=xorgate.agent=$TAG")	S7: clean removes hand-rolled containers of the tag's name
m35	hl-job.sh	  for v2 in $allhl; do envs="$envs -u $v2"; clean+=(-u "$v2"); done	  for v2 in $allhl; do clean+=(-u "$v2"); done	M5: the job command does not unset foreign HL_* knobs
m36	hl-job.sh	  if [ -f "$exp" ] && ! cmp -s "$exp" "$T/logs/$name.hlenv"; then	  if false; then	M5: exec runs with an unexpected HL_* environment
m37	hl-job.sh	  if ! env "${clean[@]}" tmux new-session	  if ! tmux new-session	M5: the tmux client (and a server it spawns) keeps HL_* in its env
m38	hl-job.sh	'$1 == g && $3 !~ /^Z/ {	'$1 == g {	stop counts a zombie-only group as alive (waits, then KILLs)
m39	hl-job.sh	  if [ "$kind" = lto ] && [ "${legacy:--}" != - ]; then	  if false; then	H2 ruling: a fat-LTO build beside a no-kind (legacy) reservation
m40	hl-job.sh	      *) legacy="${legacy:+$legacy,}$(basename "$f")" ;;	      *) ;;	H2 ruling: a no-kind reservation is not counted as a possible cargo job
m41	hl-job.sh	  echo "$cpus $gb kind:$kind" > "$SHARED/res/${TAG}_$NAME"	  if [ "$kind" = none ]; then echo "$cpus $gb"; else echo "$cpus $gb kind:$kind"; fi > "$SHARED/res/${TAG}_$NAME"	H2 ruling: a non-cargo job writes no kind (would look legacy)
EOF
}
setup() { # <dir>: the suite's inputs only
  mkdir -p "$1/scripts/tests" "$1/tools/replay"
  cp -R "$SRC/scripts/remote" "$1/scripts/remote"
  cp "$SRC/scripts/tests/hl.sh" "$SRC/scripts/tests/unit-ordering.sh" "$SRC/scripts/tests/install-rs-systemd.sh" "$1/scripts/tests/" 2>/dev/null
  cp "$SRC/tools/replay/build-bins.sh" "$1/tools/replay/build-bins.sh"
}
run_suite() { # <dir> -> rc; first FAIL line in $W/first
  (cd "$1" && bash scripts/tests/hl.sh) > "$W/out" 2>&1 < /dev/null
  local rc=$?
  grep -m1 '^FAIL' "$W/out" > "$W/first" || true
  return $rc
}
setup "$W/base"
if run_suite "$W/base"; then echo "baseline: GREEN ($(grep -c '^PASS' "$W/out") checks)"
else echo "baseline: RED — fix the suite first"; head -5 "$W/first"; exit 1; fi
red=0; total=0; bad=0; expected=0
while IFS=$'\t' read -r -u 3 id file old new what; do
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
  if run_suite "$d"; then
    case " $EXPECTED_SURVIVORS " in
      *" $id "*) expected=$((expected + 1)); echo "$id SURVIVES as expected ($what): $(why_survivor "$id")" ;;
      *) echo "$id SURVIVES ($what)"; bad=$((bad + 1)) ;;
    esac
  else red=$((red + 1)); echo "$id RED ($what) — first: $(cat "$W/first")"; fi
  rm -rf "$d"
done 3< <(table)
echo "== mutations: $red/$total RED${expected:+, $expected expected survivor(s)}"
[ "$bad" -eq 0 ] && [ $((red + expected)) -eq "$total" ]
