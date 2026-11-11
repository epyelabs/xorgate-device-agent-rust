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
if [ "${HL_TOUCH:-0}" = 1 ]; then find crates -name '*.rs' -exec touch {} +; echo "== touched every crates/**/*.rs (new tree in this target dir)"; fi
mkdir -p /src/tools/replay/.cache/bin
# shellcheck disable=SC2086  # verbatim from build-bins.sh
cargo build --profile $PROFILE -p xorgate-agent -p xorgate-replay && cp $TARGET_DIR/$OUTDIR/xorgate-rs $TARGET_DIR/$OUTDIR/xorgate-replay /src/tools/replay/.cache/bin/ \
    && cc -shared -fPIC -O2 -Wall -Wextra -o /src/tools/replay/.cache/bin/rig-statvfs.so /src/tools/replay/statvfs-shim/rig_statvfs.c -ldl || exit 1
O=/out/bins
rm -rf "$O" && mkdir -p "$O"
cp /src/tools/replay/.cache/bin/xorgate-rs /src/tools/replay/.cache/bin/xorgate-replay /src/tools/replay/.cache/bin/rig-statvfs.so "$O/"
(cd "$O" && sha256sum xorgate-rs xorgate-replay rig-statvfs.so > SHA256SUMS)
echo "# built from git ${HL_HEAD:-unknown} (the binary reports unknown: no .git on the homelab), profile $PROFILE, $(uname -m), $(rustc -V)" > "$O/HEAD"
ls -l "$O"
