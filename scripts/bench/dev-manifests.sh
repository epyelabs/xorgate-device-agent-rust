#!/usr/bin/env bash
# Give the Rust agent its OWN copies of the DEV manifests and point its env file
# at them (plan phase-2-cloud-plane.md §Bench recipe step 1). Idempotent.
#
#   scripts/bench/dev-manifests.sh [--dry-run]
#
# On the bench, as root:
#   - /var/lib/xorgate-rs/etc/ (pi, 0700) gets kvs.json from the parked DEV
#     bundle /etc/xorgate/dev-backup-20260823/, hardware.json + pipelines/
#     copied from /etc/xorgate (read-only use of the prod copies), all pi 0600;
#   - /etc/xorgate/agent-rs-dev.env (the Rust agent's own env file, created by
#     install.sh) gets KVS_MANIFEST / HARDWARE_MANIFEST / KVS_PIPELINE_DIR
#     pointing there;
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
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
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
copy_file "$DEV_BACKUP/kvs.json" "$RS_ETC/kvs.json"
if [ -f /etc/xorgate/hardware.json ]; then copy_file /etc/xorgate/hardware.json "$RS_ETC/hardware.json"; fi
if [ -d /etc/xorgate/pipelines ]; then
  [ -d "$RS_ETC/pipelines" ] || { act install -d -o pi -g pi -m 0700 "$RS_ETC/pipelines"; note "$RS_ETC/pipelines created"; }
  for f in /etc/xorgate/pipelines/*; do
    [ -f "$f" ] && copy_file "$f" "$RS_ETC/pipelines/$(basename "$f")"
  done
fi

echo "== 2. $RS_ENV"
TMP=$(mktemp)
cp "$RS_ENV" "$TMP"
setkey() { # key value
  if grep -q "^$1=" "$TMP"; then sed -i "s|^$1=.*|$1=$2|" "$TMP"; else echo "$1=$2" >> "$TMP"; fi
}
setkey KVS_MANIFEST "$RS_ETC/kvs.json"
setkey HARDWARE_MANIFEST "$RS_ETC/hardware.json"
setkey KVS_PIPELINE_DIR "$RS_ETC/pipelines"
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
  act install -o pi -g pi -m 0600 "$TMP" "$RS_ENV"; note "$RS_ENV: KVS_MANIFEST/HARDWARE_MANIFEST/KVS_PIPELINE_DIR -> $RS_ETC"
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
