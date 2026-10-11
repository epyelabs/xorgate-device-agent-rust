#!/usr/bin/env bash
# Rig PKI for the record/replay broker (idempotent): a throwaway CA, a server
# cert for mosquitto (SANs broker / mosquitto / localhost — the agents connect to
# `broker`, which the rig maps to its own proxy), and client certs for the two
# agents (a, b) and the player, and (Phase 3) one server cert for the player's
# fake AWS endpoints: the IoT credentials host and every
# `<bucket>.s3.ca-central-1.amazonaws.com` (wildcard), see aws.rs. NEVER a real
# identity: everything lives in tools/replay/.cache/certs (gitignored) and is
# useless outside the rig.
#
#   tools/replay/gen-certs.sh [--force]
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
OUT="$HERE/.cache/certs"
if [ "${1:-}" = "--force" ]; then rm -rf "$OUT"; fi
# A Phase 2 cert dir (no aws.*) is regenerated whole: one CA signs everything.
if [ -f "$OUT/ca.pem" ] && [ ! -f "$OUT/aws.key" ]; then rm -rf "$OUT"; fi
if [ -f "$OUT/ca.pem" ] && [ -f "$OUT/player.key" ] && [ -f "$OUT/aws.key" ]; then
  echo "certs: $OUT (exists)"
  exit 0
fi
mkdir -p "$OUT"
cd "$OUT"
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 3650 \
  -keyout ca.key -out ca.pem -subj "/CN=xorgate-replay-rig-ca" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null

issue() { # name cn eku [san]
  local name=$1 cn=$2 eku=$3 san=${4:-}
  openssl req -newkey rsa:2048 -nodes -sha256 -keyout "$name.key" -out "$name.csr" -subj "/CN=$cn" 2>/dev/null
  {
    echo "basicConstraints=critical,CA:FALSE"
    echo "keyUsage=critical,digitalSignature,keyEncipherment"
    echo "extendedKeyUsage=$eku"
    [ -n "$san" ] && echo "subjectAltName=$san"
  } > "$name.ext"
  openssl x509 -req -in "$name.csr" -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out "$name.crt" -days 3650 -sha256 -extfile "$name.ext" 2>/dev/null
  rm -f "$name.csr" "$name.ext"
}
issue server broker serverAuth "DNS:broker,DNS:mosquitto,DNS:localhost"
issue a 00000000-0000-4000-8000-00000000000a clientAuth
issue b 00000000-0000-4000-8000-00000000000b clientAuth
issue player rig-player clientAuth
issue aws rig-aws serverAuth "DNS:c0rig00000000.credentials.iot.ca-central-1.amazonaws.com,DNS:*.credentials.iot.ca-central-1.amazonaws.com,DNS:*.s3.ca-central-1.amazonaws.com"
# mosquitto runs as uid 1883 in its image; the rig agents run as root.
chmod 0644 ./*.key ./*.crt ca.pem
echo "certs: $OUT (new)"
