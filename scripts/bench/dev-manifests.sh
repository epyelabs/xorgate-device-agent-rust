#!/usr/bin/env bash
# Give the Rust agent its OWN copies of the DEV manifests and point its env file
# at them (plan phase-2-cloud-plane.md §Bench recipe step 1). Idempotent.
#
#   scripts/bench/dev-manifests.sh [--dry-run]
#
# On the bench, as root:
#   - /var/lib/xorgate-rs/etc/ (pi, 0700) gets TWO kvs copies of the parked DEV
#     bundle's /etc/xorgate/dev-backup-20260823/kvs.json (Phase 3, plan P3):
#     kvs.json with "channels": [] (SHARED, media-off: no masters, no bridged
#     cameras; credentialEndpoint/roleAlias/region/uploadBucket kept so
#     telemetry recording + upload still run; key order preserved) and
#     kvs-exclusive.json verbatim (cam0/cam1 on the DEV identity; the
#     exclusive window's runtime env file points KVS_MANIFEST there);
#     an EMPTY pipelines/ dir (pi 0700): NO .gst/.gst.direct/.camera is copied
#     from /etc/xorgate/pipelines any more (those renders name the JS socket
#     /var/lib/xorgate/shm/<key>.sock; the Rust reconcile renders its own at
#     the first exclusive boot, D4) and earlier copies are REMOVED (a file
#     byte-equal to its /etc/xorgate/pipelines namesake, or a .gst* naming
#     /var/lib/xorgate/shm/; anything else — the Rust renders — is kept);
#     and TWO hardware copies of /etc/xorgate/hardware.json:
#     hardware.json with "imu": null (SHARED profile: no i2cdetect probe while
#     the JS helper owns the BNO085) and hardware-exclusive.json verbatim (the
#     exclusive window's runtime drop-in points HARDWARE_MANIFEST there); all
#     pi 0600, read-only use of the prod copies;
#     (the shared-profile shims /var/lib/xorgate-rs/shared-shims/ are made by
#     profile.sh shared, which also verifies them);
#   - /etc/xorgate/agent-rs-dev.env (the Rust agent's own env file, created by
#     install.sh) gets KVS_MANIFEST / HARDWARE_MANIFEST / KVS_PIPELINE_DIR
#     pointing there, and LOSES KVS_RUNTIME_BIN / KVS_RUNTIME_LIB_DIR /
#     KVS_CACERT_PATH when they name the JS install (/opt/xorgate/...);
#   - GUARD: refuses (and restores the env file) if any path key of the env file
#     still points into /etc/xorgate outside the dev-backup dir, except
#     root-ca.pem;
#   - writes the C15 baseline /var/lib/xorgate-rs/evidence/c15-baseline.sha256
#     of /etc/xorgate/{agent.env,kvs.json,hardware.json} once (re-checks it on
#     later runs; --rebaseline overwrites).
# Never touches /etc/xorgate/{agent.env,kvs.json,hardware.json,pipelines} or
# /var/lib/xorgate. Does not restart anything (profile.sh / start.sh do).
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
DRY=0; REBASE=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    --rebaseline) REBASE=1 ;;
    -h|--help) sed -n '2,39p' "$0"; exit 0 ;;
    *) echo "unknown arg $a" >&2; exit 2 ;;
  esac
done
log "dev manifests on $BENCH$([ $DRY = 1 ] && echo ' (DRY RUN)')"
{ echo "DRY=$DRY REBASE=$REBASE"; bench_remote_lib; cat <<'REMOTE'
[ "$(id -u)" -eq 0 ] || { echo "must run as root"; exit 1; }
[ -f "$RS_ENV" ] || { echo "$RS_ENV missing — run scripts/bench/install.sh first"; exit 1; }
[ -f "$DEV_BACKUP/kvs.json" ] || { echo "no DEV kvs.json in $DEV_BACKUP"; exit 1; }

echo "== 1. $RS_ETC"
if [ ! -d "$RS_ETC" ]; then act install -d -o pi -g pi -m 0700 "$RS_ETC"; note "$RS_ETC created"; fi
copy_file() { # src dst
  if [ -f "$2" ] && cmp -s "$1" "$2"; then echo "  = $2 unchanged"; return; fi
  act install -o pi -g pi -m 0600 "$1" "$2"; note "$2 <- $1"
}
# kvs.json (SHARED, media-off) = the DEV manifest with channels: [] (P3).
KTMP=$(mktemp)
python3 - "$DEV_BACKUP/kvs.json" > "$KTMP" <<'PY'
import collections, json, sys
d = json.load(open(sys.argv[1]), object_pairs_hook=collections.OrderedDict)
d["channels"] = []  # an existing key keeps its position
print(json.dumps(d, indent=2))
PY
for k in credentialEndpoint roleAlias region uploadBucket; do
  python3 -c 'import json,sys; v=json.load(open(sys.argv[1])).get(sys.argv[2]); sys.exit(0 if isinstance(v,str) and v else 1)' "$KTMP" "$k" \
    || echo "  NOTE: DEV kvs.json has no $k$([ "$k" = uploadBucket ] && echo ' (segment upload disabled unless XORGATE_UPLOAD_BUCKET)')"
done
if [ -f "$KVS_SHARED" ] && cmp -s "$KTMP" "$KVS_SHARED"; then echo "  = $KVS_SHARED unchanged (channels: [])"
else
  if [ -f "$KVS_SHARED" ]; then echo "  $KVS_SHARED differs:"; diff "$KVS_SHARED" "$KTMP" | sed 's/^/    /' || true; fi
  act install -o pi -g pi -m 0600 "$KTMP" "$KVS_SHARED"; note "$KVS_SHARED <- $DEV_BACKUP/kvs.json with channels: [] (shared, media-off)"
fi
rm -f "$KTMP"
# kvs-exclusive.json = the DEV manifest verbatim (exclusive window only).
copy_file "$DEV_BACKUP/kvs.json" "$KVS_EXCLUSIVE"
# hardware.json: the shared copy says imu: null, hardware-exclusive.json is the
# real block (orchestrator decision on WP2 amb. 10; see _lib.sh hardware_copies).
hardware_copies
# Pipelines (Phase 3): copy NOTHING; remove what earlier runs copied. The dir
# itself stays (KVS_PIPELINE_DIR; the Rust reconcile renders into it).
[ -d "$RS_ETC/pipelines" ] || { act install -d -o pi -g pi -m 0700 "$RS_ETC/pipelines"; note "$RS_ETC/pipelines created"; }
for f in "$RS_ETC"/pipelines/*; do
  [ -f "$f" ] || continue
  b=$(basename "$f")
  if [ -f "/etc/xorgate/pipelines/$b" ] && cmp -s "$f" "/etc/xorgate/pipelines/$b"; then
    act rm -f "$f"; note "$f removed (a copy of /etc/xorgate/pipelines/$b)"
  elif case "$b" in *.gst|*.gst.direct) grep -q '/var/lib/xorgate/shm/' "$f" ;; *) false ;; esac; then
    act rm -f "$f"; note "$f removed (an old copy naming the JS socket /var/lib/xorgate/shm/)"
  else
    echo "  = $f kept (not a copy of the JS renders)"
  fi
done

echo "== 2. $RS_ENV"
TMP=$(mktemp)
cp "$RS_ENV" "$TMP"
setkey() { # key value
  if grep -q "^$1=" "$TMP"; then sed -i "s|^$1=.*|$1=$2|" "$TMP"; else echo "$1=$2" >> "$TMP"; fi
}
setkey KVS_MANIFEST "$RS_ETC/kvs.json"
setkey HARDWARE_MANIFEST "$RS_ETC/hardware.json"
setkey KVS_PIPELINE_DIR "$RS_ETC/pipelines"
# Phase 3 (P4): the DEV bundle's env names the JS install's KVS runtime
# (/opt/xorgate/media/...). The profiles own KVS_RUNTIME_BIN (profile.sh refuses
# an env file that sets it) and the Rust defaults resolve under /opt/xorgate-rs,
# so drop these keys when they point into the JS install.
DROPPED=""
for k in KVS_RUNTIME_BIN KVS_RUNTIME_LIB_DIR KVS_CACERT_PATH; do
  if grep -qE "^$k=/opt/xorgate/" "$TMP"; then sed -i "/^$k=/d" "$TMP"; DROPPED="$DROPPED $k"; fi
done
[ -z "$DROPPED" ] || echo "  dropping$DROPPED (JS install paths /opt/xorgate/media; Rust defaults /opt/xorgate-rs/media)"
# Guard: no path key may point into /etc/xorgate outside the dev-backup dir
# (root-ca.pem excepted: it is the public Amazon root).
BAD=$(grep -E '^[A-Z_]+=/etc/xorgate' "$TMP" | grep -vE "=$DEV_BACKUP/|=/etc/xorgate/root-ca\.pem$" || true)
if [ -n "$BAD" ]; then
  echo "REFUSING: these keys still point at prod paths under /etc/xorgate:"; echo "$BAD" | sed 's/=.*//; s/^/    /'
  rm -f "$TMP"; exit 1
fi
if cmp -s "$TMP" "$RS_ENV"; then
  echo "  = $RS_ENV unchanged"
else
  echo "  diff (keys only; values are paths):"; diff <(sed 's/=.*//' "$RS_ENV") <(sed 's/=.*//' "$TMP") | sed 's/^/    /' || true
  diff "$RS_ENV" "$TMP" | grep -E '^[<>] (KVS_MANIFEST|HARDWARE_MANIFEST|KVS_PIPELINE_DIR)=' | sed 's/^/    /' || true
  act install -o pi -g pi -m 0600 "$TMP" "$RS_ENV"; note "$RS_ENV: KVS_MANIFEST/HARDWARE_MANIFEST/KVS_PIPELINE_DIR -> $RS_ETC${DROPPED:+; removed$DROPPED}"
fi
rm -f "$TMP"

echo "== 3. C15 baseline"
[ -d "$RS_HOME/evidence" ] || { act install -d -o pi -g pi -m 0755 "$RS_HOME/evidence"; note "$RS_HOME/evidence created"; }
if [ -f "$C15_BASELINE" ] && [ "$REBASE" != 1 ]; then
  c15_check
else
  echo "  baseline:"; (cd / && sha256sum /etc/xorgate/agent.env /etc/xorgate/kvs.json /etc/xorgate/hardware.json) | sed 's/^/    /'
  if [ "$DRY" = 1 ]; then echo "  [dry-run] would write $C15_BASELINE"; else
    sha256sum /etc/xorgate/agent.env /etc/xorgate/kvs.json /etc/xorgate/hardware.json > "$C15_BASELINE"; chown pi:pi "$C15_BASELINE"
  fi
  note "$C15_BASELINE written"
fi
echo "== done: ${#CHANGED[@]} change(s)$([ "$DRY" = 1 ] && echo ' (dry run: nothing written)')"
REMOTE
} | bench_sudo_script
