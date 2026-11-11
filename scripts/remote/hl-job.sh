#!/usr/bin/env bash
# Homelab side of scripts/remote/hl.sh (copied to ~/xorgate-rs-p7/<tag>/bin/ and
# ~/xorgate-rs-p7/_shared/bin/ by every `hl.sh sync`). Runs ON the homelab.
#
#   hl-job.sh start <tag> <job> [args]   reserve caps, start a detached tmux session `hl_<tag>_<name>`
#   hl-job.sh exec <tag> <job> [args]    the job itself (what the session runs)
#   hl-job.sh status [tag] | preflight | clean <tag>
#   hl-job.sh list-src|prune-src|verify-src <tag>   (hl.sh sync's steps)
#   hl-job.sh js-cache <tag> <ref>       (a `git archive` tar on stdin)
#
# Layout: ~/xorgate-rs-p7/<tag>/{.hl-tag, source.txt, manifest.*, src/, bin/, logs/, out/, inbox/}
# and ~/xorgate-rs-p7/_shared/{bin/, res/, jscache-amd64/, tools/}. Jobs and their knobs:
# see hl.sh's header. HL_DRYRUN=1 prints every docker/tmux command instead of running it
# (scripts/tests/hl.sh).
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/remote/hl-lib.sh
. "$HERE/hl-lib.sh"
ROOT=${HL_ROOT:-$HOME/$HL_ROOT_REL}
SHARED=$ROOT/_shared
HL_JOBS=${HL_JOBS:-}
HL_RIG_CPUS=${HL_RIG_CPUS:-8}
HL_RIG_MEM=${HL_RIG_MEM:-8g}
HL_SUITE_CPUS=${HL_SUITE_CPUS:-4}
HL_SUITE_MEM=${HL_SUITE_MEM:-4g}
HL_RUN_SECS=${HL_RUN_SECS:-2700}
HL_PREFLIGHT=${HL_PREFLIGHT:-each}
HL_WAIT_OTHERS=${HL_WAIT_OTHERS:-0}
HL_PEERS=${HL_PEERS:-}
HL_PROFILE=${HL_PROFILE:-release}
SCRIPT_TESTS_DF=$HERE/Dockerfile.script-tests

die() { echo "hl-job: $*" >&2; exit 2; }
ts() { date -u +%FT%TZ; }
# HL_DRYRUN: print instead of run (docker, tmux).
docker() { if [ "${HL_DRYRUN:-0}" = 1 ]; then echo "DRY docker $*"; else command docker "$@"; fi; }
tmux() { if [ "${HL_DRYRUN:-0}" = 1 ]; then echo "DRY tmux $*"; else command tmux "$@"; fi; }
load1() { cut -d' ' -f1 /proc/loadavg; }
mem_avail_gb() { awk '/^MemAvailable:/ { printf "%d\n", $2 / 1048576 }' /proc/meminfo; }

tagdir() {
  hl_valid_tag "${1:-}" || die "bad tag '${1:-}'"
  TAG=$1; T=$ROOT/$TAG; SRC=$T/src
  [ -f "$T/.hl-tag" ] || die "$T is not a helper tag dir (run hl.sh sync first)"
}

# ---------------------------------------------------------------- sync steps
cmd_list_src() { tagdir "$1"; (cd "$SRC" && find . \( -type f -o -type l \) -print | sed 's#^\./##'); }
cmd_prune_src() {
  tagdir "$1"
  local f n=0
  while IFS= read -r f; do
    case "$f" in ''|/*|*..*) die "refusing to prune '$f'" ;; esac
    rm -f -- "${SRC:?}/$f" && n=$((n + 1))
  done
  echo "pruned $n"
}
cmd_verify_src() {
  tagdir "$1"
  (cd "$SRC" && sha256sum -c --quiet --strict "$T/manifest.sha256") || die "the tree on the homelab does NOT match the shipped manifest"
  local fp
  fp=$(sha256sum < "$T/manifest.sha256" | cut -c1-64)
  grep -qx "tree_fp=$fp" "$T/source.txt" || die "source.txt tree_fp does not match the manifest"
  cat "$T/source.txt" >> "$T/source.log"
  echo "verified $(wc -l < "$T/manifest.txt") files, tree_fp ${fp:0:12}"
}

# ------------------------------------------------------- reservations / caps
# One file per running job: _shared/res/<tag>_<name> = "<cpus> <mem_gb>".
# A reservation whose tmux session is gone is stale and ignored.
res_sum() {
  local f c m s total_c=0 total_m=0
  mkdir -p "$SHARED/res"
  for f in "$SHARED/res"/*; do
    [ -f "$f" ] || continue
    s=hl_$(basename "$f")
    command tmux has-session -t "=$s" 2>/dev/null || continue
    read -r c m < "$f"
    total_c=$(awk -v a="$total_c" -v b="$c" 'BEGIN { print a + b }')
    total_m=$((total_m + m))
  done
  echo "$total_c $total_m"
}
job_budget() { # <job> -> "<cpus> <mem>"
  case "$1" in
    lanes) echo "$(awk -v c="$HL_RIG_CPUS" 'BEGIN { print c + 1 }') $HL_RIG_MEM" ;;
    suites|systemd) echo "$HL_SUITE_CPUS $HL_SUITE_MEM" ;;
    *) echo "$HL_CPUS $HL_MEM" ;;
  esac
}

cmd_start() {
  tagdir "$1"; local job=$2; shift 2
  local name=${HL_JOB_NAME:-$job} s cpus mem gb used_c used_m avail v
  hl_valid_job "$name" || die "bad job name '$name'"
  case "$job" in
    gate|cargo-gate|test|cargo|suites|systemd|build-bins|release|import-bins|import-tapes|synth|lanes|images) ;;
    pack) die "pack is aarch64-only (the device tarball): build it on the Mac (handbook HOMELAB EXECUTION)" ;;
    *) die "unknown job '$job'" ;;
  esac
  hl_valid_cpus "$HL_CPUS" || die "bad HL_CPUS '$HL_CPUS'"
  hl_valid_cpus "$HL_RIG_CPUS" || die "bad HL_RIG_CPUS '$HL_RIG_CPUS'"
  read -r cpus mem <<<"$(job_budget "$job")"
  gb=$(hl_mem_gb "$mem") || exit 2
  s=hl_${TAG}_$name
  command tmux has-session -t "=$s" 2>/dev/null && die "session $s is already running"
  read -r used_c used_m <<<"$(res_sum)"
  awk -v u="$used_c" -v c="$cpus" -v t="$HL_TOTAL_CPUS" 'BEGIN { exit !(u + c <= t) }' \
    || die "refused: helper jobs already hold $used_c CPUs; +$cpus would exceed HL_TOTAL_CPUS=$HL_TOTAL_CPUS"
  avail=$(mem_avail_gb)
  [ $((avail - used_m - gb)) -ge "$HL_MIN_FREE_GB" ] \
    || die "refused: MemAvailable ${avail}G - held ${used_m}G - this ${gb}G < HL_MIN_FREE_GB=${HL_MIN_FREE_GB}G"
  mkdir -p "$T/logs" "$SHARED/res"
  rm -f "$T/logs/$name.rc" "$T/logs/$name.end"
  {
    echo "job=$job name=$name args=$* started=$(ts) budget_cpus=$cpus budget_mem=$mem"
    for v in $HL_FORWARD_VARS; do eval "[ \"\${$v+set}\" = set ]" && eval "echo \"$v=\${$v}\""; done
    cat "$T/source.txt"
  } > "$T/logs/$name.env"
  echo "$cpus $gb" > "$SHARED/res/${TAG}_$name"
  local q="" a
  for a in "$@"; do q="$q $(printf '%q' "$a")"; done
  local envs="" v2
  for v2 in $HL_FORWARD_VARS; do
    if eval "[ \"\${$v2+set}\" = set ]"; then envs="$envs $v2=$(eval "printf '%q' \"\$$v2\"")"; fi
  done
  tmux new-session -d -s "$s" "env$envs HL_JOB_NAME=$name bash $(printf '%q' "$HERE/hl-job.sh") exec $TAG $job$q > $(printf '%q' "$T/logs/$name.log") 2>&1"
  echo "started $s (log ~/$HL_ROOT_REL/$TAG/logs/$name.log; budget ${cpus} CPUs / ${mem})"
}

# ------------------------------------------------------------- docker helpers
build_image() {
  [ -f "$SRC/scripts/docker-build.sh" ] || die "no scripts/docker-build.sh in the tree"
  (cd "$SRC" && bash scripts/docker-build.sh --platform linux/amd64 image-tag) || die "the tree's docker-build.sh has no amd64 recipe (pre-7e tree?)"
}
script_image() { echo "xorgate-rs-script-tests:bookworm-$(sha256sum < "$SCRIPT_TESTS_DF" | cut -c1-12)"; }
deny_bin() { echo "$SHARED/tools/cargo-deny-$HL_DENY_VERSION"; }
# brun <suffix> <cpus> <mem> <image> <script in bin/> [args]: one build-image
# container on this tree; outputs handed back to the ai user at the end.
brun() {
  local sfx=$1 cpus=$2 mem=$3 img=$4; shift 4
  local deny=() jobs=${HL_JOBS:-$cpus}
  jobs=${jobs%.*}
  [ -x "$(deny_bin)" ] && deny=(-v "$(deny_bin):/deny/cargo-deny:ro")
  docker run --rm --name "hl_${TAG}_${sfx}" --label "xorgate.agent=$TAG" --label xorgate.hl=1 \
    --cpus "$cpus" --memory "$mem" \
    -v "$SRC:/src" -v "$T/logs:/logs" -v "$T/out:/out" -v "$HERE:/hl/bin:ro" ${deny[@]+"${deny[@]}"} \
    -v "$HL_REG_VOL:/usr/local/cargo/registry" -v "$HL_GIT_VOL:/usr/local/cargo/git" -v "$HL_TARGET_VOL:/target" \
    -e "CARGO_TARGET_DIR=/target/$TAG" -e "CARGO_BUILD_JOBS=$jobs" -e CARGO_TERM_COLOR=never \
    -e "HL_HEAD=$(sed -n 's/^head=//p' "$T/source.txt")" -e "HL_UID=$(id -u)" -e "HL_GID=$(id -g)" \
    -e "HL_JOB_NAME=${HL_JOB_NAME:-}" -e "HL_PROFILE=$HL_PROFILE" -e "HL_TOUCH=${HL_TOUCH:-0}" \
    -w /src "$img" sh -c 'rc=0; bash "$@" || rc=$?; chown -R "$HL_UID:$HL_GID" /src /logs /out 2>/dev/null; exit $rc' hl "/hl/bin/$1" "${@:2}"
}
# ONE TREE PER TARGET DIR: /target/<tag> belongs to this tag's tree. When the
# tree changed since the last cargo job here (a re-sync), every .rs is touched
# so cargo's mtime fingerprints cannot keep a stale artifact.
touch_if_new_tree() {
  local fp last
  fp=$(sed -n 's/^tree_fp=//p' "$T/source.txt")
  last=$(cat "$T/target.fp" 2>/dev/null || true)
  if [ "$fp" != "$last" ]; then HL_TOUCH=1; echo "== tree changed (${last:0:12} -> ${fp:0:12}): touching every .rs"; else HL_TOUCH=0; fi
  export HL_TOUCH
  echo "$fp" > "$T/target.fp"
}

# ---------------------------------------------------------------------- jobs
job_cargo_gate() { # [full|test]
  touch_if_new_tree
  brun "${HL_JOB_NAME:-cargo-gate}" "$HL_CPUS" "$HL_MEM" "$(build_image)" gate-in.sh "${1:-full}"
}
job_cargo() {
  touch_if_new_tree
  brun "${HL_JOB_NAME:-cargo}" "$HL_CPUS" "$HL_MEM" "$(build_image)" cargo-in.sh "$@"
}
# The plain script suites: every scripts/tests, scripts/bench/tests and
# tools/replay/tests *.sh except the helper library, the cargo probe and the two
# systemd suites (HL_SUITES overrides the list).
suite_list() {
  if [ -n "${HL_SUITES:-}" ]; then printf '%s\n' "$HL_SUITES" | tr ' ' '\n' | sed '/^$/d'; return; fi
  (cd "$SRC" && ls scripts/tests/*.sh scripts/bench/tests/*.sh tools/replay/tests/*.sh 2>/dev/null) \
    | grep -vE '/(_stubs|clippy-ban-probe|unit-ordering|install-rs-systemd)\.sh$'
}
job_suites() {
  local img t n rc fails=0
  img=$(script_image)
  docker image inspect "$img" >/dev/null 2>&1 || die "script-tests image $img missing: run the images job"
  mkdir -p "$T/logs/suites"
  echo "== suites on $(sed -n 's/^head=//p' "$T/source.txt") $(ts) image $img"
  for t in $(suite_list); do
    n=$(echo "$t" | tr / _)
    docker run --rm --name "hl_${TAG}_suite-${n%.sh}" --label "xorgate.agent=$TAG" --label xorgate.hl=1 \
      --cpus "$HL_SUITE_CPUS" --memory "$HL_SUITE_MEM" -v "$SRC:/src:ro" -v "$HERE:/hl/bin:ro" "$img" \
      timeout 1800 bash /hl/bin/suite-in.sh "$t" > "$T/logs/suites/$n.log" 2>&1
    rc=$?
    [ $rc -eq 0 ] || fails=$((fails + 1))
    echo "$(head -1 "$T/logs/suites/$n.log") (container rc=$rc)"
  done
  echo "SUITES_DONE fails=$fails"
  [ "$fails" -eq 0 ]
}
job_systemd() {
  local pair name suite pkgs rc fails=0
  mkdir -p "$T/logs/suites"
  for pair in "unit-ordering:systemd" "install-rs-systemd:systemd python3 sudo"; do
    suite=${pair%%:*}; pkgs=${pair#*:}; name=hl_${TAG}_sd-$suite
    # Recipe = each suite's header (systemd 252 as PID 1, privileged, private cgroupns).
    docker run -d --name "$name" --label "xorgate.agent=$TAG" --label xorgate.hl=1 \
      --cpus "$HL_SUITE_CPUS" --memory "$HL_SUITE_MEM" --privileged --cgroupns=private \
      --tmpfs /run --tmpfs /run/lock -v "$SRC:/src:ro" debian:bookworm \
      bash -c "apt-get update -qq && apt-get install -y -qq $pkgs >/dev/null && exec /lib/systemd/systemd" >/dev/null || { fails=$((fails + 1)); continue; }
    for _ in $(seq 120); do
      [ "$(docker exec "$name" cat /proc/1/comm 2>/dev/null)" = systemd ] && break
      [ "${HL_DRYRUN:-0}" = 1 ] && break
      sleep 5
    done
    docker exec "$name" timeout 1200 bash "/src/scripts/tests/$suite.sh" > "$T/logs/suites/systemd-$suite.log" 2>&1
    rc=$?
    [ $rc -eq 0 ] || fails=$((fails + 1))
    echo "$suite rc=$rc pass=$(grep -c '^PASS' "$T/logs/suites/systemd-$suite.log") fail=$(grep -c '^FAIL' "$T/logs/suites/systemd-$suite.log") | $(tail -1 "$T/logs/suites/systemd-$suite.log")"
    docker rm -f "$name" >/dev/null 2>&1
  done
  echo "SYSTEMD_DONE fails=$fails"
  [ "$fails" -eq 0 ]
}
job_gate() {
  local rc=0
  echo "== gate: cargo-gate"; HL_JOB_NAME=${HL_JOB_NAME:-gate}-cargo job_cargo_gate full || rc=1
  echo "== gate: suites"; job_suites || rc=1
  echo "== gate: systemd"; job_systemd || rc=1
  echo "GATE_DONE rc=$rc"
  return $rc
}
job_build_bins() {
  touch_if_new_tree
  brun "${HL_JOB_NAME:-build-bins}" "$HL_CPUS" "$HL_MEM" "$(build_image)" build-bins-in.sh "$HL_PROFILE" || return 1
  cat "$T/out/bins/SHA256SUMS"
}
job_release() {
  touch_if_new_tree
  brun "${HL_JOB_NAME:-release}" "$HL_CPUS" "$HL_MEM" "$(build_image)" release-in.sh || return 1
  cat "$T/out/bins/release/SHA256SUMS"
}
job_import_bins() { # <fromtag> [a|b]
  local from=$1 side=${2:-} F B f
  hl_valid_tag "$from" || die "bad tag $from"
  F=$ROOT/$from/out/bins; B=$SRC/tools/replay/.cache/bin
  [ -f "$F/SHA256SUMS" ] || die "$from has no out/bins (run build-bins there)"
  (cd "$F" && sha256sum -c --quiet SHA256SUMS) || die "$from's out/bins fail their own SHA256SUMS"
  mkdir -p "$B"
  case "$side" in
    "") for f in xorgate-rs xorgate-replay rig-statvfs.so; do cp "$F/$f" "$B/$f"; done ;;
    a|b) cp "$F/xorgate-rs" "$B/xorgate-rs-$side" ;;
    *) die "side must be a or b" ;;
  esac
  { echo "# imported $(ts) from $from$([ -n "$side" ] && echo " as side $side")"; cat "$F/HEAD"; } >> "$B/IMPORTS"
  (cd "$B" && sha256sum xorgate-rs* xorgate-replay rig-statvfs.so 2>/dev/null)
}
job_import_tapes() { # <fromtag> <name>...: rig tapes of another tag (sha recorded)
  local from=$1 n F D; shift
  hl_valid_tag "$from" || die "bad tag $from"
  F=$ROOT/$from/src/tools/replay/.cache/tapes; D=$SRC/tools/replay/.cache/tapes
  [ $# -gt 0 ] || die "name the tapes"
  mkdir -p "$D"
  for n in "$@"; do
    [[ $n =~ ^[A-Za-z0-9._-]+$ ]] || die "bad tape name $n"
    [ -f "$F/$n.ndjson.gz" ] || die "$from has no tape $n"
    cp "$F/$n.ndjson.gz" "$D/"
    [ -f "$F/$n.ndjson.gz.cli.ndjson" ] && cp "$F/$n.ndjson.gz.cli.ndjson" "$D/"
  done
  (cd "$D" && sha256sum ./*.ndjson.gz) | sed 's#  \./#  #' | tee "$T/out/tapes.sha256"
}
# The rig on amd64 WITHOUT editing the tree: a compose override (platform,
# caps, the helper label) + Dockerfile.rig with its one arm64 FROM rewritten.
rig_env() { # <compose-project>
  local df=$SRC/tools/replay/Dockerfile.rig n
  n=$(grep -c '^FROM --platform=linux/arm64 ' "$df" || true)
  [ "$n" = 1 ] || die "Dockerfile.rig: expected exactly one 'FROM --platform=linux/arm64' line (found $n)"
  # The rewritten Dockerfile lives INSIDE the build context (git-ignored
  # .cache/): compose v5 builds through `buildx bake`, which refuses a
  # Dockerfile outside the context without an fs entitlement.
  mkdir -p "$T/gen" "$SRC/tools/replay/.cache/hl"
  sed 's#^FROM --platform=linux/arm64 #FROM --platform=linux/amd64 #' "$df" > "$SRC/tools/replay/.cache/hl/Dockerfile.rig.amd64"
  cat > "$T/gen/compose.hl.yml" <<EOF
# generated by hl-job.sh (rig on linux/amd64; caps; helper label)
services:
  mosquitto:
    platform: linux/amd64
    cpus: 1
    labels:
      xorgate.hl: "1"
  rig:
    platform: linux/amd64
    build:
      dockerfile: .cache/hl/Dockerfile.rig.amd64
    cpus: $HL_RIG_CPUS
    mem_limit: $HL_RIG_MEM
    labels:
      xorgate.hl: "1"
EOF
  export COMPOSE_FILE="$SRC/tools/replay/docker-compose.yml:$T/gen/compose.hl.yml"
  export COMPOSE_PROJECT_NAME=$1
  RIG_IMAGE="xorgate-replay-rig:hl-amd64-$(sha256sum < "$SRC/tools/replay/.cache/hl/Dockerfile.rig.amd64" | cut -c1-12)"
  export RIG_IMAGE
  export RIG_JS_CACHE=$SHARED/jscache-amd64 JS_REF=${HL_JS_REF} XORGATE_AGENT_LABEL=$TAG
}
project() { printf 'hl_%s_%s' "$TAG" "$(printf '%s' "$1" | tr '[:upper:].' '[:lower:]-' | tr -cd 'a-z0-9_-')"; }
job_synth() {
  local t rc=0
  rig_env "$(project synth)"
  mkdir -p "$SRC/tools/replay/.cache/tapes" "$SRC/tools/replay/.cache/runs"
  "$SRC/tools/replay/gen-certs.sh" >/dev/null
  (cd "$SRC/tools/replay" && docker compose build --quiet rig) || die "rig image build failed"
  for t in "$@"; do
    if (cd "$SRC/tools/replay" && docker compose run --rm -T --no-deps rig synth "$t" -o "/rig/out/$t.ndjson.gz" --fixtures /rig/fixtures >/dev/null); then
      mv "$SRC/tools/replay/.cache/runs/$t.ndjson.gz" "$SRC/tools/replay/.cache/tapes/"
    else echo "FAIL synth $t"; rc=1; fi
  done
  (cd "$SRC/tools/replay" && docker compose down -v --remove-orphans >/dev/null 2>&1)
  (cd "$SRC/tools/replay/.cache/tapes" && sha256sum ./*.ndjson.gz) | sed 's#  \./#  #' > "$T/out/tapes.sha256"
  cat "$T/out/tapes.sha256"
  return $rc
}
job_lanes() { # <items file, relative to the tag dir>
  local items=$T/$1
  [ -f "$items" ] || die "no items file $items"
  mkdir -p "$T/out/runs"
  rig_env "$(project lanes)"
  (cd "$SRC/tools/replay" && "$SRC/tools/replay/gen-certs.sh" >/dev/null && docker compose build --quiet rig) || die "rig image build failed"
  echo "== lanes $1 on $(sed -n 's/^head=//p' "$T/source.txt") rig image $RIG_IMAGE caps ${HL_RIG_CPUS} CPUs/${HL_RIG_MEM}"
  (cd "$SRC/tools/replay/.cache/bin" && sha256sum xorgate-rs* xorgate-replay rig-statvfs.so 2>/dev/null) | tee "$T/out/lanes-bins.sha256"
  HL_HELPER_IMAGE=$(script_image)
  docker image inspect "$HL_HELPER_IMAGE" >/dev/null 2>&1 || die "helper image $HL_HELPER_IMAGE missing: run the images job"
  TAG=$TAG T=$T SRC=$SRC HL_HELPER_IMAGE=$HL_HELPER_IMAGE bash "$HERE/hl-lanes.sh" "$items"
}
job_images() {
  local img sid ok=0
  # 1. the amd64 build image, by the tree's own recipe and name.
  img=$(build_image)
  if docker image inspect "$img" >/dev/null 2>&1; then echo "build image $img present ($(docker image inspect -f '{{.Id}}' "$img"))"
  else
    echo "building $img"
    (cd "$SRC" && bash scripts/docker-build.sh --platform linux/amd64 dockerfile) \
      | docker build --platform linux/amd64 --label xorgate.agent=hl-shared --label xorgate.hl=1 -f - \
          --build-arg "RUST_TOOLCHAIN=$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$SRC/rust-toolchain.toml" | head -n1)" \
          -t "$img" "$SRC" || ok=1
  fi
  docker run --rm --label "xorgate.agent=$TAG" "$img" sh -c 'rustc -V; cargo -V; pkg-config --modversion gstreamer-1.0; uname -m' || ok=1
  # 2. the script-tests image (Dockerfile.script-tests, tagged by its hash).
  sid=$(script_image)
  if docker image inspect "$sid" >/dev/null 2>&1; then echo "script-tests image $sid present"
  else docker build --platform linux/amd64 --label xorgate.agent=hl-shared --label xorgate.hl=1 -t "$sid" -f "$SCRIPT_TESTS_DF" "$HERE" || ok=1; fi
  # 3. cargo-deny, x86_64 musl, checked against the release's .sha256.
  if [ -x "$(deny_bin)" ]; then echo "cargo-deny $HL_DENY_VERSION present"
  else
    local d u
    d=$(mktemp -d); u=https://github.com/EmbarkStudios/cargo-deny/releases/download/$HL_DENY_VERSION/cargo-deny-$HL_DENY_VERSION-x86_64-unknown-linux-musl.tar.gz
    if curl -fsSL -o "$d/c.tgz" "$u" && curl -fsSL -o "$d/c.sha" "$u.sha256" \
       && [ "$(cut -d' ' -f1 < "$d/c.sha")" = "$(sha256sum < "$d/c.tgz" | cut -c1-64)" ]; then
      mkdir -p "$SHARED/tools" && tar -xzf "$d/c.tgz" -C "$d" && cp "$d"/cargo-deny-*/cargo-deny "$(deny_bin)" && chmod 0755 "$(deny_bin)"
      echo "cargo-deny $HL_DENY_VERSION installed (sha256 $(cut -c1-12 < "$d/c.sha"))"
    else echo "cargo-deny download or checksum FAILED"; ok=1; fi
    rm -rf "$d"
  fi
  return $ok
}

cmd_exec() {
  tagdir "$1"; local job=$2; shift 2
  local name=${HL_JOB_NAME:-$job} rc=0
  echo "== $(ts) $job $* (tag $TAG, $(sed -n 's/^head=//p' "$T/source.txt"), caps ${HL_CPUS} CPUs/${HL_MEM})"
  date -u +%s > "$T/logs/$name.start"
  # A subshell: a job's `die` ends the job, never the bookkeeping below.
  (
    case "$job" in
      gate) job_gate ;;
      cargo-gate) job_cargo_gate full ;;
      test) job_cargo_gate test ;;
      cargo) job_cargo "$@" ;;
      suites) job_suites ;;
      systemd) job_systemd ;;
      build-bins) job_build_bins ;;
      release) job_release ;;
      import-bins) job_import_bins "$@" ;;
      import-tapes) job_import_tapes "$@" ;;
      synth) job_synth "$@" ;;
      lanes) job_lanes "$@" ;;
      images) job_images ;;
      *) echo "unknown job $job"; exit 2 ;;
    esac
  ) || rc=$?
  date -u +%s > "$T/logs/$name.end"
  rm -f "$SHARED/res/${TAG}_$name"
  echo "== $(ts) $job done rc=$rc ($(( $(cat "$T/logs/$name.end") - $(cat "$T/logs/$name.start") )) s)"
  echo "$rc" > "$T/logs/$name.rc"
  return "$rc"
}

# --------------------------------------------------------- status / preflight
others_labelled() { # labelled containers that are not this tag's / a peer's
  docker ps --filter label=xorgate.agent --format '{{.Label "xorgate.agent"}} {{.Names}}' \
    | awk -v me="${TAG:-}" -v peers=",${HL_PEERS}," '$1 != me && index(peers, "," $1 ",") == 0'
}
cmd_preflight() {
  local l o
  l=$(load1); o=$(others_labelled | tr '\n' ';')
  echo "load1=$l (max $HL_MAX_LOAD) mem_avail=$(mem_avail_gb)G helper_held=$(res_sum) others=[$o]"
  awk -v l="$l" -v m="$HL_MAX_LOAD" 'BEGIN { exit !(l <= m) }'
}
cmd_status() {
  local t f
  echo "homelab $(hostname) $(ts) load $(cut -d' ' -f1-3 /proc/loadavg) mem_avail $(mem_avail_gb)G helper_held(cpus mem_gb)=$(res_sum)"
  for t in ${1:-$(cd "$ROOT" 2>/dev/null && for d in */.hl-tag; do [ -f "$d" ] && dirname "$d"; done)}; do
    tagdir "$t"
    echo "-- $TAG: $(sed -n 's/^head=//p' "$T/source.txt" 2>/dev/null) ($(sed -n 's/^branch=//p' "$T/source.txt" 2>/dev/null)) dirty=$(sed -n 's/^dirty_entries=//p' "$T/source.txt" 2>/dev/null)"
    for f in "$T"/logs/*.start; do
      [ -f "$f" ] || continue
      local n; n=$(basename "$f" .start)
      if [ -f "$T/logs/$n.rc" ]; then echo "   $n: rc=$(cat "$T/logs/$n.rc")"
      elif command tmux has-session -t "=hl_${TAG}_$n" 2>/dev/null; then echo "   $n: RUNNING ($(( $(date -u +%s) - $(cat "$f") )) s) | $(tail -1 "$T/logs/$n.log" 2>/dev/null | cut -c1-160)"
      else echo "   $n: DIED (no rc, no session)"; fi
    done
    docker ps -a --filter "label=xorgate.agent=$TAG" --format '   container {{.Names}} {{.Status}}'
  done
}

# --------------------------------------------------------------------- clean
cmd_clean() {
  tagdir "$1"
  local s ids p id
  # 1. this tag's tmux sessions (exact prefix hl_<tag>_: tags cannot contain '_').
  for s in $(command tmux ls -F '#{session_name}' 2>/dev/null | grep "^hl_${TAG}_" || true); do tmux kill-session -t "=$s"; echo "killed session $s"; done
  # 2. containers with EXACTLY this tag's label (running or stopped).
  ids=$(docker ps -aq --filter "label=xorgate.agent=$TAG")
  # shellcheck disable=SC2086
  [ -z "$ids" ] || { docker rm -f $ids >/dev/null; echo "removed containers: $(echo "$ids" | wc -w)"; }
  # 3. compose networks/volumes of this tag's rig projects (hl_<tag>_*).
  for p in $(docker network ls --format '{{.Label "com.docker.compose.project"}}' | sort -u | grep "^hl_${TAG}_" || true); do
    for id in $(command docker network ls -q --filter "label=com.docker.compose.project=$p"); do docker network rm "$id" >/dev/null; done
    for id in $(command docker volume ls -q --filter "label=com.docker.compose.project=$p"); do docker volume rm "$id" >/dev/null; done
    echo "removed compose leftovers of $p"
  done
  # 4. its target dir, then its dir (root-owned leftovers through a labelled container).
  # The helper's own amd64 image: the homelab's `alpine:latest` is an arm64
  # image (exec format error without binfmt), and re-pulling would retag it.
  local img; img=$(script_image)
  docker image inspect "$img" >/dev/null 2>&1 || die "script-tests image $img missing (run the images job): nothing removed"
  if docker run --rm --platform linux/amd64 --label "xorgate.agent=$TAG" -v "$HL_TARGET_VOL:/target" "$img" rm -rf "/target/$TAG"; then
    echo "removed /target/$TAG"
  else die "could not remove /target/$TAG"; fi
  rm -f "$SHARED/res/${TAG}_"*
  if ! rm -rf "${T:?}" 2>/dev/null; then
    docker run --rm --platform linux/amd64 --label "xorgate.agent=$TAG" -v "$ROOT:/r" "$img" rm -rf "/r/$TAG"
    [ "${HL_DRYRUN:-0}" = 1 ] || rm -rf "${T:?}"
  fi
  echo "removed $T"
}

# ------------------------------------------------------------------ js-cache
cmd_js_cache() { # <tag> <ref>; tar on stdin
  tagdir "$1"; local ref=$2 dest tmp
  [[ $ref =~ ^[0-9a-f]{7,40}$ ]] || die "ref must be a commit sha"
  dest=$SHARED/jscache-amd64/js-$ref
  if [ -f "$dest/dist/worker.js" ] && [ -f "$dest/dist/cli.js" ]; then cat > /dev/null; echo "js: $dest (exists)"; return 0; fi
  mkdir -p "$SHARED/jscache-amd64"
  tmp=$(mktemp -d "$SHARED/jscache-amd64/.tmp-$ref.XXXX")
  tar -x -C "$tmp" || die "bad archive"
  # = tools/replay/js-build.sh's build, on linux/amd64.
  docker run --rm --label "xorgate.agent=$TAG" --label xorgate.hl=1 --cpus "$HL_CPUS" --memory "$HL_MEM" \
    --platform linux/amd64 -v "$tmp:/js" -w /js -e "HL_UID=$(id -u)" -e "HL_GID=$(id -g)" node:20-bookworm \
    sh -c 'npm ci --no-audit --no-fund --loglevel=error && npm run build >/dev/null && node --version; rc=$?; chown -R "$HL_UID:$HL_GID" /js; exit $rc' \
    || { rm -rf "$tmp"; die "JS build failed"; }
  echo "$ref $(ts)" > "$tmp/.hl-js-ref"
  rm -rf "$dest"; mv "$tmp" "$dest"
  echo "js: $dest (built from $ref, linux/amd64)"
}

[ $# -ge 1 ] || die "usage: hl-job.sh start|exec|status|preflight|clean|list-src|prune-src|verify-src|js-cache …"
c=$1; shift
case "$c" in
  start) [ $# -ge 2 ] || die "start <tag> <job> [args]"; cmd_start "$@" ;;
  exec) cmd_exec "$@" ;;
  status) cmd_status "$@" ;;
  preflight) cmd_preflight ;;
  clean) cmd_clean "$@" ;;
  list-src) cmd_list_src "$@" ;;
  prune-src) cmd_prune_src "$@" ;;
  verify-src) cmd_verify_src "$@" ;;
  js-cache) cmd_js_cache "$@" ;;
  *) die "unknown command $c" ;;
esac
