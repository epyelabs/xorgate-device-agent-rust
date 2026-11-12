#!/usr/bin/env bash
# shellcheck disable=SC2015  # test assertions: `a && ok || bad`
# review-wp3 F1 (phase 7c WP3 fix round): the TEST-ONLY `imu-test-bus` feature
# of xorgate-agent (the IMU reader's socket test transport) must never reach a
# shipped build. Checks, in the build image (needs cargo; the source stays
# read-only, CARGO_TARGET_DIR is the caller's):
#   1. `cargo tree -e normal,build,features -i xorgate-agent` for both shipped
#      targets (aarch64 = the device tarball, x86_64 = the simulator image)
#      shows NO `imu-test-bus` (only dev edges may enable it);
#   2. the positive control: with dev edges the method DOES see it;
#   3. the compile-time guard: a RELEASE check with the feature fails with the
#      `compile_error!` text.
#   docker run --rm --label xorgate.agent=<tag> -v "$PWD:/src:ro" \
#     -v xorgate-rs-cargo-registry:/usr/local/cargo/registry -v xorgate-rs-cargo-git:/usr/local/cargo/git \
#     -v xorgate-rs-target:/target -e CARGO_TARGET_DIR=/target/<branch> -w /src \
#     xorgate-rs-build:1.99.0-c813d06bc873 bash scripts/tests/cargo/feature-gate.sh
# Lives under scripts/tests/cargo/ so the plain-suite discovery
# (`scripts/tests/*.sh`, script-tests image, no cargo) does not pick it up.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
W=$(mktemp -d)
for t in aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  if cargo tree --locked -e normal,build,features -i xorgate-agent --target "$t" > "$W/tree-$t.txt" 2> "$W/tree-$t.err"; then
    n=$(grep -c 'imu-test-bus' "$W/tree-$t.txt")
    [ "$n" = 0 ] && ok "$t: no normal/build edge enables imu-test-bus" || { bad "$t: imu-test-bus on a normal/build edge ($n lines):"; grep 'imu-test-bus' "$W/tree-$t.txt" | head -5; }
  else
    bad "$t: cargo tree failed"; cat "$W/tree-$t.err"
  fi
done
cargo tree --locked -e normal,build,dev,features -i xorgate-agent > "$W/tree-dev.txt" 2>&1
[ "$(grep -c 'imu-test-bus' "$W/tree-dev.txt")" -ge 1 ] && ok "control: the dev edge (self dev-dependency) is seen" || bad "control: the dev-edge feature is not visible (the check above would be vacuous)"
if cargo check --locked --release -p xorgate-agent --lib --features imu-test-bus > "$W/check.log" 2>&1; then
  bad "a RELEASE build with imu-test-bus compiled (the compile_error! guard is gone)"
else
  grep -q 'the `imu-test-bus` feature is TEST-ONLY' "$W/check.log" && ok "a release build with imu-test-bus is refused by compile_error!" || { bad "the release check failed, but not with the guard's text:"; tail -20 "$W/check.log"; }
fi
echo "== result: $([ $fail = 0 ] && echo ALL PASS || echo FAILURES)"
exit $fail
