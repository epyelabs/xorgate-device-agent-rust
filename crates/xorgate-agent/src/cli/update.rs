//! `xorgate-rs update [--tag <version>] [--from <bucket-url>]` (Phase 4 WP4) —
//! the port of `ts:src/commands/update.ts:224-368` @ 89b5c64 without npm
//! (`phase-4-operations.md` §5 steps 1–8): download the `-rs` tarball and its
//! `.sha256` sidecar, verify it (Q14), stage it on the `/opt` filesystem,
//! smoke-exec the staged binary (Q13), atomically swap `/opt/xorgate-rs`
//! (keeping `.old`, rolled back on any later error), reinstall the unit, the
//! wrapper, the updater unit and the `024` grant, the shared modem/system
//! assets + `021`/`023` ONLY on a Rust-only device (Q1), then `daemon-reload`
//! and restart the agent ONLY if it was active (Q5).
//!
//! Every decision and text is in `xorgate_core::update_plan`; this module is
//! the I/O. v0.1.1 (review-v011-wp-a MUST-2, a deliberate divergence from
//! the TS `copyFileSync` in place): every file is installed atomically
//! (temp sibling + fsync + `rename(2)` + dir fsync, `install_file`), and a
//! failed install restores the files it replaced, not only `/opt`.
//! Subprocesses go through `xorgate_hal::exec::run` with bounded
//! timeouts (`curl` 10 min, `tar` 2 min, smoke 5 s, `visudo` 10 s,
//! `systemctl` 90 s); tools are found on `PATH` (rule 8) — [`Tools`] lets the
//! unit tests point them at shims. Nothing here runs `sudo`: the command needs
//! root and refuses otherwise (`update.ts:225-227`).

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use xorgate_core::camera_packages as pk;
use xorgate_core::update_plan::{self as plan, AssetPolicy, SudoersGrant};
use xorgate_hal::exec::{run, RunResult};

const CURL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const TAR_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const SMOKE_TIMEOUT: Duration = Duration::from_secs(5);
const VISUDO_TIMEOUT: Duration = Duration::from_secs(10);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(90);

/// The external tools, by `PATH` name in production.
#[derive(Debug, Clone)]
pub struct Tools {
    pub curl: String,
    pub tar: String,
    pub visudo: String,
    pub systemctl: String,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            curl: "curl".into(),
            tar: "tar".into(),
            visudo: "visudo".into(),
            systemctl: "systemctl".into(),
        }
    }
}

/// Where the rail reads and writes: every absolute path of
/// `xorgate_core::update_plan` resolved under `root` (`/` in production;
/// a temp dir in the unit tests). `tmp` is `os.tmpdir()` (`$TMPDIR` or
/// `/tmp`), where the download and the sudoers drafts go (`update.ts:120,252`).
#[derive(Debug, Clone)]
pub struct UpdatePaths {
    pub root: PathBuf,
    pub tmp: PathBuf,
    pub tools: Tools,
}

impl UpdatePaths {
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            tmp: std::env::temp_dir(),
            tools: Tools::default(),
        }
    }

    /// An absolute device path under `root`.
    pub fn abs(&self, p: &str) -> PathBuf {
        self.root.join(p.trim_start_matches('/'))
    }

    fn live(&self) -> PathBuf {
        self.abs(plan::INSTALL_PREFIX)
    }
    fn staging(&self) -> PathBuf {
        self.abs(plan::STAGING)
    }
    fn backup(&self) -> PathBuf {
        self.abs(plan::BACKUP)
    }
}

/// `update`'s output: `log` lines go to stdout with [`plan::LOG_PREFIX`].
pub trait UpdateLog {
    fn log(&mut self, line: &str);
}

/// Production sink: `console.log("[xorgate-rs] …")` (`update.ts:81`).
pub struct Stdout;

impl UpdateLog for Stdout {
    fn log(&mut self, line: &str) {
        // Never a panic (`cli::out`): the run may be past "updated to …"
        // and inside the camera-packages step when its reader goes away.
        super::out::out(&format!("{}{line}", plan::LOG_PREFIX));
    }
}

impl UpdateLog for Vec<String> {
    fn log(&mut self, line: &str) {
        self.push(line.to_string());
    }
}

/// `xorgate-rs update …` → exit code. The uid check is first (`update.ts:225`).
pub fn main(argv: &[String]) -> i32 {
    if !nix::unistd::getuid().is_root() {
        super::out::err(&format!("{}{}", plan::FAIL_PREFIX, plan::NOT_ROOT_MESSAGE));
        return 1;
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            super::out::err(&format!("{}{e}", plan::FAIL_PREFIX));
            return 1;
        }
    };
    let env = |k: &str| std::env::var(k).ok();
    match rt.block_on(update(argv, &env, &UpdatePaths::system(), &mut Stdout)) {
        Ok(()) => 0,
        Err(msg) => {
            super::out::err(&format!("{}{msg}", plan::FAIL_PREFIX));
            1
        }
    }
}

/// `readFileSync(path, "utf8")` as bytes + lossy decode (D4); `None` when
/// unreadable.
fn read_lossy(p: &Path) -> Option<String> {
    fs::read(p)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// `rmSync(p, { recursive: true, force: true })`.
fn rm_rf(p: &Path) {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => {
            let _ = fs::remove_dir_all(p);
        }
        Ok(_) => {
            let _ = fs::remove_file(p);
        }
        Err(_) => {}
    }
}

/// `mkdtempSync(join(tmpdir(), prefix))`.
fn mkdtemp(tmp: &Path, prefix: &str) -> io::Result<PathBuf> {
    nix::unistd::mkdtemp(&tmp.join(format!("{prefix}XXXXXX"))).map_err(io::Error::from)
}

fn exists(p: &Path) -> bool {
    p.exists()
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `copyFileSync(from, to)` + `chmodSync(to, mode)`, made ATOMIC (v0.1.1,
/// F-UPD-INPLACE, review-v011-wp-a MUST-2; a deliberate divergence from the
/// TS in-place copy): the bytes go to a temp file in `to`'s directory, which
/// is chmod-ed, fsync-ed and `rename(2)`-d over `to`, then the directory is
/// fsync-ed. A running reader of the old file (the bash of `sim7600-lte.sh`)
/// keeps the old inode, and a crash or ENOSPC never leaves a truncated file.
/// The previous file (if any) is recorded in `journal` so the rollback can put
/// it back, but ONLY once the rename has happened (fix-check MUST-3): a
/// replace that failed before its rename (EROFS, ENOSPC, a blocked temp)
/// changed nothing, so there is nothing to restore. The error keeps node's
/// wording (`EROFS: … copyfile 'a' -> 'b'`) for the rollback line.
fn install_file(from: &Path, to: &Path, mode: u32, journal: &mut Journal) -> Result<(), String> {
    let cf = |e: io::Error| format!("{e}, copyfile '{}' -> '{}'", from.display(), to.display());
    let bytes = fs::read(from).map_err(cf)?;
    install_bytes(to, &bytes, mode, journal).map_err(cf)
}

/// [`put_atomic`] with the journal: the snapshot of `to` is taken before the
/// write and committed once the rename happened (also when only the directory
/// fsync after it failed: `to` IS replaced then).
fn install_bytes(to: &Path, bytes: &[u8], mode: u32, journal: &mut Journal) -> io::Result<()> {
    let prev = journal.snapshot(to);
    let res = put_atomic_ex(to, bytes, mode);
    let renamed = match &res {
        Ok(()) => true,
        Err((_, renamed)) => *renamed,
    };
    if renamed {
        journal.commit(to, prev);
    }
    res.map_err(|(e, _)| e)
}

/// `to`'s temp sibling: a dot file with a `.xorgate-new` suffix, which every
/// directory the rail writes ignores (sudoers.d skips names with a `.`,
/// systemd/udev/NM/cron read only their own suffixes).
fn temp_sibling(to: &Path) -> PathBuf {
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    to.with_file_name(format!(".{name}.xorgate-new"))
}

/// Write `bytes` to `to` atomically (temp sibling, mode, fsync, rename, dir
/// fsync); the temp file is removed on any failure.
fn put_atomic(to: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    put_atomic_ex(to, bytes, mode).map_err(|(e, _)| e)
}

/// [`put_atomic`], and on an error whether the rename had already happened.
fn put_atomic_ex(to: &Path, bytes: &[u8], mode: u32) -> Result<(), (io::Error, bool)> {
    let tmp = temp_sibling(to);
    let _ = fs::remove_file(&tmp); // a crash leftover
    if let Err(e) = write_temp(&tmp, bytes, mode).and_then(|()| fs::rename(&tmp, to)) {
        let _ = fs::remove_file(&tmp);
        return Err((e, false));
    }
    if let Some(dir) = to.parent() {
        fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| (e, true))?;
    }
    Ok(())
}

fn write_temp(tmp: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    f.write_all(bytes)?;
    // fchmod: the final mode exactly, not under the umask.
    f.set_permissions(fs::Permissions::from_mode(mode))?;
    f.sync_all()
}

/// `p` is a regular file holding exactly `bytes` with mode `mode`.
fn file_is(p: &Path, bytes: &[u8], mode: u32) -> bool {
    matches!(fs::symlink_metadata(p), Ok(m) if m.is_file() && m.permissions().mode() & 0o7777 == mode)
        && fs::read(p).is_ok_and(|b| b == bytes)
}

/// What was at a path before `install` replaced it (review-v011-wp-a MUST-2).
#[derive(Debug, PartialEq)]
enum Prev {
    /// Nothing: a file the install created is removed on rollback.
    Absent,
    /// A regular file: its bytes and mode are put back (atomically).
    File(Vec<u8>, u32),
    /// A symlink (the rename replaced the link itself): re-created.
    Link(PathBuf),
    /// Anything else (a directory in the way): never touched on rollback.
    Other,
}

/// The files `install` replaced, with what was there before, so a failed
/// install restores them too (the TS rollback restored only `/opt`).
#[derive(Default)]
struct Journal {
    entries: Vec<(PathBuf, Prev)>,
}

impl Journal {
    /// What `to` holds now, unless it is already journaled in this run (the
    /// FIRST state is the one a rollback restores).
    fn snapshot(&self, to: &Path) -> Option<Prev> {
        if self.entries.iter().any(|(p, _)| p == to) {
            return None;
        }
        Some(match fs::symlink_metadata(to) {
            Err(_) => Prev::Absent,
            Ok(m) if m.is_file() => match fs::read(to) {
                Ok(b) => Prev::File(b, m.permissions().mode() & 0o7777),
                Err(_) => Prev::Other,
            },
            Ok(m) if m.file_type().is_symlink() => match fs::read_link(to) {
                Ok(t) => Prev::Link(t),
                Err(_) => Prev::Other,
            },
            Ok(_) => Prev::Other,
        })
    }

    /// Keep a snapshot once `to` has really been replaced.
    fn commit(&mut self, to: &Path, prev: Option<Prev>) {
        if let Some(prev) = prev {
            self.entries.push((to.to_path_buf(), prev));
        }
    }

    #[cfg(test)]
    fn record(&mut self, to: &Path) {
        let prev = self.snapshot(to);
        self.commit(to, prev);
    }

    /// Put every recorded path back, newest first; the failures, if any.
    /// Idempotent (fix-check MUST-3): a path that already holds its recorded
    /// state (a file with the same bytes and mode, a link to the same
    /// target) is left alone, so a re-run restore writes nothing.
    fn restore(&self) -> Vec<String> {
        let mut errs = Vec::new();
        for (path, prev) in self.entries.iter().rev() {
            let r = match prev {
                Prev::File(bytes, mode) if file_is(path, bytes, *mode) => Ok(()),
                Prev::File(bytes, mode) => put_atomic(path, bytes, *mode),
                Prev::Link(target) if fs::read_link(path).is_ok_and(|t| &t == target) => Ok(()),
                Prev::Absent => match fs::symlink_metadata(path) {
                    Ok(m) if m.is_file() || m.file_type().is_symlink() => fs::remove_file(path),
                    _ => Ok(()),
                },
                Prev::Link(target) => {
                    let tmp = temp_sibling(path);
                    let _ = fs::remove_file(&tmp);
                    std::os::unix::fs::symlink(target, &tmp)
                        .and_then(|()| fs::rename(&tmp, path))
                        .inspect_err(|_| {
                            let _ = fs::remove_file(&tmp);
                        })
                }
                Prev::Other => Ok(()),
            };
            if let Err(e) = r {
                errs.push(format!("{}: {e}", path.display()));
            }
        }
        errs
    }
}

/// `writeFileSync(p, body, { mode })` (mode applies on creation, under umask).
fn write_file(p: &Path, body: &str, mode: u32) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(p)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .map_err(|e| format!("{e}, open '{}'", p.display()))
}

fn sha256_file(p: &Path) -> io::Result<[u8; 32]> {
    let mut f = fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    Ok(out)
}

/// `run(cmd, args)` over owned strings.
async fn run_s(cmd: &str, args: &[String], t: Duration) -> RunResult {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    run(cmd, &a, Some(t)).await
}

/// `updateCommand(argv)` after the uid check (`update.ts:229-368`, §5 steps
/// 1–8). `Err(msg)` = `fail(msg)`: the caller prints [`plan::FAIL_PREFIX`] +
/// msg and exits 1. Every `Err` leaves the live tree as it was (a rollback
/// restores `.old`).
pub async fn update(
    argv: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    p: &UpdatePaths,
    out: &mut dyn UpdateLog,
) -> Result<(), String> {
    // For step 9: how much of the updater unit's budget this run has used,
    // and (under the unit: systemd sets INVOCATION_ID) what budget the
    // LOADED unit gives it. Asked now, before this run changes the unit
    // file, and again after its `daemon-reload`: the smaller answer counts.
    let started = std::time::Instant::now();
    let under_unit = env("INVOCATION_ID").is_some_and(|v| !v.is_empty());
    let budget_at_start = if under_unit {
        Some(loaded_unit_budget(p).await)
    } else {
        None
    };
    // Step 1: resolution (`update.ts:229-245`).
    let file = plan::parse_env_file(&read_lossy(&p.abs(plan::AGENT_ENV)).unwrap_or_default());
    let src = plan::resolve_source(
        plan::flag_value(argv, "--from"),
        plan::flag_value(argv, "--tag"),
        env,
        &file,
    )?;
    let tarball = plan::tarball_name(&src.version);
    let url = plan::artifact_url(&src, &tarball);

    // Step 2: download to a scratch dir outside /opt (`update.ts:250-259`).
    let dl = mkdtemp(&p.tmp, "xg-update-").map_err(|e| format!("mkdtemp: {e}"))?;
    let tgz = dl.join(&tarball);
    out.log(&format!("downloading {url}"));
    let got = run_s(
        &p.tools.curl,
        &plan::curl_args(&url, &path_str(&tgz)),
        CURL_TIMEOUT,
    )
    .await;
    if got.failed {
        rm_rf(&dl);
        return Err(plan::download_failed_message(&got.stderr, got.exit_code));
    }

    // Step 3 (Q14): the sidecar, fail-closed.
    let verified = verify_checksum(p, &src, &tarball, &dl, &tgz, out).await;
    if let Err(e) = verified {
        rm_rf(&dl);
        return Err(e);
    }

    // Step 4: stage + validate on /opt (`update.ts:261-277`).
    let staging = p.staging();
    rm_rf(&staging);
    if let Err(e) = fs::create_dir_all(&staging) {
        rm_rf(&dl);
        return Err(format!("{e}, mkdir '{}'", staging.display()));
    }
    // D-WP4-11: `--no-same-owner` — the TS ran plain `tar -xzf` (`update.ts:265`),
    // which as root restores the archive's owners; CI packs 0:0, but a Mac
    // (bsdtar) pack records the packing uid and would leave /opt/xorgate-rs
    // owned by uid 501. Modes are still restored (root's default). E9
    // `uid501-tarball` pins it.
    let untar = run_s(
        &p.tools.tar,
        &[
            "--no-same-owner".into(),
            "-xzf".into(),
            path_str(&tgz),
            "-C".into(),
            path_str(&staging),
        ],
        TAR_TIMEOUT,
    )
    .await;
    rm_rf(&dl);
    if untar.failed {
        rm_rf(&staging);
        return Err(format!(
            "extract failed: {}",
            xorgate_core::js::trim(&untar.stderr)
        ));
    }
    if plan::REQUIRED_MEMBERS
        .iter()
        .any(|m| !exists(&staging.join(m)))
    {
        rm_rf(&staging);
        return Err(format!(
            "downloaded build is missing {}",
            plan::REQUIRED_MEMBERS.join(", ")
        ));
    }
    // Q13: smoke-exec in place of `npm ci` (`update.ts:279-290`).
    if let Err(e) = smoke_exec(&staging, out).await {
        rm_rf(&staging);
        return Err(e);
    }

    // Step 5: the swap (`update.ts:292-297`). Q5: note whether the agent
    // runs BEFORE anything moves; Q1: the JS install decides asset ownership.
    let was_active = !run_s(
        &p.tools.systemctl,
        &["is-active".into(), "--quiet".into(), plan::SERVICE.into()],
        SYSTEMCTL_TIMEOUT,
    )
    .await
    .failed;
    let policy = plan::asset_policy(
        exists(&p.abs(plan::JS_INSTALL_PREFIX)),
        exists(&p.abs(plan::MODEM_DAEMON)),
    );
    let (live, backup) = (p.live(), p.backup());
    rm_rf(&backup);
    let had_prev = exists(&live);
    if had_prev {
        // D-WP4-8: the TS `renameSync(INSTALL_PREFIX, BACKUP)` sits OUTSIDE
        // its try (`update.ts:293-295`), so a failure there is an uncaught
        // throw (stack, exit 1). Here: drop the staged tree, leave the live
        // one where it is, print one `update failed:` line (exit 1).
        // Pinned by E9 `old-busy` (`.old` is a busy mountpoint → EBUSY).
        if let Err(e) = fs::rename(&live, &backup) {
            rm_rf(&staging);
            return Err(format!(
                "{e}, rename '{}' -> '{}'",
                live.display(),
                backup.display()
            ));
        }
    }
    let mut journal = Journal::default();
    if let Err(e) = install(p, policy, out, &mut journal).await {
        // Roll back to the previous build (`update.ts:351-356`), and (v0.1.1,
        // MUST-2) every file this install replaced: the unit, the wrapper,
        // the modem/system assets, the sudoers grants, the updater unit.
        let restore_errs = journal.restore();
        rm_rf(&live);
        if had_prev {
            if let Err(re) = fs::rename(&backup, &live) {
                return Err(format!("install step failed, rollback FAILED ({re}): {e}"));
            }
        }
        if !restore_errs.is_empty() {
            return Err(format!(
                "install step failed, rollback FAILED (restore {}): {e}",
                restore_errs.join("; ")
            ));
        }
        return Err(format!("install step failed, rolled back: {e}"));
    }

    // Step 8: reload + restart (`update.ts:358-364`), Q5.
    run_s(
        &p.tools.systemctl,
        &["daemon-reload".into()],
        SYSTEMCTL_TIMEOUT,
    )
    .await;
    if was_active {
        let restart = run_s(
            &p.tools.systemctl,
            &["restart".into(), plan::SERVICE.into()],
            SYSTEMCTL_TIMEOUT,
        )
        .await;
        if restart.failed {
            for l in plan::restart_failed_logs(&restart.stderr) {
                out.log(&l);
            }
        }
    } else {
        out.log(&plan::not_started_log());
    }
    rm_rf(&backup);
    out.log(&plan::updated_log(&src.version));

    // Step 9 (RUST-ONLY, support-other-camera-models Phase 4): the extra OS
    // packages the declared camera sensors need, by the NEW build (its
    // catalog holds the pins). AFTER the update is complete: the build is
    // in place, systemd reloaded, the agent restarted, `updated to …` said.
    // Whatever this step does or fails to do, the update has happened. It
    // restarts the agent once more itself, and only when it installed
    // something (`try-restart`).
    let home = env("XORGATE_HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| file.get("XORGATE_HOME").cloned().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| plan::DEFAULT_HOME.to_string());
    // Under the updater unit the step may use what is left of the unit's
    // start budget, less a margin; by hand, its own worst case. The budget
    // is what systemd says of the unit it has LOADED (before and after the
    // reload above, whose result is not taken on trust), never more than
    // this build's unit file gives, and the conservative floor when
    // systemd cannot be asked.
    let unit_budget = match budget_at_start {
        Some(at_start) => {
            let now = loaded_unit_budget(p).await;
            Some(plan::updater_unit_budget(&[
                at_start.as_deref(),
                now.as_deref(),
            ]))
        }
        None => None,
    };
    let limit = plan::camera_packages_limit(unit_budget, started.elapsed().as_secs());
    // SIGTERM from here on (systemd stopping the unit: its start timeout, a
    // `systemctl stop`, a shutdown) is passed on to the step and waited
    // out, instead of ending this process. See `camera_packages_step`.
    let stop = terminate_signal();
    camera_packages_step(
        p,
        &home,
        limit,
        Duration::from_secs(pk::STEP_GRACE),
        Duration::from_secs(pk::T_INSTALL + pk::T_AFTER + pk::STEP_GRACE),
        stop,
        out,
    )
    .await;
    Ok(())
}

/// What systemd says the LOADED updater unit's `TimeoutStartSec` is
/// (`45min`); `None` when it could not be asked.
async fn loaded_unit_budget(p: &UpdatePaths) -> Option<String> {
    let args: Vec<String> = plan::updater_budget_argv()
        .iter()
        .map(|a| a.to_string())
        .collect();
    let r = run_s(&p.tools.systemctl, &args, SYSTEMCTL_TIMEOUT).await;
    (!r.failed).then_some(r.stdout)
}

/// Resolves at the first SIGTERM. Once this is called SIGTERM no longer
/// ends the process by itself; it is called only where the camera-packages
/// step is about to run, the last thing an update does.
fn terminate_signal() -> impl std::future::Future<Output = ()> {
    let stream = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    async move {
        match stream {
            Ok(mut s) => {
                s.recv().await;
            }
            // It could not be caught: the default disposition stays.
            Err(_) => std::future::pending::<()>().await,
        }
    }
}

/// Q14 (§5 step 3): fetch `<tarball>.sha256`, find the tarball's entry and
/// compare; a missing sidecar, an unusable one or a mismatch is fatal.
async fn verify_checksum(
    p: &UpdatePaths,
    src: &plan::UpdateSource,
    tarball: &str,
    dl: &Path,
    tgz: &Path,
    out: &mut dyn UpdateLog,
) -> Result<(), String> {
    let sidecar = plan::sidecar_name(tarball);
    let sum_url = plan::artifact_url(src, &sidecar);
    let sum_path = dl.join(&sidecar);
    out.log(&format!("downloading {sum_url}"));
    let got = run_s(
        &p.tools.curl,
        &plan::curl_args(&sum_url, &path_str(&sum_path)),
        CURL_TIMEOUT,
    )
    .await;
    if got.failed {
        return Err(plan::no_checksum_message(
            tarball,
            &plan::failure_detail(&got.stderr, got.exit_code),
        ));
    }
    let text = read_lossy(&sum_path).unwrap_or_default();
    let Some(want) = plan::parse_sha256_sidecar(&text, tarball) else {
        return Err(plan::no_checksum_message(
            tarball,
            &format!("{sidecar} has no entry for it"),
        ));
    };
    let have = sha256_file(tgz).map_err(|e| format!("cannot hash {}: {e}", tgz.display()))?;
    if have != want {
        return Err(plan::checksum_mismatch_message(tarball, &want, &have));
    }
    out.log(&plan::checksum_ok_log(&have));
    Ok(())
}

/// Step 9: `<live>/bin/xorgate-rs camera requirements --install --quiet
/// --time-limit <limit>`, the installed build's own command
/// (`cli::camera_packages`): it installs the extra OS packages a declared
/// camera sensor needs when they are missing, restarts the agent when it
/// did, and prints nothing when there is nothing to do. Only its lines
/// that begin with `camera packages: ` are passed on, as they come (a
/// `WARN` among them when a download or a checksum failed: the system is
/// then as it was). A build without the command (an older `--tag`) prints
/// its usage, which is not passed on.
///
/// TIME. The command keeps to `limit` by itself: it cuts its killable
/// steps and starts the real `apt-get install` only when that can finish
/// inside the limit. This function is the backstop for a command that does
/// not keep its word:
/// - `limit + grace` after the start it sends the command SIGTERM. The
///   command then ends its killable step at once (it kills that step's
///   process group), or, if an apt transaction is running, finishes it
///   first: the transaction is never interrupted, and this function sends
///   nothing to it (it runs in its own process group).
/// - `last_resort` later still, a command that has not ended is killed as
///   a process group. That group does not hold a transaction's `apt-get`.
///
/// The command runs in its own process group, so nothing of it is left
/// running after this function returns, save an apt transaction that a
/// doubly broken command left behind, which is left to finish.
async fn camera_packages_step(
    p: &UpdatePaths,
    home: &str,
    limit: u64,
    grace: Duration,
    last_resort: Duration,
    stop: impl std::future::Future<Output = ()>,
    out: &mut dyn UpdateLog,
) {
    use tokio::io::AsyncBufReadExt;
    let prefix = pk::LINE_PREFIX;
    let bin = p.live().join(plan::BIN_SRC);
    let mut cmd = xorgate_hal::child::command(&bin);
    cmd.args([
        "camera",
        "requirements",
        "--install",
        "--quiet",
        "--time-limit",
        &limit.to_string(),
    ])
    .env("XORGATE_HOME", home)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .process_group(0);
    // It could not be run at all: the smoke-exec ran this very binary a
    // moment ago, so there is nothing useful to add.
    let Ok(mut child) = cmd.spawn() else {
        return;
    };
    let pid = child.id().map(|id| nix::unistd::Pid::from_raw(id as i32));
    let Some(stdout) = child.stdout.take() else {
        let _ = child.wait().await;
        return;
    };
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let cap = tokio::time::sleep(Duration::from_secs(limit) + grace);
    tokio::pin!(cap);
    let end = tokio::time::sleep(Duration::from_secs(limit) + grace + last_resort);
    tokio::pin!(end);
    tokio::pin!(stop);
    // Armed when the command has exited: what is still in its pipe is read
    // for a moment, no longer. (Not "until the pipe closes": a process the
    // command left behind with the pipe open must not hold the update.)
    let drained = tokio::time::sleep(Duration::from_secs(365 * 86_400));
    tokio::pin!(drained);
    let (mut asked, mut killed, mut passed_on, mut exited) = (false, false, false, false);
    loop {
        tokio::select! {
            _ = child.wait(), if !exited => {
                exited = true;
                drained
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(2));
            }
            _ = &mut drained, if exited => break,
            // This run was asked to stop (SIGTERM: systemd stopping the
            // unit). The request is passed on and WAITED OUT: the command
            // ends a killable step at once and finishes an apt transaction
            // first. This process must outlive it: with `KillMode=mixed`
            // systemd SIGKILLs whatever is left of the unit the moment its
            // main process is gone.
            _ = &mut stop, if !passed_on && !exited => {
                passed_on = true;
                if let Some(pid) = pid {
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
                }
                out.log(&format!(
                    "{prefix}this run was asked to stop: the step is asked to stop too, and is waited for (an apt transaction that is running finishes first)"
                ));
            }
            line = lines.next_line() => match line {
                Ok(Some(l)) if l.starts_with(prefix) => out.log(&l),
                Ok(Some(_)) => {}
                // End of its output: the command and whatever held its
                // pipe are gone.
                _ => break,
            },
            _ = &mut cap, if !asked && !exited => {
                asked = true;
                if let Some(pid) = pid {
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
                }
                out.log(&format!(
                    "{prefix}WARN the step did not keep to its time limit and was asked to stop (an apt transaction that is running is left to finish); the update itself is complete"
                ));
            }
            _ = &mut end, if asked && !killed && !exited => {
                killed = true;
                if let Some(pid) = pid {
                    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                }
                out.log(&format!(
                    "{prefix}WARN the step did not stop and was killed; check with `xorgate-rs camera requirements`"
                ));
            }
        }
    }
    let _ = child.wait().await;
}

/// Q13 (§5 step 4): `<staging>/bin/xorgate-rs -v` must exit 0 within 5 s and
/// print the `xorgate-rs …` banner — a wrong-arch, truncated or foreign
/// binary fails here, before the live tree moves.
async fn smoke_exec(staging: &Path, out: &mut dyn UpdateLog) -> Result<(), String> {
    let bin = staging.join(plan::BIN_SRC);
    out.log(plan::SMOKE_LOG);
    let r = run(&path_str(&bin), &["-v"], Some(SMOKE_TIMEOUT)).await;
    let line = xorgate_core::js::trim(&r.stdout);
    if r.failed {
        return Err(plan::smoke_failed_message(&plan::smoke_detail(
            &r.stderr,
            r.exit_code,
        )));
    }
    if !r.stdout.starts_with(plan::SMOKE_PREFIX) {
        return Err(plan::smoke_failed_message(&format!("printed {line:?}")));
    }
    out.log(&format!("staged {line}"));
    Ok(())
}

/// Steps 5–7 inside the TS `try` (`update.ts:296-350`): any `Err` makes the
/// caller roll back.
async fn install(
    p: &UpdatePaths,
    policy: AssetPolicy,
    out: &mut dyn UpdateLog,
    journal: &mut Journal,
) -> Result<(), String> {
    let (staging, live) = (p.staging(), p.live());
    fs::rename(&staging, &live).map_err(|e| {
        format!(
            "{e}, rename '{}' -> '{}'",
            staging.display(),
            live.display()
        )
    })?;

    // Step 6: unit + wrapper (`update.ts:299-305`).
    let unit_dest = p.abs(plan::UNIT_DEST);
    install_file(&live.join(plan::UNIT_SRC), &unit_dest, 0o644, journal)?;
    let wrapper = p.abs(plan::WRAPPER);
    if fs::symlink_metadata(&wrapper).is_ok_and(|m| m.is_dir()) {
        rm_rf(&wrapper); // a directory in the way (rename cannot replace it)
    }
    // Atomic, like install_file; the rename also replaces a prior symlink
    // (it is never written through).
    install_bytes(&wrapper, plan::WRAPPER_BODY.as_bytes(), 0o755, journal)
        .map_err(|e| format!("{e}, open '{}'", wrapper.display()))?;

    // Step 7 (Q1): shared assets only on a Rust-only device.
    if policy.skip_shared {
        out.log(plan::SHARED_SKIP_LOG);
    } else {
        // `update.ts:307-323`.
        if policy.modem {
            let mut refreshed = 0;
            for (src, dest, mode) in plan::MODEM_ASSETS {
                let from = live.join(src);
                if !exists(&from) {
                    continue;
                }
                install_file(&from, &p.abs(dest), *mode, journal)?;
                refreshed += 1;
            }
            if refreshed > 0 {
                out.log(plan::MODEM_REFRESHED_LOG);
            }
        }
        // `update.ts:325-339`.
        if policy.system {
            for (src, dest, mode) in plan::SYSTEM_ASSETS {
                let from = live.join(src);
                if !exists(&from) {
                    continue;
                }
                install_file(&from, &p.abs(dest), *mode, journal)?;
            }
            install_sudoers(p, SudoersGrant::Clock, out, journal).await?;
            if exists(&p.abs(plan::CAMROTATE_BIN)) {
                install_sudoers(p, SudoersGrant::CamRotate, out, journal).await?;
            }
        }
    }

    // The camera-line owner (Rust-only, never shared, so installed on a
    // coexisting device too): its allow-list, the helper, then `025` once
    // the helper is in place.
    for (src, dest, mode) in plan::RS_SYSTEM_ASSETS {
        let from = live.join(src);
        if !exists(&from) {
            continue;
        }
        install_file(&from, &p.abs(dest), *mode, journal)?;
    }
    if exists(&p.abs(plan::CAMCFG_BIN)) {
        install_sudoers(p, SudoersGrant::CamCfg, out, journal).await?;
    }

    // `update.ts:341-350`: the remote rail (Rust-only, never shared).
    let updater_unit = live.join(plan::UPDATER_UNIT_SRC);
    if exists(&updater_unit) {
        install_file(
            &updater_unit,
            &p.abs(plan::UPDATER_UNIT_DEST),
            0o644,
            journal,
        )?;
        install_sudoers(p, SudoersGrant::UpdateRs, out, journal).await?;
    }
    Ok(())
}

/// `serviceUser()` (`update.ts:97-105`): the Rust unit's `10-user.conf`,
/// falling back to the JS unit's when the Rust one is unreadable (§5 step 6:
/// on a coexisting device provisioning wrote only the JS drop-in).
fn service_user(p: &UpdatePaths) -> Option<String> {
    let rs = p.abs(&format!("{}.d/10-user.conf", plan::UNIT_DEST));
    let js = p.abs(&format!("{}.d/10-user.conf", plan::JS_UNIT_DEST));
    let text = read_lossy(&rs).or_else(|| read_lossy(&js))?;
    plan::parse_service_user(&text)
}

/// `install{Clock,CamRotate,Update}Sudoers` (`update.ts:113-205`): the body
/// goes to a mkdtemp draft (0440), `visudo -cf` must pass, then the SAME
/// bytes are installed atomically (0440; v0.1.1 [`install_file`]); a refusal
/// is a warning, never fatal; the draft dir is always removed. No sudoers
/// file is ever written without a passing `visudo -cf`, and the temp sibling
/// (`.024_….xorgate-new`) is a name `sudo`'s includedir skips.
async fn install_sudoers(
    p: &UpdatePaths,
    grant: SudoersGrant,
    out: &mut dyn UpdateLog,
    journal: &mut Journal,
) -> Result<(), String> {
    let user = service_user(p);
    let Some(user) = plan::grant_user(user.as_deref()) else {
        return Ok(()); // root agent needs no sudo grant
    };
    let dir = mkdtemp(&p.tmp, "xg-sudoers-").map_err(|e| format!("mkdtemp: {e}"))?;
    let draft = dir.join(grant.file_name());
    let res = async {
        write_file(&draft, &grant.body(user), 0o440)?;
        let check = run(
            &p.tools.visudo,
            &["-cf", &path_str(&draft)],
            Some(VISUDO_TIMEOUT),
        )
        .await;
        if check.failed || check.not_found {
            out.log(grant.refused_warning());
            return Ok(());
        }
        let dest = p.abs(grant.dest());
        install_file(&draft, &dest, 0o440, journal)
    }
    .await;
    rm_rf(&dir);
    res
}

#[allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: test harness spawns, not the agent
#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_core::commands::{plan as cmd_plan, CommandPlan, DeviceCommandName};
    use xorgate_hal::privileged::PrivilegedArgv;

    /// E10 / CF47: the argv the agent runs for a cloud `update` and the
    /// `024` sudoers line are one spelling (`UPDATE_RS_ARGV`).
    #[test]
    fn update_argv_equals_the_024_grant() {
        let CommandPlan::Privileged(call) = cmd_plan(DeviceCommandName::Update) else {
            panic!("update is privileged");
        };
        let argv = PrivilegedArgv::for_rust_agent(&call);
        assert_eq!(argv.program(), "sudo");
        assert_eq!(argv.args(), plan::UPDATE_RS_ARGV);
        // sudoers names the absolute systemctl; the rest is argv[2..] verbatim.
        let line = plan::updater_sudoers_line("pi");
        let cmd = line.split_once("NOPASSWD: ").unwrap().1;
        let mut words = cmd.split(' ');
        assert_eq!(words.next(), Some(plan::SYSTEMCTL_PATH));
        assert!(plan::SYSTEMCTL_PATH.ends_with(&format!("/{}", argv.args()[1])));
        assert_eq!(words.collect::<Vec<_>>(), argv.args()[2..]);
        assert_eq!(argv.args()[0], "-n");
    }

    #[test]
    fn updater_unit_runs_the_rust_update_and_has_no_install_section() {
        let unit = include_str!("../../../../systemd/xorgate-updater-rs.service");
        let directives: Vec<&str> = unit
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .collect();
        assert!(directives.contains(&"Type=oneshot"));
        assert!(directives.contains(&"ExecStart=/usr/local/bin/xorgate-rs update"));
        // The budget the camera-packages step is bounded by is THIS line
        // (`update_plan::UPDATER_UNIT_BUDGET_SECS`).
        assert!(directives.contains(
            &format!("TimeoutStartSec={}min", plan::UPDATER_UNIT_BUDGET_SECS / 60).as_str()
        ));
        assert_eq!(plan::UPDATER_UNIT_BUDGET_SECS, 45 * 60);
        // A stop of the unit reaches the updater only, and systemd waits
        // long enough for an install that had begun (unit-ordering.sh G
        // proves what these two lines do under a real systemd).
        assert!(directives.contains(&"KillMode=mixed"));
        assert!(directives.contains(
            &format!("TimeoutStopSec={}min", plan::UPDATER_UNIT_STOP_SECS / 60).as_str()
        ));
        assert!(!directives.iter().any(|l| l.starts_with("[Install]")));
        assert!(!unit.contains("ExecStart=/usr/local/bin/xorgate update"));
    }

    // ---- an off-root run over shims (the full rail is tools/update-e2e) ----

    struct Rig {
        dir: PathBuf,
        p: UpdatePaths,
    }

    fn sh(path: &Path, body: &str) {
        xorgate_hal::test_support::write_executable(path, &format!("#!/bin/sh\n{body}\n")).unwrap();
    }

    fn rig(name: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("xg-wp4-{name}-{}", std::process::id()));
        rm_rf(&dir);
        let root = dir.join("root");
        let bin = dir.join("bin");
        let tmp = dir.join("tmp");
        let bucket = dir.join("bucket");
        for d in [
            &bin,
            &tmp,
            &bucket,
            &root.join("opt"),
            &root.join("etc/xorgate"),
            &root.join("etc/systemd/system"),
            &root.join("etc/sudoers.d"),
            &root.join("usr/local/bin"),
        ] {
            fs::create_dir_all(d).unwrap();
        }
        // curl <flags…> <url> -o <dest>: serve file://-style from the bucket dir.
        sh(
            &bin.join("curl"),
            &format!(
                "url=\"$6\"; dest=\"$8\"; f=\"{}/${{url##*/}}\"; [ -f \"$f\" ] || {{ echo 'curl: (22) The requested URL returned error: 404' >&2; exit 22; }}; cp \"$f\" \"$dest\"",
                bucket.display()
            ),
        );
        sh(&bin.join("visudo"), "exit 0");
        sh(
            &bin.join("systemctl"),
            &format!(
                "echo \"$*\" >> {}/systemctl.log; [ \"$1\" = is-active ] && exit 3; exit 0",
                dir.display()
            ),
        );
        let p = UpdatePaths {
            root,
            tmp,
            tools: Tools {
                curl: path_str(&bin.join("curl")),
                tar: "tar".into(),
                visudo: path_str(&bin.join("visudo")),
                systemctl: path_str(&bin.join("systemctl")),
            },
        };
        Rig { dir, p }
    }

    /// A tarball whose `bin/xorgate-rs` is a shell script printing a banner.
    fn publish(r: &Rig, version: &str, banner: &str, with_sidecar: bool) {
        publish_with(r, version, banner, with_sidecar, &[]);
    }

    /// [`publish`] plus extra members (`(path, body)`, e.g. `modem/…`).
    fn publish_with(
        r: &Rig,
        version: &str,
        banner: &str,
        with_sidecar: bool,
        extra: &[(&str, &str)],
    ) {
        let tree = r.dir.join(format!("tree-{version}"));
        for d in ["bin", "systemd"] {
            fs::create_dir_all(tree.join(d)).unwrap();
        }
        sh(&tree.join("bin/xorgate-rs"), &format!("echo '{banner}'"));
        fs::write(
            tree.join("systemd/xorgate-agent-rs.service"),
            format!("unit {version}\n"),
        )
        .unwrap();
        fs::write(tree.join("systemd/xorgate-updater-rs.service"), "updater\n").unwrap();
        fs::write(
            tree.join("build-info.json"),
            format!("{{\"version\":\"{version}\"}}"),
        )
        .unwrap();
        let mut members = vec!["bin", "systemd", "build-info.json"];
        for (path, body) in extra {
            let f = tree.join(path);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(&f, body).unwrap();
            let top = path.split('/').next().unwrap();
            if !members.contains(&top) {
                members.push(top);
            }
        }
        let tb = plan::tarball_name(version);
        let tgz = r.dir.join("bucket").join(&tb);
        let st = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&tree)
            .args(&members)
            .status()
            .unwrap();
        assert!(st.success());
        if with_sidecar {
            let h = plan::hex32(&sha256_file(&tgz).unwrap());
            fs::write(
                r.dir.join("bucket").join(format!("{tb}.sha256")),
                format!("{h}  {tb}\n"),
            )
            .unwrap();
        }
    }

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn happy_path_off_root_inactive_agent() {
        let r = rig("happy");
        publish(&r, "v1", "xorgate-rs 0.0.1 (v1, dev)", true);
        fs::write(
            r.p.abs(plan::AGENT_ENV),
            "DEVICE_DIST_BUCKET_URL=https://bucket.example/dist/\n",
        )
        .unwrap();
        let mut log: Vec<String> = Vec::new();
        update(&argv(&["--tag", "v1"]), &|_| None, &r.p, &mut log)
            .await
            .unwrap();
        let tb = "xorgate-agent-rs-v1.tar.gz";
        assert_eq!(
            log[0],
            format!("downloading https://bucket.example/dist/{tb}")
        );
        assert_eq!(
            log[1],
            format!("downloading https://bucket.example/dist/{tb}.sha256")
        );
        assert!(log[2].starts_with("checksum ok (sha256 "), "{log:?}");
        assert_eq!(log[3], plan::SMOKE_LOG);
        assert_eq!(log[4], "staged xorgate-rs 0.0.1 (v1, dev)");
        // no /opt/xorgate → Rust-only; no modem daemon; no user drop-in → no grants.
        assert_eq!(log[5], plan::not_started_log());
        assert_eq!(log[6], plan::updated_log("v1"));
        assert_eq!(log.len(), 7);
        assert_eq!(
            fs::read_to_string(r.p.abs(plan::WRAPPER)).unwrap(),
            plan::WRAPPER_BODY
        );
        assert_eq!(
            fs::read_to_string(r.p.abs(plan::UNIT_DEST)).unwrap(),
            "unit v1\n"
        );
        assert!(!r.p.backup().exists() && !r.p.staging().exists());
        let sc = fs::read_to_string(r.dir.join("systemctl.log")).unwrap();
        assert_eq!(sc, "is-active --quiet xorgate-agent-rs\ndaemon-reload\n");
        assert_eq!(
            fs::read_dir(&r.p.tmp).unwrap().count(),
            0,
            "scratch dirs removed"
        );
        rm_rf(&r.dir);
    }

    /// The camera-line owner (support-other-camera-models Phase 1) is
    /// Rust-only, so `xorgate-rs update` delivers it on EVERY device: its
    /// allow-list, the helper and the `025` grant land on a coexisting
    /// device (`/opt/xorgate` present: a device converted in place) too,
    /// where the shared system assets and 021/023 are left to the JS agent.
    #[tokio::test]
    async fn the_camera_line_owner_is_installed_on_a_coexisting_device_too() {
        for coexisting in [true, false] {
            let r = rig(if coexisting { "camcfg-js" } else { "camcfg-rs" });
            publish_with(
                &r,
                "v1",
                "xorgate-rs 0.0.1 (v1, dev)",
                true,
                &[
                    ("system/xorgate-camcfg", "#!/bin/sh\n# camcfg v1\n"),
                    ("system/xorgate-camcfg.overlays", "imx290\n"),
                    ("system/xorgate-camrotate", "#!/bin/sh\n# camrotate v1\n"),
                    ("system/xorgate-setclock", "#!/bin/sh\n# setclock v1\n"),
                ],
            );
            fs::write(
                r.p.abs(plan::AGENT_ENV),
                "DEVICE_DIST_BUCKET_URL=https://bucket.example/dist/\n",
            )
            .unwrap();
            // The service user comes from the JS unit's drop-in (a converted
            // device), so the grants are written for `pi`.
            let dropin = r.p.abs(&format!("{}.d/10-user.conf", plan::JS_UNIT_DEST));
            fs::create_dir_all(dropin.parent().unwrap()).unwrap();
            fs::write(&dropin, "[Service]\nUser=pi\n").unwrap();
            if coexisting {
                fs::create_dir_all(r.p.abs(plan::JS_INSTALL_PREFIX)).unwrap();
                // The JS agent's helper, as its installer left it.
                fs::write(r.p.abs(plan::CAMROTATE_BIN), "#!/bin/sh\n# JS camrotate\n").unwrap();
            }
            let mut log: Vec<String> = Vec::new();
            update(&argv(&["--tag", "v1"]), &|_| None, &r.p, &mut log)
                .await
                .unwrap();
            assert_eq!(
                log.contains(&plan::SHARED_SKIP_LOG.to_string()),
                coexisting,
                "{log:?}"
            );
            // The helper, its allow-list and 025: either way.
            let helper = r.p.abs(plan::CAMCFG_BIN);
            assert_eq!(
                fs::read_to_string(&helper).unwrap(),
                "#!/bin/sh\n# camcfg v1\n"
            );
            assert_eq!(mode_of(&helper), 0o755);
            let list = r.p.abs(xorgate_core::boot_config::CAMCFG_ALLOW_LIST);
            assert_eq!(fs::read_to_string(&list).unwrap(), "imx290\n");
            assert_eq!(mode_of(&list), 0o644);
            let grant = r.p.abs(plan::CAMCFG_SUDOERS_DEST);
            assert_eq!(
                fs::read_to_string(&grant).unwrap(),
                plan::camcfg_sudoers_body("pi")
            );
            assert_eq!(mode_of(&grant), 0o440);
            // The shared helpers and their grants: only on a Rust-only device.
            let camrotate = fs::read_to_string(r.p.abs(plan::CAMROTATE_BIN)).unwrap();
            if coexisting {
                assert_eq!(
                    camrotate, "#!/bin/sh\n# JS camrotate\n",
                    "left to the JS agent"
                );
                assert!(!r.p.abs("/usr/local/bin/xorgate-setclock").exists());
                assert!(!r.p.abs(plan::CAMROTATE_SUDOERS_DEST).exists());
                assert!(!r.p.abs(plan::CLOCK_SUDOERS_DEST).exists());
            } else {
                assert_eq!(camrotate, "#!/bin/sh\n# camrotate v1\n");
                assert!(r.p.abs(plan::CAMROTATE_SUDOERS_DEST).exists());
                assert!(r.p.abs(plan::CLOCK_SUDOERS_DEST).exists());
            }
            rm_rf(&r.dir);
        }
        // A build that ships no helper (an older tarball) installs no grant.
        let r = rig("camcfg-none");
        publish(&r, "v1", "xorgate-rs 0.0.1 (v1, dev)", true);
        fs::write(
            r.p.abs(plan::AGENT_ENV),
            "DEVICE_DIST_BUCKET_URL=https://bucket.example/dist/\n",
        )
        .unwrap();
        let dropin = r.p.abs(&format!("{}.d/10-user.conf", plan::JS_UNIT_DEST));
        fs::create_dir_all(dropin.parent().unwrap()).unwrap();
        fs::write(&dropin, "[Service]\nUser=pi\n").unwrap();
        let mut log: Vec<String> = Vec::new();
        update(&argv(&["--tag", "v1"]), &|_| None, &r.p, &mut log)
            .await
            .unwrap();
        assert!(!r.p.abs(plan::CAMCFG_BIN).exists());
        assert!(!r.p.abs(plan::CAMCFG_SUDOERS_DEST).exists());
        rm_rf(&r.dir);
    }

    #[tokio::test]
    async fn missing_sidecar_and_mismatch_never_touch_opt() {
        let r = rig("sha");
        publish(&r, "v1", "xorgate-rs x", false);
        let mut log: Vec<String> = Vec::new();
        let e = update(
            &argv(&["--from", "http://b", "--tag", "v1"]),
            &|_| None,
            &r.p,
            &mut log,
        )
        .await
        .unwrap_err();
        assert_eq!(
            e,
            plan::no_checksum_message(
                "xorgate-agent-rs-v1.tar.gz",
                "curl: (22) The requested URL returned error: 404"
            )
        );
        fs::write(
            r.dir.join("bucket/xorgate-agent-rs-v1.tar.gz.sha256"),
            format!("{}  xorgate-agent-rs-v1.tar.gz\n", "0".repeat(64)),
        )
        .unwrap();
        let e = update(
            &argv(&["--from", "http://b", "--tag", "v1"]),
            &|_| None,
            &r.p,
            &mut log,
        )
        .await
        .unwrap_err();
        assert!(
            e.starts_with("checksum mismatch for xorgate-agent-rs-v1.tar.gz"),
            "{e}"
        );
        assert_eq!(fs::read_dir(r.p.abs("/opt")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&r.p.tmp).unwrap().count(), 0);
        rm_rf(&r.dir);
    }

    fn mode_of(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn ino(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(p).unwrap().ino()
    }

    /// v0.1.1 MUST-2 (F-UPD-INPLACE): the destination is REPLACED (a new
    /// inode), never rewritten: a reader holding the old file (bash running
    /// sim7600-lte.sh) still reads the old bytes; the mode is exact; no temp
    /// sibling is left; the journal holds the old bytes + mode.
    #[test]
    fn install_file_replaces_atomically_and_an_open_reader_keeps_the_old_bytes() {
        let dir = std::env::temp_dir().join(format!("xg-atomic-{}", std::process::id()));
        rm_rf(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("new.sh"), dir.join("sim7600-lte.sh"));
        fs::write(&src, "#!/bin/bash\necho NEW, and longer than the old one\n").unwrap();
        fs::write(&dest, "#!/bin/bash\necho OLD\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        let ino0 = ino(&dest);
        let mut reader = fs::File::open(&dest).unwrap();
        let mut j = Journal::default();
        install_file(&src, &dest, 0o755, &mut j).unwrap();
        let mut old = String::new();
        reader.read_to_string(&mut old).unwrap();
        assert_eq!(
            old, "#!/bin/bash\necho OLD\n",
            "the open fd still reads the old file"
        );
        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "#!/bin/bash\necho NEW, and longer than the old one\n"
        );
        assert_ne!(ino(&dest), ino0, "a new inode, not an in-place rewrite");
        assert_eq!(mode_of(&dest), 0o755);
        assert!(!temp_sibling(&dest).exists(), "no temp sibling left");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        assert_eq!(j.entries.len(), 1);
        assert_eq!(
            j.entries[0].1,
            Prev::File(b"#!/bin/bash\necho OLD\n".to_vec(), 0o644)
        );
        // A failed replace (a non-empty directory in the way) removes its
        // temp file and leaves the directory as it was.
        let blocker = dir.join("blocked");
        fs::create_dir_all(blocker.join("x")).unwrap();
        let e = install_file(&src, &blocker, 0o644, &mut j).unwrap_err();
        assert!(e.contains("copyfile '") && e.contains("blocked'"), "{e}");
        assert!(!temp_sibling(&blocker).exists());
        assert!(blocker.join("x").is_dir());
        rm_rf(&dir);
    }

    /// v0.1.1 MUST-2: a failure late in the install restores EVERY file the
    /// install replaced (the unit, the wrapper, the modem assets), removes the
    /// ones it created, and rolls /opt back; the TS restored only /opt.
    #[tokio::test]
    async fn a_failed_install_restores_the_replaced_files_too() {
        let r = rig("rollback");
        publish_with(
            &r,
            "v2",
            "xorgate-rs 0.1.1 (v2, dev)",
            true,
            &[
                ("modem/sim7600-lte.sh", "#!/bin/bash\n# v2 daemon\n"),
                ("modem/99-sim7600-lte.rules", "# v2 rules\n"),
                ("system/xorgate-setclock", "#!/bin/sh\n# v2\n"),
            ],
        );
        fs::write(
            r.p.abs(plan::AGENT_ENV),
            "DEVICE_DIST_BUCKET_URL=https://bucket.example/dist/\n",
        )
        .unwrap();
        // The device before: build v1, its unit and wrapper, the v1 daemon.
        let live = r.p.live();
        fs::create_dir_all(live.join("bin")).unwrap();
        fs::write(live.join("build-info.json"), "{\"version\":\"v1\"}").unwrap();
        fs::write(r.p.abs(plan::UNIT_DEST), "unit v1\n").unwrap();
        fs::write(r.p.abs(plan::WRAPPER), "wrapper v1\n").unwrap();
        fs::set_permissions(r.p.abs(plan::WRAPPER), fs::Permissions::from_mode(0o755)).unwrap();
        let daemon = r.p.abs(plan::MODEM_DAEMON);
        fs::write(&daemon, "#!/bin/bash\n# v1 daemon\n").unwrap();
        fs::set_permissions(&daemon, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(r.p.abs("/etc/udev/rules.d")).unwrap();
        // The injected failure: a non-empty directory where the first system
        // asset goes (after the unit, the wrapper and the modem assets).
        fs::create_dir_all(r.p.abs("/usr/local/bin/xorgate-setclock/in-the-way")).unwrap();
        let mut log: Vec<String> = Vec::new();
        let e = update(&argv(&["--tag", "v2"]), &|_| None, &r.p, &mut log)
            .await
            .unwrap_err();
        assert!(
            e.starts_with("install step failed, rolled back: ") && e.contains("xorgate-setclock"),
            "{e}"
        );
        assert!(
            log.iter().any(|l| l == plan::MODEM_REFRESHED_LOG),
            "{log:?}"
        );
        assert_eq!(
            fs::read_to_string(&daemon).unwrap(),
            "#!/bin/bash\n# v1 daemon\n"
        );
        assert_eq!(mode_of(&daemon), 0o755);
        assert_eq!(
            fs::read_to_string(r.p.abs(plan::UNIT_DEST)).unwrap(),
            "unit v1\n"
        );
        assert_eq!(
            fs::read_to_string(r.p.abs(plan::WRAPPER)).unwrap(),
            "wrapper v1\n"
        );
        assert!(
            !r.p.abs("/etc/udev/rules.d/99-sim7600-lte.rules").exists(),
            "a file the failed install created is removed"
        );
        assert_eq!(
            fs::read_to_string(live.join("build-info.json")).unwrap(),
            "{\"version\":\"v1\"}",
            "/opt rolled back"
        );
        assert!(!r.p.backup().exists() && !r.p.staging().exists());
        assert!(
            r.p.abs("/usr/local/bin/xorgate-setclock/in-the-way")
                .is_dir(),
            "what was in the way is left alone"
        );
        for d in ["/usr/local/bin", "/etc/systemd/system", "/etc/udev/rules.d"] {
            let left: Vec<_> = fs::read_dir(r.p.abs(d))
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".xorgate-new"))
                .collect();
            assert!(left.is_empty(), "temp siblings left in {d}");
        }
        rm_rf(&r.dir);
    }

    /// fix-check MUST-3: a replace that fails BEFORE its rename (here its
    /// temp sibling is blocked; EROFS/ENOSPC fail the same way) changes
    /// nothing, so it is not journaled and the rollback has nothing to do;
    /// and `restore` of a file that already holds its recorded state is a
    /// no-op, never an error.
    #[test]
    fn restore_after_a_failed_replace_is_not_a_rollback_failure() {
        let dir = std::env::temp_dir().join(format!("xg-must3-{}", std::process::id()));
        rm_rf(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (src, dest) = (dir.join("new.service"), dir.join("unit.service"));
        fs::write(&src, "new\n").unwrap();
        fs::write(&dest, "old\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o644)).unwrap();
        fs::create_dir_all(temp_sibling(&dest).join("x")).unwrap(); // the temp is blocked
        let mut j = Journal::default();
        assert!(install_file(&src, &dest, 0o644, &mut j).is_err());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        assert!(
            j.entries.is_empty(),
            "a replace that never renamed is not journaled"
        );
        assert!(j.restore().is_empty());
        // (b) idempotent: a File entry whose path already holds those bytes
        // and that mode is skipped, even where a write would fail.
        let mut j = Journal::default();
        j.record(&dest);
        assert_eq!(j.restore(), Vec::<String>::new());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old\n");
        // A link already pointing at its recorded target is skipped too.
        let link = dir.join("link");
        std::os::unix::fs::symlink("unit.service", &link).unwrap();
        fs::create_dir_all(temp_sibling(&link).join("x")).unwrap();
        let mut j = Journal::default();
        j.record(&link);
        assert_eq!(j.restore(), Vec::<String>::new());
        rm_rf(&dir);
    }

    /// fix-check MUST-3 / update-e2e `ro-systemd`: an `update()` that fails
    /// at its FIRST install step (the agent unit cannot be replaced) reports
    /// "rolled back", never "rollback FAILED", and leaves the unit as it was.
    #[tokio::test]
    async fn a_failure_at_the_first_install_step_reports_rolled_back() {
        let r = rig("first-step");
        publish(&r, "v2", "xorgate-rs 0.1.1 (v2, dev)", true);
        fs::write(
            r.p.abs(plan::AGENT_ENV),
            "DEVICE_DIST_BUCKET_URL=https://bucket.example/dist/\n",
        )
        .unwrap();
        let live = r.p.live();
        fs::create_dir_all(live.join("bin")).unwrap();
        fs::write(live.join("build-info.json"), "{\"version\":\"v1\"}").unwrap();
        let unit = r.p.abs(plan::UNIT_DEST);
        fs::write(&unit, "unit v1\n").unwrap();
        // The unit's directory refuses the temp file (as EROFS would).
        fs::create_dir_all(temp_sibling(&unit).join("x")).unwrap();
        let mut log: Vec<String> = Vec::new();
        let e = update(&argv(&["--tag", "v2"]), &|_| None, &r.p, &mut log)
            .await
            .unwrap_err();
        assert!(e.starts_with("install step failed, rolled back: "), "{e}");
        assert!(!e.contains("rollback FAILED"), "{e}");
        assert!(
            e.contains("copyfile '") && e.contains("xorgate-agent-rs.service'"),
            "{e}"
        );
        assert_eq!(fs::read_to_string(&unit).unwrap(), "unit v1\n");
        assert_eq!(
            fs::read_to_string(live.join("build-info.json")).unwrap(),
            "{\"version\":\"v1\"}"
        );
        assert!(!r.p.backup().exists() && !r.p.staging().exists());
        rm_rf(&r.dir);
    }

    /// Step 9: AFTER the update is complete (the reload done, `updated to`
    /// said), the NEW build's `camera requirements --install --quiet
    /// --time-limit N` runs with the agent's home. Only its `camera
    /// packages: ` lines reach the log; a WARN among them (a failed
    /// download) does not fail the update; a build without the command
    /// says nothing here.
    #[tokio::test]
    async fn the_camera_packages_step_runs_after_the_update_and_never_fails_it() {
        // (the build's answer to `camera …`, the lines expected in the log)
        let cases: [(&str, &str, &[&str]); 3] = [
            (
                "installed",
                "echo 'camera packages: arducam-libcamera: installed libcamera0.5 2 (was 1)'; echo 'not for the log'; exit 0",
                &["camera packages: arducam-libcamera: installed libcamera0.5 2 (was 1)"],
            ),
            (
                "failed",
                "echo 'camera packages: WARN arducam-libcamera NOT installed: x was not accepted: sha256 mismatch; nothing was changed'; exit 1",
                &["camera packages: WARN arducam-libcamera NOT installed: x was not accepted: sha256 mismatch; nothing was changed"],
            ),
            // An older build: usage on stderr, exit 2.
            ("older", "echo 'usage: xorgate-rs camera apply' >&2; exit 2", &[]),
        ];
        for (name, answer, expected) in cases {
            let r = rig(&format!("campkg-{name}"));
            let args = r.dir.join("camera.args");
            // What systemctl had been asked by the time the command ran.
            let banner = format!(
                "' >/dev/null; [ \"$1\" = camera ] && {{ echo \"$XORGATE_HOME|$*\" > {}; cp {}/systemctl.log {}/systemctl.then; {answer}; }}; echo 'xorgate-rs 0.0.1 (v1, dev)",
                args.display(),
                r.dir.display(),
                r.dir.display()
            );
            publish(&r, "v1", &banner, true);
            fs::write(
                r.p.abs(plan::AGENT_ENV),
                "DEVICE_DIST_BUCKET_URL=https://bucket.example/dist/\nXORGATE_HOME=/srv/agent-home\n",
            )
            .unwrap();
            let mut log: Vec<String> = Vec::new();
            update(&argv(&["--tag", "v1"]), &|_| None, &r.p, &mut log)
                .await
                .unwrap_or_else(|e| panic!("{name}: the update must not fail: {e}"));
            // The new build was asked, with the home of agent.env and (by
            // hand: no unit budget) the step's own time limit. It restarts
            // the agent itself when it installed: no --no-restart.
            assert_eq!(
                fs::read_to_string(&args).unwrap(),
                "/srv/agent-home|camera requirements --install --quiet --time-limit 1320\n",
                "{name}"
            );
            // The update was COMPLETE by then: systemd reloaded (and the
            // agent restarted, had it been running), `updated to` logged
            // before any line of the step.
            assert_eq!(
                fs::read_to_string(r.dir.join("systemctl.then")).unwrap(),
                "is-active --quiet xorgate-agent-rs\ndaemon-reload\n",
                "{name}"
            );
            let done = log
                .iter()
                .position(|l| *l == plan::updated_log("v1"))
                .unwrap_or_else(|| panic!("{name}: {log:?}"));
            let got: Vec<&String> = log
                .iter()
                .filter(|l| {
                    l.contains("camera packages")
                        || l.contains("not for the log")
                        || l.contains("usage")
                })
                .collect();
            assert_eq!(got, expected.iter().collect::<Vec<_>>(), "{name}: {log:?}");
            assert_eq!(
                log[done + 1..],
                *expected,
                "{name}: the step's lines come last"
            );
            // The update itself asked systemctl nothing more.
            let sc = fs::read_to_string(r.dir.join("systemctl.log")).unwrap();
            assert_eq!(
                sc, "is-active --quiet xorgate-agent-rs\ndaemon-reload\n",
                "{name}"
            );
            assert!(r.p.live().join("bin/xorgate-rs").exists());
            assert!(!r.p.backup().exists());
            rm_rf(&r.dir);
        }
        // The environment's XORGATE_HOME wins; with neither, the wrapper's
        // default. By hand the limit is the step's own.
        for (env_home, want) in [
            (Some("/env/home"), "/env/home|1320"),
            (None, "/var/lib/xorgate|1320"),
        ] {
            let r = rig("campkg-home");
            let args = r.dir.join("camera.args");
            let banner = format!(
                "' >/dev/null; [ \"$1\" = camera ] && {{ echo \"$XORGATE_HOME|$6\" > {}; exit 0; }}; echo 'xorgate-rs 0.0.1 (v1, dev)",
                args.display()
            );
            publish(&r, "v1", &banner, true);
            fs::write(
                r.p.abs(plan::AGENT_ENV),
                "DEVICE_DIST_BUCKET_URL=https://bucket.example/dist/\n",
            )
            .unwrap();
            let env = move |k: &str| match k {
                "XORGATE_HOME" => env_home.map(str::to_string),
                _ => None,
            };
            let mut log: Vec<String> = Vec::new();
            update(&argv(&["--tag", "v1"]), &env, &r.p, &mut log)
                .await
                .unwrap();
            assert_eq!(fs::read_to_string(&args).unwrap(), format!("{want}\n"));
            rm_rf(&r.dir);
        }
    }

    /// Under the updater unit (systemd sets INVOCATION_ID) the step's limit
    /// comes from the budget of the unit systemd has LOADED, asked before
    /// this run touches the unit file and again after its daemon-reload:
    /// the smaller answer counts, never more than this build's own unit
    /// gives, and the 20-minute floor when systemd cannot be asked.
    #[tokio::test]
    async fn under_the_unit_the_limit_follows_the_loaded_units_budget() {
        // (what `systemctl show` answers the 1st and the 2nd time, the limit)
        let cases: [(&str, &str, &str, std::ops::RangeInclusive<u64>); 6] = [
            // This build's unit: the step's whole limit.
            ("ok", "echo 45min", "echo 45min", 1320..=1320),
            // The loaded unit still has the old 20 minutes (a drop-in, or
            // a daemon-reload that failed earlier): what is left of THOSE.
            ("old", "echo 20min", "echo 20min", 1070..=1080),
            // 20 at the start, 45 after this run's reload: the smaller.
            ("grew", "echo 20min", "echo 45min", 1070..=1080),
            // A longer one than this build's unit file: not trusted beyond it.
            ("long", "echo 2h", "echo infinity", 1320..=1320),
            // systemd cannot be asked, or answers something unreadable.
            ("fails", "exit 1", "echo 45min", 1070..=1080),
            ("odd", "echo 45min", "echo soon", 1070..=1080),
        ];
        for (name, first, second, want) in cases {
            let r = rig(&format!("campkg-unit-{name}"));
            let args = r.dir.join("camera.args");
            let banner = format!(
                "' >/dev/null; [ \"$1\" = camera ] && {{ echo \"$6\" > {}; exit 0; }}; echo 'xorgate-rs 0.0.1 (v1, dev)",
                args.display()
            );
            publish(&r, "v1", &banner, true);
            fs::write(
                r.p.abs(plan::AGENT_ENV),
                "DEVICE_DIST_BUCKET_URL=https://bucket.example/dist/\n",
            )
            .unwrap();
            // systemctl: `show` answers by turn; the rest as the rig's.
            sh(
                Path::new(&r.p.tools.systemctl),
                &format!(
                    "echo \"$*\" >> {d}/systemctl.log; if [ \"$1\" = show ]; then if [ -f {d}/asked ]; then {second}; else : > {d}/asked; {first}; fi; exit $?; fi; [ \"$1\" = is-active ] && exit 3; exit 0",
                    d = r.dir.display()
                ),
            );
            let env = |k: &str| (k == "INVOCATION_ID").then(|| "0123456789abcdef".to_string());
            let mut log: Vec<String> = Vec::new();
            update(&argv(&["--tag", "v1"]), &env, &r.p, &mut log)
                .await
                .unwrap();
            let limit: u64 = fs::read_to_string(&args).unwrap().trim().parse().unwrap();
            assert!(want.contains(&limit), "{name}: --time-limit {limit}");
            // Asked before anything else, and again after the reload.
            assert_eq!(
                fs::read_to_string(r.dir.join("systemctl.log")).unwrap(),
                "show -p TimeoutStartUSec --value xorgate-updater-rs.service\nis-active --quiet xorgate-agent-rs\ndaemon-reload\nshow -p TimeoutStartUSec --value xorgate-updater-rs.service\n",
                "{name}"
            );
            rm_rf(&r.dir);
        }
    }

    /// The limit the step is given: its own by hand; under the updater unit
    /// never more than what is left of the unit's budget less the margin,
    /// so systemd's timeout cannot fall into an apt transaction.
    #[test]
    fn the_camera_packages_step_fits_the_updater_unit() {
        let (budget, margin) = (
            plan::UPDATER_UNIT_BUDGET_SECS,
            plan::UPDATER_UNIT_MARGIN_SECS,
        );
        let unit = Some(budget);
        assert_eq!(pk::STEP_LIMIT, 1320);
        // By hand: nothing else bounds the run.
        assert_eq!(plan::camera_packages_limit(None, 0), 1320);
        assert_eq!(plan::camera_packages_limit(None, 100_000), 1320);
        // Under the unit: the whole limit while it fits…
        assert_eq!(plan::camera_packages_limit(unit, 0), 1320);
        assert_eq!(
            plan::camera_packages_limit(unit, budget - margin - 1320),
            1320
        );
        // …then what is left, down to nothing.
        assert_eq!(
            plan::camera_packages_limit(unit, budget - margin - 1000),
            1000
        );
        assert_eq!(plan::camera_packages_limit(unit, budget - margin), 0);
        assert_eq!(plan::camera_packages_limit(unit, budget + 500), 0);
        // The backstop (the limit, the grace, a few seconds to stop) ends
        // inside the unit's budget for every start time and every budget…
        for b in [1200, 1800, budget] {
            for elapsed in [0, 60, 600, 1260, 1261, 2000, 2579, 2580, 2700] {
                let limit = plan::camera_packages_limit(Some(b), elapsed);
                if limit > 0 {
                    assert!(
                        elapsed + limit + pk::STEP_GRACE + 15 <= b,
                        "{b} {elapsed}: {limit}"
                    );
                }
            }
        }
        // (That the command's own bounds fit its limit, and its backstop
        // the margin, is asserted at compile time beside the constants.)
        // An ordinary update (a few minutes) leaves the step its whole
        // limit; that is what the 45 minutes are for.
        assert_eq!(plan::camera_packages_limit(unit, 20 * 60), 1320);

        // The budget itself: what systemd says of the loaded unit.
        assert_eq!(plan::parse_timespan("45min"), Some(Some(2700)));
        assert_eq!(plan::parse_timespan("20min\n"), Some(Some(1200)));
        assert_eq!(plan::parse_timespan("1h 30min"), Some(Some(5400)));
        assert_eq!(plan::parse_timespan("1min 30s"), Some(Some(90)));
        assert_eq!(plan::parse_timespan("2min 30s 500ms"), Some(Some(150)));
        assert_eq!(plan::parse_timespan("90s"), Some(Some(90)));
        assert_eq!(plan::parse_timespan("infinity"), Some(None));
        for odd in [
            "",
            "soon",
            "45",
            "min",
            "45 min",
            "-5min",
            "1e3s",
            "45minutes",
        ] {
            assert_eq!(plan::parse_timespan(odd), None, "{odd:?}");
        }
        let floor = plan::UPDATER_UNIT_BUDGET_FLOOR_SECS;
        assert_eq!(floor, 1200);
        assert_eq!(
            plan::updater_unit_budget(&[Some("45min"), Some("45min")]),
            budget
        );
        assert_eq!(
            plan::updater_unit_budget(&[Some("20min"), Some("45min")]),
            1200
        );
        assert_eq!(
            plan::updater_unit_budget(&[Some("45min"), Some("10min")]),
            600
        );
        assert_eq!(
            plan::updater_unit_budget(&[Some("2h"), Some("infinity")]),
            budget
        );
        assert_eq!(plan::updater_unit_budget(&[None, Some("45min")]), floor);
        assert_eq!(
            plan::updater_unit_budget(&[Some("45min"), Some("soon")]),
            floor
        );
        assert_eq!(plan::updater_unit_budget(&[Some("5min"), None]), 300);
        assert_eq!(
            plan::updater_budget_argv().join(" "),
            "show -p TimeoutStartUSec --value xorgate-updater-rs.service"
        );
    }

    /// The backstop for a command that does not keep to its limit: asked
    /// to stop (SIGTERM, which the real command obeys at once unless an apt
    /// transaction runs), and only much later killed as a group.
    #[tokio::test]
    async fn a_step_that_overruns_is_asked_to_stop_then_killed() {
        async fn step(tag: &str, script: &str, last_resort: u64) -> (Vec<String>, Duration) {
            let r = rig(tag);
            let bin = r.p.live().join(plan::BIN_SRC);
            fs::create_dir_all(bin.parent().unwrap()).unwrap();
            sh(&bin, script);
            let mut log: Vec<String> = Vec::new();
            let t0 = std::time::Instant::now();
            camera_packages_step(
                &r.p,
                "/h",
                0,
                Duration::from_millis(300),
                Duration::from_secs(last_resort),
                std::future::pending(),
                &mut log,
            )
            .await;
            rm_rf(&r.dir);
            (log, t0.elapsed())
        }
        // It obeys SIGTERM: gone at once, one line.
        let (log, took) = step(
            "campkg-over1",
            "echo 'camera packages: x: downloading y'; exec sleep 30",
            20,
        )
        .await;
        assert_eq!(
            log,
            [
                "camera packages: x: downloading y",
                "camera packages: WARN the step did not keep to its time limit and was asked to stop (an apt transaction that is running is left to finish); the update itself is complete",
            ]
        );
        assert!(took < Duration::from_secs(10), "{took:?}");
        // It ignores SIGTERM (as during a transaction) and ends by itself:
        // it is waited for, not killed.
        let (log, took) = step(
            "campkg-over2",
            "trap '' TERM; sleep 2; echo 'camera packages: x: installed y'",
            20,
        )
        .await;
        assert_eq!(log.len(), 2, "{log:?}");
        assert_eq!(log[1], "camera packages: x: installed y");
        assert!(took >= Duration::from_secs(2), "{took:?}");
        // It never ends: the last resort, its process group.
        let (log, took) = step("campkg-over3", "trap '' TERM; exec sleep 30", 1).await;
        assert_eq!(
            log[1],
            "camera packages: WARN the step did not stop and was killed; check with `xorgate-rs camera requirements`"
        );
        assert!(took < Duration::from_secs(10), "{took:?}");
    }

    /// When the RUN is asked to stop (SIGTERM: systemd stopping the unit at
    /// its start timeout, a `systemctl stop`, a shutdown), the request is
    /// passed on to the command and the run WAITS for it. It must: with
    /// `KillMode=mixed` systemd SIGKILLs what is left of the unit as soon
    /// as the main process is gone (scripts/tests/unit-ordering.sh, G).
    #[tokio::test]
    async fn a_stop_of_the_run_is_passed_on_and_waited_out() {
        async fn step(tag: &str, script: &str) -> (Vec<String>, Duration) {
            let r = rig(tag);
            let bin = r.p.live().join(plan::BIN_SRC);
            fs::create_dir_all(bin.parent().unwrap()).unwrap();
            sh(&bin, script);
            let mut log: Vec<String> = Vec::new();
            let t0 = std::time::Instant::now();
            camera_packages_step(
                &r.p,
                "/h",
                100,
                Duration::from_secs(60),
                Duration::from_secs(60),
                tokio::time::sleep(Duration::from_millis(400)),
                &mut log,
            )
            .await;
            rm_rf(&r.dir);
            (log, t0.elapsed())
        }
        const PASSED: &str = "camera packages: this run was asked to stop: the step is asked to stop too, and is waited for (an apt transaction that is running finishes first)";
        // The command is in a step it may end: it stops at once.
        let (log, took) = step(
            "campkg-stop1",
            "trap 'echo \"camera packages: x: stopped on request\"; exit 1' TERM; echo 'camera packages: x: downloading y'; sleep 30 & wait",
        )
        .await;
        assert_eq!(
            log,
            [
                "camera packages: x: downloading y",
                PASSED,
                "camera packages: x: stopped on request",
            ]
        );
        assert!(took < Duration::from_secs(10), "{took:?}");
        // The command is inside its transaction: it notes the request and
        // finishes; the run waits for it and does not signal again.
        let (log, took) = step(
            "campkg-stop2",
            "trap 'echo \"camera packages: a stop was requested: waiting\"' TERM; echo 'camera packages: x: installing y'; sleep 3; echo 'camera packages: x: installed y'",
        )
        .await;
        assert_eq!(
            log,
            [
                "camera packages: x: installing y",
                PASSED,
                "camera packages: a stop was requested: waiting",
                "camera packages: x: installed y",
            ]
        );
        assert!(took >= Duration::from_secs(3), "{took:?}");
        // No stop request: nothing is said, nothing is signalled.
        let r = rig("campkg-stop3");
        let bin = r.p.live().join(plan::BIN_SRC);
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        sh(&bin, "echo 'camera packages: x: installed y'");
        let mut log: Vec<String> = Vec::new();
        camera_packages_step(
            &r.p,
            "/h",
            100,
            Duration::from_secs(60),
            Duration::from_secs(60),
            std::future::pending(),
            &mut log,
        )
        .await;
        assert_eq!(log, ["camera packages: x: installed y"]);
        rm_rf(&r.dir);
    }
}
