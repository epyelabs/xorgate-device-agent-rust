#!/usr/bin/env bash
# Build the Rust simulator image (Phase 7e WP2) from this repo.
#
#   sim/build-image.sh [--sha <short sha>] [--channel <c>] [--tag <name:tag>]
#                      [--label k=v]... [--dry-run] [-- <extra docker build args>]
#
# Defaults: sha = `git rev-parse --short HEAD` (+ "-dirty" on an unclean tree),
# channel = bench (a hand build, scripts/build-info.sh), tag =
# xorgate-sim-rs:<sha>. A tree without .git (an rsync or `git archive` copy)
# must pass --sha: the image never reports `unknown` (Q-E3: honest build-info).
#
# Build on an x86_64 host (the homelab) for the amd64 image the sim hosts run;
# on an arm64 Mac it builds natively for arm64. Never tag `xorgate-sim:*`:
# rebuilding `xorgate-sim:dev`/`:prod` recreates every dev/prod sim on the
# next sim-host tick (sim-host README "Roll out a new sim image"), so this
# script refuses any tag outside `xorgate-sim-rs:*`.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
SHA=""
CHANNEL=bench
TAG=""
DRY=0
LABELS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --sha) SHA="$2"; shift 2 ;;
    --channel) CHANNEL="$2"; shift 2 ;;
    --tag) TAG="$2"; shift 2 ;;
    --label) LABELS+=(--label "$2"); shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --) shift; break ;;
    *) echo "unknown arg: $1 (see the header)" >&2; exit 2 ;;
  esac
done

if [ -z "$SHA" ]; then
  if git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1; then
    SHA=$(git -C "$REPO" rev-parse --short HEAD)
    [ -z "$(git -C "$REPO" status --porcelain)" ] || SHA="${SHA}-dirty"
  else
    echo "no git checkout at $REPO: pass --sha <short sha>" >&2
    exit 2
  fi
fi
case "$SHA" in
  *[!0-9a-z-]* | "" | -*) echo "bad --sha '$SHA'" >&2; exit 2 ;;
esac
[ -n "$TAG" ] || TAG="xorgate-sim-rs:$SHA"
case "$TAG" in
  xorgate-sim-rs:?*) ;;
  *) echo "refusing tag '$TAG': only xorgate-sim-rs:<tag> (never xorgate-sim:*)" >&2; exit 2 ;;
esac

cmd=(docker build -f "$REPO/sim/Dockerfile"
  --build-arg "GIT_SHA=$SHA" --build-arg "BUILD_CHANNEL=$CHANNEL"
  ${LABELS[@]+"${LABELS[@]}"} -t "$TAG" "$@" "$REPO")
if [ "$DRY" = 1 ]; then
  printf '%q ' "${cmd[@]}"
  echo
  exit 0
fi
echo ">> building $TAG (sha $SHA, channel $CHANNEL)" >&2
t0=$(date +%s)
DOCKER_BUILDKIT=1 "${cmd[@]}"
echo ">> $TAG built in $(( $(date +%s) - t0 )) s" >&2
docker image inspect "$TAG" --format '>> {{.Id}} {{.Size}} bytes' >&2
