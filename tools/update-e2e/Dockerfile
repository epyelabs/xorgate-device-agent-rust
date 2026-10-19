# Phase 4 WP4 (E9): the self-update rail end-to-end in a throwaway device.
# debian:bookworm arm64 = the CM5's userland; the Docker-built aarch64
# xorgate-rs runs natively on Apple Silicon. Real curl, real GNU tar, real
# visudo (package `sudo`), python3 for the fake dist bucket
# (`python3 -m http.server`), and the GStreamer runtime the binary links
# (without it the smoke-exec would fail for the wrong reason). systemd is
# NOT installed: `systemctl` is the shim in shims/, which records its argv
# and answers is-active/restart from flag files.
FROM --platform=linux/arm64 debian:bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      curl ca-certificates python3 sudo \
      libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 \
 && rm -rf /var/lib/apt/lists/*
COPY shims/systemctl /usr/local/sbin/systemctl
RUN chmod 0755 /usr/local/sbin/systemctl && useradd -m pi
