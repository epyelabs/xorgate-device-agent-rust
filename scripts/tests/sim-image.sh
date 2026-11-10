#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2034,SC1003,SC2020,SC2317,SC2329,SC2016  # `a && b || c` asserts; vars + functions used inside check's eval; tr char sets; a literal $ in sed
# Phase 7e WP2: the Rust simulator image's recipe, checked without building it.
#   - sim/Dockerfile: built from source (no JS image), SDK + fork pinned, the
#     agent stage's apt list = Dockerfile.build's;
#   - sim/build-image.sh: honest identity, `xorgate-sim-rs:*` tags only, no
#     image labels;
#   - scripts/docker-build.sh: the default arm64 image tag is still the hash of
#     Dockerfile.build; --platform linux/amd64 gets its own name; amd64 `pack`
#     is refused before any docker call;
#   - sim/entrypoint.sh, run for real against a STUB agent: argv/env, exit 0,
#     exit 3 + marker + parked restart, required inputs, and D74 (a `docker
#     stop` waits for the agent's own graceful exit, also through a power_off's
#     exit 3, also when the TERM races the exit; a signal-killed agent's
#     128+n passes through), with NEGATIVE controls: the JS shape (one `wait`)
#     exits 143 before the agent finishes, and loses a racing power_off.
# Run as root in a throwaway container (it installs a stub at
# /opt/xorgate-rs/bin/xorgate-rs):
#   docker run --rm -v "$PWD:/src:ro" -w /src debian:bookworm bash scripts/tests/sim-image.sh
# Race stress (review-wp2-fixes N6): SIM_RACE_RUNS=<n> (default 20) sets the
# iterations of the TERM-races-a-power_off case, e.g. -e SIM_RACE_RUNS=600.
set -uo pipefail
[ -f /.dockerenv ] && [ "$(id -u)" = 0 ] || { echo "refusing: run me as root in a throwaway container (see header)"; exit 2; }
SRC=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
fail=0
ok() { echo "PASS: $1"; }
bad() { echo "FAIL: $1"; fail=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }
command -v jq >/dev/null || { apt-get update -qq >/dev/null && apt-get install -y -qq jq >/dev/null; }
command -v git >/dev/null || apt-get install -y -qq git >/dev/null 2>&1 || { apt-get update -qq >/dev/null && apt-get install -y -qq git >/dev/null; }
DF="$SRC/sim/Dockerfile"
W=$(mktemp -d)

echo "== sim/Dockerfile"
check "no stage or COPY comes from a JS (xorgate-sim) image" "! grep -nE '^(FROM|COPY).*xorgate-sim(:|[[:space:]])' '$DF'"
check "KVS SDK pinned to v1.19.1 = d7322f63" "grep -q '^ARG KVS_SDK_TAG=v1.19.1$' '$DF' && grep -q '^ARG KVS_SDK_COMMIT=d7322f63af3c600ee7031b28436e3f8a12664272$' '$DF'"
check "the SDK commit is verified after the clone" "grep -q 'rev-parse HEAD)\" = \"\${KVS_SDK_COMMIT}\"' '$DF'"
check "the fork is pinned to a full commit and verified" "grep -qE '^ARG KVS_FORK_COMMIT=[0-9a-f]{40}$' '$DF' && grep -q 'rev-parse HEAD)\" = \"\${KVS_FORK_COMMIT}\"' '$DF'"
check "the agent stage is debian:bookworm" "grep -q '^FROM debian:bookworm AS agent-build$' '$DF'"
check "the runtime is ubuntu:24.04" "[ \"\$(grep '^FROM ' '$DF' | tail -1)\" = 'FROM ubuntu:24.04' ]"
check "cargo build --release --locked -p xorgate-agent" "grep -q 'cargo build --release --locked -p xorgate-agent' '$DF'"
check "build-info.json is stamped by scripts/build-info.sh" "grep -q 'scripts/build-info.sh \"\${BUILD_CHANNEL}\" --version \"\${GIT_SHA}\"' '$DF'"
check "no sha = refuse" "grep -q 'GIT_SHA is required' '$DF'"
check "both binaries are ldd-checked at build time" "grep -q 'ldd /opt/xorgate/media/xorgate-kvs-webrtc | grep \"not found\"' '$DF' && grep -q 'ldd /opt/xorgate-rs/bin/xorgate-rs | grep \"not found\"' '$DF'"
apt_list() { # the package tokens of the first `apt-get install` after $2 in $1
  awk -v start="$2" '
    index($0, start) {on=1}
    on && /apt-get install/ {grab=1}
    grab {line=line " " $0; if ($0 !~ /\\$/) {print line; exit}}' "$1" |
    tr ' \\' '\n\n' | grep -vE '^(|&&|apt-get|install|-y|--no-install-recommends|rm|-rf|/var/lib/apt/lists/\*)$' | sort
}
a=$(apt_list "$SRC/Dockerfile.build" "FROM ")
b=$(apt_list "$DF" "AS agent-build")
check "agent-build apt list = Dockerfile.build's ($(echo "$a" | wc -w) packages)" "[ -n \"\$a\" ] && [ \"\$a\" = \"\$b\" ]"
tc=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$SRC/rust-toolchain.toml")
check "RUST_TOOLCHAIN default = rust-toolchain.toml ($tc)" "grep -q '^ARG RUST_TOOLCHAIN=$tc\$' '$DF'"
check "the dockerignore keeps .git and the aarch64 media out" "grep -qx '\*\*/.git' '$SRC/sim/Dockerfile.dockerignore' && grep -qx 'media/lib' '$SRC/sim/Dockerfile.dockerignore' && grep -qx 'media/xorgate-kvs-webrtc' '$SRC/sim/Dockerfile.dockerignore'"
# S1 (review-wp2): every source is touched in the SAME RUN, before the build
# that uses the shared target cache, so no stale workspace crate is reused.
run_block=$(awk '/^RUN --mount=type=cache,id=xorgate-sim-rs-registry/ {on=1} on {print} on && !/\\$/ {exit}' "$DF")
check "the cached cargo build touches every source first, in the same RUN" "printf '%s\n' \"\$run_block\" | grep -q 'find /src -path /src/target -prune -o -type f -exec touch {} +' && [ \"\$(printf '%s\n' \"\$run_block\" | grep -n 'exec touch' | cut -d: -f1)\" -lt \"\$(printf '%s\n' \"\$run_block\" | grep -n 'cargo build' | cut -d: -f1)\" ]"
# N1 (review-wp2-fixes): the target mount of THAT RUN is sharing=locked, so an
# overlapping build cannot write newer artifacts between this build's touch
# and its cargo run (BuildKit holds a locked mount for the whole RUN).
target_mount=$(printf '%s\n' "$run_block" | grep -E -- '--mount=type=cache,([^ ]*,)?id=xorgate-sim-rs-target(,| )')
check "the target cache mount of that RUN is sharing=locked (touch + build under one lock)" "[ \"\$(printf '%s\n' \"\$target_mount\" | grep -c .)\" = 1 ] && printf '%s\n' \"\$target_mount\" | grep -qE ',sharing=locked( |,|\$)'"
# S5 + N5 (review-wp2-fixes): every spelling BuildKit honours as the parser
# directive (directives are case-insensitive, spaces optional).
syntax_re='^#[[:space:]]*syntax[[:space:]]*='
no_syntax() { ! grep -qiE "$syntax_re" "$1"; }
check "no floating '# syntax=' frontend line (any spelling)" "no_syntax '$DF'"
for d in '# syntax=docker/dockerfile:1' '#syntax=docker/dockerfile:1' '# syntax = docker/dockerfile:1' '#SYNTAX=docker/dockerfile:1'; do
  { printf '%s\n' "$d"; cat "$DF"; } > "$W/df-syntax"
  check "NEGATIVE: the syntax pin catches '$d'" "! no_syntax '$W/df-syntax'"
done
# S4: the image's labels are the Dockerfile's own, nothing else (a container
# inherits every image label; the build-image.sh half is pinned below).
# N7: Dockerfile instructions are case-insensitive and may be indented, so the
# LABEL instruction is matched on its first word, case-folded.
label_keys() { awk 'toupper($1)=="LABEL" {on=1} on {print} on && !/\\$/ {on=0}' "$1" | grep -oE '[A-Za-z0-9._-]+=' | tr -d = | sort | tr '\n' ' '; }
want_labels='dev.xorgate.kvs-fork dev.xorgate.kvs-sdk org.opencontainers.image.revision org.opencontainers.image.title '
check "the Dockerfile's only image labels: OCI title/revision + the two pins ($(label_keys "$DF"))" "[ \"\$(label_keys '$DF')\" = \"\$want_labels\" ]"
for l in 'LABEL xorgate.agent="p7e"' 'label xorgate.agent="p7e"' '  LABEL xorgate.agent="p7e"'; do
  { cat "$DF"; printf '%s\n' "$l"; } > "$W/df-label"
  check "NEGATIVE: the label allowlist catches '$l'" "[ \"\$(label_keys '$W/df-label')\" != \"\$want_labels\" ]"
done
check "the JS timedatectl shim is installed and answers NTPSynchronized=yes" "grep -q '^COPY sim/timedatectl /usr/local/bin/timedatectl$' '$DF' && [ \"\$(sh '$SRC/sim/timedatectl' show -p NTPSynchronized --value)\" = yes ] && ! sh '$SRC/sim/timedatectl' status 2>/dev/null"

echo "== sim/build-image.sh"
B="$SRC/sim/build-image.sh"
cp -a "$SRC/sim" "$W/sim"  # a copy with no .git around it
out=$("$W/sim/build-image.sh" --dry-run 2>&1); rc=$?
check "no git and no --sha: refuses (rc 2)" "[ $rc = 2 ] && [[ \"\$out\" == *'pass --sha'* ]]"
out=$("$B" --sha abc1234 --dry-run 2>&1)
check "default tag xorgate-sim-rs:<sha>, sha + channel passed" "[[ \"\$out\" == *'-t xorgate-sim-rs:abc1234 '* ]] && [[ \"\$out\" == *'GIT_SHA=abc1234'* ]] && [[ \"\$out\" == *'BUILD_CHANNEL=bench'* ]]"
for t in xorgate-sim:dev xorgate-sim:prod xorgate-sim:latest other:1 xorgate-sim-rs: ; do
  "$B" --sha abc1234 --tag "$t" --dry-run >/dev/null 2>&1; rc=$?
  check "refuses tag '$t'" "[ $rc = 2 ]"
done
for s in 'abc 1' 'a;b' '-x' 'ABC'; do
  "$B" --sha "$s" --dry-run >/dev/null 2>&1; rc=$?
  check "refuses sha '$s'" "[ $rc = 2 ]"
done
out=$("$B" --sha abc1234-dirty --channel ci --tag xorgate-sim-rs:x --dry-run 2>&1); rc=$?
check "channel + -dirty sha pass through; the command carries no --label" "[ $rc = 0 ] && [[ \"\$out\" == *'BUILD_CHANNEL=ci'* ]] && [[ \"\$out\" == *'GIT_SHA=abc1234-dirty'* ]] && [[ \"\$out\" != *--label* ]]"
# S4 (review-wp2-fixes): round 0's baked `xorgate.agent=p7e` came through
# `build-image.sh --label`, so every image label is refused, in each spelling
# docker accepts, also after `--`.
for l in '--label xorgate.agent=p7e' '--label=xorgate.agent=p7e' '--label org.example=1' '-- --label xorgate.agent=p7e' '-- --label=xorgate.agent=p7e'; do
  # shellcheck disable=SC2086  # $l is split into its words on purpose
  out=$("$B" --sha abc1234 --dry-run $l 2>&1); rc=$?
  check "refuses an image label: '$l' (rc 2, no build command)" "[ $rc = 2 ] && [[ \"\$out\" == *'no image labels'* ]] && [[ \"\$out\" != *'docker build'* ]]"
done
G=$(mktemp -d)
cp -a "$SRC/sim" "$G/sim"
git -C "$G" init -q && git -C "$G" -c user.name=t -c user.email=t@t add -A && git -C "$G" -c user.name=t -c user.email=t@t commit -qm t
want=$(git -C "$G" rev-parse --short HEAD)
out=$("$G/sim/build-image.sh" --dry-run 2>&1)
check "from git: the short sha ($want)" "[[ \"\$out\" == *\"GIT_SHA=\$want \"* ]] && [[ \"\$out\" == *\"-t xorgate-sim-rs:\$want \"* ]]"
echo x >> "$G/sim/README.md"
out=$("$G/sim/build-image.sh" --dry-run 2>&1)
check "from git: an unclean tree is <sha>-dirty" "[[ \"\$out\" == *\"GIT_SHA=\$want-dirty \"* ]]"
rm -rf "$G"

echo "== scripts/docker-build.sh"
D="$SRC/scripts/docker-build.sh"
arm=$("$D" image-tag); amd=$("$D" --platform linux/amd64 image-tag)
h=$(sha256sum "$SRC/Dockerfile.build" | cut -c1-12)
check "default (arm64) tag = xorgate-rs-build:$tc-<sha256(Dockerfile.build)[:12]> ($arm)" "[ \"\$arm\" = \"xorgate-rs-build:$tc-$h\" ]"
check "amd64 tag has its own name ($amd)" "[[ \"\$amd\" == xorgate-rs-build-amd64:$tc-* ]] && [ \"\${amd##*-}\" != \"$h\" ]"
check "Dockerfile.build has exactly one arm64 FROM (the line amd64 rewrites)" "[ \"\$(grep -c '^FROM --platform=linux/arm64 ' '$SRC/Dockerfile.build')\" = 1 ]"
check "the default Dockerfile text is Dockerfile.build byte for byte" "[ \"\$(\"$D\" dockerfile | sha256sum)\" = \"\$(sha256sum < '$SRC/Dockerfile.build')\" ]"
check "the amd64 text differs from Dockerfile.build in that one FROM line only" "[ \"\$(diff <(\"$D\" --platform linux/amd64 dockerfile) '$SRC/Dockerfile.build' | grep -c '^[<>]')\" = 2 ] && \"$D\" --platform linux/amd64 dockerfile | grep -qx 'FROM --platform=linux/amd64 debian:bookworm'"
amd_hash=$("$D" --platform linux/amd64 dockerfile | sha256sum | cut -c1-12)
check "the amd64 tag is the hash of the amd64 text" "[ \"\${amd##*-}\" = \"$amd_hash\" ]"
"$D" --platform linux/riscv64 image-tag >/dev/null 2>&1; rc=$?
check "an unknown platform is refused" "[ $rc = 2 ]"
# N4 (review-wp2-fixes): amd64 `pack` is refused before ANY docker call (round
# 1's late refusal started a real docker build). A stub docker that logs its
# argv is first on PATH; `--clean` is the control that the stub is reached.
SD=$(mktemp -d)
printf '#!/bin/sh\necho "$*" >> "%s/docker.log"\nexit 0\n' "$SD" > "$SD/docker"; chmod +x "$SD/docker"
PATH="$SD:$PATH" "$D" --clean >/dev/null 2>&1
check "control: the stub docker is reached (--clean calls it)" "grep -q '^volume rm' '$SD/docker.log'"
rm -f "$SD/docker.log"
PATH="$SD:$PATH" "$D" --platform linux/amd64 pack > "$SD/out" 2>&1; rc=$?
check "amd64 pack: refused (rc 2) with ZERO docker calls" "[ $rc = 2 ] && [ ! -e '$SD/docker.log' ] && grep -q 'pack is aarch64-only' '$SD/out'"
rm -rf "$SD"

echo "== sim/entrypoint.sh against a stub agent"
E="$SRC/sim/entrypoint.sh"
mkdir -p /opt/xorgate-rs/bin
STUB=/opt/xorgate-rs/bin/xorgate-rs
fixture() { # $1 = dir
  rm -rf "$1"; mkdir -p "$1/creds" "$1/assets" "$1/state"
  for f in cert.crt private.key ca.pem; do echo "dummy $f" > "$1/creds/$f"; done
  echo '{"deviceId":"dev-stub","channels":[]}' > "$1/creds/kvs.json"
  echo '{"v":1,"scenarios":[]}' > "$1/assets/catalog.json"
}
run_ep() { # $1 = dir, $2 = entrypoint; extra env from the caller
  CREDS_DIR="$1/creds" ASSETS_DIR="$1/assets" XORGATE_HOME="$1/state" IOT_DATA_ENDPOINT=127.0.0.1 bash "$2"
}
T=$(mktemp -d)

# A. argv, env, exit 0.
cat > "$STUB" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" > "$XORGATE_HOME/argv"
env > "$XORGATE_HOME/env"
exit "${STUB_EXIT:-0}"
EOF
chmod +x "$STUB"
fixture "$T/a"
run_ep "$T/a" "$E" > "$T/a.log" 2>&1; rc=$?
check "exit 0 → container exit 0, no marker" "[ $rc = 0 ] && [ ! -f '$T/a/state/power-off' ]"
check "argv = run --mode replay" "[ \"\$(cat '$T/a/state/argv')\" = 'run --mode replay' ]"
getenv() { sed -n "s/^$1=//p" "$T/a/state/env"; }
check "DEVICE_ID from kvs.json" "[ \"\$(getenv DEVICE_ID)\" = dev-stub ]"
check "creds copied into a writable /state/creds (0600)" "[ \"\$(getenv CERT_PATH)\" = '$T/a/state/creds/cert.crt' ] && [ \"\$(stat -c %a '$T/a/state/creds/private.key')\" = 600 ]"
check "replay assets + KVS runtime env" "[ \"\$(getenv XORGATE_REPLAY_ASSETS)\" = '$T/a/assets' ] && [ \"\$(getenv KVS_RUNTIME_BIN)\" = /opt/xorgate/media/xorgate-kvs-webrtc ] && [ \"\$(getenv KVS_PIPELINE_DIR)\" = '$T/a/state/pipelines' ]"
check "a fresh boot id per start (a uuid, not the kernel boot_id)" "getenv XORGATE_BOOT_ID | grep -Eq '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' && [ \"\$(getenv XORGATE_BOOT_ID)\" != \"\$(cat /proc/sys/kernel/random/boot_id)\" ]"
MODE=sim SCENARIO=s-1 run_ep "$T/a" "$E" >/dev/null 2>&1
check "MODE=sim → run --mode sim; SCENARIO → XORGATE_REPLAY_SCENARIO" "[ \"\$(cat '$T/a/state/argv')\" = 'run --mode sim' ] && [ \"\$(getenv XORGATE_REPLAY_SCENARIO)\" = s-1 ]"

# B. power_off: exit 3 → marker; the next start parks.
fixture "$T/b"
STUB_EXIT=3 run_ep "$T/b" "$E" > "$T/b.log" 2>&1; rc=$?
check "exit 3 → container exit 3 + /state/power-off + the parking line" "[ $rc = 3 ] && grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z$' '$T/b/state/power-off' && grep -q '\[sim\] power_off: parking this container (exit 3; marker written)' '$T/b.log'"
rm -f "$T/b/state/argv"
timeout 3 bash -c "CREDS_DIR='$T/b/creds' ASSETS_DIR='$T/b/assets' XORGATE_HOME='$T/b/state' IOT_DATA_ENDPOINT=127.0.0.1 bash '$E'" > "$T/b2.log" 2>&1; rc=$?
check "a start with the marker parks (sleeps; the agent never runs)" "[ $rc = 124 ] && grep -q '\[sim\] parked: power_off marker present since' '$T/b2.log' && [ ! -f '$T/b/state/argv' ]"
STUB_EXIT=1 run_ep "$T/a" "$E" >/dev/null 2>&1; rc=$?
check "any other code passes through (exit 1)" "[ $rc = 1 ]"

# C. required inputs.
fixture "$T/c"
CREDS_DIR="$T/c/creds" ASSETS_DIR="$T/c/assets" XORGATE_HOME="$T/c/state" bash "$E" > "$T/c.log" 2>&1; rc=$?
check "no IOT_DATA_ENDPOINT → exit 1" "[ $rc = 1 ] && grep -q 'IOT_DATA_ENDPOINT is required' '$T/c.log'"
rm "$T/c/creds/kvs.json"
run_ep "$T/c" "$E" > "$T/c.log" 2>&1; rc=$?
check "a missing creds file → exit 1" "[ $rc = 1 ] && grep -q 'missing .*kvs.json' '$T/c.log'"

# D. D74: SIGTERM to the entrypoint (docker stop) waits for the agent.
# stub_term <exit code> <secs>: on TERM, finish for <secs>, write `finalized`, exit <code>.
stub_term() {
  cat > "$STUB" <<STUB
#!/usr/bin/env bash
trap 'sleep $2; echo done > "\$XORGATE_HOME/finalized"; exit $1' TERM
touch "\$XORGATE_HOME/ready"
while :; do sleep 0.1; done
STUB
  chmod +x "$STUB"
}
sigterm_case() { # $1 = dir, $2 = entrypoint → sets rc (999 = hung), fin, marker
  fixture "$1"
  CREDS_DIR="$1/creds" ASSETS_DIR="$1/assets" XORGATE_HOME="$1/state" IOT_DATA_ENDPOINT=127.0.0.1 bash "$2" > "$1.log" 2>&1 &
  local ep=$!
  for _ in $(seq 100); do [ -f "$1/state/ready" ] && break; sleep 0.05; done
  kill -TERM "$ep"
  # Bounded (review-wp2 S6): a regression that never reaches the agent FAILs, never hangs.
  local i
  for i in $(seq 100); do kill -0 "$ep" 2>/dev/null || break; sleep 0.1; done
  if kill -0 "$ep" 2>/dev/null; then
    kill -KILL "$ep" 2>/dev/null; wait "$ep" 2>/dev/null; rc=999
  else
    wait "$ep"; rc=$?
  fi
  fin=no; [ -f "$1/state/finalized" ] && fin=yes
  marker=no; [ -f "$1/state/power-off" ] && marker=yes
  pkill -f "$STUB" 2>/dev/null; sleep 2.2
}
# The JS shape: the `[7e] D74` block put back to the JS lines it replaces
# (09da38d:sim/entrypoint.sh, `child=""` .. `set -e`, with only the worker
# line swapped for the Rust agent, as everywhere), nothing else.
cat > "$T/js-block" <<'JS'
child=""
forward() { [[ -n "$child" ]] && kill -TERM "$child" 2>/dev/null || true; }
trap forward TERM INT
/opt/xorgate-rs/bin/xorgate-rs run --mode "${MODE:-replay}" &
child=$!
set +e
wait "$child"
code=$?
set -e
JS
awk -v blk="$T/js-block" '
  /^# \[7e\] D74 begin/ {while ((getline l < blk) > 0) print l; skip=1; next}
  skip && /^# \[7e\] D74 end$/ {skip=0; next}
  !skip {print}' "$E" > "$T/js-shape.sh"
check "NEGATIVE control is the JS shape (the one D74 block → the 9 JS lines, nothing else)" "[ \"\$(grep -c '^# \[7e\] D74 begin' '$E')\" = 1 ] && [ \"\$(grep -cx '# \[7e\] D74 end' '$E')\" = 1 ] && [ \"\$(sed '/^# \[7e\] D74 begin/,\$d' '$E')\" = \"\$(sed '/^child=\"\"\$/,\$d' '$T/js-shape.sh')\" ] && [ \"\$(sed '1,/^# \[7e\] D74 end\$/d' '$E')\" = \"\$(sed '1,/^set -e\$/d' '$T/js-shape.sh')\" ] && ! grep -qE 'agent-exit|^# \[7e\] D74 (begin|end)' '$T/js-shape.sh'"

stub_term 0 2
sigterm_case "$T/d" "$E"
check "D74: SIGTERM → the agent's own code (0) AFTER its shutdown finished" "[ $rc = 0 ] && [ $fin = yes ] && [ $marker = no ]"
sigterm_case "$T/n" "$T/js-shape.sh"
check "NEGATIVE: the JS shape exits 143 before the agent finished (the bug D74 fixes)" "[ $rc = 143 ] && [ $fin = no ]"

stub_term 3 0.5
sigterm_case "$T/p" "$E"
check "D74: TERM during a power_off (the agent exits 3) → exit 3 AND the marker" "[ $rc = 3 ] && [ $fin = yes ] && [ $marker = yes ]"
sigterm_case "$T/pn" "$T/js-shape.sh"
check "NEGATIVE: the JS shape loses that power_off (143, no marker)" "[ $rc = 143 ] && [ $marker = no ]"

# An agent that the forwarded TERM kills (no handler): its 143 passes through.
cat > "$STUB" <<'STUB'
#!/usr/bin/env bash
touch "$XORGATE_HOME/ready"
while :; do sleep 0.1; done
STUB
chmod +x "$STUB"
sigterm_case "$T/t" "$E"
check "D74: an agent killed by the forwarded TERM → exit 143, no marker, no .agent-* files left" "[ $rc = 143 ] && [ $marker = no ] && [ -z \"\$(ls -A '$T/t/state' | grep '^\.agent-')\" ]"

# N2 + N6 (review-wp2-fixes): the race, by behaviour. The agent ignores TERM,
# sends TERM to the entrypoint itself and exits 3 at once, so the TERM lands
# while the power_off is already exiting. Each run must give exit 3 AND the
# marker. (Round 0's `kill -0` loop lost about 1 run in 4 here; 48869e6's
# re-wait lost ~1-2% as exit 255, N6.) The entrypoint is the topmost ancestor
# running `bash <script>`: the agent's parent is the D74 wrapper (a subshell
# of the entrypoint, same argv) in the Rust shape, the entrypoint itself in
# the JS shape.
cat > "$STUB" <<'STUB'
#!/usr/bin/env bash
trap '' TERM
p=$PPID
while :; do
  read -r _ _ _ pp _ < "/proc/$p/stat"
  [ "$(tr '\0' ' ' < "/proc/$pp/cmdline" 2>/dev/null)" = "bash $RACE_EP " ] || break
  p=$pp
done
kill -TERM "$p"
exit 3
STUB
chmod +x "$STUB"
race() { # $1 = entrypoint, $2 = runs → race_ok, race_bad
  race_ok=0; race_bad=""
  local i
  for i in $(seq "$2"); do
    fixture "$T/r"
    timeout 10 env RACE_EP="$1" CREDS_DIR="$T/r/creds" ASSETS_DIR="$T/r/assets" XORGATE_HOME="$T/r/state" IOT_DATA_ENDPOINT=127.0.0.1 bash "$1" > "$T/r.log" 2>&1; rc=$?
    if [ $rc = 3 ] && [ -f "$T/r/state/power-off" ] && grep -q 'power_off: parking this container' "$T/r.log"; then
      race_ok=$((race_ok + 1))
    else
      race_bad="$race_bad run$i:rc=$rc"
    fi
  done
}
RACE_RUNS=${SIM_RACE_RUNS:-20}
race "$E" "$RACE_RUNS"
echo "race: $race_ok/$RACE_RUNS kept 3 + marker${race_bad:+; lost:$race_bad}"
check "D74 race: TERM landing as a power_off exits keeps 3 + the marker ($race_ok/$RACE_RUNS)" "[ $race_ok = $RACE_RUNS ]"
race "$T/js-shape.sh" 20
check "NEGATIVE: the JS shape loses racing power_offs ($((20 - race_ok))/20 lost)" "[ $race_ok -lt 20 ]"

# An agent killed by a signal (no TERM to the entrypoint): 128+n passes through.
for sig in SEGV KILL; do
  printf '#!/usr/bin/env bash\nkill -%s $$\n' "$sig" > "$STUB"; chmod +x "$STUB"
  fixture "$T/k$sig"
  run_ep "$T/k$sig" "$E" > "$T/k$sig.log" 2>&1; rc=$?
  want=$([ $sig = SEGV ] && echo 139 || echo 137)
  check "an agent killed by SIG$sig → exit $want, no marker" "[ $rc = $want ] && [ ! -f '$T/k$sig/state/power-off' ]"
done

# Two starts get two boot ids; an uploadBucket an earlier run persisted is kept.
cat > "$STUB" <<'STUB'
#!/usr/bin/env bash
echo "$XORGATE_BOOT_ID" >> "$XORGATE_HOME/bootids"
exit 0
STUB
chmod +x "$STUB"
fixture "$T/e"
run_ep "$T/e" "$E" >/dev/null 2>&1
jq '. + {uploadBucket: "persisted-bucket"}' "$T/e/state/creds/kvs.json" > "$T/e/kvs.tmp" && mv "$T/e/kvs.tmp" "$T/e/state/creds/kvs.json"
run_ep "$T/e" "$E" >/dev/null 2>&1
check "two starts, two different boot ids" "[ \"\$(sort -u '$T/e/state/bootids' | wc -l)\" = 2 ]"
check "a persisted uploadBucket survives the next start's creds refresh" "[ \"\$(jq -r .uploadBucket '$T/e/state/creds/kvs.json')\" = persisted-bucket ]"

rm -rf "$T" "$W" "$STUB"
if [ $fail = 0 ]; then echo "== result: ALL PASS"; else echo "== result: FAILURES"; fi
exit $fail
