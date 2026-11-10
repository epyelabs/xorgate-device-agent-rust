# Xorgate device simulator image, RUST agent (Phase 7e WP2).
#
# A drop-in for the JS simulator image (device/xorgate-device-agent
# sim/Dockerfile @ 09da38d): the same entrypoint contract (/creds ro,
# /assets/<v> ro, /state rw), env, exit codes (reboot → 0, power_off → 3 + the
# /state/power-off marker) and state.json health fields. The only swap is the
# agent: `xorgate-rs run --mode replay` instead of `node dist/worker.js`.
# Everything is built from source here; nothing is copied from a JS image.
#
# Three stages:
#   kvs-build    ubuntu 24.04 + GStreamer dev + the AWS KVS WebRTC C SDK at
#                v1.19.1 (verified against the commit the fork's submodule
#                pins) + the in-house `xorgate-kvs-webrtc` master at a pinned
#                commit of the PUBLIC fork. The SDK-tree libs the master
#                resolves are collected via ldd (the JS image's recipe).
#   agent-build  debian:bookworm + Dockerfile.build's apt list + rustup at the
#                pinned toolchain: `cargo build --release --locked -p
#                xorgate-agent`, then scripts/build-info.sh stamps
#                build-info.json. Bookworm (glibc 2.36, GStreamer 1.22, feature
#                v1_22) is deliberate: the SAME artifact shape as the device
#                build, forward-compatible with 24.04's 1.24 (7e decision R3).
#   runtime      ubuntu 24.04 (on 1.20 the bridge-only file chain stalls after
#                ~1 s; JS Phase 0) + GStreamer base/good/bad/ugly/libav +
#                libcurl/libssl for the master + ca-certificates jq procps +
#                the JS image's `timedatectl` shim (so a hand-run `MODE=sim`
#                reports `ntp` as the JS image does; replay forces `ntp`
#                in-process anyway, worker.rs / `worker.ts:465-469`). No node.
#
# No `# syntax=` line: the built-in Dockerfile frontend (BuildKit) supports
# `RUN --mount`, and a floating `docker/dockerfile:1` would be fetched from
# Docker Hub on every build (review-wp2 S5).
#
# Build context = the Rust repo root. Use sim/build-image.sh (it passes the
# git identity and refuses `xorgate-sim:*` tags):
#   sim/build-image.sh                  # → xorgate-sim-rs:<short sha>
# Every stage builds for the BUILD platform; `docker buildx build --platform
# linux/arm64` gives an arm64 variant (e.g. Fargate) from the same file.

# The pins (global, so the runtime stage can label the image with them).
# The fork pins the SDK as a submodule (xorgate-kvs-webrtc/.gitmodules,
# third_party/amazon-kinesis-video-streams-webrtc-sdk-c @ v1.19.1 = d7322f63).
# The master comes from the PUBLIC fork at a pinned commit: epyelabs/
# xorgate-kvs-webrtc main @ 731b924 (the merge of PR #4, "Bound the encoder
# teardown so a wedged EOS drain cannot dark the channel").
ARG KVS_SDK_TAG=v1.19.1
ARG KVS_SDK_COMMIT=d7322f63af3c600ee7031b28436e3f8a12664272
ARG KVS_FORK_REPO=https://github.com/epyelabs/xorgate-kvs-webrtc.git
ARG KVS_FORK_COMMIT=731b924f467c7d5d1319a45ec94ce5019020ad08

# ------------------------------------------------------------------ kvs-build
FROM ubuntu:24.04 AS kvs-build
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      git ca-certificates cmake m4 pkg-config build-essential \
      libssl-dev libcurl4-openssl-dev liblog4cplus-dev \
      libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  && rm -rf /var/lib/apt/lists/*
ARG KVS_SDK_TAG
ARG KVS_SDK_COMMIT
RUN git clone --depth 1 --branch "${KVS_SDK_TAG}" \
      https://github.com/awslabs/amazon-kinesis-video-streams-webrtc-sdk-c.git /sdk \
  && test "$(git -C /sdk rev-parse HEAD)" = "${KVS_SDK_COMMIT}" \
  || { echo "KVS SDK ${KVS_SDK_TAG} is not ${KVS_SDK_COMMIT}" >&2; exit 1; }
# SDK first (samples off; it fetches + builds its own openssl/libwebsockets/srtp/usrsctp).
RUN cmake -S /sdk -B /sdk/build -DBUILD_SAMPLE=OFF \
  && make -C /sdk/build -j"$(nproc)"
ARG KVS_FORK_REPO
ARG KVS_FORK_COMMIT
RUN git init -q /app \
  && git -C /app fetch -q --depth 1 "${KVS_FORK_REPO}" "${KVS_FORK_COMMIT}" \
  && git -C /app checkout -q FETCH_HEAD \
  && test "$(git -C /app rev-parse HEAD)" = "${KVS_FORK_COMMIT}"
RUN cmake -S /app -B /app/build -DKVS_SDK_DIR=/sdk \
  && make -C /app/build -j"$(nproc)" xorgate-kvs-webrtc
# Runtime layout mirrors the device tarball's media/: binary + cert.pem + lib/.
RUN mkdir -p /out/lib \
  && cp /app/build/xorgate-kvs-webrtc /out/ \
  && cp /sdk/certs/cert.pem /out/cert.pem \
  && ldd /out/xorgate-kvs-webrtc | awk '/=> \/sdk\//{print $3}' | xargs -I{} cp -L {} /out/lib/ \
  && ls -la /out/lib

# ---------------------------------------------------------------- agent-build
FROM debian:bookworm AS agent-build
ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# Keep in sync with Dockerfile.build (scripts/tests/sim-image.sh checks it).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential curl ca-certificates pkg-config git \
      libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libssl-dev lld \
 && rm -rf /var/lib/apt/lists/*
# The pinned toolchain; the RUN after the COPY checks it equals rust-toolchain.toml.
ARG RUST_TOOLCHAIN=1.99.0
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none \
 && rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal --component rustfmt,clippy \
 && rustup default "$RUST_TOOLCHAIN" \
 && rustc -V && pkg-config --modversion gstreamer-1.0
WORKDIR /src
COPY . .
RUN grep -Eq "^channel = \"${RUST_TOOLCHAIN}\"" rust-toolchain.toml \
  || { echo "RUST_TOOLCHAIN ${RUST_TOOLCHAIN} != rust-toolchain.toml" >&2; exit 1; }
# The build identity (Q-E3: the honest Rust build-info): sim/build-image.sh
# passes the short sha (+ "-dirty" for an unclean tree) and the channel. No
# sha = refuse: the image must never report `unknown`.
ARG GIT_SHA=
ARG BUILD_CHANNEL=bench
RUN test -n "${GIT_SHA}" || { echo "GIT_SHA is required (use sim/build-image.sh)" >&2; exit 1; }
# The target cache is shared by every build on this BuildKit host (all shas,
# branches, worktrees), and cargo judges a path crate fresh by mtime: a tree
# whose files are OLDER than the cached artifacts (a `git archive` of an
# earlier commit, an rsync) would silently reuse another tree's workspace
# crates under this sha (review-wp2 S1). So every source file is touched
# first: the workspace crates always rebuild (~1 min), while registry crates,
# judged by version, stay cached. Chosen over dropping the cache (cold
# dependency builds every time) and over a per-sha cache id (unbounded
# growth, one prune per sha).
RUN --mount=type=cache,id=xorgate-sim-rs-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=xorgate-sim-rs-target,target=/src/target \
    find /src -path /src/target -prune -o -type f -exec touch {} + \
 && XORGATE_GIT_SHA="${GIT_SHA}" cargo build --release --locked -p xorgate-agent \
 && mkdir -p /out/bin \
 && cp target/release/xorgate-rs /out/bin/xorgate-rs \
 && scripts/build-info.sh "${BUILD_CHANNEL}" --version "${GIT_SHA}" --out /out/build-info.json \
 && cat /out/build-info.json

# -------------------------------------------------------------------- runtime
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates jq procps \
      gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
      gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav \
      libcurl4t64 libssl3t64 \
  && rm -rf /var/lib/apt/lists/*
COPY --from=kvs-build /out /opt/xorgate/media
# Every shared lib each binary needs must resolve here (the master's SDK-tree
# libs via media/lib, the rest from apt above): fail the build, not the first
# stream.
RUN ! (LD_LIBRARY_PATH=/opt/xorgate/media/lib ldd /opt/xorgate/media/xorgate-kvs-webrtc | grep "not found")
# `<exe dir>/../build-info.json` is where the agent reads its identity.
COPY --from=agent-build /out/bin/xorgate-rs /opt/xorgate-rs/bin/xorgate-rs
COPY --from=agent-build /out/build-info.json /opt/xorgate-rs/build-info.json
RUN ! (ldd /opt/xorgate-rs/bin/xorgate-rs | grep "not found") \
  && /opt/xorgate-rs/bin/xorgate-rs --version
COPY sim/entrypoint.sh /entrypoint.sh
# The JS image's shim, byte for byte (device/xorgate-device-agent sim/timedatectl @ 09da38d).
COPY sim/timedatectl /usr/local/bin/timedatectl
RUN chmod +x /entrypoint.sh /usr/local/bin/timedatectl
ARG GIT_SHA=
ARG KVS_SDK_TAG
ARG KVS_FORK_COMMIT
LABEL org.opencontainers.image.title="xorgate-sim-rs" \
      org.opencontainers.image.revision="${GIT_SHA}" \
      dev.xorgate.kvs-sdk="${KVS_SDK_TAG}" \
      dev.xorgate.kvs-fork="${KVS_FORK_COMMIT}"
ENV XORGATE_HOME=/state
WORKDIR /opt/xorgate-rs
ENTRYPOINT ["/entrypoint.sh"]
