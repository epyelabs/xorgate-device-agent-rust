#!/bin/sh
# Phase 4 rig: run one agent as a Raspberry Pi of its own (crates/xorgate-replay
# src/p4.rs). The player starts it as
#   unshare -m --propagation private -- pi-ns.sh <work>/pi <program> [args…]
# so the mounts below exist ONLY in that agent's private mount namespace:
#   /boot/firmware                  ← <work>/pi/boot  (its own config.txt)
#   /sys/firmware/devicetree/base   ← <work>/pi/dt    (= /proc/device-tree: model,
#                                                      camera rotation bytes)
# Bind mounts of directories, so the player's later DT writes (the simulated
# reboot of p4-cam-mount) show through. Needs CAP_SYS_ADMIN (docker-compose.yml).
# `exec` keeps the pid: the player's /proc/<pid>/fd checks and signals still
# reach the agent itself.
#
# Phase 5b (WP4, R-P3G-4): every agent of a run gets this namespace, and
# $RIG_MEMINFO (the player's per-agent file) is bound over /proc/meminfo, so
# `ram_usage` reads the TAPE's memory, not the rig VM's (which swung 9 → 94 %
# with the scenario's own 1080p encoders, phase-3 gate D3). A non-Pi tape
# passes a <pi> dir without `boot/`: only the meminfo bind is made.
set -eu
pi=$1
shift
if [ -d "$pi/boot" ]; then
  mount --bind "$pi/boot" /boot/firmware
  mount -t tmpfs -o mode=0755 xorgate-rig-fw /sys/firmware
  mkdir -p /sys/firmware/devicetree/base
  mount --bind "$pi/dt" /sys/firmware/devicetree/base
fi
if [ -n "${RIG_MEMINFO:-}" ]; then
  mount --bind "$RIG_MEMINFO" /proc/meminfo
fi
exec "$@"
