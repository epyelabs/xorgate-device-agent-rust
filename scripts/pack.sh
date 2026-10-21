#!/usr/bin/env bash
# Pack the Rust agent into the dist tarball xorgate-agent-rs-<version>.tar.gz.
#
#   scripts/pack.sh [--channel <c> [--version <v>]] [--bin <path>] [--out-dir <dir>]
#
#   --channel  stamp build-info.json first (scripts/build-info.sh <c>); without
#              it an existing target/pack/build-info.json (from a previous
#              build-info.sh run — what the CI workflows do) is required.
#   --version  forwarded to build-info.sh (prod passes the release tag).
#   --bin      the release binary (default target/release/xorgate-rs, the
#              native CI / bench build; the Mac docker build lands at
#              target/docker/release/xorgate-rs — pass it explicitly).
#   --out-dir  where the tarball + .sha256 go (default target/dist).
#
# Tarball layout (= /opt/xorgate-rs on the device, README §6):
#   bin/xorgate-rs           the stripped release binary (0755)
#   build-info.json          {version, channel, builtAt, packageVersion}
#   systemd/                 xorgate-agent-rs.service (+ bench/ drop-in, which
#                            install-rs.sh never installs)
#   install-rs.sh            the converter (scripts/install-rs.sh, 0755; Phase 5
#                            WP1) — the device copy that makes --revert work
#                            offline and without a working binary
#   python/ modem/ media/ system/   ONLY if present in the repo — Phase 0 ships
#                            what exists; they are vendored in Phase 4/5. The
#                            JS tarball (device-deploy-dev.yml:98) ships all
#                            four unconditionally; here a missing dir is not an
#                            error, a missing REQUIRED entry is.
# Required entries verified after packing: bin/xorgate-rs, build-info.json,
# systemd/xorgate-agent-rs.service, install-rs.sh, python/bno085_read.py (Phase 2 WP2: the
# IMU collector spawns `python3 <root>/python/bno085_read.py`, vendored
# byte-identical from the JS repo @ 89b5c64, sha256 7af7a856…; the JS
# tarball ships python/ too, device-deploy-dev.yml:98).
#
# Sidecar: <tarball>.sha256 in `sha256sum -c` format ("<hex>  <basename>").
# Prints key=value lines (tarball, version, channel, builtAt, packageVersion,
# sha256, path) and appends them to $GITHUB_OUTPUT when set.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CHANNEL=""
VERSION=""
BIN="$REPO/target/release/xorgate-rs"
OUT_DIR="$REPO/target/dist"
while [ $# -gt 0 ]; do
  case "$1" in
    --channel) CHANNEL="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --bin) BIN="$2"; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

STAGE="$REPO/target/pack"
INFO="$STAGE/build-info.json"

if [ -n "$CHANNEL" ]; then
  if [ -n "$VERSION" ]; then
    "$REPO/scripts/build-info.sh" "$CHANNEL" --version "$VERSION" --out "$INFO"
  else
    "$REPO/scripts/build-info.sh" "$CHANNEL" --out "$INFO"
  fi
fi
[ -f "$INFO" ] || { echo "pack: $INFO missing — run scripts/build-info.sh <channel> or pass --channel" >&2; exit 1; }
[ -f "$BIN" ] || { echo "pack: binary $BIN missing — cargo build --release first (or --bin <path>)" >&2; exit 1; }
[ -f "$REPO/systemd/xorgate-agent-rs.service" ] || { echo "pack: systemd/xorgate-agent-rs.service missing" >&2; exit 1; }
[ -f "$REPO/python/bno085_read.py" ] || { echo "pack: python/bno085_read.py missing (the IMU helper)" >&2; exit 1; }
[ -f "$REPO/scripts/install-rs.sh" ] || { echo "pack: scripts/install-rs.sh missing (the converter)" >&2; exit 1; }

# Tiny JSON field reader (no jq on the bench): first "key": "value" occurrence.
json_str() { sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$1" | head -n1; }
VERSION=$(json_str "$INFO" version)
CHANNEL=$(json_str "$INFO" channel)
BUILT_AT=$(json_str "$INFO" builtAt)
PKG_VERSION=$(json_str "$INFO" packageVersion)
[ -n "$VERSION" ] && [ -n "$CHANNEL" ] || { echo "pack: $INFO lacks version/channel" >&2; exit 1; }

# Rebuild the staging tree from scratch (keep build-info.json).
find "$STAGE" -mindepth 1 -maxdepth 1 ! -name build-info.json -exec rm -rf {} +
mkdir -p "$STAGE/bin"
install -m 0755 "$BIN" "$STAGE/bin/xorgate-rs"
cp -R "$REPO/systemd" "$STAGE/systemd"
# Phase 5 WP1: the converter at the tarball root (= /opt/xorgate-rs/install-rs.sh).
install -m 0755 "$REPO/scripts/install-rs.sh" "$STAGE/install-rs.sh"
MEMBERS=(bin build-info.json systemd install-rs.sh)
for d in python modem media system; do
  if [ -d "$REPO/$d" ]; then
    cp -R "$REPO/$d" "$STAGE/$d"
    MEMBERS+=("$d")
  else
    echo "pack: no $d/ in the repo yet — not shipped (vendored in Phase 4/5)" >&2
  fi
done

TARBALL="xorgate-agent-rs-${VERSION}.tar.gz"
mkdir -p "$OUT_DIR"
TAR_OPTS=()
if tar --version 2>/dev/null | grep -q GNU; then
  TAR_OPTS=(--owner=0 --group=0 --numeric-owner)  # root-owned entries like the installer expects
elif tar --version 2>/dev/null | grep -q bsdtar; then
  # macOS bsdtar (WP4 note 9): root-owned entries, no Apple provenance xattrs
  # (GNU tar on the device warns on the unknown pax keywords).
  TAR_OPTS=(--uid 0 --gid 0 --no-xattrs --no-mac-metadata)
  export COPYFILE_DISABLE=1
fi
# (the `+` expansion keeps macOS bash 3.2 happy with an empty array under set -u)
tar -czf "$OUT_DIR/$TARBALL" ${TAR_OPTS[@]+"${TAR_OPTS[@]}"} -C "$STAGE" "${MEMBERS[@]}"

# List once, into a variable: piping `tar -tzf` into anything that stops
# reading early (`grep -q`, `head -30`) under pipefail fails spuriously — the
# reader exits and tar dies of SIGPIPE (rc 141). F-E1-1 (Phase 4 gate): the old
# `tar -tzf | head -30` aborted every pack once the tarball passed 30 entries
# (system/ vendored, 35). A real listing error still aborts here (set -e).
LISTING=$(tar -tzf "$OUT_DIR/$TARBALL")
echo "Contents ($(wc -l <<<"$LISTING" | tr -d ' ') entries; first 30):" >&2
sed -n '1,30p' <<<"$LISTING" >&2
# Keys install-rs.sh / scripts/bench/install.sh extract and expect.
for required in bin/xorgate-rs build-info.json systemd/xorgate-agent-rs.service python/bno085_read.py install-rs.sh; do
  grep -qx "$required" <<<"$LISTING" || { echo "pack: missing $required in $TARBALL" >&2; exit 1; }
done

# Phase 3 WP5: the vendored KVS runtime (media/, byte-identical to the JS repo
# @ 89b5c64, P4) must be packed, and the STAGED copy must verify against
# media/SHA256SUMS (= the JS repo's blobs) — a corrupted/LFS-pointer blob or a
# missing .so fails the pack instead of the first live view on a device.
# Phase 4 (WP6, WP3/WP4 hand-offs): the same for the vendored modem/ (WP4,
# CF62) and system/ (WP3: xorgate-camrotate, xorgate-setclock, the
# fake-hwclock cron) — both required (review-wp6 nit: a missing helper dir
# fails the pack).
verify_vendored() { # <dir> <required:1|0>
  local d=$1 req=$2 _sum name
  if [ ! -d "$STAGE/$d" ]; then
    [ "$req" = 1 ] && { echo "pack: $d/ missing (vendored)" >&2; exit 1; }
    return 0
  fi
  [ -f "$STAGE/$d/SHA256SUMS" ] || { echo "pack: $d/SHA256SUMS missing (vendored $d/)" >&2; exit 1; }
  while read -r _sum name; do
    grep -qx "$d/$name" <<<"$LISTING" || { echo "pack: missing $d/$name in $TARBALL" >&2; exit 1; }
  done < "$STAGE/$d/SHA256SUMS"
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$STAGE/$d" && sha256sum --quiet -c SHA256SUMS) >&2 || { echo "pack: $d/ does not match $d/SHA256SUMS" >&2; exit 1; }
  else
    (cd "$STAGE/$d" && shasum -a 256 --quiet -c SHA256SUMS) >&2 || { echo "pack: $d/ does not match $d/SHA256SUMS" >&2; exit 1; }
  fi
}
verify_vendored media 1
verify_vendored modem 1
verify_vendored system 1
# Phase 4 (WP4): the remote update rail's unit (the bench bootstrap and every
# `xorgate-rs update` install it from the tarball).
grep -qx "systemd/xorgate-updater-rs.service" <<<"$LISTING" || { echo "pack: missing systemd/xorgate-updater-rs.service in $TARBALL" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then
  SHA=$(cd "$OUT_DIR" && sha256sum "$TARBALL" | cut -d' ' -f1)
else
  SHA=$(cd "$OUT_DIR" && shasum -a 256 "$TARBALL" | cut -d' ' -f1)
fi
printf '%s  %s\n' "$SHA" "$TARBALL" > "$OUT_DIR/$TARBALL.sha256"

SIZE=$(wc -c < "$OUT_DIR/$TARBALL" | tr -d ' ')
BIN_SIZE=$(wc -c < "$STAGE/bin/xorgate-rs" | tr -d ' ')
echo "packed $OUT_DIR/$TARBALL ($SIZE bytes; binary $BIN_SIZE bytes; sha256 $SHA)" >&2

OUTPUTS=(
  "tarball=$TARBALL"
  "path=$OUT_DIR/$TARBALL"
  "version=$VERSION"
  "channel=$CHANNEL"
  "builtAt=$BUILT_AT"
  "packageVersion=$PKG_VERSION"
  "sha256=$SHA"
  "tarballBytes=$SIZE"
  "binaryBytes=$BIN_SIZE"
)
printf '%s\n' "${OUTPUTS[@]}"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  printf '%s\n' "${OUTPUTS[@]}" >> "$GITHUB_OUTPUT"
fi
