# tools/convert-e2e — G2: install-rs.sh over a simulated JS-provisioned device

Phase 5 WP2 (`plans/device-agent-js-to-rust/phase-5-distribution.md` §4, §5, §11 G2/G8).
The adversary of `scripts/install-rs.sh`: it never reads the converter's code and
has its own I3 manifest implementation (`manifest.sh`).

```
E2E_BIN=<shared aarch64 xorgate-rs> tools/convert-e2e/run.sh [case…]
#   INSTALL_RS=<path>  converter under test (default scripts/install-rs.sh)
#   JS_REF=<ref>       JS repo ref for /opt/xorgate + JS units (default 89b5c64)
#   DOCS_MD=<path>     runbook whose bootstrap one-liner is run verbatim (default docs/convert.md)
```

`E2E_BIN` is required; the harness never builds a binary (DISK BUDGET rule 1).
Everything it writes goes to `target/convert-e2e/` (gitignored): `in/`, `bucket/`,
`run-<ts>.log`. It uses one image tag, `xorgate-convert-e2e:bookworm`. Each case
runs in its own `--rm --init` container with no volumes.

## Pieces

| File | Role |
|---|---|
| `Dockerfile` | `debian:bookworm` arm64 + curl, ca-certificates, python3 (bucket only), sudo (visudo), procps, the GStreamer libs the binary links; user `pi`; the shims |
| `provision-js.sh` | rebuilds the device `install-script.ts` @ 96a91f2 leaves behind; each block cites its `be:<lines>` (`--svc`, `--no-user-dropin`, `--dist-version`, `--js` select the case's shape) |
| `manifest.sh` | independent I3: `f/d/l <mode> <uid:gid> <sha|-|->tgt> <path>` + `t <digest> <tree>`; `--expand`, `--root <prefix>`; read-only, so it also does G6 on the bench |
| `prep-bucket.sh` | the fake dist bucket: v1, v2 (pack.sh, re-packed with GNU tar `--owner=0`), `-latest` (= v1), `install-rs.sh` + sidecar, and the variants badsha, nosum, nomember-{buildinfo,installrs,updater}, wrongarch, runfails |
| `case.sh` | one case: provision → M0 → `sudo bash install-rs.sh …` as pi → exit code, I1 final line, I2 record, I3 diff, plus the G8 invariants every case ends with |
| `shims/systemctl` | enable/disable from `WantedBy=` (real `*.wants/` symlinks), is-enabled/is-active, `Conflicts=` both ways, `User=`/`Environment=`/`EnvironmentFile=`/`StateDirectory=` from the unit + drop-ins, notify = "alive after 3 s", oneshot runs synchronously, stale-config detection after daemon-reload; unmodelled calls fail loudly; reboot/poweroff are FORBIDDEN |
| `shims/node-stub` | `/usr/bin/node`: `node /opt/xorgate/dist/worker.js` stays up (`pgrep -f` = JS is running) |
| `shims/journalctl` | prints what the systemctl shim captured per unit |
| `shims/visudo` | logs every check (digest, whether 024 already exists), then runs the real visudo; refuses the 024 body in `visudo-refuses` |

The JS agent is the node-stub. The Rust agent is the real `E2E_BIN`, started from
the shipped unit's `ExecStart`. It stays up with the per-case local CA + device
cert, reconnecting to a closed `127.0.0.1:8883`. `sim7600-lte` is only ever marked
active, never executed.

## Cases (= G2)

convert-happy, convert-twice, convert-repair, revert-keep, revert-purge, revert-twice,
convert-revert-convert, update-after-convert, js-pin, bad-sha, missing-sidecar,
missing-member, wrong-arch, failed-start, interrupt-1…8, interrupt-1…8-revert,
root-svc, root-svc-dropin, js-disabled-before, js-stopped-before, not-root,
not-provisioned, foreign-dropin, bootstrap-oneliner, offline-revert, tarball-mode,
visudo-refuses, usage-status, wrapper-path-is-dir, predir-blocked-refuses (a file where a
needed directory goes: refused before any change), rollback-step6, rollback-step6-preexisting
(a REAL mid-step-6 write failure via a one-shot `install` shim in /usr/local/sbin),
bench-window, bench-window-rollback, status-nonroot, revert-runtime-drift,
failed-start-js-disabled, failed-start-js-stopped,
clean-fail-step8-{reload,enable-rs,disable-js,start-rs} (a clean failure: exit 4, JS back),
signal-convert-{hup,term}-{8,9}, signal-convert-int-9, signal-revert-{hup,term,int},
n7-refusal, lock-stale, lock-concurrent,
powercut-convert-1…14, powercut-revert-1…12 (a crash after the k-th systemctl call of
the run, then a simulated boot: ≥ 1 agent unit enabled, and a re-run converges).

Fault injection goes through the systemctl shim: `/run/e2e/inject` lines
`fail|crash|hup|int|term <ERE over the call>`, and `/run/e2e/crash-at <k>`.

Every case ends with `invariants`:
- The never-touched rows equal M0. These are `/etc/xorgate`, `/opt/xorgate`, the JS wrapper, unit, `.d/` and updater, sudoers 010 and 020–023, and the modem, system, cron, udev, NM and logind assets.
- A whole-filesystem footprint sweep finds nothing outside the I3 surface. It covers every file or link changed since provisioning and every new directory on the root fs, minus the commented allowlist in `case.sh` (`FP_ALLOW_LIST`). The I1 lock is released, and every `/opt/xorgate-rs*` on disk has its own I3 tree line.
- The shim saw no reboot or poweroff, no sim7600-lte action, no JS-updater start, no unmodelled call and no stale-config start.
- The sim7600-lte `ActiveEnterTimestamp` is unchanged.
- A source grep of the converter finds no reboot, poweroff or sim7600 restart.

A full run of the 121 cases takes about 10–12 minutes on an M-series Mac, one container at a time.

## Not modelled: Phase 6 / bench residuals

The shim is not systemd. These behaviours are NOT proven by G2, and need a real device (the Phase 6 soak, or a bench window):

| Residual | Why it matters | Where to prove it |
|---|---|---|
| A real boot with BOTH agent units enabled | It is reachable by design: a power cut between `enable xorgate-agent-rs` and `disable xorgate-agent` (the M2 order). systemd puts both start jobs in ONE transaction and resolves `Conflicts=` there ("Fixing conflicting jobs"). The shim starts them one by one, so the powercut cases run in both orders (`-rev` twins) and only assert "≥ 1 agent unit enabled, and a re-run converges". | Phase 6: leave a device with both enabled, reboot once, then `systemctl is-active` both + `journalctl -b` for the transaction |
| READY=1 and `WatchdogSec=60` | `Type=notify` is approximated as "alive after 3 s"; no sd_notify, no watchdog kill | bench G9 (`systemctl show -p ActiveState,NRestarts` over minutes) |
| `Restart=always` / `StartLimitBurst` | A dead service is reported `activating`; nothing restarts it | Phase 6 soak |
| The implicit reload `enable`/`disable` perform | The shim's stale-config check is coarser than systemd's | bench G9/G11 |
| `/run` wiped at boot | `power_cycle` removes the lock dir by hand; other `/run` state is kept | Phase 6 reboot |

## G6 (bench layout fidelity, read-only)

```
ssh pi@192.168.8.203 sudo bash -s < tools/convert-e2e/manifest.sh > bench.manifest
```
Then diff the path, type, mode and owner columns against a `provision-js.sh` device's M0.
