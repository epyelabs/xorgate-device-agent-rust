#!/usr/bin/env bash
# Stamp build-info.json for a Rust agent build.
#
#   scripts/build-info.sh <channel> [--version <v>] [--out <path>]
#
#   channel   dev   — CI build of main (version = short sha)
#             prod  — CI build of a GitHub Release (pass --version <tag>)
#             bench — hand build (version = short sha, "-dirty" on an unclean tree)
#             ci    — ci.yml artifact of a non-main branch (never published to S3)
#   --version override the version string (prod passes the release tag)
#   --out     where to write (default target/pack/build-info.json; `-` = stdout)
#
# Shape is the JS agent's dist/build-info.json, byte-compatible field set
# (device/xorgate-device-agent/.github/workflows/device-deploy-dev.yml:90-92 and
# scripts/stamp-build-info.mjs):
#   { "version": "<sha|tag>", "channel": "<c>", "builtAt": "<ISO-8601 Z>", "packageVersion": "<x.y.z>" }
# The agent reports it under effective.agent.build and the console compares
# `version` as a plain string against latest(-rs).json.
#
# packageVersion is the workspace version: [workspace.package].version in the
# root Cargo.toml, falling back to crates/xorgate-agent/Cargo.toml [package].
# No cargo/jq needed (runs on the bench and in a bare container).
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CHANNEL="${1:-}"
[ -n "$CHANNEL" ] || { echo "usage: $0 <dev|prod|bench|ci> [--version <v>] [--out <path>]" >&2; exit 2; }
shift
VERSION=""
OUT="$REPO/target/pack/build-info.json"
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
case "$CHANNEL" in
  dev|prod|bench|ci) ;;
  *) echo "warning: unusual channel '$CHANNEL' (expected dev|prod|bench|ci)" >&2 ;;
esac

git_q() { git -C "$REPO" "$@" 2>/dev/null || true; }

if [ -z "$VERSION" ]; then
  VERSION=$(git_q rev-parse --short HEAD)
  [ -n "$VERSION" ] || VERSION="unknown"
  # A bench build can never be mistaken for the CI artifact of the same sha.
  if [ "$CHANNEL" = "bench" ] && [ -n "$(git_q status --porcelain)" ]; then
    VERSION="${VERSION}-dirty"
  fi
fi
if [ "$CHANNEL" = "prod" ] && ! [[ "$VERSION" =~ ^v?[0-9] ]]; then
  echo "warning: prod channel with non-tag version '$VERSION' (pass --version <tag>)" >&2
fi

# Workspace version: first `version = "…"` inside [workspace.package]; else the
# agent crate's [package] version; else 0.0.0 (warn).
toml_version() {
  # $1 = file, $2 = section header (e.g. workspace.package)
  awk -v sec="[$2]" '
    /^[[:space:]]*\[/ { in_sec = ($0 ~ "^[[:space:]]*\\" sec "[[:space:]]*$") ? 1 : 0; next }
    in_sec && /^[[:space:]]*version[[:space:]]*=/ {
      line = $0; sub(/^[^"]*"/, "", line); sub(/".*$/, "", line); print line; exit
    }' "$1"
}
PKG_VERSION=""
[ -f "$REPO/Cargo.toml" ] && PKG_VERSION=$(toml_version "$REPO/Cargo.toml" "workspace.package")
if [ -z "$PKG_VERSION" ] && [ -f "$REPO/crates/xorgate-agent/Cargo.toml" ]; then
  PKG_VERSION=$(toml_version "$REPO/crates/xorgate-agent/Cargo.toml" "package")
fi
if [ -z "$PKG_VERSION" ]; then
  echo "warning: could not read a workspace version from Cargo.toml; stamping 0.0.0" >&2
  PKG_VERSION="0.0.0"
fi

BUILT_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
JSON=$(printf '{\n  "version": "%s",\n  "channel": "%s",\n  "builtAt": "%s",\n  "packageVersion": "%s"\n}\n' \
  "$VERSION" "$CHANNEL" "$BUILT_AT" "$PKG_VERSION")

if [ "$OUT" = "-" ]; then
  printf '%s' "$JSON"
else
  mkdir -p "$(dirname "$OUT")"
  printf '%s' "$JSON" > "$OUT"
  echo "stamped $OUT: $PKG_VERSION ($VERSION, $CHANNEL, $BUILT_AT)" >&2
fi

# Machine-readable outputs for pack.sh / GitHub Actions.
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    echo "version=$VERSION"
    echo "channel=$CHANNEL"
    echo "builtAt=$BUILT_AT"
    echo "packageVersion=$PKG_VERSION"
  } >> "$GITHUB_OUTPUT"
fi
