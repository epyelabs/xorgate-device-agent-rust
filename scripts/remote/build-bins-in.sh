#!/usr/bin/env bash
# hl.sh `build-bins`, inside the amd64 build image. The cargo and cc lines are
# tools/replay/build-bins.sh's, verbatim (scripts/tests/hl.sh checks they have
# not drifted); only the image (amd64) and the copy targets differ.
#   build-bins-in.sh [release|dev]
set -u
PROFILE=${1:-release}
TARGET_DIR=$CARGO_TARGET_DIR
OUTDIR=release; [ "$PROFILE" = dev ] && OUTDIR=debug
git config --global --add safe.directory '*' 2>/dev/null
# (the ONE-TREE touch runs on the host before this container: hl-job.sh touch_if_new_tree)
mkdir -p /src/tools/replay/.cache/bin
# shellcheck disable=SC2086  # verbatim from build-bins.sh
cargo build --locked --profile $PROFILE -p xorgate-agent -p xorgate-replay && cp $TARGET_DIR/$OUTDIR/xorgate-rs $TARGET_DIR/$OUTDIR/xorgate-replay /src/tools/replay/.cache/bin/ \
    && cc -shared -fPIC -O2 -Wall -Wextra -o /src/tools/replay/.cache/bin/rig-statvfs.so /src/tools/replay/statvfs-shim/rig_statvfs.c -ldl || exit 1
O=/out/bins
rm -rf "$O" && mkdir -p "$O"
cp /src/tools/replay/.cache/bin/xorgate-rs /src/tools/replay/.cache/bin/xorgate-replay /src/tools/replay/.cache/bin/rig-statvfs.so "$O/"
(cd "$O" && sha256sum xorgate-rs xorgate-replay rig-statvfs.so > SHA256SUMS)
# Provenance (review F6): the tree the bins come from, not only its HEAD.
{
  echo "# built by hl build-bins, profile $PROFILE, $(uname -m), $(rustc -V) (the binary reports unknown: no .git on the homelab)"
  echo "head=${HL_HEAD:-unknown}"
  echo "tree_fp=${HL_TREE_FP:-unknown}"
  echo "dirty_entries=${HL_DIRTY:-unknown}"
  echo "diff_vs_head_sha256=${HL_DIFFSHA:-unknown}"
  echo "tag=${HL_TAG:-unknown}"
} > "$O/HEAD"
printf 'tree_fp=%s\nvia=build-bins:%s\n' "${HL_TREE_FP:-unknown}" "${HL_TAG:-unknown}" > /src/tools/replay/.cache/bin/BUILT_FROM
ls -l "$O"
