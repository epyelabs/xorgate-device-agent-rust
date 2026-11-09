#!/usr/bin/env bash
# Phase 7e WP2: the Rust simulator image's recipe, checked without building it.
#   - sim/Dockerfile: built from source (no JS image), SDK + fork pinned, the
#     agent stage's apt list = Dockerfile.build's;
#   - sim/build-image.sh: honest identity, `xorgate-sim-rs:*` tags only;
#   - scripts/docker-build.sh: the default arm64 image tag is still the hash of
#     Dockerfile.build; --platform linux/amd64 gets its own name;
#   - sim/entrypoint.sh, run for real against a STUB agent: argv/env, exit 0,
#     exit 3 + marker + parked restart, required inputs, and D74 (a `docker
#     stop` waits for the agent's own graceful exit), with a NEGATIVE control:
#     the JS shape (one `wait`) exits 143 before the agent finishes.
# Run as root in a throwaway container (it installs a stub at
# /opt/xorgate-rs/bin/xorgate-rs):
#   docker run --rm -v "$PWD:/src:ro" -w /src debian:bookworm bash scripts/tests/sim-image.sh
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
check "agent-build apt list = Dockerfile.build's ($(echo $a | wc -w) packages)" "[ -n \"\$a\" ] && [ \"\$a\" = \"\$b\" ]"
tc=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$SRC/rust-toolchain.toml")
check "RUST_TOOLCHAIN default = rust-toolchain.toml ($tc)" "grep -q '^ARG RUST_TOOLCHAIN=$tc\$' '$DF'"
check "the dockerignore keeps .git and the aarch64 media out" "grep -qx '\*\*/.git' '$SRC/sim/Dockerfile.dockerignore' && grep -qx 'media/lib' '$SRC/sim/Dockerfile.dockerignore' && grep -qx 'media/xorgate-kvs-webrtc' '$SRC/sim/Dockerfile.dockerignore'"

echo "== sim/build-image.sh"
B="$SRC/sim/build-image.sh"
W=$(mktemp -d)
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
out=$("$B" --sha abc1234-dirty --channel ci --label xorgate.agent=p7e --tag xorgate-sim-rs:x --dry-run 2>&1)
check "label + channel + -dirty sha pass through" "[[ \"\$out\" == *'--label xorgate.agent=p7e'* ]] && [[ \"\$out\" == *'BUILD_CHANNEL=ci'* ]] && [[ \"\$out\" == *'GIT_SHA=abc1234-dirty'* ]]"
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
"$D" --platform linux/riscv64 image-tag >/dev/null 2>&1; rc=$?
check "an unknown platform is refused" "[ $rc = 2 ]"

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
cat > "$STUB" <<'EOF'
#!/usr/bin/env bash
trap 'sleep 2; echo done > "$XORGATE_HOME/finalized"; exit 0' TERM
touch "$XORGATE_HOME/ready"
while :; do sleep 0.1; done
EOF
chmod +x "$STUB"
sigterm_case() { # $1 = dir, $2 = entrypoint → sets rc, fin
  fixture "$1"
  CREDS_DIR="$1/creds" ASSETS_DIR="$1/assets" XORGATE_HOME="$1/state" IOT_DATA_ENDPOINT=127.0.0.1 bash "$2" > "$1.log" 2>&1 &
  local ep=$!
  for _ in $(seq 100); do [ -f "$1/state/ready" ] && break; sleep 0.05; done
  kill -TERM "$ep"
  wait "$ep"; rc=$?
  fin=no; [ -f "$1/state/finalized" ] && fin=yes
  pkill -f "$STUB" 2>/dev/null; sleep 2.2
}
sigterm_case "$T/d" "$E"
check "D74: SIGTERM → the entrypoint exits with the agent's own code (0) AFTER its shutdown finished" "[ $rc = 0 ] && [ $fin = yes ]"
sed '/^while kill -0 "\$child"/,/^done$/d' "$E" > "$T/js-shape.sh"
check "NEGATIVE control is the JS shape (the loop removed, nothing else)" "[ \"\$(diff '$E' '$T/js-shape.sh' | grep -c '^<')\" = 4 ]"
sigterm_case "$T/n" "$T/js-shape.sh"
check "NEGATIVE: the JS shape exits 143 before the agent finished (the bug D74 fixes)" "[ $rc = 143 ] && [ $fin = no ]"

rm -rf "$T" "$W" "$STUB"
if [ $fail = 0 ]; then echo "== result: ALL PASS"; else echo "== result: FAILURES"; fi
exit $fail
