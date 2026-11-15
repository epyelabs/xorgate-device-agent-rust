#!/usr/bin/env bash
# xorgate device install script: Rust agent bootstrap.
# Generated per-request by GET /get/:code. Do not edit by hand on the host.
#
# Downloads the Rust agent build, verifies it, and runs the provisioner that
# ships inside it (provision-rs.sh). Nothing is claimed until that provisioner
# has installed its dependencies and smoke-run the binary: if anything here
# fails, the registration code is still unused.
#
# Integrity: the .sha256 sidecar comes from the same bucket, over the same TLS
# origin, as the tarball. It catches a corrupt or truncated download, not a
# compromised bucket. The registration code is single-use and expires 15
# minutes after it was minted.
#
# Usage: sudo bash install.sh [--reprovision]
#   --reprovision  re-register a device that already runs the Rust agent.
set -euo pipefail

REG_CODE='abcdefghijkmnpq2'
API_BASE='https://fvcro5guw7.execute-api.us-east-1.amazonaws.com/v1'
DIST_BUCKET_URL='https://xorgate-device-dist-865609249890-us-east-1.s3.us-east-1.amazonaws.com'
DIST_VERSION='latest'

log()  { echo "[xorgate] $*"; }
fail() { echo "[xorgate] ERROR: $*" >&2; exit 1; }

if [ "$(id -u)" -ne 0 ]; then
  fail "run with sudo"
fi
ARCH=$(uname -m)
if [ "$ARCH" != "aarch64" ]; then
  fail "the Rust agent needs a 64-bit ARM (aarch64) host; this one is $ARCH"
fi

# Only --reprovision is forwarded to the provisioner. Anything else is a usage
# error (exit 2), so a typo never reaches a step that claims the code.
REPROVISION=0
for arg in "$@"; do
  case "$arg" in
    --reprovision) REPROVISION=1 ;;
    *)
      echo "[xorgate] unknown argument: $arg" >&2
      echo "[xorgate] usage: sudo bash install.sh [--reprovision]" >&2
      exit 2
      ;;
  esac
done

# 0a. Keep NetworkManager off the SIM7600's usb0 before anything touches the
#     network: on a fresh device NM grabs the modem's DHCP lease and its
#     metric-100 default route black-holes the download below. Written ONLY
#     when absent (its purpose is a fresh device); the provisioner installs the
#     fully commented copy from the tarball later.
NM_CONF=/etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf
if [ -d /etc/NetworkManager ] && [ ! -e "$NM_CONF" ]; then
  install -d -m 0755 /etc/NetworkManager/conf.d
  cat > "$NM_CONF" <<'NMEOF'
# usb0 is owned exclusively by the xorgate sim7600-lte daemon (its own
# dhclient + health checks); NetworkManager must not manage it. Installed
# early by install.sh; replaced by the fully commented copy from the agent
# tarball later in the same run.
[keyfile]
unmanaged-devices=interface-name:usb0
NMEOF
  systemctl reload NetworkManager 2>/dev/null || true
fi

for tool in curl tar sha256sum mktemp; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool is required (sudo apt-get install -y $tool)"
done

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
fetch() { curl -fsSL --proto =https --retry 3 --retry-delay 2 "$1" -o "$2"; }

# 1. Resolve the build. "latest" is resolved through latest-rs.json to the
#    IMMUTABLE versioned tarball it names: the latest alias and its sidecar are
#    uploaded one after the other, so they can briefly disagree.
TARBALL_RE='^xorgate-agent-rs-[A-Za-z0-9._+-]+\.tar\.gz$'
if [ "$DIST_VERSION" = "latest" ]; then
  fetch "$DIST_BUCKET_URL/latest-rs.json" "$TMP/latest-rs.json" \
    || fail "could not fetch latest-rs.json; is a Rust agent build published for this stage?"
  if command -v python3 >/dev/null 2>&1; then
    TARBALL=$(python3 -c 'import json, sys
v = json.load(open(sys.argv[1])).get("tarball")
print(v if isinstance(v, str) else "")' "$TMP/latest-rs.json" 2>/dev/null || true)
  else
    TARBALL=$(sed -n 's/.*"tarball"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$TMP/latest-rs.json" | head -n 1)
  fi
else
  TARBALL="xorgate-agent-rs-$DIST_VERSION.tar.gz"
fi
if ! [[ "$TARBALL" =~ $TARBALL_RE ]]; then
  fail "latest-rs.json names no valid Rust agent tarball"
fi

# 2. Download the tarball + its sidecar, and verify. Fail closed.
log "downloading $DIST_BUCKET_URL/$TARBALL"
fetch "$DIST_BUCKET_URL/$TARBALL" "$TMP/$TARBALL" || fail "could not download $TARBALL"
fetch "$DIST_BUCKET_URL/$TARBALL.sha256" "$TMP/$TARBALL.sha256" \
  || fail "could not download $TARBALL.sha256"
SHA_RE='^[0-9a-f]{64}$'
SUM=$(awk 'NR == 1 { print $1 }' "$TMP/$TARBALL.sha256")
NAMED=$(awk 'NR == 1 { print $2 }' "$TMP/$TARBALL.sha256")
if ! [[ "$SUM" =~ $SHA_RE ]] || [ "$NAMED" != "$TARBALL" ] \
   || [ "$(wc -l < "$TMP/$TARBALL.sha256" | tr -d ' ')" != "1" ]; then
  fail "$TARBALL.sha256 is not a single checksum line for $TARBALL"
fi
(cd "$TMP" && sha256sum -c --status "$TARBALL.sha256") \
  || fail "checksum mismatch for $TARBALL; not installing"
log "checksum ok ($TARBALL)"

# 3. Unpack the provisioner, which ships inside the build.
mkdir -p "$TMP/x"
tar -xzf "$TMP/$TARBALL" -C "$TMP/x" provision-rs.sh 2>/dev/null \
  || fail "$TARBALL has no provision-rs.sh (a build older than native Rust installs?)"
[ -f "$TMP/x/provision-rs.sh" ] || fail "$TARBALL has no provision-rs.sh"

# 4. Hand over. exec skips the EXIT trap; --cleanup tells the provisioner to
#    remove the temp dir (and the tarball in it) when it is done.
trap - EXIT
log "running provision-rs.sh from $TARBALL"
if [ "$REPROVISION" = "1" ]; then
  exec bash "$TMP/x/provision-rs.sh" --reg-code "$REG_CODE" --api-base "$API_BASE" \
    --from "$DIST_BUCKET_URL" --tarball "$TMP/$TARBALL" --cleanup "$TMP" --reprovision
fi
exec bash "$TMP/x/provision-rs.sh" --reg-code "$REG_CODE" --api-base "$API_BASE" \
  --from "$DIST_BUCKET_URL" --tarball "$TMP/$TARBALL" --cleanup "$TMP"
