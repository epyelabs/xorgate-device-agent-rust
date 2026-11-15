# Native Rust install: `provision-rs.sh`

`scripts/provision-rs.sh` provisions a clean Raspberry Pi with the Rust agent. It is the Rust twin of the backend's JS `install.sh`: the same system steps, but no Node, no `npm` and no `/opt/xorgate`. Plan: `plans/device-agent-js-to-rust/phase-8-dual-agent-registration.md` D1.1–D1.4. To move an existing JS device to Rust, use the converter instead (`docs/convert.md`).

## How it runs

1. In the console, **Register device**, agent **Rust**, gives the usual two lines:

   ```
   curl -s <install url> -o install.sh
   sudo bash install.sh
   ```

2. `install.sh` is the backend's thin bootstrap (`install-script-rs.ts`). It:
   - checks root and `aarch64`;
   - writes the minimal NetworkManager usb0 conf, only if none exists;
   - resolves `latest-rs.json` to the versioned tarball;
   - downloads the tarball and its `.sha256`, and verifies them;
   - extracts `provision-rs.sh` from the tarball root and `exec`s it.

   Any failure in the bootstrap exits 1 before the claim.

3. `provision-rs.sh` is invoked through the frozen interface I-P8-1:

   ```
   provision-rs.sh --reg-code <c> --api-base <url> --from <bucket> --tarball <f> [--cleanup <dir>] [--reprovision]
   provision-rs.sh --uninstall
   ```

   - `--api-base` ends in `/v1`; `--api-base` and `--from` must be `https://`.
   - `--tarball` must have its `<f>.sha256` next to it. The provisioner checks it again.
   - `--cleanup` is removed at exit, but only if it is a temp dir that holds the tarball.
   - No version is passed. `agent.env` always gets the literal `DEVICE_DIST_VERSION=latest`, so a native device follows `latest`, as JS devices do. It is never pinned to its install build (review-wp-b S2).

## Exit codes

| Code | Meaning | What the operator does |
|---|---|---|
| 0 | provisioned, `xorgate-agent-rs` active | nothing (reboot if the REBOOT notice printed) |
| 1 | a pre-claim failure: the tarball, its checksum, apt, or the staged binary does not run | fix the cause, then run the **same** command again: the code is still usable |
| 2 | usage | fix the command |
| 3 | refused, nothing changed: not root, not aarch64, not Debian, a JS-provisioned device, or already provisioned without `--reprovision` | follow the message |
| 4 | claimed, but the agent did not become active within 90 s (or a post-claim step failed) | follow the printed RECOVERY: delete the device in the console, register again, run `sudo bash install.sh --reprovision` |
| 5 | claim refused (409 duplicate serial, 404 used/expired code); only the dependencies are installed | 409: delete the old device row, then the same command; 404: mint a new code |

## The step table (D1.4): JS `install.sh` → `provision-rs.sh`

The JS column is `install-script.ts @ 1a0c730` (vendored as `tools/provision-e2e/golden/js-install.golden.sh`). `scripts/tests/provision-rs-bodies.sh` fails if the JS golden gains a numbered step that this table does not name.

| JS step | JS does | `provision-rs.sh` does |
|---|---|---|
| 0a | NM usb0 unmanaged conf, always rewritten, NM reloaded | the **bootstrap** writes it only if absent; step 7 installs the tarball's full copy cmp-before-write and reloads NM only if it changed |
| 0 | `SVC_USER`/`SVC_GROUP` from `$SUDO_USER`, else root | same |
| — | — | **P0** preconditions (exit 3): root, aarch64, Debian/Ubuntu; refuse a JS device (`/opt/xorgate` or `xorgate-agent.service`), even with `--reprovision`; refuse an already-provisioned device (`/etc/xorgate/agent.env`, `/opt/xorgate-rs` or the unit) without `--reprovision`; a lock; a `bcm2711` warning (D7) |
| — | — | **P1** the tarball's sidecar is re-checked (exit 1) |
| 1 | apt lock timeout; `apt-get update`; `curl ca-certificates jq build-essential python3`; NodeSource + `nodejs` | the same lock conf (cmp-before-write), `update` and call; **no** NodeSource/`nodejs`. A window-only `APT::Get::Upgrade "false"` is honoured, because no call overrides apt's configuration |
| 1b | IMU: `i2c-tools python3-pip python3-dev` (WARN), pip `adafruit-circuitpython-bno08x adafruit-blinka` (WARN), `raspi-config do_i2c` or `modprobe i2c-dev`, group `i2c` | identical; `usermod` only if the user is not already in the group |
| — | (JS installs GStreamer in its step 8, WARN only) | **1c** before the claim: `libglib2.0-0 libgstreamer1.0-0` are **fatal** (they provide the binary's NEEDED `libglib-2.0.so.0` and `libgstreamer-1.0.so.0`; `libgcc_s`/`libm`/`libc` are Essential); the rest of the JS GStreamer set WARNs, and so does `gstreamer1.0-libcamera` |
| — | — | **P2** remove a stale `/opt/xorgate-rs.new`; extract there (`--no-same-owner`); require the members; smoke-exec `bin/xorgate-rs -v` (5 s, `xorgate-rs ` banner); on a failure, `ldd`'s missing libraries are named (exit 1) |
| 2 | claim: `{serial, hostname, platform, dtModel, cameras}` | the same body **+ `agent:"rust"`**, over `curl --proto =https`; 409/404/other → exit 5 |
| 3 | `/etc/xorgate` 0700, cert/key/root CA/`agent.env` 0600, owned by the service user | same files, modes and owner. `agent.env` = the JS text with `KVS_RUNTIME_*` → `/opt/xorgate-rs/media{,/lib}`, `xorgate-rs` in the comments, `DEVICE_DIST_VERSION=latest`. On `--reprovision` the agent stops first and `config-sync.json` is deleted |
| 4 | download + extract the JS tarball to `/opt/xorgate` | none (bootstrap + P2); `.new` → `/opt/xorgate-rs`. On `--reprovision` the current tree goes to `.old` first, removed once the agent is active |
| 5 | `npm ci` | none |
| 5b | `/usr/local/bin/xorgate` wrapper | `/usr/local/bin/xorgate-rs` = `update_plan::WRAPPER_BODY`; no `xorgate` alias (Q-8.7) |
| 6 | the JS unit + `.d/10-user.conf`; sudoers 020; `daemon-reload`; `enable --now xorgate-agent` | `xorgate-agent-rs.service` from the tarball + `.d/10-user.conf` (the JS body) + **`.d/20-native.conf`** (D1.3); 020 (the JS body, `visudo -cf` first); the start moves to **P9** |
| 6a | `xorgate-setclock` + sudoers 021 + the fake-hwclock cron | the same files from `system/`; **021 = `update_plan::clock_sudoers_body`** (= the JS body) |
| 6a1 | `xorgate-camrotate` + sudoers 023 | the same; **023 = `update_plan::camrotate_sudoers_body`** (= the JS body) |
| 6a2 | the JS updater unit + sudoers 022 | `xorgate-updater-rs.service` + `.d/10-dist.conf` + **024** (`update_plan`; none for a root agent). No 022 |
| 6b | logind `RemoveIPC=no`; `try-restart systemd-logind` | the same body cmp-before-write; `try-restart` only if the file changed |
| 7 | modem: apt `isc-dhcp-client usbutils` (WARN); ModemManager off; group `dialout`; the daemon, unit, udev rule, NM conf; `enable --now sim7600-lte` | same, **cmp-before-write**; `udevadm` reload only if the rule changed, NM reload only if its conf changed, `daemon-reload` only if the unit changed; an enabled + active daemon is left alone (never restarted) |
| 7b | NM uplink-priority conf + reload | same text, cmp-before-write, reload only if it changed |
| 8 | GStreamer apt; `kvs.json`, `hardware.json`, pipelines dir, the legacy pipeline writer; media checks | `kvs.json`/`hardware.json` the same `jq` projections; a stale `hardware.json` is removed when the new model has none; the pipeline writer verbatim; media checks under `/opt/xorgate-rs/media` |
| 8b | overlays in `config.txt` (+ `.bak-xorgate`); REBOOT notice | the same lines; `config.txt` is edited only when a line is missing or wrong (JS always rewrites `camera_auto_detect`) |
| — | — | **P9** `daemon-reload`; `enable`; `start --no-block` (`restart` if active); wait ≤ 90 s for `active` (Type=notify = READY) plus a 3 s re-check, else exit 4 with the journal tail |

## Byte pins (one source)

`fixtures/install_rs/bodies.json` holds every body the provisioner writes:

- the wrapper, 024, 021, 023 — tied to `update_plan` by `crates/xorgate-core/tests/install_rs_bodies.rs`, so the first `xorgate-rs update` rewrites nothing;
- 020, `10-user.conf`, logind, uplink, the apt lock — tied to the JS golden's heredocs by `scripts/tests/provision-rs-bodies.sh`;
- `20-native.conf`, the native `10-dist.conf`.

`provision-rs-bodies.sh` also pins `agent.env` against the JS heredoc under a fixed mapping, and the apt package set (= JS's minus `nodejs`). `scripts/tests/install-rs-bodies.sh` keeps the converter's copies equal to the same golden.

## Re-provisioning and switching agents (D1.3, Q-8.14)

- **Rust → Rust (re-register):** register again (agent: Rust), then run line 2 as `sudo bash install.sh --reprovision`. This mints a new identity, swaps the build, restarts the agent and deletes `config-sync.json`. Delete the old device row first, or the claim returns 409.
- **Rust installer on a JS device:** refused (exit 3). Convert it with `install-rs.sh`, or re-register it with the JS installer.
- **JS installer on a native Rust device:** its `enable --now xorgate-agent` stops Rust through `Conflicts=`. From the next boot `20-native.conf` (`ConditionPathExists=!/opt/xorgate`) keeps the Rust unit from starting, so JS alone runs. Then remove the inert Rust files:

  ```
  sudo /opt/xorgate-rs/provision-rs.sh --uninstall
  ```

  `--uninstall` disables the Rust unit and removes `/opt/xorgate-rs{,.new,.old}`, both Rust units and their drop-ins, the wrapper and 024. It never touches `/etc/xorgate`, `/var/lib/xorgate`, the modem/system assets or 020–023. It refuses (exit 3) when the JS agent is not installed (the device would be left with no agent), and on a device converted by `install-rs.sh` (use its `--revert`).
- **Do not run `install-rs.sh` on a JS-over-native device before `--uninstall`.** Its foreign-drop-in rule keeps `20-native.conf`, so the converted Rust unit would not start and the converter would auto-revert.

## Updates on a native device (D4)

- **The console Update button:** the agent runs `sudo -n systemctl start --no-block xorgate-updater-rs.service` (024). The updater runs `xorgate-rs update`, which follows `-rs` `latest`.
- **From a shell:** `sudo xorgate-rs update`. There is no `xorgate` command.
- With no `/opt/xorgate`, the update takes the Rust-only branch. It refreshes the modem assets in place (identical bytes, no restart), the system assets and 021/023.

## Tests

| What | Where | Runs |
|---|---|---|
| bodies, JS-shared steps, apt set, step-table tripwire, `DEVICE_DIST_VERSION=latest` | `scripts/tests/provision-rs-bodies.sh` | plain suite (`hl.sh run <tag> suites`) |
| DA6 e2e: the bootstrap golden unmodified → `provision-rs.sh`, the full case matrix | `scripts/tests/provision-e2e.sh` + `tools/provision-e2e/` | real systemd (`hl.sh run <tag> systemd`) |
| DA7 real systemd: notify, conditions, the updater, the first update rewrites nothing | `scripts/tests/provision-rs-systemd.sh` | real systemd; the update rows need `hl.sh run <tag> build-bins` first |
| DA8 real arm64 apt + pip, up to the claim | `tools/provision-e2e/da8-arm64.sh` | the Mac (arm64 Docker, ≤ 4 CPUs, alone) |

`tools/provision-e2e/lib.sh` says what the harness keeps real and what it stubs. In short:

- **Real:** systemd, curl + TLS, `jq`, `visudo`, `pack.sh`, the bootstrap golden.
- **Stubbed:** apt and pip (a fake dpkg state), `uname -m`, `modprobe`/`udevadm`, and the agent binary (a Type=notify stand-in). The modem daemon and the `system/` helpers in the test tarball are inert stubs.
- **The fake API** answers for the golden's PROD hostnames only through `/etc/hosts` and a test CA. Setup refuses to run unless each name resolves to 127.0.0.1.

**Not proven here:**

- NetworkManager reality: the NM unit is a counting stand-in;
- overlays → reboot → cameras;
- a real modem;
- pip without a compiler on a clean image (that is DA8);
- a clean-media install (DA21).
