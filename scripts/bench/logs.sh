#!/usr/bin/env bash
# journalctl -u xorgate-agent-rs on the bench.
#
#   scripts/bench/logs.sh            # last 100 lines
#   scripts/bench/logs.sh -n 500     # last N lines
#   scripts/bench/logs.sh -f         # follow (Ctrl-C to stop)
#   scripts/bench/logs.sh -f -n 50
set -euo pipefail
# shellcheck source=scripts/bench/_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/_lib.sh"

FOLLOW=0
LINES=100
while [ $# -gt 0 ]; do
  case "$1" in
    -f|--follow) FOLLOW=1; shift ;;
    -n|--lines) LINES="$2"; shift 2 ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [ "$FOLLOW" -eq 1 ]; then
  bench_ssh_tty "journalctl -u $UNIT -n '$LINES' -f -o short-iso"
else
  bench_ssh "journalctl -u $UNIT -n '$LINES' --no-pager -o short-iso"
fi
