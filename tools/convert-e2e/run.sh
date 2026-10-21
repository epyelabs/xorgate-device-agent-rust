#!/usr/bin/env bash
# G2 (phase-5-distribution.md §11): install-rs.sh end-to-end over a simulated
# JS-provisioned device, in Docker, against a LOCAL fake dist bucket. Never
# touches a real bucket or the bench. Runs on the Mac (any arm64 Docker host).
#
#   tools/convert-e2e/run.sh [case…]        # default: every case
#
#   E2E_BIN=<path>     REQUIRED: the shared aarch64 release xorgate-rs (DISK
#                      BUDGET rule 1: this harness never builds one)
#   INSTALL_RS=<path>  the converter under test (default scripts/install-rs.sh)
#   JS_REF=<ref>       the JS repo ref for /opt/xorgate + the JS units/assets
#                      (default 89b5c64; read with `git archive`, never the
#                      working tree)
#   JS_REPO=<dir>      default ../xorgate-device-agent
#   DOCS_MD=<path>     the runbook whose bootstrap one-liner is run verbatim
#                      (default docs/convert.md, if present)
#   BODIES_JSON=<path> WP1's golden bodies (default fixtures/install_rs/bodies.json)
#
# Steps: (1) inputs into target/convert-e2e/in (install-rs.sh, the JS tree,
# an x86_64 ELF); (2) two REAL tarballs v1 + v2 from scripts/pack.sh
# (channel dev); (3) the image xorgate-convert-e2e:bookworm (rebuilt in place,
# dangling layers pruned); (4) the bucket (prep-bucket.sh); (5) one fresh
# `--rm --init` container per case (case.sh; SYS_PTRACE so root may read
# /proc/<pid>/exe|environ of the agent running as pi). Results on stdout and in
# target/convert-e2e/run-<ts>.log; exit 1 if any case fails.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
WORK="$REPO/target/convert-e2e"
IMAGE=xorgate-convert-e2e:bookworm
JS_REF=${JS_REF:-89b5c64}
JS_REPO=${JS_REPO:-$(cd "$REPO/.." && pwd)/xorgate-device-agent}
INSTALL_RS=${INSTALL_RS:-$REPO/scripts/install-rs.sh}
DOCS_MD=${DOCS_MD:-$REPO/docs/convert.md}
ALL_CASES=(convert-happy convert-twice convert-repair revert-keep revert-purge revert-twice
  convert-revert-convert update-after-convert js-pin bad-sha missing-sidecar missing-member
  wrong-arch failed-start failed-start-js-disabled failed-start-js-stopped
  wrapper-path-is-dir rollback-step6 rollback-step6-preexisting
  clean-fail-step8-reload clean-fail-step8-enable-rs clean-fail-step8-disable-js clean-fail-step8-start-rs
  signal-convert-hup-8 signal-convert-term-8 signal-convert-hup-9 signal-convert-term-9
  signal-convert-int-9 signal-revert-hup signal-revert-term signal-revert-int
  n7-refusal lock-stale lock-concurrent
  powercut-convert-1 powercut-convert-2 powercut-convert-3 powercut-convert-4 powercut-convert-5 powercut-convert-6 powercut-convert-7 powercut-convert-8 powercut-convert-9 powercut-convert-10 powercut-convert-11 powercut-convert-12 powercut-convert-13 powercut-convert-14
  powercut-revert-1 powercut-revert-2 powercut-revert-3 powercut-revert-4 powercut-revert-5 powercut-revert-6 powercut-revert-7 powercut-revert-8 powercut-revert-9 powercut-revert-10 powercut-revert-11 powercut-revert-12
  bench-window-rollback
  interrupt-1 interrupt-2 interrupt-3 interrupt-4 interrupt-5 interrupt-6 interrupt-7 interrupt-8
  interrupt-1-revert interrupt-2-revert interrupt-3-revert interrupt-4-revert
  interrupt-5-revert interrupt-6-revert interrupt-7-revert interrupt-8-revert
  root-svc root-svc-dropin js-disabled-before js-stopped-before not-root not-provisioned
  foreign-dropin bench-window status-nonroot revert-runtime-drift bootstrap-oneliner offline-revert tarball-mode visudo-refuses usage-status)
CASES=("$@")
[ ${#CASES[@]} -gt 0 ] || CASES=("${ALL_CASES[@]}")

mkdir -p "$WORK"
LOG="$WORK/run-$(date -u +%Y%m%dT%H%M%SZ).log"
exec > >(tee "$LOG") 2>&1
echo "== convert-e2e $(date -u +%FT%TZ) repo $(git -C "$REPO" rev-parse --short HEAD) cases ${#CASES[@]}"

if [ -z "${E2E_BIN:-}" ] || [ ! -f "$E2E_BIN" ]; then echo "E2E_BIN=<aarch64 xorgate-rs> is required (DISK BUDGET rule 1)" >&2; exit 2; fi
[ -f "$INSTALL_RS" ] || { echo "INSTALL_RS $INSTALL_RS missing" >&2; exit 2; }

echo "== 0. DISK BUDGET preconditions"
free_gb=$(df -g /System/Volumes/Data 2>/dev/null | awk 'NR == 2 {print $4}' || true)
[ -n "$free_gb" ] || free_gb=$(df -BG --output=avail "$WORK" | tail -n1 | tr -dc 0-9)
echo "free: ${free_gb} GB"
[ "$free_gb" -ge 100 ] || { echo "STOP: < 100 GB free (DISK BUDGET rule 4)" >&2; exit 2; }
wait_slot() { while [ "$(docker ps --format '{{.Image}}' | wc -l | tr -d ' ')" -ge 3 ]; do echo "   (3 docker jobs running; waiting)"; sleep 20; done; }

echo "== 1. inputs (INSTALL_RS=$INSTALL_RS, JS_REF=$JS_REF)"
rm -rf "$WORK/in" && mkdir -p "$WORK/in/js"
install -m 0755 "$INSTALL_RS" "$WORK/in/install-rs.sh"
echo "install-rs.sh sha256 $(shasum -a 256 "$WORK/in/install-rs.sh" 2>/dev/null | cut -d' ' -f1 || sha256sum "$WORK/in/install-rs.sh" | cut -d' ' -f1)"
git -C "$JS_REPO" archive "$JS_REF" package.json package-lock.json systemd python modem media system | tar -x -C "$WORK/in/js"
[ -z "$(git -C "$JS_REPO" status --porcelain)" ] || echo "WARNING: $JS_REPO working tree is not clean (not ours; read via git archive only)"
if [ -f "$DOCS_MD" ]; then cp "$DOCS_MD" "$WORK/in/convert.md"; echo "docs: $DOCS_MD"; else echo "docs: none ($DOCS_MD absent)"; fi
BODIES_JSON=${BODIES_JSON:-$REPO/fixtures/install_rs/bodies.json}
if [ -f "$BODIES_JSON" ]; then cp "$BODIES_JSON" "$WORK/in/bodies.json"; echo "golden: $BODIES_JSON"; else echo "golden: none"; fi
X86="$WORK/x86_64-true"
if [ ! -s "$X86" ]; then wait_slot; docker run --rm --label xorgate.agent=p5-e2e --platform linux/amd64 debian:bookworm cat /bin/true >"$X86"; fi
cp "$X86" "$WORK/in/x86_64-true"

echo "== 2. dist tarballs v1 + v2 (scripts/pack.sh --bin \$E2E_BIN)"
for v in v1 v2; do
  "$REPO/scripts/pack.sh" --channel dev --version "$v" --bin "$E2E_BIN" --out-dir "$WORK/in" >"$WORK/pack-$v.out" 2>"$WORK/pack-$v.err" ||
    { cat "$WORK/pack-$v.err"; exit 1; }
  grep -E '^(tarball|sha256)=' "$WORK/pack-$v.out"
done

echo "== 3. image $IMAGE"
wait_slot
docker build --platform linux/arm64 -q -t "$IMAGE" "$HERE" >/dev/null
docker image prune -f >/dev/null

echo "== 4. bucket"
rm -rf "$WORK/bucket" && mkdir -p "$WORK/bucket"
wait_slot
docker run --rm --label xorgate.agent=p5-e2e --platform linux/arm64 -v "$WORK/in:/in:ro" -v "$WORK/bucket:/bucket" \
  -v "$HERE:/e2e:ro" "$IMAGE" /e2e/prep-bucket.sh

echo "== 5. cases"
pass=0
fail=0
failed=()
for c in "${CASES[@]}"; do
  wait_slot
  rm -rf "$WORK/cases/$c" && mkdir -p "$WORK/cases/$c"
  if docker run --rm --label xorgate.agent=p5-e2e --init --cap-add SYS_PTRACE --platform linux/arm64 -v "$WORK/cases/$c:/out" \
    -v "$WORK/bucket:/bucket:ro" -v "$HERE:/e2e:ro" -v "$WORK/in:/in:ro" \
    -v "$REPO/fixtures:/fixtures:ro" "$IMAGE" /e2e/case.sh "$c"; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    failed+=("$c")
  fi
done
echo "== G2 SUMMARY: ${pass} case(s) PASS, ${fail} FAIL ${failed[*]+(${failed[*]})}"
echo "== log: $LOG"
[ "$fail" = 0 ]
