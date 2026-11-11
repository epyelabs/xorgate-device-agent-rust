#!/bin/sh
# Phase 7c WP2: build / run the Python oracle image on the homelab.
#   run.sh image-tag            -> xorgate-p7c-oracle:<sha12 of Dockerfile+requirements.txt+stubs/lgpio.py>
#   run.sh build                -> docker build (label xorgate.agent=$AGENT)
#   run.sh run [-i] ARGS...     -> python3 /oracle/ARGS... in the image, --network none, non-root,
#                                  the repo's tools/imu/oracle at /oracle (ro), python/bno085_read.py
#                                  at /helper (ro), $OUT (default ./oracle-out) at /out.
# Env: AGENT (label, default p7c-wp2), CPUS (default 4), OUT.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
AGENT=${AGENT:-p7c-wp2}
CPUS=${CPUS:-4}
tag() {
  sha=$(cd "$HERE" && cat Dockerfile requirements.txt stubs/lgpio.py | sha256sum | cut -c1-12)
  echo "xorgate-p7c-oracle:$sha"
}
cmd=${1:-}
[ $# -gt 0 ] && shift
case "$cmd" in
  image-tag) tag ;;
  build)
    img=$(tag)
    docker build --label "xorgate.agent=$AGENT" -t "$img" -f "$HERE/Dockerfile" "$HERE"
    echo "$img" ;;
  run)
    tty=""
    if [ "${1:-}" = "-i" ]; then tty="-i"; shift; fi
    OUT=${OUT:-$PWD/oracle-out}
    mkdir -p "$OUT"
    docker run --rm $tty --label "xorgate.agent=$AGENT" --network none --cpus "$CPUS" --memory 4g \
      -v "$HERE:/oracle:ro" -v "$REPO/python/bno085_read.py:/helper/bno085_read.py:ro" -v "$OUT:/out" \
      -e "P7C_ORACLE_IMAGE=$(tag)" "$(tag)" python3 "$@" ;;
  *) echo "usage: run.sh image-tag|build|run [-i] ARGS..." >&2; exit 2 ;;
esac
