# Phase 7c WP2: the Python oracle image (plan phase-7c-rust-imu.md §WP2, C4).
# The bench's stack (phase-7c/evidence/wp0.md): Debian bookworm apt python3
# 3.11.2, Blinka 9.1.0, PlatformDetect 3.88.0, PureIO 1.1.11, busdevice 5.2.17,
# bno08x 1.3.3 (hash-pinned in requirements.txt; the interpreter packages pinned
# to the exact Debian build, review-wp2 F10), plus a stub lgpio (0.2.2's
# API surface; stubs/lgpio.py). The harness (oracle.py, scenarios.py, shim/)
# and the helper are MOUNTED at run time, so this image's inputs are only this
# file, requirements.txt and stubs/lgpio.py; its tag is their short sha
# (tools/imu/oracle/run.sh image-tag).
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3=3.11.2-1+b1 python3-pip \
      python3.11=3.11.2-6+deb12u9 python3.11-minimal=3.11.2-6+deb12u9 \
      libpython3.11-stdlib=3.11.2-6+deb12u9 libpython3.11-minimal=3.11.2-6+deb12u9 \
 && rm -rf /var/lib/apt/lists/*
COPY requirements.txt /tmp/requirements.txt
RUN python3 -m pip install --no-cache-dir --break-system-packages --require-hashes -r /tmp/requirements.txt \
 && rm /tmp/requirements.txt
COPY stubs/lgpio.py /usr/local/lib/python3.11/dist-packages/lgpio.py
RUN useradd -m -u 1000 oracle
USER oracle
WORKDIR /home/oracle
