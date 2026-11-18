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

   On a fresh image with a SIM7600 attached, line 1 and the bootstrap's download can meet a dead usb0 route left by NetworkManager; see "Fresh image with a SIM7600" below.

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
| 5 | claim refused (409 duplicate serial, 404 used/expired code), or the claim state is UNKNOWN (no HTTP answer, or interrupted during the request; the message says so); only the dependencies are installed | 409: delete the old device row, then the same command; 404: mint a new code; unknown: if the console lists the device delete it and register again, else the same command |

## The step table (D1.4): JS `install.sh` → `provision-rs.sh`

The JS column is `install-script.ts @ 1a0c730` (vendored as `tools/provision-e2e/golden/js-install.golden.sh`). `scripts/tests/provision-rs-bodies.sh` fails if the JS golden gains a numbered step that this table does not name.

| JS step | JS does | `provision-rs.sh` does |
|---|---|---|
| 0a | NM usb0 unmanaged conf, always rewritten, NM reloaded | the **bootstrap** writes it only if absent; step 7 installs the tarball's full copy cmp-before-write and reloads NM only if it changed |
| 0 | `SVC_USER`/`SVC_GROUP` from `$SUDO_USER`, else root | same |
| — | — | **P0** preconditions (exit 3): root, aarch64, Debian/Ubuntu; refuse a JS device (`/opt/xorgate` or `xorgate-agent.service`), even with `--reprovision`; refuse an already-provisioned device (`/etc/xorgate/agent.env`, `/opt/xorgate-rs` or the unit) without `--reprovision`; a lock; a `bcm2711` warning (D7) |
| — | — | **P1** the hand-off to the transient unit `xorgate-provision-rs` (see "A dropped ssh session"); the tarball's sidecar is re-checked (exit 1) |
| — | — | **P1b** (v0.1.1, F-DA21-NM) before the first apt call: when a SIM7600 is present (usb0 exists or a SIMCom `1e0e` USB device), ModemManager off (`disable --now`, moved up from step 7, so it stays off even if a later step fails). Then usb0's stale IPv4 and IPv6 default routes are deleted, but only when ALL of these hold: nothing live owns them (no running `dhclient` with a `usb0` argument; `sim7600-lte` not active/activating/reloading; NetworkManager not listing usb0 as managed, i.e. absent or `unmanaged`); **another interface holds an IPv4 default** (on an LTE-only device usb0's default is the live uplink and is always kept); and **usb0 does not work** (a usb0-bound probe fails: `ping -I usb0` to 1.1.1.1 / 8.8.8.8, 3 s each, then `curl --interface usb0` to the dist bucket, 5 s). Only `ip -4/-6 route del default dev usb0`, then IPv6 off on usb0 (`disable_ipv6`, runtime only, as the LTE daemon does) so a modem RA cannot re-add the v6 default; no IPv4 address, no other interface, no NM reload. Every keep and removal is logged with its reason |
| 1 | apt lock timeout; `apt-get update`; `curl ca-certificates jq build-essential python3`; NodeSource + `nodejs` | the same lock conf (cmp-before-write), `update` and call; **no** NodeSource/`nodejs`. A window-only `APT::Get::Upgrade "false"` is honoured, because no call overrides apt's configuration |
| 1b | IMU: `i2c-tools python3-pip python3-dev` (WARN), pip `adafruit-circuitpython-bno08x adafruit-blinka` (WARN), `raspi-config do_i2c` or `modprobe i2c-dev`, group `i2c` | identical; `usermod` only if the user is not already in the group |
| — | (JS installs GStreamer in its step 8, WARN only) | **1c** before the claim: `libglib2.0-0 libgstreamer1.0-0` are **fatal** (they provide the binary's NEEDED `libglib-2.0.so.0` and `libgstreamer-1.0.so.0`; `libgcc_s`/`libm`/`libc` are Essential); the rest of the JS GStreamer set WARNs, and so does `gstreamer1.0-libcamera` |
| — | — | **P2** remove a stale `/opt/xorgate-rs.new`; extract there (`--no-same-owner`); require the members; smoke-exec `bin/xorgate-rs -v` (5 s, `xorgate-rs ` banner); on a failure, `ldd`'s missing libraries are named (exit 1) |
| 2 | claim: `{serial, hostname, platform, dtModel, cameras}` | the same body **+ `agent:"rust"`**, over `curl --proto =https`; 409/404/other → exit 5 |
| 3 | `/etc/xorgate` 0700, cert/key/root CA/`agent.env` 0600, owned by the service user | same files, modes and owner. `agent.env` = the JS text with `KVS_RUNTIME_*` → `/opt/xorgate-rs/media{,/lib}`, `xorgate-rs` in the comments, `DEVICE_DIST_VERSION=latest`. On `--reprovision` the agent stops first and `config-sync.json` is deleted |
| 4 | download + extract the JS tarball to `/opt/xorgate` | none (bootstrap + P2); `.new` → `/opt/xorgate-rs`. On `--reprovision` the current tree goes to `.old` first, removed once the agent is active |
| 5 | `npm ci` | none |
| 5b | `/usr/local/bin/xorgate` wrapper | `/usr/local/bin/xorgate-rs` = `update_plan::WRAPPER_BODY`; no `xorgate` alias (Q-8.7) |
| 6 | the JS unit + `.d/10-user.conf`; sudoers 020; `daemon-reload`; `enable --now xorgate-agent` | `xorgate-agent-rs.service` from the tarball + `.d/10-user.conf` (the JS body) + **`.d/20-native.conf`** (D1.3) + the **boot generator** (in place of `enable`); 020 (the JS body, `visudo -cf` first); the start moves to **P9** |
| 6a | `xorgate-setclock` + sudoers 021 + the fake-hwclock cron | the same files from `system/`; **021 = `update_plan::clock_sudoers_body`** (= the JS body) |
| 6a1 | `xorgate-camrotate` + sudoers 023 | the same; **023 = `update_plan::camrotate_sudoers_body`** (= the JS body) |
| 6a2 | the JS updater unit + sudoers 022 | `xorgate-updater-rs.service` + `.d/10-dist.conf` + **024** (`update_plan`; none for a root agent). No 022 |
| 6b | logind `RemoveIPC=no`; `try-restart systemd-logind` | the same body cmp-before-write; `try-restart` only if the file changed |
| 7 | modem: apt `isc-dhcp-client usbutils` (WARN); ModemManager off; group `dialout`; the daemon, unit, udev rule, NM conf; `enable --now sim7600-lte` | same (ModemManager is already off since P1b), **cmp-before-write**; the daemon is the Rust fork (LTE route metric 1000, see "The LTE route metric"); `udevadm` reload only if the rule changed, NM reload only if its conf changed, `daemon-reload` only if the unit changed; an enabled + active daemon is left alone (never restarted) |
| 7b | NM uplink-priority conf + reload | the same settings; its comment names metric 1000 (the Rust daemon's) instead of 100; cmp-before-write, reload only if it changed |
| 8 | GStreamer apt; `kvs.json`, `hardware.json`, pipelines dir, the legacy pipeline writer; media checks | `kvs.json`/`hardware.json` the same `jq` projections; a stale `hardware.json` is removed when the new model has none; the pipeline writer verbatim; media checks under `/opt/xorgate-rs/media` |
| 8b | overlays in `config.txt` (+ `.bak-xorgate`); REBOOT notice | the same lines; `config.txt` is edited only when a line is missing or wrong (JS always rewrites `camera_auto_detect`) |
| — | — | **P9** enablement by the **boot generator** `/etc/systemd/system-generators/xorgate-agent-rs-native` (written in step 6; `systemctl is-enabled` = `generated`, or `enabled-runtime` on systemd 252; see below), not `systemctl enable`; `daemon-reload`; `start --no-block` (`restart` if active); wait ≤ 90 s for `active` (Type=notify = READY) plus a 3 s re-check, else exit 4 with the journal tail |

## Byte pins (one source)

`fixtures/install_rs/bodies.json` holds every body the provisioner writes:

- the wrapper, 024, 021, 023 — tied to `update_plan` by `crates/xorgate-core/tests/install_rs_bodies.rs`, so the first `xorgate-rs update` rewrites nothing;
- 020, `10-user.conf`, logind, uplink, the apt lock — tied to the JS golden's heredocs by `scripts/tests/provision-rs-bodies.sh`;
- `20-native.conf`, the native `10-dist.conf`, the boot generator (`nativeGenerator`): pinned to the script by `provision-rs-bodies.sh` and checked for its guards by `install_rs_bodies.rs`. The generator is **not on the update rail**: changing its body later needs a `--reprovision` (or a future update-rail step).

`provision-rs-bodies.sh` also pins `agent.env` against the JS heredoc under a fixed mapping, and the apt package set (= JS's minus `nodejs`). `scripts/tests/install-rs-bodies.sh` keeps the converter's copies equal to the same golden.

## Re-provisioning and switching agents (D1.3, Q-8.14)

- **Rust → Rust (re-register):** register again (agent: Rust), then run line 2 as `sudo bash install.sh --reprovision`. This mints a new identity, swaps the build, restarts the agent and deletes `config-sync.json`. Delete the old device row first, or the claim returns 409.
- **Rust installer on a JS device:** refused (exit 3). Convert it with `install-rs.sh`, or re-register it with the JS installer.
- **JS installer on a native Rust device:** its `enable --now xorgate-agent` stops Rust through `Conflicts=`. From the next boot JS alone runs, because a native install never `systemctl enable`s the Rust unit. Its generator wants it only while `/opt/xorgate` is absent.
  - **Why not plain `enable` + `20-native.conf` (the plan's D1.3):** the real-systemd e2e showed that with both units enabled, the boot transaction holds both start jobs. Neither start job matters to the anchor, but the Rust unit's `Conflicts=` adds a JS *stop* job, and systemd's tie-break for a start and a stop on the same unit deletes the start. JS is dropped, `20-native.conf` then skips Rust, and **no agent runs**.
  - The generator decides before any transaction. `20-native.conf` stays as the belt for a hand-run start.
  - A hand-run `systemctl start xorgate-agent-rs` on such a device still stops JS through `Conflicts=` before the condition skips Rust; do not do that.
- Then remove the inert Rust files:

  ```
  sudo /opt/xorgate-rs/provision-rs.sh --uninstall
  ```

  `--uninstall` stops the Rust unit and removes the generator, `/opt/xorgate-rs{,.new,.old}`, both Rust units and their drop-ins, the wrapper and 024. It never touches `/etc/xorgate`, `/var/lib/xorgate`, the modem/system assets or 020–023. It refuses (exit 3) when the JS agent is not installed (the device would be left with no agent), and on a device converted by `install-rs.sh` (use its `--revert`).
- **Do not run `install-rs.sh` on a JS-over-native device before `--uninstall`.** Its foreign-drop-in rule keeps `20-native.conf`, so the converted Rust unit would not start and the converter would auto-revert.

## Stopping the agent from starting at boot (native devices)

A native device is wanted at boot by the generator, not by an `/etc` wants link. `systemctl is-enabled xorgate-agent-rs` says `enabled-runtime` on systemd 252, and `generated` on newer versions. So:

- `systemctl disable xorgate-agent-rs` is a **no-op at the next boot**: there is no `/etc` link to remove, and the generator re-creates its runtime link on every boot and `daemon-reload`.
- `systemctl mask xorgate-agent-rs` **fails**: the unit file itself lives in `/etc/systemd/system`, where the mask symlink would go.

To keep the agent from starting at boot, move the generator aside:

```
sudo mv /etc/systemd/system-generators/xorgate-agent-rs-native /root/xorgate-agent-rs-native.off
sudo systemctl daemon-reload
sudo systemctl stop xorgate-agent-rs
```

`systemctl is-enabled xorgate-agent-rs` then says `disabled`. To undo it, move the file back (mode 0755, root), then `daemon-reload` and `start`.

`--uninstall` removes the generator for good, but only on a device that the JS installer has taken over.

## A dropped ssh session (review N2)

Once the refusals have passed, `provision-rs.sh` hands the rest of the run to the transient systemd unit `xorgate-provision-rs`:

```
systemd-run --no-block --collect --unit=xorgate-provision-rs …
```

**The ssh session only follows.**
- It polls the unit and prints its run's part of `/var/log/xorgate-provision-rs.log`.
- It returns the exit code the unit writes to `/run/xorgate-provision-rs.rc.<run id>`.
- The log is append-only, with a `===== run <id>` header per run, so a second run can never truncate or delete the first's. A second run refuses (3) anyway, on the lock or because the unit is active.

**Why not `--wait`:** it would stop the unit when the client is interrupted. With `--no-block`, a Ctrl-C, a hang-up or a closed output pipe makes the session detach at once, and the run continues. That matters after the claim, where an interrupted run would leave a half-provisioned device with the code used.

**If the unit dies without an exit code** (SIGKILL, a timeout), the session reads how far that run got from its part of the log:

| The log reached | Result |
|---|---|
| before the claim | exit 1 |
| during the claim | exit 5, "claim state UNKNOWN" |
| after the claim | exit 4, "claim done, install state UNKNOWN" + RECOVERY |

**No hand-off happens in two cases. The run stays in-process:**
- **The caller is already a systemd service** (`INVOCATION_ID` is set; `sudo` from ssh strips it). The caller's unit then owns the run, so stopping it stops the provisioner too. Example: the bench window's `systemd-run --unit=p8-install-…` (fix-check MUST-2).
- **systemd is not running** (a container): there is a NOTE. Keep that session alive (`tmux`).

**Relative paths:** `--tarball`/`--cleanup` are made absolute before the hand-off (the unit runs in `/`).

## Updates on a native device (D4)

- **The console Update button:** the agent runs `sudo -n systemctl start --no-block xorgate-updater-rs.service` (024). The updater runs `xorgate-rs update`, which follows `-rs` `latest`.
- **From a shell:** `sudo xorgate-rs update`. There is no `xorgate` command.
- With no `/opt/xorgate`, the update takes the Rust-only branch. It refreshes the modem assets (no restart), the system assets and 021/023.
- **Every file the update installs is replaced atomically (v0.1.1):** written to a dot-named temp sibling (`.<name>.xorgate-new`, a name sudoers.d, systemd, udev, NM and cron all ignore), chmod-ed, fsync-ed and renamed over the old file, then the directory is fsync-ed. A running `sim7600-lte.sh` keeps reading its old inode, and a power cut never leaves a truncated file. A sudoers grant still passes `visudo -cf` before its bytes are installed. If any install step fails, the rollback restores `/opt/xorgate-rs` **and** every file the run replaced (unit, wrapper, modem/system assets, sudoers, updater unit); files it created are removed. (v0.1.0 copied in place and restored only `/opt`, as the TS did.)
  - When the daemon's bytes change (v0.1.0 → v0.1.1: the LTE route fix), the running `sim7600-lte` keeps its old behaviour until it restarts: at the next boot, or now with `sudo systemctl restart sim7600-lte` (a short LTE drop). The NM uplink conf (7b) is not on the update rail; its v0.1.1 change is a comment only.
- On a **converted** device (`install-rs.sh`, `/opt/xorgate` kept) the update logs "shared assets owned by the JS agent (coexistence) — not refreshed": the modem daemon stays the JS copy. See "The LTE route metric".

## The LTE route metric (v0.1.1, F-LTE-METRIC / F-DA21-NET)

- v0.1.0's `sim7600-lte.sh` (then byte-identical to the JS repo's) installed the LTE default route with `ip route replace default … dev usb0 metric 100`. An IPv4 replace overwrites the first default with the same metric **whatever its interface**, and NetworkManager's wired default is also metric 100. So on a wired + SIM7600 device, the first LTE connect deleted the Ethernet default; with no or poor LTE data, the device had no working route.
- From v0.1.1 the Rust copy:
  - defaults the LTE metric to **1000**, above every NM default (wired 100, WiFi 50 via 10-uplink-priority.conf, NM's own WiFi default 600);
  - deletes only usb0's own defaults (`dev usb0`) and adds the new one with `ip route append`, never `replace`. Even an operator override equal to another interface's metric leaves that route in place, and first.
  - Preference: wired and WiFi win; LTE is the fallback. The daemon's health checks (`ping -I usb0`) are bound to usb0 and work at any metric.
  - dhclient gets the metric too (`-e IF_METRIC=<metric>`), so Debian's dhclient-script installs the default at that metric on the first bind and on every renewal that changes the lease; the daemon's own fix-up after dhclient stays as the belt.
  - **Failover limits.** LTE takes over only when the other default route is **removed** (NetworkManager removes it when the cable is pulled or WiFi disconnects). A wired or WiFi link that stays up to a router with no internet keeps its default (100 or 50) primary, and LTE at 1000 stays healthy but unused: there is no connectivity-based demotion. v0.1.0's accidental `replace` made such a box ride LTE; it also deleted working wired routes. A link that is down but whose route NetworkManager has not removed (`linkdown`) still wins the lookup as well.
- **The Rust copy now differs from the JS repo's** (user decision 2026-11-18: no JS change). `modem/SHA256SUMS` carries the Rust hash.
- **Drop-ins keep working.** `SIM7600_ROUTE_METRIC` in a `sim7600-lte.service.d/*.conf` drop-in still overrides the default (a value that is not a non-negative integer falls back to 1000, with a warning). DA21's bench drop-in `90-da21-route-metric.conf` (`=1000`) equals the new default: it can stay, or be removed once v0.1.1 runs.
- **Workaround for a device that cannot take the new daemon** (a converted device, any JS device): the same drop-in, then a daemon restart:

  ```
  sudo mkdir -p /etc/systemd/system/sim7600-lte.service.d
  printf '[Service]\nEnvironment="SIM7600_ROUTE_METRIC=1000"\n' | sudo tee /etc/systemd/system/sim7600-lte.service.d/90-route-metric.conf
  sudo systemctl daemon-reload && sudo systemctl restart sim7600-lte
  ```

  With the old script, `replace … metric 1000` can only collide with another metric-1000 default, which NM does not create.

## Fresh image with a SIM7600: the first two lines (F-DA21-NM, known limitation)

On a fresh Raspberry Pi OS boot, ModemManager is enabled. NetworkManager auto-connects the SIM7600's usb0 and takes the IPv4 **and** IPv6 default routes at metric 100. About 20 s later ModemManager claims the modem, and NM drops usb0 from its device list but **leaves its address and both default routes behind**. Without LTE data they are black holes.

- **Fixed in the provisioner (v0.1.1, P1b):** before its first apt call it turns ModemManager off and deletes those stale usb0 defaults, only when nothing live owns them, another interface has an IPv4 default and usb0 itself does not work. On an LTE-only device usb0's default is the uplink, and it is kept. In DA21, apt and pip stalled about 4 minutes on IPv6 mirrors before this fix.
- **Not fixable here:** the console's **line 1** (`curl -s <install url> -o install.sh`) and the bootstrap's own download run before the provisioner. With the stale IPv4 default, line 1 times out or fails; curl's IPv4/IPv6 fallback usually survives the IPv6 one. (A backend bootstrap change could clean usb0 before its download; not done.)
- **Manual workaround** (wired or WiFi device, SIM7600 fitted, line 1 hangs or fails). Check first:

  ```
  ip route show default        # a "dev usb0" default, usually metric 100, beside or above eth0/eth1/wlan0
  ip -6 route show default
  nmcli -t -f DEVICE,STATE device | grep usb0   # absent, or "unmanaged"
  ```

  Delete usb0's defaults **only if** `ip route show default` ALSO lists eth0, eth1 or wlan0 (another uplink), **and** usb0 does not work (`ping -c1 -W3 -I usb0 1.1.1.1` fails). **Never on an LTE-only box:** there usb0's default is the only uplink, and deleting it cuts the device off (over ssh or pitunnel you lose the session). Then (runtime only; nothing on disk changes) run the two lines as usual:

  ```
  sudo ip -4 route del default dev usb0
  sudo ip -6 route del default dev usb0
  ```

  Each command only touches usb0; "No such process" means there was none. If they come back before line 2 starts (ModemManager re-probing), run `sudo systemctl disable --now ModemManager` first.
- After the install it does not recur: the NM unmanaged conf is in place and ModemManager is disabled (DA21, after R1).

## Tests

| What | Where | Runs |
|---|---|---|
| bodies, JS-shared steps, apt set, step-table tripwire, `DEVICE_DIST_VERSION=latest` | `scripts/tests/provision-rs-bodies.sh` | plain suite (`hl.sh run <tag> suites`) |
| DA6 e2e: the bootstrap golden unmodified → `provision-rs.sh`, the full case matrix | `scripts/tests/provision-e2e.sh` + `tools/provision-e2e/` | real systemd (`hl.sh run <tag> systemd`) |
| DA7 real systemd: notify, conditions, the updater, the first update rewrites nothing | `scripts/tests/provision-rs-systemd.sh` | real systemd; the update rows need `hl.sh run <tag> build-bins` first |
| DA8 real arm64 apt + pip, up to the claim | `tools/provision-e2e/da8-arm64.sh` | the Mac (arm64 Docker, ≤ 4 CPUs, alone) |
| the LTE default route (daemon) and P1b (provisioner) on a REAL kernel FIB, in a private network namespace | `scripts/tests/lte-routes.sh` | plain suite: static rows only; the kernel rows run inside `provision-rs-systemd.sh` (`--kernel`) |

`tools/provision-e2e/lib.sh` says what the harness keeps real and what it stubs. In short:

- **Real:** systemd, curl + TLS, `jq`, `visudo`, `pack.sh`, the bootstrap golden.
- **Stubbed:** apt and pip (a fake dpkg state), `uname -m`, `modprobe`/`udevadm`, and the agent binary (a Type=notify stand-in). The modem daemon and the `system/` helpers in the test tarball are inert stubs.
- **The fake API** answers for the golden's PROD hostnames only through `/etc/hosts` and a test CA. Setup refuses to run unless each name resolves to 127.0.0.1.

**W2 / DA13 on hardware:**

- expect `systemctl is-enabled xorgate-agent-rs` = `enabled-runtime` (systemd 252);
- the generator present;
- **no** `/etc/systemd/system/multi-user.target.wants/xorgate-agent-rs.service`;
- compare `config.txt` by sha: `raspi-config` gives it a new inode.

**Not proven here:**

- NetworkManager reality: the NM unit is a counting stand-in;
- overlays → reboot → cameras;
- a real modem;
- pip without a compiler on a clean image (that is DA8);
- a clean-media install (DA21).
