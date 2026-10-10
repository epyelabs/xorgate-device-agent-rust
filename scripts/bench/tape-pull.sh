#!/usr/bin/env bash
# Copy window tapes from the bench to the Mac (LOCAL ONLY — tapes hold the
# bench location and SIM identity and are never committed, decision O8).
#
#   scripts/bench/tape-pull.sh [--all | <tape name>] [--dest DIR] [--dry-run]
#
# Default: the newest /var/lib/xorgate-rs/tapes/*.ndjson.gz into
# plans/device-agent-js-to-rust/phase-2/evidence/tapes/. Reports a tape cut short
# (truncated gzip = the agent was killed mid-write) — still usable up to the last
# flush; `xorgate-replay inspect` says how much.
# Then on the Mac: tools/replay/run-rig.sh <tape>.
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"
WHICH=latest; DEST="$EVIDENCE_PHASE2_LOCAL/tapes"; DRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --all) WHICH=all; shift ;;
    --dest) DEST="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) WHICH="$1"; shift ;;
  esac
done
LIST=$(bench_ssh "ls -1t $RS_HOME/tapes/*.ndjson.gz 2>/dev/null" || true)
[ -n "$LIST" ] || { echo "no tapes in $BENCH:$RS_HOME/tapes" >&2; exit 1; }
case "$WHICH" in
  latest) PICK=$(head -n1 <<<"$LIST") ;;
  all) PICK=$LIST ;;
  *) PICK=$(grep -F "/$WHICH" <<<"$LIST" || true); [ -n "$PICK" ] || { echo "no tape $WHICH; have:"; echo "$LIST"; exit 1; } ;;
esac
mkdir -p "$DEST"
while read -r f; do
  [ -n "$f" ] || continue
  size=$(bench_ssh "stat -c %s '$f'")
  if [ "$DRY" = 1 ]; then
    echo "[dry-run] would copy $f ($size bytes) -> $DEST/"
    bench_ssh "test -f '$f.cli.ndjson'" && echo "[dry-run] would copy $f.cli.ndjson -> $DEST/"
    continue
  fi
  bench_scp_from "$f" "$DEST/"
  local_f="$DEST/$(basename "$f")"
  if gzip -t "$local_f" 2>/dev/null; then state=complete; else state="TRUNCATED (readable up to the last flush)"; fi
  echo "$local_f  $size bytes  $state"
  # `xorgate-rs set` writes its `cli` events to a plain side file next to the
  # tape (WP6 A6); `xorgate-replay` merges it by wall time on read.
  if bench_ssh "test -f '$f.cli.ndjson'"; then
    bench_scp_from "$f.cli.ndjson" "$DEST/"
    echo "$local_f.cli.ndjson  $(wc -l < "$local_f.cli.ndjson" | tr -d ' ') cli events"
  fi
done <<<"$PICK"
