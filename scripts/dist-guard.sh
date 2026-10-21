#!/usr/bin/env bash
# The dist-bucket key guard (phase-5-distribution.md §6/§7): both deploy
# workflows call it for EVERY key they write, before the write.
#
#   scripts/dist-guard.sh <key>      exit 0 = allowed, 1 = refused (reason on stderr)
#
# Allow-list (the only keys this repo may ever write):
#   xorgate-agent-rs-<v>.tar.gz           versioned tarball (and the -latest alias)
#   xorgate-agent-rs-<v>.tar.gz.sha256    its sidecar
#   latest-rs.json                        the Rust manifest
#   install-rs.sh, install-rs.sh.sha256   the converter + its sidecar (Phase 5)
# <v> = [A-Za-z0-9._+-]+ (a short sha, a release tag, or `latest`).
# Everything else is refused — in particular the JS agent's keys
# (`latest.json`, `xorgate-agent-<v>.tar.gz`, `xorgate-agent-latest.tar.gz`,
# their sidecars), any prefix/path (`a/latest-rs.json`), and empty keys. The
# JS keys stay the JS agent's until the cutover plan (README §6, Q7).
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: $0 <key>" >&2; exit 2; }
KEY=$1
case "$KEY" in
  latest-rs.json|install-rs.sh|install-rs.sh.sha256) exit 0 ;;
esac
if [[ "$KEY" =~ ^xorgate-agent-rs-[A-Za-z0-9._+-]+\.tar\.gz(\.sha256)?$ ]] && [[ "$KEY" != *..* ]]; then
  exit 0
fi
echo "dist-guard: refusing to write key '$KEY' (outside the -rs allow-list: xorgate-agent-rs-<v>.tar.gz{,.sha256}, latest-rs.json, install-rs.sh{,.sha256})" >&2
exit 1
