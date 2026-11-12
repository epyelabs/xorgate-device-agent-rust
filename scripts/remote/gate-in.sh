#!/usr/bin/env bash
# hl.sh `cargo-gate` / `test`, INSIDE the amd64 build image (cwd /src = the
# synced tree, CARGO_TARGET_DIR=/target/<tag>). The same steps as the Mac
# gates' gate.sh (phase-7e f-p2-round): fmt, clippy -D warnings, test
# --workspace, cargo-deny, clippy-ban-probe. Logs: /logs/<job>/.
#   gate-in.sh full|test
set -u
mode=${1:-full}
L=/logs/${HL_JOB_NAME:-cargo-gate}
mkdir -p "$L"
git config --global --add safe.directory '*' 2>/dev/null
echo "== HEAD ${HL_HEAD:-unknown} mode=$mode | $(rustc -V) | $(uname -m) | nproc $(nproc) | jobs ${CARGO_BUILD_JOBS:-?} | target $CARGO_TARGET_DIR"
# (the ONE-TREE touch runs on the host before this container: hl-job.sh touch_if_new_tree)
rc=0
step() { # <name> <cmd...>: run, log, report
  local n=$1; shift
  local t0=$SECONDS
  "$@" > "$L/$n.log" 2>&1
  local r=$?
  echo "$n rc=$r ($((SECONDS - t0)) s)"
  [ $r -eq 0 ] || rc=1
  return $r
}
if [ "$mode" = full ]; then
  step fmt cargo fmt --all -- --check
  step clippy cargo clippy --locked --workspace --all-targets -- -D warnings
  echo "   clippy warning/error lines: $(grep -c '^warning\|^error' "$L/clippy.log")"
fi
step test cargo test --locked --workspace --no-fail-fast
grep -E '^test result:' "$L/test.log" | awk '{p+=$4; f+=$6; i+=$8; n++} END {print "   binaries", n, "passed", p, "failed", f, "ignored", i}'
grep -E '^test .* \.\.\. FAILED$|^    [a-z_:]+$' "$L/test.log" | sed -n '1,40p' | sed 's/^/   /'
if [ "$mode" = full ]; then
  if [ -x /deny/cargo-deny ]; then
    step deny /deny/cargo-deny --all-features --locked check
    tail -2 "$L/deny.log" | sed 's/^/   /'
  else echo "deny SKIPPED: no cargo-deny binary (run the images job)"; rc=1; fi
  step ban-probe bash scripts/tests/clippy-ban-probe.sh
  tail -1 "$L/ban-probe.log" | sed 's/^/   /'
fi
echo "GATE_DONE rc=$rc"
exit $rc
