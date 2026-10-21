# Phase 5 WP2 (G2): a JS-provisioned device, converted by install-rs.sh, in a
# throwaway container. debian:bookworm arm64 = the CM5's userland (the
# Docker-built aarch64 xorgate-rs runs natively on Apple Silicon). Only what a
# provisioned Bookworm device has and the converter may use (§5.9): bash,
# coreutils, curl, GNU tar, sha256sum, visudo (package `sudo`), find, cmp
# (diffutils, Essential). python3 is the fake dist bucket ONLY
# (`python3 -m http.server`); procps gives pgrep/pkill for the JS-running
# signal. The GStreamer runtime libs are the ones the binary links, so the
# smoke-exec and `xorgate-rs run` work for the right reasons. systemd is NOT
# installed: `systemctl` is shims/systemctl (enablement symlinks from
# WantedBy=, Conflicts= both ways, User= from the unit + drop-ins).
FROM --platform=linux/arm64 debian:bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      curl ca-certificates python3 sudo procps \
      libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 \
 && rm -rf /var/lib/apt/lists/*
# /usr/bin/systemctl: the path the 022/024 sudoers grants name (merged /usr,
# so /bin/systemctl is the same file). node-stub = /usr/bin/node (NodeSource's
# path). journalctl prints what the systemctl shim captured. visudo logs each
# check (order vs the 024 install) and runs the real one, moved aside.
COPY shims/systemctl /usr/bin/systemctl
COPY shims/journalctl /usr/bin/journalctl
COPY shims/node-stub /usr/bin/node
COPY shims/visudo /usr/sbin/visudo.shim
RUN mv /usr/sbin/visudo /usr/sbin/visudo.real \
 && mv /usr/sbin/visudo.shim /usr/sbin/visudo \
 && chmod 0755 /usr/bin/systemctl /usr/bin/journalctl /usr/bin/node /usr/sbin/visudo \
 && useradd -m -s /bin/bash pi
