#!/usr/bin/env bash
# hl.sh `release`, inside the amd64 build image: `cargo build --release` (the
# simulator image's x86_64 agent; NOT the device tarball, which is aarch64).
set -u
git config --global --add safe.directory '*' 2>/dev/null
# (the ONE-TREE touch runs on the host before this container: hl-job.sh touch_if_new_tree)
cargo build --release --locked || exit 1
O=/out/bins/release
rm -rf "$O" && mkdir -p "$O"
cp "$CARGO_TARGET_DIR/release/xorgate-rs" "$O/"
(cd "$O" && sha256sum xorgate-rs > SHA256SUMS)
printf '# cargo build --release, %s, %s\nhead=%s\ntree_fp=%s\ndirty_entries=%s\ndiff_vs_head_sha256=%s\ntag=%s\n' "$(uname -m)" "$(rustc -V)" \
  "${HL_HEAD:-unknown}" "${HL_TREE_FP:-unknown}" "${HL_DIRTY:-unknown}" "${HL_DIFFSHA:-unknown}" "${HL_TAG:-unknown}" > "$O/HEAD"
ldd "$O/xorgate-rs" | grep -E 'gstreamer|libc\.so' || true
ls -l "$O"
