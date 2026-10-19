#!/usr/bin/env bash
# E9 (phase-4-operations.md §4): the `xorgate-rs update` rail end-to-end in
# Docker against a LOCAL fake dist bucket. Never touches a real bucket or the
# bench. Runs on the Mac (or any arm64 Docker host).
#
#   tools/update-e2e/run.sh [case…]        # default: every case
#
#   E2E_BIN=<path>   the aarch64 release binary (default: build it in the
#                    xorgate-rs-build image, CARGO_TARGET_DIR=/target/p4-update-e2e)
#
# Steps: (1) the Docker-built aarch64 `xorgate-rs`; (2) two REAL dist tarballs
# v1 + v2 from scripts/pack.sh (channel bench); (3) the e2e image
# (debian:bookworm + curl/tar/visudo/python3 + a systemctl shim); (4) the
# bucket variants (prep-bucket.sh: latest alias, + system/, bad sha, missing
# sidecar, wrong sidecar name, missing member, x86_64 binary, truncated binary,
# not-a-tarball); (5) one fresh container per case (case.sh). Results go to
# stdout; exit 1 if any case fails.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
WORK="$REPO/target/update-e2e"
JS_REPO="${JS_REPO:-$(cd "$REPO/.." && pwd)/xorgate-device-agent}"
JS_COMMIT=89b5c64
IMAGE=xorgate-update-e2e:bookworm
BUILD_IMAGE=xorgate-rs-build:1.99.0-c813d06bc873
ALL_CASES=(happy-coexist rust-only rust-only-root-no-modem rust-only-no-user restart-fails
  visudo-refuses bad-sha missing-sidecar sidecar-wrong-name missing-member wrong-arch
  truncated-binary not-a-tarball download-404 no-bucket not-root ro-systemd ro-sudoers old-busy uid501-tarball)
CASES=("$@")
[ ${#CASES[@]} -gt 0 ] || CASES=("${ALL_CASES[@]}")

mkdir -p "$WORK/in" "$WORK/bucket"

echo "== 1. aarch64 binary"
BIN="${E2E_BIN:-$WORK/xorgate-rs}"
if [ -z "${E2E_BIN:-}" ]; then
  docker run --rm --platform linux/arm64 -v "$REPO:/src" \
    -v xorgate-rs-cargo-registry:/usr/local/cargo/registry -v xorgate-rs-cargo-git:/usr/local/cargo/git \
    -v xorgate-rs-target:/target -e CARGO_TARGET_DIR=/target/p4-update-e2e -e CARGO_BUILD_JOBS=3 -w /src \
    "$BUILD_IMAGE" sh -c 'cargo build --release -p xorgate-agent 2>&1 | tail -2 && cp /target/p4-update-e2e/release/xorgate-rs /src/target/update-e2e/xorgate-rs'
fi
file "$BIN" 2>/dev/null || true

echo "== 2. dist tarballs v1 + v2 (scripts/pack.sh)"
for v in v1 v2; do
  "$REPO/scripts/pack.sh" --channel bench --version "$v" --bin "$BIN" --out-dir "$WORK/in" >"$WORK/pack-$v.out"
  grep -E '^(tarball|sha256)=' "$WORK/pack-$v.out"
done
echo "-- pack.sh listing (v2):"
tar -tzf "$WORK/in/xorgate-agent-rs-v2.tar.gz" | grep -vE '^media/.+' | sort

echo "== 3. JS system/ @ $JS_COMMIT (read-only git archive) + an x86_64 ELF"
rm -rf "$WORK/in/jsys" && mkdir -p "$WORK/in/jsys"
git -C "$JS_REPO" archive "$JS_COMMIT" system | tar -x -C "$WORK/in/jsys"
if [ ! -s "$WORK/in/x86_64-true" ]; then
  docker run --rm --platform linux/amd64 debian:bookworm cat /bin/true >"$WORK/in/x86_64-true"
fi
file "$WORK/in/x86_64-true" 2>/dev/null || true

echo "== 4. e2e image + bucket"
docker build --platform linux/arm64 -q -t "$IMAGE" "$HERE" >/dev/null
rm -rf "$WORK/bucket" && mkdir -p "$WORK/bucket"
docker run --rm --platform linux/arm64 -v "$WORK/in:/in:ro" -v "$WORK/bucket:/bucket" \
  -v "$HERE:/e2e:ro" "$IMAGE" /e2e/prep-bucket.sh

echo "== 5. cases"
pass=0
fail=0
failed=()
for c in "${CASES[@]}"; do
  extra=()
  case "$c" in ro-* | old-busy) extra=(--privileged) ;; esac
  if docker run --rm --platform linux/arm64 ${extra[@]+"${extra[@]}"} \
    -v "$WORK/bucket:/bucket:ro" -v "$HERE:/e2e:ro" -v "$REPO/fixtures:/fixtures:ro" \
    -v "$WORK/in/jsys:/in/jsys:ro" "$IMAGE" /e2e/case.sh "$c"; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    failed+=("$c")
  fi
done
echo "== E9 SUMMARY: ${pass} case(s) PASS, ${fail} FAIL ${failed[*]+(${failed[*]})}"
[ "$fail" = 0 ]
