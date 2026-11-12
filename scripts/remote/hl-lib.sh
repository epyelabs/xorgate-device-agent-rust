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
# budgets, and the memory left for the simulators/ollama/the rest). These are
# the plan's HARD limits: an override may only make them STRICTER (lower
# HL_TOTAL_CPUS, higher HL_MIN_FREE_GB); hl-job.sh refuses a looser value
# (review F3). HL_GUARD_TESTING=1 (never forwarded by hl.sh) lifts that for
# scripts/tests/hl.sh only.
HL_CAP_TOTAL_CPUS=24
HL_CAP_MIN_FREE_GB=16
HL_TOTAL_CPUS=${HL_TOTAL_CPUS:-$HL_CAP_TOTAL_CPUS}
HL_MIN_FREE_GB=${HL_MIN_FREE_GB:-$HL_CAP_MIN_FREE_GB}
# Quiet-host preflight for rig lanes: homelab 1-min load.
HL_MAX_LOAD=${HL_MAX_LOAD:-8}
# Pinned tools.
HL_DENY_VERSION=${HL_DENY_VERSION:-0.20.2}
HL_JS_REF=${HL_JS_REF:-09da38d}

# Tag names that must never be a helper tag (review F14): `clean <tag>` removes
# containers by `label=xorgate.agent=<tag>`, and these labels are carried by
# images (`p7e`: the amd64 build image; `hl-shared`: the helper's images), by
# the rig's compose default (`rig`) or by hand-made bring-up containers.
HL_RESERVED_TAGS="rig hl-shared p7e hl-bringup shared"
# A tag names ONE tree on the homelab: ~/xorgate-rs-p7/<tag>/, /target/<tag>,
# label xorgate.agent=<tag>. Lower-case, digits and '-' only, so the session
# and compose-project prefixes `hl_<tag>_` can never match another tag.
hl_valid_tag() {
  [[ ${1:-} =~ ^[a-z0-9][a-z0-9-]{0,39}$ ]] || return 1
  case " $HL_RESERVED_TAGS " in *" $1 "*) return 1 ;; esac
  return 0
}
hl_valid_job() {
  [[ ${1:-} =~ ^[a-z0-9][a-z0-9.-]{0,39}$ ]]
}
# The jobs `run` accepts (review F12: checked on the Mac BEFORE the remote shell
# parses anything, and again on the homelab).
HL_JOBS_ALLOWED="gate cargo-gate test cargo suites systemd build-bins release import-bins import-tapes synth lanes images"
hl_known_job() {
  case " $HL_JOBS_ALLOWED " in *" ${1:-} "*) return 0 ;; esac
  return 1
}
# Which reservations a job holds against the "fat-LTO runs alone" rule
# (handbook; fix-check H2): `lto` = a fat-LTO release link (build-bins,
# release) runs only when NO other cargo job of ANY tag is reserved, and no
# cargo job starts while one runs; `cargo` = the other cargo jobs.
hl_job_kind() {
  case "${1:-}" in
    build-bins|release) echo lto ;;
    gate|cargo-gate|test|cargo) echo cargo ;;
    *) echo none ;;
  esac
}
# Docker memory string (16g, 512m) -> whole GiB, rounded up. (bash 3.2-safe:
# the Mac's /bin/bash runs hl.sh.)
hl_mem_gb() {
  local m
  m=$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')
  # Zero is refused (fix-check S2): docker reads --memory 0 as NO limit.
  case "$m" in
    [0-9]*g) [[ ${m%g} =~ ^[0-9]+$ ]] && [ "${m%g}" -gt 0 ] && { echo "${m%g}"; return 0; } ;;
    [0-9]*m) [[ ${m%m} =~ ^[0-9]+$ ]] && [ "${m%m}" -gt 0 ] && { echo $(( (${m%m} + 1023) / 1024 )); return 0; } ;;
  esac
  echo "bad memory value: ${1:-} (use <n>g or <n>m, > 0)" >&2
  return 1
}
# --cpus value: a positive number with at most 2 decimals.
hl_valid_cpus() {
  [[ ${1:-} =~ ^[0-9]+(\.[0-9]{1,2})?$ ]] && awk -v c="$1" 'BEGIN { exit !(c > 0) }'
}
# The env vars hl.sh forwards to a remote job (and hl-job.sh records).
HL_FORWARD_VARS="HL_CPUS HL_MEM HL_JOBS HL_TOTAL_CPUS HL_MIN_FREE_GB HL_MAX_LOAD HL_JOB_NAME HL_PEERS HL_PREFLIGHT HL_WAIT_OTHERS HL_RIG_CPUS HL_RIG_MEM HL_RUN_SECS HL_JS_REF HL_SUITES HL_SUITE_CPUS HL_SUITE_MEM HL_PROFILE HL_DENY_VERSION HL_PREFLIGHT_STEP HL_PREFLIGHT_MAX HL_JS_CPUS HL_JS_MEM HL_STOP_WAIT HL_MUTANTS HL_SUITE_TIMEOUT HL_SAMPLE_SECS"
