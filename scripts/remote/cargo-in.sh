#!/usr/bin/env bash
# hl.sh `cargo ARGS`, inside the amd64 build image: any cargo command line on the synced tree.
set -u
git config --global --add safe.directory '*' 2>/dev/null
echo "== HEAD ${HL_HEAD:-unknown} | $(rustc -V) | $(uname -m) | cargo $*"
# (the ONE-TREE touch runs on the host before this container: hl-job.sh touch_if_new_tree)
exec cargo "$@"
