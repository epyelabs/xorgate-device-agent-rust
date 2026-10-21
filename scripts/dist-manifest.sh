#!/usr/bin/env bash
# Print latest-rs.json for one dist publish (phase-5-distribution.md §6, I5).
#
#   scripts/dist-manifest.sh --version <v> --channel <dev|prod> --built-at <t> \
#                            --package-version <p> --tarball <name>
#
# The bytes are FROZEN (Q7): exactly what the deploy workflows' heredoc wrote
# up to Rust main b3dad3c (device-rs-deploy-{dev,prod}.yml, "Upload to S3"):
#
#   { "version": "<v>", "channel": "<c>", "builtAt": "<t>",
#     "packageVersion": "<p>", "tarball": "<name>" }
#
# (two lines, the second indented two spaces, trailing newline). Five string
# fields — the JS latest.json field set plus `tarball` — so a future
# `GET /device-dist/latest-rs` can reuse the console's comparison. Adding a
# field (e.g. sha256) is refused by this plan: the checksum is the sidecar.
# scripts/tests/dist.sh pins the bytes against that heredoc.
#
# Refuses (exit 2, nothing on stdout): a missing/empty argument; a channel
# other than dev|prod; a tarball not matching ^xorgate-agent-rs-.+\.tar\.gz$ or
# not equal to xorgate-agent-rs-<version>.tar.gz (the versioned key the same
# run uploads); any value carrying a character that would need JSON escaping
# (", \, control characters) — the frozen bytes have no escaping.
set -euo pipefail

usage() { sed -n '2,23p' "$0" >&2; exit 2; }
die() { echo "dist-manifest: $*" >&2; exit 2; }

VERSION="" CHANNEL="" BUILT_AT="" PKG_VERSION="" TARBALL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --version|--channel|--built-at|--package-version|--tarball)
      [ $# -ge 2 ] || die "$1 needs a value"
      case "$1" in
        --version) VERSION=$2 ;;
        --channel) CHANNEL=$2 ;;
        --built-at) BUILT_AT=$2 ;;
        --package-version) PKG_VERSION=$2 ;;
        --tarball) TARBALL=$2 ;;
      esac
      shift 2 ;;
    -h|--help) usage ;;
    *) die "unknown argument: $1" ;;
  esac
done

for pair in "version:$VERSION" "channel:$CHANNEL" "built-at:$BUILT_AT" \
            "package-version:$PKG_VERSION" "tarball:$TARBALL"; do
  [ -n "${pair#*:}" ] || die "--${pair%%:*} is required (non-empty)"
  # Printable ASCII/UTF-8 only, no JSON metacharacters.
  case "${pair#*:}" in
    *\"*|*\\*|*[[:cntrl:]]*) die "--${pair%%:*} carries a character that needs JSON escaping: ${pair#*:}" ;;
  esac
done
case "$CHANNEL" in dev|prod) ;; *) die "--channel must be dev or prod, got '$CHANNEL'" ;; esac
[[ "$TARBALL" =~ ^xorgate-agent-rs-.+\.tar\.gz$ ]] || die "--tarball '$TARBALL' does not match ^xorgate-agent-rs-.+\\.tar\\.gz\$"
[ "$TARBALL" = "xorgate-agent-rs-$VERSION.tar.gz" ] || die "--tarball '$TARBALL' is not the versioned key xorgate-agent-rs-$VERSION.tar.gz"

printf '{ "version": "%s", "channel": "%s", "builtAt": "%s",\n  "packageVersion": "%s", "tarball": "%s" }\n' \
  "$VERSION" "$CHANNEL" "$BUILT_AT" "$PKG_VERSION" "$TARBALL"
