# sim/ — the Rust device-simulator image (Phase 7e)

`xorgate-sim-rs` is a drop-in for the JS simulator image (`device/xorgate-device-agent`
`sim/`): the same container contract, with the Rust agent (`xorgate-rs run --mode replay`)
instead of `node dist/worker.js`.

| | |
|---|---|
| Mounts | `/creds` ro (`cert.crt`, `private.key`, `ca.pem`, `kvs.json`), `/assets/v1` ro (the scenario pack: `catalog.json` + `<scenarioId>/…`), `/state` rw (`XORGATE_HOME`) |
| Env | `IOT_DATA_ENDPOINT` (required), `DEVICE_ID` (default: `kvs.json`'s), `XORGATE_UPLOAD_BUCKET`, `SCENARIO`, `MODE` (default `replay`), `AWS_KVS_LOG_LEVEL` |
| Exit codes | `reboot` → 0 (Docker restarts it), `power_off` → 3 + `/state/power-off` (every later start parks until the marker is removed), `docker stop` → the agent's own code after its graceful shutdown (D74) |
| Health | `/state/state.json` `{connection, lastWriteAt, lastError, published}` (the sim host's proof of life) |
| Identity | `effective.agent.build` = the honest Rust build-info: `version` = short git sha, `channel` (`bench` for a hand build, `ci` from the workflow), `packageVersion` = the workspace version (7e Q-E3) |

## Build

Build on the host that runs the sims (x86_64, the homelab), from a git checkout:

```sh
sim/build-image.sh                          # → xorgate-sim-rs:<short sha>
sim/build-image.sh --sha <short sha>        # a tree without .git (rsync / git archive)
```

- Everything is built from source:
  - the KVS WebRTC C SDK at `v1.19.1` (commit-verified);
  - the in-house master from the public `epyelabs/xorgate-kvs-webrtc` fork at a pinned commit;
  - the agent on `debian:bookworm` with the device build's toolchain and apt list.
- No stage copies from a JS image. Pins and rationale are in the `sim/Dockerfile` header.
- `docker buildx build --platform linux/arm64 -f sim/Dockerfile …` (with the same `--build-arg`s) gives an arm64 variant.
- No image labels beyond the Dockerfile's own (the OCI revision and the two pins): Docker copies image labels onto every container created from it, so an `xorgate.agent=<tag>` baked in would put a sim host's containers under an agent's cleanup filter. `sim/build-image.sh` refuses `--label` (also after `--`). Label the containers you create instead.
- The script only ever tags `xorgate-sim-rs:*`. **Never tag `xorgate-sim:dev` or `xorgate-sim:prod`**: a sim host recreates every one of its sims when the image it runs changes.
- CI: `.github/workflows/sim-image-amd64.yml` is manual only (`gh workflow run sim-image-amd64.yml --ref <branch>`). It uploads a `docker save` artifact and pushes nothing.

The agent binary alone, for x86_64, in the device build image:
`scripts/docker-build.sh --platform linux/amd64 build` → `target/docker-amd64/release/xorgate-rs`.

## Run one sim by hand

Use the JS repo's single-container recipe, `device/xorgate-device-agent/sim/docker-compose.sim.yml`, with the Rust image.
- Pass `SIM_IMAGE`, and never pass `--build`: that file's build section builds the JS image.
- Its default `SIM_IMAGE` is `xorgate-sim:dev`.

```sh
cd device/xorgate-device-agent/sim
SIM_IMAGE=xorgate-sim-rs:<sha> SIM_NAME=<name> \
IOT_DATA_ENDPOINT=<iot data endpoint> XORGATE_UPLOAD_BUCKET=<core bucket, dev devices only> \
SIM_CREDS=/path/to/creds SIM_ASSETS=/path/to/demo-assets/v1 SIM_STATE=/path/to/state \
docker compose -f docker-compose.sim.yml up -d --no-build
```

The same with plain `docker run`:

```sh
docker run -d --name <name> --restart unless-stopped --shm-size 256m \
  -e IOT_DATA_ENDPOINT=<endpoint> -e XORGATE_UPLOAD_BUCKET=<bucket> -e SCENARIO= -e AWS_KVS_LOG_LEVEL=3 \
  -v /path/to/creds:/creds:ro -v /path/to/demo-assets/v1:/assets/v1:ro -v /path/to/state:/state \
  xorgate-sim-rs:<sha>
```

Never run a container under a device id that another agent (a real device, or the sim host's own JS container) is connected as. The MQTT client id is the device id, and a second client kicks the first one off the broker.

An offline smoke needs no cloud: dummy self-signed creds, a `kvs.json` with one `cam0` channel, `IOT_DATA_ENDPOINT=127.0.0.1`, and the repo's `fixtures/sim-replay/` pack as `/assets/v1`. MQTT is refused, but replay, capture, recording and seeding all run (`phase-7e/evidence/wp2.md`).

## Differences from the JS image

The entrypoint is the JS one with three changes, each marked `[7e]`:
1. the agent command;
2. the boot-id fallback (there is no node in this image);
3. the D74 block: the wrapper that records the agent's exit status, and the wait for it.

On `docker stop`, the JS entrypoint's single `wait` returns 143 as soon as its trap runs. The container then dies before the agent finishes its shutdown, so the last segment is left to the next boot's recovery and presence goes offline through the Last-Will. Here an untrapped wrapper subshell runs the agent and records its exact exit status in `/state/.agent-exit` (and its pid in `.agent-pid`, which the TERM forward signals); the entrypoint waits until the wrapper has recorded it and exits with that code, so it never reads the agent's status from a `wait` that a trap interrupted (bash can lose it there, review-wp2-fixes N6). Both files are removed at start-up (stale ones from an earlier start in a persistent `/state`, before the trap can read them) and again before the exit; once the status is recorded, TERM/INT are ignored so the marker write and the exit cannot be cut (review-wp2-fixes N8, T1). So:
- `docker stop` takes the agent's graceful shutdown: finalize, then `online:false` plus DISCONNECT within its 2 s exit budget (the sim host's stop timeout is 20 s);
- a `power_off` that is exiting when the TERM lands keeps its 3 and its marker.

The image carries the JS image's `timedatectl` shim (`sim/timedatectl`, byte for byte), so a hand-run `MODE=sim` reports `ntp` as the JS image does. Replay forces `ntp` in-process anyway.
