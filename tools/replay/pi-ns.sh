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
set -eu
pi=$1
shift
mount --bind "$pi/boot" /boot/firmware
mount -t tmpfs -o mode=0755 xorgate-rig-fw /sys/firmware
mkdir -p /sys/firmware/devicetree/base
mount --bind "$pi/dt" /sys/firmware/devicetree/base
exec "$@"
