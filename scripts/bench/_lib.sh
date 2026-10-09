#!/usr/bin/env bash
# Shared helpers for scripts/bench/*.sh — run from the Mac against the bench CM5.
#
#   BENCH=pi@192.168.8.203   (default; pi@10.0.0.111 on home Wi-Fi)
#
# All bench scripts are idempotent and only ever touch the Rust agent's own
# paths: /opt/xorgate-rs{,.new,.prev}, /usr/local/bin/xorgate-rs,
# /etc/systemd/system/xorgate-agent-rs.service(.d/), /etc/xorgate/agent-rs-dev.env,
# /var/lib/xorgate-rs. They NEVER touch xorgate-agent.service,
# /etc/xorgate/agent.env, /var/lib/xorgate or sim7600-lte.service (the JS prod
# agent and the modem daemon keep running throughout Phase 0-5).
# shellcheck disable=SC2034  # variables are consumed by the sourcing scripts
set -euo pipefail

BENCH="${BENCH:-pi@192.168.8.203}"
BENCH_SSH_OPTS=(-o ConnectTimeout=10 -o ServerAliveInterval=15)
UNIT=xorgate-agent-rs
JS_UNIT=xorgate-agent
RS_ROOT=/opt/xorgate-rs
RS_HOME=/var/lib/xorgate-rs
RS_ENV=/etc/xorgate/agent-rs-dev.env
DEV_BACKUP=/etc/xorgate/dev-backup-20260823
EVIDENCE_LOCAL="${EVIDENCE_LOCAL:-/Users/vynci/vynci/xorgate/plans/device-agent-js-to-rust/phase-0/evidence}"

# shellcheck disable=SC2029  # remote command words are expanded client-side on purpose
bench_ssh() { ssh "${BENCH_SSH_OPTS[@]}" "$BENCH" "$@"; }
# Interactive (tty) variant for `journalctl -f` and friends so Ctrl-C reaches the remote.
bench_ssh_tty() { ssh -t "${BENCH_SSH_OPTS[@]}" "$BENCH" "$@"; }
bench_scp_to() { scp "${BENCH_SSH_OPTS[@]}" -q "$1" "$BENCH:$2"; }
bench_scp_from() { scp "${BENCH_SSH_OPTS[@]}" -q "$BENCH:$1" "$2"; }
# Run a bash script (stdin) as root on the bench; arguments become $1.. there.
# shellcheck disable=SC2120  # callers may pass no args (script via stdin only)
bench_sudo_script() { ssh "${BENCH_SSH_OPTS[@]}" "$BENCH" sudo bash -s -- "$@"; }

log() { printf '\033[1;34m>>\033[0m %s\n' "$*" >&2; }
