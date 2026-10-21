# Converting a device to the Rust agent (`install-rs.sh`)

`scripts/install-rs.sh` converts a device that was provisioned the standard way (the backend's
`install.sh`, JS agent) to the Rust agent, and `--revert` takes it back. It stands in for the
backend's future Rust install flavour, which belongs to the cutover plan. Everything the JS
provisioning left on the device stays where it is: `/etc/xorgate` (identity, `agent.env`,
manifests), `/var/lib/xorgate` (settings, config-sync, counters), the sudo helpers and the modem
daemon. The Rust agent uses the same identity and the same state, so the cloud sees one device
that changed agent.

The JS agent is **stopped and disabled, never removed**: it is the rollback.

## Before you start

- The device is JS-provisioned: `/etc/xorgate/agent.env`, `/opt/xorgate` and
  `/etc/systemd/system/xorgate-agent.service` all exist. If not, the script refuses with exit 3.
- It is an aarch64 Bookworm device (CM4/CM5). The script checks the architecture by running the
  staged binary (`bin/xorgate-rs -v`, 5 s) before it changes anything.
- You have `sudo`. The script needs nothing else the device does not already have: bash,
  coreutils, curl, GNU tar, sha256sum, visudo, systemctl, find, cmp.
- The device is not in the middle of a JS `xorgate update`.

## Convert

### From the dist bucket (the bootstrap one-liner)

Run this on the device as the provisioning user (`pi`). It fetches the bucket copy of the script,
checks it against its published sha256 and runs it:

```
B=$(sudo sed -n 's/^DEVICE_DIST_BUCKET_URL=//p' /etc/xorgate/agent.env); B=${B%/}
curl -fsSLO "$B/install-rs.sh" -O "$B/install-rs.sh.sha256" && sha256sum -c install-rs.sh.sha256 && sudo bash install-rs.sh
```

On a terminal the script asks for confirmation; `-y` skips the question. It installs
`xorgate-agent-rs-latest.tar.gz` from the same bucket, after it checks that file's `.sha256`
sidecar too.

Options:

| Option | Meaning |
|---|---|
| `--from <url>` | another dist bucket. Default: env `DEVICE_DIST_BUCKET_URL`, else `/etc/xorgate/agent.env`. One trailing `/` is stripped |
| `--tag <ver>` | install `xorgate-agent-rs-<ver>.tar.gz` (a dev short sha or a prod release tag). Default `latest`. The `DEVICE_DIST_VERSION` in `agent.env` is the **JS** agent's pin and is never used |
| `--tarball <file>` | install a local tarball instead (below) |
| `--reinstall` | on a converted device, also fetch and swap the build |
| `-y` | no confirmation question |

### Offline, LAN or bench (`--tarball`)

Copy a tarball **and its sidecar** to the device, then run the script from the tarball (it ships at
its root):

```
scp xorgate-agent-rs-<ver>.tar.gz xorgate-agent-rs-<ver>.tar.gz.sha256 pi@<device>:
ssh pi@<device>
mkdir -p /tmp/rs && tar -xzf xorgate-agent-rs-<ver>.tar.gz -C /tmp/rs install-rs.sh
sudo bash /tmp/rs/install-rs.sh --tarball ~/xorgate-agent-rs-<ver>.tar.gz
```

`<file>.sha256` is required, in `sha256sum -c` format, and must name the tarball's own file name.
A missing or mismatching sidecar refuses the build.

### What a conversion does

The script numbers its steps. Each step can be re-run, so an interrupted conversion is finished by
running the script again, or undone with `--revert`.

0. **Preconditions**: root, JS-provisioned (else exit 3, nothing changed).
1. **Record**: `/var/lib/xorgate-rs-install/record` (`state=converting`) and `pre.manifest` (a
   listing of every file the conversion could touch, with modes, owners and sha256), written
   **once**, before any change. A re-run never rewrites them.
2. **Fetch + verify**: the tarball and its `.sha256`, `curl -fsSL --retry 3 --retry-delay 2`. If
   the sidecar is missing or does not match, the device is unchanged (exit 1).
3. **Stage + smoke**: extract to `/opt/xorgate-rs.new`, check the required members, run
   `bin/xorgate-rs -v`. A failure here leaves the device unchanged (exit 1).
4. **Swap**: `/opt/xorgate-rs.new` → `/opt/xorgate-rs`. A previous build is kept at
   `/opt/xorgate-rs.old` until the end.
5. **Wrapper**: `/usr/local/bin/xorgate-rs` (defaults `XORGATE_HOME=/var/lib/xorgate`).
6. **Units**: `xorgate-agent-rs.service` (the shipped unit, with
   `Conflicts=xorgate-agent.service`); a byte copy of the JS
   `xorgate-agent.service.d/10-user.conf` (same service user), when the JS agent has one;
   `xorgate-updater-rs.service`; `xorgate-updater-rs.service.d/10-dist.conf`
   (`DEVICE_DIST_VERSION=latest`, see "Updates"). Drop-ins that the script did not install are
   named and left alone. `systemd/bench/*` in the tarball is never installed.
7. **Sudoers**: `/etc/sudoers.d/024_xorgate-update-rs` (the service user may run
   `systemctl start --no-block xorgate-updater-rs.service`, nothing else), checked with
   `visudo -cf` before it lands. A refusal is a warning, not a failure. A root service user gets
   no grant.
8. **systemctl**: `daemon-reload`, `enable xorgate-agent-rs`, `disable xorgate-agent`,
   `start xorgate-agent-rs`. The `Conflicts=` stops the JS agent. The incoming unit is enabled
   before the outgoing one is disabled, back to back (the revert does the same the other way), so
   a power cut never leaves a device with no agent enabled. Should both be enabled at boot,
   `Conflicts=` lets only one of them run, and running the script again finishes the job.
9. **Verify**: within 90 s `xorgate-agent-rs` must be `active` and `xorgate-agent` inactive. If
   not, the script reverts by itself (keep mode, JS restored) and exits 4.

If a step from 4 to 7 fails, the script puts `/opt/xorgate-rs` back, removes the files that run
created, restores the files it overwrote (a device with an earlier Rust install, like the bench),
and exits 1.

An interrupted session is handled the same way: SIGHUP (a dropped ssh or LTE link), SIGINT or
SIGTERM during a conversion runs that step's cleanup, and from step 8 on it reverts to the JS agent
(exit 4). `--revert` ignores those signals and finishes. Over a flaky link, run the script inside
`tmux` or with `nohup … > install-rs.log 2>&1` anyway, so you can read its output afterwards.

It never writes `/etc/xorgate`, `/opt/xorgate`, `/usr/local/bin/xorgate`, the JS unit or its
drop-ins, `xorgate-updater.service`, sudoers 020–023, or the modem, logind, NetworkManager, udev
and cron files. It never restarts the modem daemon and never reboots the device.

The last line on success:

```
[install-rs] converted: xorgate-agent-rs <ver> (<channel>) active; xorgate-agent stopped + disabled. revert with: sudo /opt/xorgate-rs/install-rs.sh --revert
```

### Running it again

On a converted device the script re-checks the unit, the wrapper, the drop-ins, the 024 grant and
the enablement, repairs what differs, and prints what it changed. It does **not** change the build:

```
[install-rs] already converted (<convertedAt>, <ver>); install verified, changed (0). change the build with: sudo systemctl start xorgate-updater-rs
```

Use `--reinstall` to fetch and swap the build through the same steps 2–4.

## Check

```
sudo /opt/xorgate-rs/install-rs.sh --status     # state=converted|converting|reverted|not-converted, the record, both units, the build
systemctl status xorgate-agent-rs
xorgate-rs status                               # the agent's own view (wrapper → XORGATE_HOME=/var/lib/xorgate)
pgrep -a node                                   # empty on a converted device
```

## Updates

The cloud `update` command and the console button work unchanged. The agent runs
`sudo -n systemctl start --no-block xorgate-updater-rs.service` (the 024 grant). The updater runs
`xorgate-rs update`, which fetches `xorgate-agent-rs-latest.tar.gz`, verifies its sha256, swaps
`/opt/xorgate-rs` and restarts the agent. By hand:

```
sudo systemctl start xorgate-updater-rs && journalctl -u xorgate-updater-rs -n 50
```

**The JS pin.** `agent.env` may carry `DEVICE_DIST_VERSION=<v>`, a JS build tag. `xorgate-rs update`
honours `DEVICE_DIST_VERSION` like the JS updater does, so it would request
`xorgate-agent-rs-<v>.tar.gz`, which does not exist, and fail closed. The drop-in
`xorgate-updater-rs.service.d/10-dist.conf` sets `DEVICE_DIST_VERSION=latest` for the updater unit,
and an environment variable wins over `agent.env`. A conversion on such a device prints:

```
WARNING: /etc/xorgate/agent.env pins DEVICE_DIST_VERSION=<v> (the JS agent's build); the Rust updater follows 'latest' …
```

That is expected. Update through the unit (`systemctl start xorgate-updater-rs`). A **hand-run**
`sudo xorgate-rs update` does not see the drop-in, so pass `--tag latest` to it. `xorgate-rs update`
rewrites the unit files but never their `.d/` directories, so the drop-in survives every update.

## Revert

### Keep mode (default)

```
sudo /opt/xorgate-rs/install-rs.sh --revert
```

It needs no network and no working Rust binary. It disables and stops `xorgate-agent-rs`, puts the
JS unit's enablement back to what the record says, and starts the JS agent if the record says it
was enabled **or** running. The Rust files stay on disk but inert (disabled, nothing points at
them), so converting again is quick. The record moves to `record.reverted-<UTC ts>`.

The script then compares the JS-owned files against `pre.manifest` and prints every difference:

```
[install-rs] reverted: xorgate-agent enabled + active; xorgate-agent-rs disabled (files kept). JS install matches the pre-conversion record: yes
```

`NO (<n> differences, listed above)` exits 1. The JS agent has been restored anyway; the list says
what is not byte-identical. Like the JS agent, the Rust agent writes two kinds of `/etc/xorgate`
files at run time: `hardware.json` when the cloud sends a hardware block, and `pipelines/` on
`xorgate-rs pipeline set`. A difference there is expected when that happened during the
conversion, and the JS agent reads the same files. The hardware reconciler also rewrites
`pipelines/<key>.{gst,camera,gst.direct}` on every reconcile, and the self-check runs right after
`start xorgate-agent`, so it can race the JS agent's own startup writes there: re-run `--revert`
(a no-op that repeats the check) a minute later before you chase a `pipelines/` difference. Any
other difference needs a look.

Running `--revert` again is a no-op (exit 0).

### Purge

```
sudo /opt/xorgate-rs/install-rs.sh --revert --purge
```

This is keep mode, then it deletes everything the conversion installed (`/opt/xorgate-rs`, the
wrapper, the Rust units and their drop-ins, 024, the record directory) and runs `daemon-reload`.
The whole device then matches `pre.manifest`. The script runs from a temporary copy, because it
deletes its own file. It never deletes a file it did not install, and it names and keeps foreign
drop-ins (for example the bench's `90-bench-dev.conf`).

If a Rust install was already there before the conversion (`rsPreexisting=yes` in the record, as on
the bench), `--purge` refuses with exit 3. Use keep mode there. `--purge --force` deletes only what
the conversion itself created.

## Exit codes and troubleshooting

| Exit | Meaning | What to do |
|---|---|---|
| 0 | done, or nothing to do | — |
| 1 | failed; the device is back at its pre-run state (convert), or the revert's self-check found differences | read the `failed:` line (download, checksum, missing member, smoke, write error) or the listed differences |
| 2 | usage error | `install-rs.sh --help` |
| 3 | refused, nothing changed: not root, not JS-provisioned, `--purge` of a pre-existing install, another `install-rs.sh` running | fix the precondition |
| 4 | the Rust agent started but was not healthy within 90 s; the script reverted to the JS agent by itself | `journalctl -u xorgate-agent-rs -n 100`; the script printed the tail |
| 9 | only with the test-only `XORGATE_RS_CONVERT_FAIL_AT` (a simulated crash); never on a real device | run the script again, or `--revert` |

Common failures:

- `no checksum published for …` / `checksum mismatch …`: the bucket's sidecar is missing or stale
  (a deploy in progress uploads the objects one by one). Retry after the deploy.
- `download failed (… 404 …)`: wrong `--tag`, or no Rust build in that bucket yet.
- `staged build does not run on this device (…)`: wrong architecture or a truncated download.
- `the build is missing install-rs.sh`: a tarball packed before Phase 5. Use a newer build.
- `… is in the way …`: a directory or special file sits where the script writes a file (for
  example `/usr/local/bin/xorgate-rs` is a directory). Nothing was changed; move it away and run
  the script again. It never deletes such content itself.
- An interrupted run (`--status` shows `state=converting`): run the same command again to finish
  it, or `--revert`.

Logs: `journalctl -u xorgate-agent-rs`, `journalctl -u xorgate-updater-rs`; the JS agent:
`journalctl -u xorgate-agent`.

## Notes

- **KVS runtime (CF69).** `agent.env` points `KVS_RUNTIME_BIN` / `KVS_RUNTIME_LIB_DIR` at the JS
  tree (`/opt/xorgate/media/…`). A converted Rust agent runs that same C runtime, which the Rust
  agent does not change, and `install-rs.sh` leaves `agent.env` alone. The backend's future Rust
  install flavour (cutover plan) writes `/opt/xorgate-rs/media` paths. On a converted device,
  `readlink /proc/<kvs pid>/exe` shows `/opt/xorgate/media/xorgate-kvs-webrtc`.
- **`config-sync.json` (CF70).** A conversion keeps the device's identity, so the Rust agent takes
  over `/var/lib/xorgate/config-sync.json` and its `config_rev` guard as is: **do not** delete it.
  The known trap (an old `config-sync.json` rejecting a new identity's lower `config_rev`, so it
  must be deleted) applies only when a device is **re-provisioned** to another identity, never to
  a conversion or a revert.
- **No `xorgate` symlink.** The JS CLI stays `/usr/local/bin/xorgate` (it talks to the same
  `/var/lib/xorgate`). Use `xorgate-rs …` on a converted device.
- **Bench.** The bench CM5 runs a bench-flavoured Rust install next to the JS agent
  (`scripts/bench/install.sh`: `Conflicts=` stripped, DEV identity). A conversion there installs
  the shipped unit (with `Conflicts=`) and the shipped wrapper, and keeps the bench drop-ins as
  foreign files. Pass `XORGATE_HOME=/var/lib/xorgate-rs` to bench `xorgate-rs` calls, and run
  `scripts/bench/install.sh` again before `scripts/bench/exclusive.sh end`. The full recipe is
  `scripts/bench/convert-window.md`.
- **Test-only knob.** `XORGATE_RS_CONVERT_FAIL_AT=<n>` makes the script exit 9 right after step
  `<n>` with no cleanup, to simulate a crash for the Docker e2e (`tools/convert-e2e`). Never set
  it on a real device.

Tests: `scripts/tests/install-rs-unit.sh` and `scripts/tests/install-rs-bodies.sh` (Docker, see their
headers); the full matrix is `tools/convert-e2e/run.sh`.
