#!/usr/bin/env bash
# Offline suite: the WP3 ground-truth timing checker (tools/replay/timing_truth.py).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
exec python3 -I "$HERE/timing_truth_test.py"
