# shellcheck shell=bash disable=SC2034  # the sourcing scripts use these
# Shared defaults for the homelab execution helper (scripts/remote/hl.sh on the
# Mac, scripts/remote/hl-job.sh on the homelab). Sourced, never run. Every
# value is an environment override; the defaults are the plan's (handbook
# "HOMELAB EXECUTION").
HL_HOST=${HL_HOST:-ai@192.168.8.107}
# Remote root, relative to the remote $HOME (hl-job.sh resolves it there).
HL_ROOT_REL=${HL_ROOT_REL:-xorgate-rs-p7}
# Docker volumes on the homelab. The target volume is shared by every tag; each
# tag (= ONE tree) builds only into /target/<tag>.
HL_TARGET_VOL=${HL_TARGET_VOL:-xorgate-rs-p7-target}
HL_REG_VOL=${HL_REG_VOL:-xorgate-rs-p7-cargo-registry}
HL_GIT_VOL=${HL_GIT_VOL:-xorgate-rs-p7-cargo-git}
# Per-job caps (one job = its containers, one after the other).
HL_CPUS=${HL_CPUS:-16}
HL_MEM=${HL_MEM:-32g}
# Guard rails across ALL helper jobs on the host (sum of the running jobs'
# HL_CPUS, and the memory left for the simulators/ollama/the rest).
HL_TOTAL_CPUS=${HL_TOTAL_CPUS:-24}
HL_MIN_FREE_GB=${HL_MIN_FREE_GB:-16}
# Quiet-host preflight for rig lanes: homelab 1-min load.
HL_MAX_LOAD=${HL_MAX_LOAD:-8}
# Pinned tools.
HL_DENY_VERSION=${HL_DENY_VERSION:-0.20.2}
HL_JS_REF=${HL_JS_REF:-09da38d}

# A tag names ONE tree on the homelab: ~/xorgate-rs-p7/<tag>/, /target/<tag>,
# label xorgate.agent=<tag>. Lower-case, digits and '-' only, so the session
# and compose-project prefixes `hl_<tag>_` can never match another tag.
hl_valid_tag() {
  [[ ${1:-} =~ ^[a-z0-9][a-z0-9-]{0,39}$ ]]
}
hl_valid_job() {
  [[ ${1:-} =~ ^[a-z0-9][a-z0-9.-]{0,39}$ ]]
}
# Docker memory string (16g, 512m) -> whole GiB, rounded up. (bash 3.2-safe:
# the Mac's /bin/bash runs hl.sh.)
hl_mem_gb() {
  local m
  m=$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')
  case "$m" in
    [0-9]*g) [[ ${m%g} =~ ^[0-9]+$ ]] && { echo "${m%g}"; return 0; } ;;
    [0-9]*m) [[ ${m%m} =~ ^[0-9]+$ ]] && { echo $(( (${m%m} + 1023) / 1024 )); return 0; } ;;
  esac
  echo "bad memory value: ${1:-} (use <n>g or <n>m)" >&2
  return 1
}
# --cpus value: a positive number with at most 2 decimals.
hl_valid_cpus() {
  [[ ${1:-} =~ ^[0-9]+(\.[0-9]{1,2})?$ ]] && awk -v c="$1" 'BEGIN { exit !(c > 0) }'
}
# The env vars hl.sh forwards to a remote job (and hl-job.sh records).
HL_FORWARD_VARS="HL_CPUS HL_MEM HL_JOBS HL_TOTAL_CPUS HL_MIN_FREE_GB HL_MAX_LOAD HL_JOB_NAME HL_PEERS HL_PREFLIGHT HL_WAIT_OTHERS HL_RIG_CPUS HL_RIG_MEM HL_RUN_SECS HL_JS_REF HL_SUITES HL_SUITE_CPUS HL_SUITE_MEM HL_PROFILE HL_DENY_VERSION HL_PREFLIGHT_STEP HL_PREFLIGHT_MAX"
