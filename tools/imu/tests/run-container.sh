#!/bin/sh
# WP0a tooling gate, run INSIDE a throwaway bookworm container (python3 3.11.2
# and strace 6.1, as on the bench). From the repo root on the homelab:
#   docker run --rm --label xorgate.agent=p7c-tools --cpus 2 \
#     -v "$PWD/tools/imu:/imu:ro" debian:bookworm-slim sh /imu/tests/run-container.sh
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends python3 python3-pip strace procps >/dev/null 2>&1
pip3 install -q --break-system-packages ruff==0.8.4 >/dev/null 2>&1
cp -r /imu /work && cd /work
echo "python: $(python3 --version)  strace: $(strace -V | head -1)  ruff: $(ruff --version)"
echo "== py_compile"
python3 -m py_compile capture/bno085_read.py imutx.py scrub_imutx.py imutx scrub-imutx tests/*.py && echo ok
echo "== ruff"
ruff check --config ruff.toml . && echo "ruff check: ok"
echo "== unittest"
python3 -m unittest discover -s tests -v 2>&1 | tail -n 60
echo "== mutations"
python3 tests/mutations.py
