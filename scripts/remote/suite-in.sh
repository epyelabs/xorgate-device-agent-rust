#!/usr/bin/env bash
# hl.sh `suites`: ONE plain script suite, inside a fresh script-tests container
# (the source mounted read-only, copied without caches; the Mac gates'
# suites.sh recipe). First line = the summary hl-job.sh prints.
#   suite-in.sh <path of the suite in the tree>
t=$1
mkdir /work && cd /src && tar --exclude=./tools/replay/.cache --exclude=./tools/capture-host/.cache --exclude=./target -cf - . | tar -C /work -xf - && cd /work || exit 2
git config --global --add safe.directory "*" 2>/dev/null
# These two suites stub sudo themselves and must not find the real one.
case "$t" in scripts/bench/tests/p4-profile.sh|scripts/bench/tests/p7a-capture-host.sh) mv /usr/bin/sudo /usr/bin/sudo.real ;; esac
out=$(bash "$t" 2>&1); rc=$?
p=$(printf "%s\n" "$out" | grep -cE "^PASS|^ok |^  ok "); f=$(printf "%s\n" "$out" | grep -cE "^FAIL|^not ok")
echo "$t rc=$rc pass=$p fail=$f | $(printf "%s\n" "$out" | tail -1)"
echo "---- full output"
printf "%s\n" "$out"
exit $rc
