# syntax=docker/dockerfile:1
# Rust variant of the device simulator image (Phase 7e SPIKE).
#
# Same contract as device/xorgate-device-agent/sim/Dockerfile (the JS image
# the sim host runs today): ubuntu 24.04 (GStreamer 1.24 — the bridge-only
# file chain stalls on 1.20), the in-house `xorgate-kvs-webrtc` master built
# for this arch, `/entrypoint.sh` reading /creds, /assets/v1, /state, exit 3
# → the power-off marker. The only swap is the agent: the Rust `xorgate-rs`
# binary (built x86_64 on debian:bookworm, GStreamer 1.22 headers, feature
# v1_22 — forward-compatible with 1.24) instead of node + dist/.
#
# SPIKE shortcut: the KVS master + its SDK libs are COPIED from the existing
# JS sim image (`KVS_FROM`, default xorgate-sim:dev) instead of rebuilt — it
# is byte-for-byte the master the prod sims run. The WP that productises this
# builds it from source with the JS Dockerfile's `kvs-build` stage.
#
# Build context = a dir holding this file, entrypoint.sh and `xorgate-rs`
# (the x86_64 release binary):
#   docker build --build-arg KVS_FROM=xorgate-sim:dev -t xorgate-sim-rs:p7e <ctx>
ARG KVS_FROM=xorgate-sim:dev
FROM ${KVS_FROM} AS kvs

FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates jq procps \
      gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
      gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav \
      libcurl4t64 libssl3t64 \
  && rm -rf /var/lib/apt/lists/*
COPY --from=kvs /opt/xorgate/media /opt/xorgate/media
RUN ! (LD_LIBRARY_PATH=/opt/xorgate/media/lib ldd /opt/xorgate/media/xorgate-kvs-webrtc | grep "not found")
COPY xorgate-rs /opt/xorgate-rs/bin/xorgate-rs
RUN ! (ldd /opt/xorgate-rs/bin/xorgate-rs | grep "not found") \
  && /opt/xorgate-rs/bin/xorgate-rs --version
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh /opt/xorgate-rs/bin/xorgate-rs
ENV XORGATE_HOME=/state
WORKDIR /opt/xorgate-rs
ENTRYPOINT ["/entrypoint.sh"]
