#!/usr/bin/env bash
# DA8 (Phase 8 WP-R1): provision-rs.sh steps 1-1c + P2 + the claim with REAL
# apt-get (Debian bookworm + the Raspberry Pi archive) and REAL pip, on arm64,
# no shims. Runs on the Mac (Apple Silicon: arm64 natively), <= 4 CPUs, alone
# (no other cargo job; handbook §3a). Two fresh containers:
#   A. NEEDED: before any apt the real binary does not load; after ONLY
#      provision-rs.sh's fatal set (APT_NEEDED), ldd resolves every NEEDED and
#      `bin/xorgate-rs -v` prints the banner;
#   B. the provisioner itself, up to the claim: real apt + pip, the smoke-exec
#      passes, the claim reaches a fake API (409 -> exit 5, so nothing past the
#      claim runs); `import adafruit_bno08x` works; then a second run under a
#      window-only APT::Get::Upgrade "false" (the bench mechanism).
#   tools/provision-e2e/da8-arm64.sh <real aarch64 xorgate-agent-rs-*.tar.gz>
# The tarball only donates its bin/xorgate-rs; the tested tarball is packed
# from THIS tree by scripts/pack.sh.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
SRC_TGZ=${1:?usage: da8-arm64.sh <real aarch64 tarball>}
WORK=$REPO/target/provision-da8
rm -rf "$WORK" && mkdir -p "$WORK/donor" "$WORK/in"
tar -xzf "$SRC_TGZ" -C "$WORK/donor" bin/xorgate-rs build-info.json
echo "== donor binary sha256 $(shasum -a 256 "$WORK/donor/bin/xorgate-rs" | cut -c1-64) ($(tr -d '\n' < "$WORK/donor/build-info.json"))"
"$REPO/scripts/pack.sh" --channel bench --version da8 --bin "$WORK/donor/bin/xorgate-rs" --out-dir "$WORK/in" > "$WORK/pack.out" 2> "$WORK/pack.err" \
  || { cat "$WORK/pack.err"; exit 1; }
grep -E '^(tarball|sha256)=' "$WORK/pack.out"
cp "$HERE/fake-dist-api.py" "$HERE/da8-in.sh" "$WORK/in/"
for c in A B; do
  t0=$(date +%s)
  docker run --rm --label xorgate.agent=p8-provision-rs --platform linux/arm64 --cpus 4 --memory 4g \
    -v "$WORK/in:/in:ro" debian:bookworm bash /in/da8-in.sh "$c" 2>&1 | tee "$WORK/da8-$c.log" | grep -E '^(PASS|FAIL|==|  )' || true
  echo "== container $c: $(( $(date +%s) - t0 )) s wall"
done
if grep -q '^FAIL' "$WORK"/da8-*.log; then echo "== DA8: FAILURES"; exit 1; fi
echo "== DA8: ALL PASS (logs $WORK/da8-*.log)"
