#!/usr/bin/env bash
# hl.sh `release`, inside the amd64 build image: `cargo build --release` (the
# simulator image's x86_64 agent; NOT the device tarball, which is aarch64).
set -u
git config --global --add safe.directory '*' 2>/dev/null
if [ "${HL_TOUCH:-0}" = 1 ]; then find crates -name '*.rs' -exec touch {} +; echo "== touched every crates/**/*.rs (new tree in this target dir)"; fi
cargo build --release || exit 1
O=/out/bins/release
rm -rf "$O" && mkdir -p "$O"
cp "$CARGO_TARGET_DIR/release/xorgate-rs" "$O/"
(cd "$O" && sha256sum xorgate-rs > SHA256SUMS)
echo "# built from git ${HL_HEAD:-unknown}, cargo build --release, $(uname -m), $(rustc -V)" > "$O/HEAD"
ldd "$O/xorgate-rs" | grep -E 'gstreamer|libc\.so' || true
ls -l "$O"
