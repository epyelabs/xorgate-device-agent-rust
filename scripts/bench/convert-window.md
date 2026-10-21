# Bench conversion window — gate-runner checklist (Phase 5, G9–G11)

The plan's §12 recipe (`plans/device-agent-js-to-rust/phase-5-distribution.md`) as a checklist. Q1 = B: convert the CM5 bench in place, under the DEV identity, inside ONE exclusive window, then revert to the PROD JS agent. Only the gate runner runs this, and it is the only thing running on the bench at the time.

Bench: `pi@192.168.8.203`. The PROD JS agent `xorgate-agent` (58b51737…) is never commanded except by the window tooling and `install-rs.sh`. `sim7600-lte` is never restarted. Never reboot, never `--purge` (the bench has `rsPreexisting=yes`), never edit `/etc/xorgate/agent.env`.

Dev dist base URL, in the form devices get (CDK `DeviceDistBucketUrl` = `https://<bucketRegionalDomainName>`):
`https://xorgate-device-dist-dev-111910761410-us-east-1.s3.us-east-1.amazonaws.com`.
A LAN fake bucket works too: `http://<mac-ip>:8000`.

## What the tooling does for you (WP3, I6)
- `exclusive.sh end`, the `begin` abort trap and the dead-man all **enable** `xorgate-agent` when `is-enabled` ≠ `enabled`, then start it and verify (CF64). A conversion disables it.
- `exclusive.sh begin|tape-off|end`, `profile.sh` and `start.sh` **refuse** to (re)start a Rust unit whose `Conflicts=` names `xorgate-agent.service`, which is how `install-rs.sh` installs the shipped unit. They print:
  `shipped (converted) unit installed — run scripts/bench/install.sh first; NOT starting xorgate-agent-rs (it would stop the PROD JS agent)` (CF65).
- The **dead-man** (armed by `begin`, default 180 min) runs `/opt/xorgate-rs/install-rs.sh --revert` first when `/var/lib/xorgate-rs-install/record` says `state=converted|converting`. It then enables, starts and verifies the JS agent, even if the revert failed.
- `exclusive.sh end` and `status` only **warn** about an unreverted conversion: `conversion record … says state=converted — run: sudo /opt/xorgate-rs/install-rs.sh --revert; …`. The revert is your step (G11).

## Preconditions (else STOP: G9–G11 BLOCKED)
- [ ] The user has checked the bench PSU. `vcgencmd get_throttled` shows no undervoltage bit.
- [ ] The Phase 3 and Phase 4 bench windows have already run (one gate per window, CF71).
- [ ] The Alocate session `vynci-00` has been told the PROD bench device will be **OFFLINE** for the window, and to hold updates and commands to 58b51737.
- [ ] The build to convert to is in the dev bucket. Check: `scripts/dist-check.sh <dev-url> <sha> --channel dev` passes (G5). Otherwise serve a LAN fake holding build A, plus build B for G10.
- [ ] `scripts/bench/exclusive.sh status` shows `inputs: SAFE`, `conversion record: state=none` and no `UNSAFE` line.

## Steps
1. **Outside the window** (read-only): run the G6 capture, then the JS-surface pre-capture:
   `ssh pi@192.168.8.203 sudo bash -s < tools/convert-e2e/manifest.sh > plans/…/phase-5/evidence/bench-M0.manifest` (see WP2's README for arguments).
2. `scripts/bench/exclusive.sh begin`. This arms the dead-man, stops JS (never disables it) and applies the exclusive media-on profile.
   - [ ] `== window OPEN` printed. `exclusive.sh status` shows the dead-man `active`.
3. **Convert (G9).** Use the bucket copy (the Q3 one-liner, with `--from` set to the DEV bucket, because the bench `agent.env` names the PROD bucket), or `scp` the tarball and use `--tarball <file>` (its `.sha256` next to it):
   `ssh pi@192.168.8.203 'B=https://xorgate-device-dist-dev-111910761410-us-east-1.s3.us-east-1.amazonaws.com; cd /tmp && curl -fsSLO "$B/install-rs.sh" -O "$B/install-rs.sh.sha256" && sha256sum -c install-rs.sh.sha256 && sudo bash install-rs.sh --from "$B" -y'`
   - [ ] Final line: `[install-rs] converted: xorgate-agent-rs <ver> (dev) active; xorgate-agent stopped + disabled. …`
   - [ ] `90-bench-dev.conf`, `95-inputs.conf` and `99-exclusive.conf` are named as foreign drop-ins and kept. The identity stays DEV (a73f7a36…) and the state stays in `/var/lib/xorgate-rs`.
   - [ ] `pgrep -a node` is empty. `/proc/<master>/exe` is recorded (CF69: the JS tree's KVS runtime).
   - [ ] The dev console live tile plays cam0, and a recorded session lands in `media-sessions/v1/<a73f7a36…>/…`.
   - [ ] A second `sudo bash /opt/xorgate-rs/install-rs.sh` prints `changed (0)`.
   - Pass `XORGATE_HOME=/var/lib/xorgate-rs` to every bench `xorgate-rs` call from here on (CF66: the shipped wrapper defaults to `/var/lib/xorgate`).
4. **Update (G10).** Add the runtime drop-in `/run/systemd/system/xorgate-updater-rs.service.d/99-bench.conf` with `Environment=DEVICE_DIST_BUCKET_URL=<url>` (`10-dist.conf` already supplies `latest`). Run `daemon-reload`, then trigger `update` from the dev console.
   - [ ] The next report's `effective.agent.build.version` = build B. `10-dist.conf` is still present.
5. **Revert (G11).** `ssh pi@192.168.8.203 sudo /opt/xorgate-rs/install-rs.sh --revert` (keep mode).
   - [ ] Final line: `reverted: xorgate-agent enabled + active; xorgate-agent-rs disabled (files kept). JS install matches the pre-conversion record: yes`.
   - [ ] Re-run `manifest.sh`: the JS-owned rows equal `bench-M0.manifest`. `sim7600-lte` `ActiveEnterTimestamp` is unchanged.
   - [ ] The PROD device is online for its consumers.
6. **Restore the bench flavour BEFORE `end`.** `scripts/bench/install.sh <main tarball>` re-strips `Conflicts=` and restores the bench wrapper. Then `scripts/bench/profile.sh shared --no-restart`.
   - [ ] `exclusive.sh status` shows no `UNSAFE: shipped (converted) unit installed …`.
7. `scripts/bench/exclusive.sh end`.
   - [ ] `systemctl is-enabled xorgate-agent` = `enabled`, and `is-active` = `active` three times.
   - [ ] `xorgate-agent-rs` is active on DEV in the shared profile. The dead-man is cancelled.

## On ANY abort
1. `ssh pi@192.168.8.203 sudo /opt/xorgate-rs/install-rs.sh --revert` FIRST, if a conversion started (`exclusive.sh status` shows `conversion record: state=converted|converting`).
2. `scripts/bench/install.sh <main tarball>` (step 6).
3. `scripts/bench/exclusive.sh end`.
4. Never leave the PROD JS agent stopped or disabled. If you cannot finish, the dead-man reverts the conversion and enables and starts JS on its own when it fires.
5. A `throttled` undervoltage bit at any point: stop, revert, report.
