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
#   python/ modem/ media/ system/   ONLY if present in the repo — Phase 0 ships
#                            what exists; they are vendored in Phase 4/5. The
#                            JS tarball (device-deploy-dev.yml:98) ships all
#                            four unconditionally; here a missing dir is not an
#                            error, a missing REQUIRED entry is.
# Required entries verified after packing: bin/xorgate-rs, build-info.json,
# systemd/xorgate-agent-rs.service.
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
MEMBERS=(bin build-info.json systemd)
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
fi
# (the `+` expansion keeps macOS bash 3.2 happy with an empty array under set -u)
tar -czf "$OUT_DIR/$TARBALL" ${TAR_OPTS[@]+"${TAR_OPTS[@]}"} -C "$STAGE" "${MEMBERS[@]}"

echo "Contents:" >&2
tar -tzf "$OUT_DIR/$TARBALL" | head -30 >&2
# Keys install-rs.sh / scripts/bench/install.sh extract and expect.
for required in bin/xorgate-rs build-info.json systemd/xorgate-agent-rs.service; do
  tar -tzf "$OUT_DIR/$TARBALL" | grep -qx "$required" || { echo "pack: missing $required in $TARBALL" >&2; exit 1; }
done

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
