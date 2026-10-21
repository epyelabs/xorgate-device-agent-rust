#!/bin/bash
# I3 manifest of a device's install surface (phase-5 wp-specific-briefs.md
# §Interfaces I3), written INDEPENDENTLY of install-rs.sh (WP2 never reads
# WP1's implementation; the two are compared, never shared).
#
#   manifest.sh [--expand] [--root <prefix>] [<path>…]
#
# Default paths = the I3 roots. One line per path, sorted by path in the C
# locale:
#   f <mode> <uid>:<gid> <sha256> <path>
#   d <mode> <uid>:<gid> - <path>
#   l <mode> <uid>:<gid> -><target> <path>
#   t <digest> <tree>            (/opt/xorgate, /opt/xorgate-rs, .new, .old)
# <mode> = 4 octal digits (stat %a, zero-padded). A tree digest = sha256 of
# that tree's own sorted f/d/l lines (the tree's root dir included), so
# node_modules costs one line. --expand prints the per-file lines of the trees
# instead of the `t` lines (to locate a tree difference).
#
# --root <prefix>: read the surface under <prefix> but print the device paths
# (`<prefix>/opt/x` → `/opt/x`). The e2e uses it to digest an extracted
# tarball as if it sat at /opt/xorgate-rs (`--root /tmp/x /opt/xorgate-rs`).
#
# Read-only by construction: find + stat + sha256sum + readlink, nothing
# else (G6 runs it on the bench as `ssh pi@… sudo bash -s < manifest.sh`).
set -euo pipefail
export LC_ALL=C

EXPAND=0
PREFIX=""
PATHS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --expand) EXPAND=1; shift ;;
    --root) PREFIX=${2%/}; shift 2 ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) PATHS+=("$1"); shift ;;
  esac
done

ROOTS=(/etc/xorgate /usr/local/bin /etc/systemd/system /etc/sudoers.d /etc/cron.d
  /etc/udev/rules.d /etc/NetworkManager/conf.d /etc/systemd/logind.conf.d
  /var/lib/xorgate-rs-install)
TREES=(/opt/xorgate /opt/xorgate-rs /opt/xorgate-rs.new /opt/xorgate-rs.old)
[ ${#PATHS[@]} -gt 0 ] || PATHS=("${ROOTS[@]}" "${TREES[@]}")

# entries <device path>: the f/d/l lines of everything at and under it
# (no -xdev: a tree is what it contains). Directories are not followed through
# symlinks (find default -P).
entries() {
  local real="$PREFIX$1" typ mode own tgt path p h f
  [ -e "$real" ] || [ -L "$real" ] || return 0
  # Hash every regular file in ONE sha256sum pass (-z: NUL-terminated, no
  # name escaping), then walk the metadata. Records are NUL-terminated with
  # \037 between fields (not a tab: IFS whitespace would merge the empty
  # link-target field), so a space in a name cannot shift a column.
  local -A sum=()
  while IFS= read -r -d '' h; do
    sum["${h:66}"]=${h:0:64}
  done < <(find "$real" -type f -print0 | xargs -0 -r sha256sum -z)
  while IFS=$'\037' read -r -d '' typ mode own tgt path; do
    p=${path#"$PREFIX"}
    printf -v mode '%04d' "$mode"
    case "$typ" in
      f) f=${sum["$path"]:-}
         [ -n "$f" ] || f=$(sha256sum <"$path" | cut -d' ' -f1)
         printf 'f %s %s %s %s\n' "$mode" "$own" "$f" "$p" ;;
      d) printf 'd %s %s - %s\n' "$mode" "$own" "$p" ;;
      l) printf 'l %s %s ->%s %s\n' "$mode" "$own" "$tgt" "$p" ;;
      *) printf '%s %s %s - %s\n' "$typ" "$mode" "$own" "$p" ;;
    esac
  done < <(find "$real" -printf '%y\037%m\037%U:%G\037%l\037%p\0')
}

# Sort key = the path (last field; paths with spaces keep their tail).
by_path() { awk '{ n = index($0, " /"); print substr($0, n + 1) "\t" $0 }' | sort -t$'\t' -k1,1 | cut -f2-; }

{
  for p in "${PATHS[@]}"; do
    is_tree=0
    for t in "${TREES[@]}"; do [ "$p" = "$t" ] && is_tree=1; done
    if [ $is_tree = 1 ] && [ $EXPAND = 0 ]; then
      [ -e "$PREFIX$p" ] || continue
      printf 't %s %s\n' "$(entries "$p" | by_path | sha256sum | cut -d' ' -f1)" "$p"
    else
      entries "$p"
    fi
  done
} | by_path
