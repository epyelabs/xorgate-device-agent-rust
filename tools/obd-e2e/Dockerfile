# Phase 5b WP2: `xorgate-obd-py:bookworm` — one image for H4 (the vendored
# python/ suite) and H12 (the Rust agent + the vendored OBD helper over an
# ELM327-emulator pty). debian:bookworm arm64 = the CM5's userland and its
# Python 3.11; the Docker-built aarch64 xorgate-rs runs natively on Apple
# Silicon and links GStreamer, hence its runtime libs. ELM327-emulator 4.0.0
# lives in /opt/elm-venv ONLY (test-only, never on a device, README Q1/§4);
# the helper itself is stdlib-only and runs under the system python3.
# No BlueZ: nothing here can pair or scan (`bluetoothctl` is absent).
FROM --platform=linux/arm64 debian:bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      python3 python3-venv ca-certificates procps \
      libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 \
 && rm -rf /var/lib/apt/lists/*
RUN python3 -m venv /opt/elm-venv \
 && /opt/elm-venv/bin/pip install --no-cache-dir "ELM327-emulator==4.0.0" \
 && /opt/elm-venv/bin/pip freeze > /opt/elm-venv/PIP-FREEZE.txt
ENV OBD_ELM_BIN=/opt/elm-venv/bin/elm PYTHONDONTWRITEBYTECODE=1
