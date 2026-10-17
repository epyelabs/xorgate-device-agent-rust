#!/usr/bin/env bash
# Fetch the KVS SDK shared libraries the xorgate-kvs-webrtc runtime needs into
# media/lib, by asking the build device's loader (ldd) which libs resolve into
# the SDK build tree and scp'ing exactly those. This is robust to SONAME/version
# drift (e.g. an SDK bump moving libwebsockets.so.19 -> .so.20): it copies
# whatever the binary actually links today. Re-run after rebuilding the runtime.
#
# Run from anywhere on your Mac/Linux box (it writes next to itself):
#   media/fetch-kvs-libs.sh [user@host] [remote-binary-path]
#
# Defaults match the reference Pi:
#   user@host           pi@10.0.0.111
#   remote-binary-path  /home/pi/xorgate-kvs-webrtc/build/xorgate-kvs-webrtc
#
# It multiplexes ONE SSH connection (ControlMaster), so you enter the password
# once, not per file. For zero prompts, set up a key first: ssh-copy-id <host>.
#
# The apt-installable libs (GStreamer/glib/...) are NOT fetched here — the device
# install script apt-installs those. Only the SDK + vendored OpenSSL 1.1 +
# libwebsockets (which live under the build tree) are pulled.
set -euo pipefail

PI="${1:-pi@10.0.0.111}"
BIN="${2:-/home/pi/xorgate-kvs-webrtc/build/xorgate-kvs-webrtc}"
DEST="$(cd "$(dirname "$0")" && pwd)/lib"

mkdir -p "$DEST"

# Multiplex a single SSH connection: authenticate once, reuse the socket for the
# ldd query and every scp below.
CTRL="$(mktemp -u "${TMPDIR:-/tmp}/xg-kvs-ssh.XXXXXX")"
SSHOPTS=(-o "ControlMaster=auto" -o "ControlPath=$CTRL" -o "ControlPersist=120")
cleanup() { ssh "${SSHOPTS[@]}" -O exit "$PI" 2>/dev/null || true; }
trap cleanup EXIT

echo "connecting to $PI (password asked once)..."
ssh "${SSHOPTS[@]}" "$PI" true

echo "querying which libs '$BIN' resolves into the build tree..."
libs="$(ssh "${SSHOPTS[@]}" "$PI" "ldd '$BIN' | awk '/=> \\/home\\//{print \$3}'")"
[ -n "$libs" ] || { echo "no build-tree libs found (check the binary path)"; exit 1; }

printf '%s\n' "$libs" | while IFS= read -r f; do
  [ -n "$f" ] || continue
  echo "  fetching $(basename "$f")"
  scp "${SSHOPTS[@]}" -q "$PI:$f" "$DEST/"
done

echo "done. contents of $DEST:"
ls -la "$DEST"
