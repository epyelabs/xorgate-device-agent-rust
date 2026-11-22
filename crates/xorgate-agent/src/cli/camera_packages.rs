//! `xorgate-rs camera requirements [--install] [--from-claim <claim.json>]
//! [--no-restart] [--quiet] [--time-limit <seconds>]`: the extra OS packages
//! a declared camera sensor needs (support-other-camera-models Phase 4;
//! RUST-ONLY, no TS counterpart). Today that is one set: Arducam's libcamera
//! build, which the IMX335 needs.
//!
//! Without `--install` it only LOOKS and lists, for the sensors the device
//! declares (the held `cameraSensors` setting, or a claim response with
//! `--from-claim`), each package set with its required and installed
//! versions. Anyone may run that. It also lists what an earlier install
//! left of a set no declared sensor needs any more (still installed, still
//! held), with the lines to go back; that state asks for nothing (exit 0).
//!
//! With `--install` (ROOT only) it makes the device match, and is
//! idempotent: a set that is already installed and held costs no download
//! and no apt call. What it does, in order:
//!
//! 1. takes the state directory (`/var/lib/xorgate-packages`: a real
//!    directory owned by root that nobody else can write, created 0755 when
//!    absent, refused otherwise), locks it against a second run, and
//!    removes the `download-*` directories an earlier run left behind (a
//!    run that was killed, or cut by a power loss, cannot clean up);
//! 2. refuses when the catalog lists no packages for this OS release and
//!    architecture, or a pinned entry is not plain
//!    (`camera_packages::check_package`): one sentence, nothing changed;
//! 3. refuses, BEFORE any download, when dpkg was interrupted (a package
//!    left half-installed, or dpkg's journal of an unfinished run): apt
//!    would refuse anyway, and the remedy is `sudo dpkg --configure -a`;
//! 4. downloads each missing package over HTTPS from the catalog's URL
//!    (`xorgate_cloud::fetch`; the allowed hosts and redirect hosts are
//!    `camera_packages::{PACKAGE_HOSTS, REDIRECT_HOSTS}`) into a new
//!    directory only root can read, and accepts it only when its size and
//!    sha256 are the pinned ones. Nothing has been installed at this point:
//!    a failed download or a wrong checksum leaves the system as it was;
//! 5. asks `apt-get` to SIMULATE installing exactly those files and reads
//!    the plan: exactly the pinned packages in the pinned versions, nothing
//!    else installed, nothing removed, or the install is refused;
//! 6. THE TRANSACTION: installs exactly those files with `apt-get install`
//!    (the simulation's arguments without `-s`), then reads the whole dpkg
//!    database back: the pinned versions must be there, and nothing outside
//!    the pinned packages may have changed. What was replaced is recorded
//!    at once (`camera-packages.json`), also when apt failed half-way;
//! 7. sets `apt-mark hold` when the set says so;
//! 8. unless `--no-restart`: `systemctl try-restart` of the agent, so the
//!    camera is listed again at once instead of at the agent's next
//!    re-listing (up to 5 minutes). The closing line says "no reboot is
//!    needed" unless a camera line waits for one by the agent's own record
//!    of this boot ([`reboot_still_needed`]); then it names the port.
//!
//! TIME (`camera_packages::T_*`). Every step but one is KILLABLE: its own
//! bound cuts it, a stop request (SIGTERM, SIGINT, SIGHUP) ends it at once,
//! and its tool is killed as a process group so nothing is left running.
//! The one exception is the transaction: once the real `apt-get install`
//! has started, NOTHING in this program ends it but its own, generous bound
//! (`T_INSTALL`); a stop request is noted and the command waits. With
//! `--time-limit N` (what `xorgate-rs update` and the installer pass) the
//! killable steps are cut so that they end `T_INSTALL + T_AFTER` seconds
//! before the limit, and the transaction is started only then: a
//! transaction that could not finish inside the limit is never started
//! ("not enough time left in this run"). The transaction's `apt-get` runs
//! in its own process group, so a signal sent to this command's group
//! (`timeout`, a terminal's Ctrl-C) does not reach it.
//!
//! It never runs a script of the package's vendor, never adds an apt
//! source, and never installs anything the compiled-in catalog does not
//! pin. No catalog string reaches a shell: every tool is run with an
//! argument vector and a fixed environment.
//!
//! Who calls it: `scripts/provision-rs.sh` after `camera apply` (with the
//! claim, `--no-restart`), and `xorgate-rs update` AFTER the update is
//! complete (the held setting), both with `--quiet --time-limit`. A failure
//! there is a loud `WARN` line and never fails the provisioning or the
//! update.
//!
//! Exit codes: 0 = nothing is needed, or everything is in place; 1 =
//! something is missing (listing), or an install did not succeed; 2 =
//! usage, or an unreadable claim; 3 = refused for this device (no packages
//! for its OS release / architecture, an unusable catalog entry).

use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use xorgate_cloud::fetch::{fetch_client, fetch_pinned, FetchLimits, Pinned};
use xorgate_core::boot_config::{Board, BOOT_CONFIG_CANDIDATES, PROC_DEVICE_TREE};
use xorgate_core::camera_catalog::{catalog, Catalog, Package};
use xorgate_core::camera_packages::{
    self as pk, DpkgSnapshot, InstallRecord, Replaced, SetPlan, SetState, LINE_PREFIX,
};
use xorgate_core::camera_sensors::{
    apply_plan, os_codename, ports_waiting_for_reboot, wanted, BootLines, HostFacts, Wanted,
};
use xorgate_core::settings::clip;

pub const USAGE: &str = "usage: xorgate-rs camera requirements [--install] [--from-claim <claim.json>] [--no-restart] [--quiet] [--time-limit <seconds>]\n  Lists the extra OS packages the declared camera sensors need (the held cameraSensors setting,\n  or a claim response), with the required and the installed versions.\n  --install      (root) download the missing packages from the URLs the sensor catalog pins,\n                 check size and sha256, install exactly those files, hold them, restart the agent\n  --no-restart   do not restart the agent after an install\n  --quiet        print only what was changed or what failed\n  --time-limit   with --install: finish within this many seconds; the install itself is only\n                 started when it can finish inside them, and is never cut short\n  Never reboots.";

/// Where the record of what an install replaced lives (root-owned).
pub const STATE_DIR: &str = "/var/lib/xorgate-packages";
pub const RECORD_FILE: &str = "camera-packages.json";
/// Held for the length of an `--install` run.
pub const LOCK_FILE: &str = "lock";
/// How a download directory's name begins.
pub const DOWNLOAD_PREFIX: &str = "download-";

/// Where a pinned URL is fetched from instead (a test seam).
pub type UrlMap = Box<dyn Fn(&str) -> String + Send + Sync>;

/// Checks a redirect target.
pub type RedirectCheck = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// A request to stop (SIGTERM, SIGINT, SIGHUP in production): `true` once
/// it was made.
pub type Stop = tokio::sync::watch::Receiver<bool>;

/// Where the command looks and which tools it runs. Every field has ONE
/// production value ([`PkgEnv::from_process`]); a root run takes none of
/// them from its environment or its arguments. The tests build their own.
pub struct PkgEnv<'c> {
    pub catalog: &'c Catalog,
    pub is_root: bool,
    pub apt_get: PathBuf,
    pub apt_mark: PathBuf,
    pub dpkg_query: PathBuf,
    pub systemctl: PathBuf,
    /// The whole environment of the tools (nothing is inherited).
    pub tool_env: Vec<(String, String)>,
    pub os_release: PathBuf,
    /// The Debian architecture of this device.
    pub arch: Option<String>,
    /// dpkg's journal of an unfinished run (`/var/lib/dpkg/updates`).
    pub dpkg_updates: PathBuf,
    /// The locks a running package operation holds
    /// (`camera_host::DPKG_LOCKS`).
    pub dpkg_locks: Vec<PathBuf>,
    /// The record and the download directories.
    pub state_dir: PathBuf,
    /// The uid that must own the state directory (root).
    pub state_owner: u32,
    /// settings.json (the held `cameraSensors`).
    pub settings: PathBuf,
    /// The camera lines as the agent first saw them in this boot
    /// (`<home>/camera-boot-lines.json`, kept by the agent's
    /// `BootLineStore`): what its `reboot-required` verdict goes by. Only
    /// read here, never written.
    pub boot_lines: PathBuf,
    /// The kernel's boot id, as the agent reads it.
    pub boot_id: String,
    /// config.txt; `None` = this OS has none.
    pub boot_config: Option<PathBuf>,
    /// The board, for config.txt's `[section]` filters.
    pub board: Board,
    /// The agent's unit.
    pub service: String,
    pub limits: FetchLimits,
    /// The bound of the real `apt-get install` (`camera_packages::T_INSTALL`
    /// in every real run; the time limit's arithmetic always reserves that
    /// constant).
    pub install_bound: Duration,
    pub redirect_ok: RedirectCheck,
    /// TEST SEAM: where a pinned URL is fetched from instead. `None` in
    /// every real run: the catalog's URL is the one fetched.
    pub url_map: Option<UrlMap>,
    pub now_ms: i64,
    /// The stop request. [`never_stop`] where nothing can ask.
    pub stop: Stop,
}

/// A [`Stop`] nobody will ever make.
pub fn never_stop() -> Stop {
    let (tx, rx) = tokio::sync::watch::channel(false);
    // Kept alive for the life of the process: a dropped sender would read
    // as a closed channel, not as "no stop".
    std::mem::forget(tx);
    rx
}

impl PkgEnv<'static> {
    /// The real device.
    pub fn from_process() -> Self {
        let home = crate::env::runtime_home();
        Self {
            catalog: catalog(),
            is_root: nix::unistd::geteuid().is_root(),
            apt_get: "/usr/bin/apt-get".into(),
            apt_mark: "/usr/bin/apt-mark".into(),
            dpkg_query: "/usr/bin/dpkg-query".into(),
            systemctl: xorgate_core::update_plan::SYSTEMCTL_PATH.into(),
            tool_env: [
                ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
                ("DEBIAN_FRONTEND", "noninteractive"),
                ("LC_ALL", "C"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            os_release: "/etc/os-release".into(),
            arch: xorgate_hal::camera_host::package_arch(),
            dpkg_updates: xorgate_hal::camera_host::DPKG_UPDATES.into(),
            dpkg_locks: xorgate_hal::camera_host::DPKG_LOCKS
                .iter()
                .map(PathBuf::from)
                .collect(),
            state_dir: STATE_DIR.into(),
            state_owner: 0,
            settings: xorgate_hal::settings_io::settings_path(&home),
            boot_lines: home.join(crate::ops::camera_mount::BOOT_LINES_FILE),
            boot_id: xorgate_hal::boot_id::read_boot_id(),
            boot_config: xorgate_hal::boot_config_io::resolve_boot_config_path_fs(
                BOOT_CONFIG_CANDIDATES,
            )
            .map(PathBuf::from),
            board: xorgate_hal::boot_config_io::read_board(Path::new(PROC_DEVICE_TREE)),
            service: xorgate_core::update_plan::SERVICE.to_string(),
            limits: FetchLimits::default(),
            install_bound: Duration::from_secs(pk::T_INSTALL),
            redirect_ok: Box::new(pk::redirect_allowed),
            url_map: None,
            now_ms: crate::now_ms(),
            stop: never_stop(),
        }
    }
}

/// The command line after `camera requirements`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    pub install: bool,
    pub from_claim: Option<String>,
    pub no_restart: bool,
    pub quiet: bool,
    /// Seconds the whole run may take (`--install` only).
    pub time_limit: Option<u64>,
}

pub fn parse(rest: &[String]) -> Option<Args> {
    let mut a = Args::default();
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--install" => a.install = true,
            "--no-restart" => a.no_restart = true,
            "--quiet" => a.quiet = true,
            "--from-claim" => a.from_claim = Some(it.next()?.clone()),
            "--time-limit" => {
                let secs = it.next()?;
                // Plain digits only.
                if secs.is_empty() || !secs.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                a.time_limit = Some(secs.parse().ok()?);
            }
            _ => return None,
        }
    }
    Some(a)
}

/// `main()` → exit code. `rest` excludes `camera requirements`.
pub fn main(rest: &[String]) -> i32 {
    let Some(args) = parse(rest) else {
        super::out::err(USAGE);
        return 2;
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            super::out::err(&format!("xorgate-rs camera requirements: {e}"));
            return 1;
        }
    };
    rt.block_on(async {
        let mut env = PkgEnv::from_process();
        // A stop request is honoured between and during the killable steps,
        // and only noted during the transaction. (The listing keeps the
        // default dispositions: it has nothing to protect.)
        if args.install {
            env.stop = stop_on_signals();
        }
        // Printed as it happens: a download takes a while.
        // Through `out`: a line that cannot be written (the terminal hung
        // up, the reader of the pipe is gone) is dropped, never a panic.
        run(&args, &env, &mut |line| super::out::out(&line)).await
    })
}

/// SIGTERM, SIGINT and SIGHUP as a [`Stop`]. Once registered they stay
/// caught for the life of the process, which is what a running
/// transaction needs: a second Ctrl-C does not end it either.
fn stop_on_signals() -> Stop {
    use tokio::signal::unix::{signal, SignalKind};
    let (tx, rx) = tokio::sync::watch::channel(false);
    let streams = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    );
    tokio::spawn(async move {
        let (Ok(mut term), Ok(mut int), Ok(mut hup)) = streams else {
            // They could not be caught: the default dispositions stay, and
            // nobody will make a stop request.
            return std::future::pending::<()>().await;
        };
        loop {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
                _ = hup.recv() => {}
            }
            let _ = tx.send(true);
        }
    });
    rx
}

/// Wait for the stop request.
async fn stopped(stop: &Stop) {
    let mut rx = stop.clone();
    if rx.wait_for(|v| *v).await.is_err() {
        // The sender is gone: nobody can ask any more.
        std::future::pending::<()>().await;
    }
}

/// How much time the run has.
struct Clock {
    started: Instant,
    limit: Option<u64>,
}

impl Clock {
    /// Seconds left of the limit (`None` = no limit).
    fn left(&self) -> Option<u64> {
        self.limit
            .map(|l| l.saturating_sub(self.started.elapsed().as_secs()))
    }

    /// Seconds left before the transaction would have to start, so that it
    /// and what follows it still fit.
    fn left_before_transaction(&self) -> Option<u64> {
        self.left()
            .map(|l| l.saturating_sub(pk::T_INSTALL + pk::T_AFTER))
    }

    /// The bound of a killable step BEFORE a transaction; `None` = no time.
    fn before(&self, own: u64) -> Option<Duration> {
        pk::killable_secs(own, self.left_before_transaction()).map(Duration::from_secs)
    }

    /// The bound of a killable step that no transaction follows.
    fn plain(&self, own: u64) -> Option<Duration> {
        pk::killable_secs(own, self.left()).map(Duration::from_secs)
    }
}

/// What a tool run came to.
#[derive(Debug, Default)]
struct Ran {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    /// Non-zero exit, a spawn failure, its bound, or a stop.
    failed: bool,
    /// Ended by its bound.
    timed_out: bool,
    /// Ended by a stop request.
    stopped: bool,
}

/// May the tool be ended before it is done?
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    /// By its bound and by a stop request.
    Killable,
    /// By its bound only: a short step AFTER a transaction (the hold, the
    /// restart), whose time was reserved. A stop request that was waited
    /// out during the transaction does not undo what follows it.
    Bound,
    /// THE TRANSACTION: by its own bound only, as the last resort. A stop
    /// request is noted and waited out.
    Never,
}

/// Run a tool: its argument vector, the fixed environment, nothing on
/// stdin, its own process group (so the tool AND what it started are ended
/// together, and so a signal sent to this command's group does not reach a
/// running transaction).
async fn tool(
    env: &PkgEnv<'_>,
    program: &Path,
    args: &[String],
    bound: Duration,
    end: End,
    say: &mut dyn FnMut(String),
) -> Ran {
    let mut cmd = xorgate_hal::child::command(program);
    cmd.args(args)
        .env_clear()
        .envs(env.tool_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ran {
                stderr: e.to_string(),
                failed: true,
                ..Default::default()
            }
        }
    };
    let group = child.id().map(|id| nix::unistd::Pid::from_raw(id as i32));
    let kill_group = || {
        if let Some(g) = group {
            let _ = nix::sys::signal::killpg(g, nix::sys::signal::Signal::SIGKILL);
        }
    };
    let (mut out, mut err) = (child.stdout.take(), child.stderr.take());
    // Done when the TOOL has exited. Its output is read until then, and
    // for a moment after (what is still in the pipes). Not "until the pipes
    // close": something a package's script left running with the pipe open
    // must not make a finished install look like one that hangs.
    // Set the moment the tool's exit status is known. From then on neither
    // its bound nor a stop request applies: a tool that has finished is not
    // "timed out", and its process group (whose leader is reaped) is not
    // signalled.
    let exited = std::cell::Cell::new(false);
    let done = async {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let status = {
            let reads = async {
                let read_out = async {
                    if let Some(p) = out.as_mut() {
                        let _ = p.read_to_end(&mut o).await;
                    }
                };
                let read_err = async {
                    if let Some(p) = err.as_mut() {
                        let _ = p.read_to_end(&mut e).await;
                    }
                };
                tokio::join!(read_out, read_err);
            };
            tokio::pin!(reads);
            tokio::select! {
                // Both pipes closed: the tool is going or gone.
                _ = &mut reads => {
                    let status = child.wait().await;
                    exited.set(true);
                    status
                }
                status = child.wait() => {
                    exited.set(true);
                    let _ = tokio::time::timeout(Duration::from_secs(2), &mut reads).await;
                    status
                }
            }
        };
        (status, o, e)
    };
    tokio::pin!(done);
    let timer = tokio::time::sleep(bound);
    tokio::pin!(timer);
    let stop = stopped(&env.stop);
    tokio::pin!(stop);
    let (mut timed_out, mut was_stopped, mut noted, mut expired) = (false, false, false, false);
    let (status, o, e) = loop {
        tokio::select! {
            r = &mut done => break r,
            _ = &mut timer, if !timed_out && !expired => {
                expired = true;
                // A killable step: its bound. The transaction: the last
                // resort against an apt that hangs. Not a tool that has
                // already exited (its output is only being collected).
                if !exited.get() {
                    timed_out = true;
                    kill_group();
                }
            }
            _ = &mut stop, if !was_stopped && !noted => match end {
                _ if exited.get() => noted = true,
                End::Killable => {
                    was_stopped = true;
                    kill_group();
                }
                End::Bound => noted = true,
                End::Never => {
                    noted = true;
                    say("a stop was requested: waiting for apt-get to finish its transaction, which is never interrupted".to_string());
                }
            },
        }
    };
    let code = status.ok().and_then(|s| s.code());
    let stdout = String::from_utf8_lossy(&o).into_owned();
    let mut stderr = String::from_utf8_lossy(&e).into_owned();
    if timed_out {
        stderr = format!(
            "it did not finish within {} s and was stopped",
            bound.as_secs()
        );
    }
    Ran {
        stdout,
        stderr,
        exit_code: code,
        failed: code != Some(0) || timed_out || was_stopped,
        timed_out,
        stopped: was_stopped,
    }
}

/// The last non-empty line a failed tool printed (stderr, else stdout),
/// clipped: the reason, for a sentence.
fn last_line(r: &Ran) -> String {
    let pick = |s: &str| {
        s.lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| l.chars().take(300).collect::<String>())
    };
    pick(&r.stderr)
        .or_else(|| pick(&r.stdout))
        .unwrap_or_else(|| match r.exit_code {
            Some(c) => format!("exit {c}"),
            None => "killed".to_string(),
        })
}

/// Is `path` a regular file? (A FIFO or a device in its place would block
/// a read for ever; such a path is treated as unreadable.)
fn regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// `(stream key, sensor id)` of every declared port whose entry names a
/// sensor this build's catalog knows.
fn declared_sensors(args: &Args, env: &PkgEnv<'_>) -> Result<Vec<(String, String)>, String> {
    let entries: Vec<(String, serde_json::Value)> = match &args.from_claim {
        Some(path) => {
            if !regular_file(Path::new(path)) {
                return Err(format!(
                    "cannot read the claim response {path}: not a readable regular file"
                ));
            }
            let claim: serde_json::Value = std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
                .map_err(|e| format!("cannot read the claim response {path}: {e}"))?;
            apply_plan(&claim)
                .into_iter()
                .filter(|p| p.port.is_ok())
                .map(|p| (p.stream_key, p.entry))
                .collect()
        }
        None if !regular_file(&env.settings) => Vec::new(),
        None => xorgate_hal::settings_io::load_settings(&env.settings)
            .camera_sensors
            .and_then(|s| s.cameras)
            .map(|cams| cams.into_iter().collect())
            .unwrap_or_default(),
    };
    Ok(entries
        .into_iter()
        .filter_map(|(key, entry)| match wanted(&entry, env.catalog) {
            Ok(Wanted::Line { sensor, .. }) => Some((clip(&key), sensor.id.clone())),
            _ => None,
        })
        .collect())
}

/// Why a reboot is still needed after the packages are in place, as the
/// end of a sentence, or `None`.
///
/// THE SOURCE IS THE AGENT'S: the snapshot of the camera lines it took
/// when it first ran in this boot (`camera-boot-lines.json`), compared with
/// config.txt as it is now, by the same function the per-port report's
/// `reboot-required` goes by (`camera_sensors::changed_since_boot`). So the
/// closing line and the port's verdict cannot disagree. Every port the
/// snapshot has a word on counts (the camera ports of the device model),
/// managed or not, as in the report.
///
/// `None` also when nothing can be said: no snapshot of THIS boot (the
/// agent has not run in it; it will take its snapshot from config.txt as it
/// is, and report no pending change either), no config.txt. With
/// `--from-claim` (the installer, before the agent first starts) the
/// question is not asked: the installer has its own reboot notice.
///
/// The snapshot is only read. It is the agent's file; a root command that
/// took one would leave a root-owned file in the agent's home.
fn reboot_still_needed(args: &Args, env: &PkgEnv<'_>) -> Option<String> {
    if args.from_claim.is_some() {
        return None;
    }
    // A small regular file, or nothing.
    let meta = std::fs::metadata(&env.boot_lines).ok()?;
    if !meta.is_file() || meta.len() > 64 * 1024 {
        return None;
    }
    let booted: BootLines = xorgate_hal::read_lossy::read_text(&env.boot_lines)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())?;
    if booted.boot_id != env.boot_id {
        return None;
    }
    let config = env.boot_config.as_deref()?;
    if !regular_file(config) {
        return None;
    }
    let text = xorgate_hal::read_lossy::read_text(config).ok()?;
    let ports = ports_waiting_for_reboot(&booted, &text, env.board, env.catalog);
    // The ports by their fixed names (nothing of the file is printed).
    let names: Vec<&str> = ports.iter().map(|p| p.token()).collect();
    match names.as_slice() {
        [] => None,
        [one] => Some(format!(
            "the camera line of {one} in config.txt was changed since this device booted (sudo reboot)"
        )),
        many => Some(format!(
            "the camera lines of {} in config.txt were changed since this device booted (sudo reboot)",
            many.join(" and ")
        )),
    }
}

/// The whole dpkg database, within `bound`; `None` = it could not be read.
async fn snapshot(env: &PkgEnv<'_>, bound: Duration) -> Option<DpkgSnapshot> {
    let (program, tool_env) = (env.dpkg_query.clone(), env.tool_env.clone());
    tokio::task::spawn_blocking(move || {
        xorgate_hal::camera_host::dpkg_snapshot(&program, Some(&tool_env), bound, None)
    })
    .await
    .ok()
    .flatten()
}

/// The packages on hold; `None` = `apt-mark` could not be asked.
async fn held_packages(
    env: &PkgEnv<'_>,
    bound: Duration,
    say: &mut dyn FnMut(String),
) -> Option<Vec<String>> {
    let r = tool(
        env,
        &env.apt_mark,
        &["showhold".to_string()],
        bound,
        End::Killable,
        say,
    )
    .await;
    (!r.failed).then(|| pk::parse_showhold(&r.stdout))
}

fn record_path(env: &PkgEnv<'_>) -> PathBuf {
    env.state_dir.join(RECORD_FILE)
}

fn read_record(env: &PkgEnv<'_>) -> InstallRecord {
    if !regular_file(&record_path(env)) {
        return InstallRecord::default();
    }
    xorgate_hal::read_lossy::read_text(&record_path(env))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write the record, readable by everyone (the unprivileged listing prints
/// the way back from it).
fn write_record(env: &PkgEnv<'_>, record: &InstallRecord) -> std::io::Result<()> {
    let path = record_path(env);
    xorgate_hal::atomic_write::atomic_write_json(&path, record)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
}

/// The state directory, checked: a real directory (not a link to one),
/// owned by `state_owner`, that neither its group nor others can write.
/// Created 0755 when `create` and it is absent. `Ok(false)` = absent and
/// not created. `Err` = why it is refused.
fn state_dir_ok(env: &PkgEnv<'_>, create: bool) -> Result<bool, String> {
    let dir = &env.state_dir;
    let refused = |what: String| {
        format!(
            "{} is not a directory owned by root that only root can write ({what}); it is not used",
            dir.display()
        )
    };
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !create {
                return Ok(false);
            }
            std::fs::DirBuilder::new()
                .mode(0o755)
                .create(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            // Whatever the umask made of it.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("cannot set the mode of {}: {e}", dir.display()))?;
            std::fs::symlink_metadata(dir)
                .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        }
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    if meta.file_type().is_symlink() {
        return Err(refused("it is a symbolic link".into()));
    }
    if !meta.is_dir() {
        return Err(refused("it is not a directory".into()));
    }
    if meta.uid() != env.state_owner {
        return Err(refused(format!("its owner is uid {}", meta.uid())));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(refused(format!("its mode is {:o}", meta.mode() & 0o7777)));
    }
    Ok(true)
}

/// The run's lock on the state directory (released when dropped). `Err` =
/// another run holds it, or it could not be taken.
fn lock_state_dir(env: &PkgEnv<'_>) -> Result<std::fs::File, String> {
    let path = env.state_dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(
            "another `xorgate-rs camera requirements --install` is running on this device"
                .to_string(),
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

/// Remove the `download-*` directories of earlier runs (the lock is held:
/// none of them is in use). A run that was killed, or cut by a power loss,
/// could not remove its own. Returns how many went.
fn sweep_downloads(env: &PkgEnv<'_>) -> usize {
    let Ok(entries) = std::fs::read_dir(&env.state_dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with(DOWNLOAD_PREFIX)
                // A real directory only: a link is not followed.
                && e.file_type().is_ok_and(|t| t.is_dir())
        })
        .filter(|e| std::fs::remove_dir_all(e.path()).is_ok())
        .count()
}

/// Why a set is not (fully) as it should be.
enum Failure {
    /// The packages were not installed. The sentence says whether
    /// anything was changed.
    NotInstalled(String),
    /// The packages are in place (`installed`: by this run), but something
    /// is not as it should be (the hold, a package outside the pinned set).
    Installed { installed: bool, why: String },
    /// The time limit leaves no room: nothing was started.
    NoTime,
    /// A stop request ended a killable step: nothing was installed.
    Stopped,
}

/// A download directory that is removed when this is dropped, WHILE IT IS
/// ARMED. It is disarmed for the length of the apt transaction: whatever
/// ends this command then (a panic, an early return), the files apt-get
/// and dpkg are reading stay where they are. It is armed again only once
/// apt-get is known to have exited by itself. A directory that is left (a
/// run that was killed, an apt-get that had to be stopped at its bound and
/// whose dpkg may live on) is removed by a later run's sweep, which does
/// not run while a package operation holds the dpkg lock.
struct Scratch {
    dir: PathBuf,
    armed: bool,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// The command. Every line goes to `out` as it is produced.
pub async fn run(args: &Args, env: &PkgEnv<'_>, out: &mut dyn FnMut(String)) -> i32 {
    let clock = Clock {
        started: Instant::now(),
        limit: args.time_limit.filter(|_| args.install),
    };
    // With `--install` every line carries the prefix the callers look for.
    let prefix = if args.install { LINE_PREFIX } else { "" };
    let mut say = |line: String| out(format!("{prefix}{line}"));
    let declared = match declared_sensors(args, env) {
        Ok(d) => d,
        Err(why) => {
            say(why);
            return 2;
        }
    };
    if pk::package_names(&declared, env.catalog).is_empty() {
        if !args.quiet {
            say("no declared camera sensor needs extra packages".to_string());
        }
        // The listing also says what an earlier install left on the device
        // (a sensor that needed packages was here and is gone): it is still
        // installed and held, and nothing else would mention it. Nothing is
        // asked of the device unless the record names a set; `--install`
        // does nothing here, as before.
        if !args.install && !args.quiet {
            let record = read_record(env);
            if !record.sets.is_empty() {
                let held = held_packages(env, Duration::from_secs(pk::T_HELD), &mut say).await;
                let db = snapshot(env, Duration::from_secs(pk::T_DPKG)).await;
                let installed = db.as_ref().map(pk::installed_versions);
                for line in pk::leftover_listing(
                    &record,
                    &[],
                    env.catalog,
                    installed.as_ref(),
                    held.as_deref(),
                ) {
                    say(line);
                }
            }
        }
        return 0;
    }
    // Root is asked for before any tool is run.
    if args.install && !env.is_root {
        say(
            "installing packages needs root: sudo xorgate-rs camera requirements --install"
                .to_string(),
        );
        return 1;
    }
    // What the device has: the holds and the whole dpkg database.
    const NO_TIME: &str =
        "not enough time left in this run; run `sudo xorgate-rs camera requirements --install`";
    const BUSY: &str =
        "another package operation is running on this device; try again when it has finished";
    let (Some(held_bound), Some(db_bound)) = (clock.plain(pk::T_HELD), clock.plain(pk::T_DPKG))
    else {
        say(format!("WARN nothing was looked at: {NO_TIME}"));
        return 1;
    };
    let held = held_packages(env, held_bound, &mut say).await;
    let db = snapshot(env, db_bound).await;
    let host = HostFacts {
        os: xorgate_hal::read_lossy::read_text(&env.os_release)
            .ok()
            .as_deref()
            .and_then(os_codename),
        arch: env.arch.clone(),
        packages: db.as_ref().map(pk::installed_versions),
        interrupted: None,
    };
    let interrupted = db.as_ref().and_then(|db| {
        pk::interrupted(
            db,
            xorgate_hal::camera_host::dpkg_journal_pending(&env.dpkg_updates),
        )
    });
    // Is a package operation RUNNING (someone holds the dpkg lock)? Asked
    // after the database was read: while one runs, the database shows the
    // very signs of an interrupted dpkg, and they pass by themselves.
    // (Only root can ask; an unprivileged listing cannot tell.)
    let busy = xorgate_hal::camera_host::dpkg_lock_held(&env.dpkg_locks) == Some(true);
    let plans = pk::plan(&declared, env.catalog, &host);
    let mut record = read_record(env);

    if !args.install {
        let mut code = 0;
        for plan in &plans {
            for line in pk::listing(plan, held.as_deref(), record.sets.get(&plan.id)) {
                say(line);
            }
            code = code.max(match &plan.state {
                SetState::NoTarget { .. } | SetState::Invalid(_) => 3,
                SetState::Unknown(_) => 1,
                _ => i32::from(pk::needs_work(plan, held.as_deref())),
            });
        }
        // What an earlier install left of a set no declared sensor needs
        // any more (the exit code is not its business).
        let needed: Vec<&str> = plans.iter().map(|p| p.id.as_str()).collect();
        for line in pk::leftover_listing(
            &record,
            &needed,
            env.catalog,
            host.packages.as_ref(),
            held.as_deref(),
        ) {
            say(line);
        }
        if interrupted.is_some() && busy {
            say(BUSY.to_string());
            code = code.max(1);
        } else if let Some(how) = &interrupted {
            say(format!(
                "dpkg was interrupted on this device ({how}): nothing can be installed until `{}` has run",
                pk::DPKG_REMEDY
            ));
            code = code.max(1);
        } else if code == 1 {
            // What is true: a package to install, or only its hold to set.
            let packages = plans
                .iter()
                .any(|p| !matches!(p.state, SetState::Satisfied));
            say(if packages {
                "to install what is missing: sudo xorgate-rs camera requirements --install (or sudo xorgate-rs update)"
            } else {
                "to set the hold again: sudo xorgate-rs camera requirements --install (or sudo xorgate-rs update)"
            }
            .to_string());
        }
        return code;
    }

    // The state directory: checked, locked against a second run, and
    // cleared of what an earlier run left. Created only when there is work.
    let work = plans.iter().any(|p| pk::needs_work(p, held.as_deref()));
    let _lock = match state_dir_ok(env, work) {
        Ok(false) => None,
        Ok(true) => match lock_state_dir(env) {
            Ok(lock) => {
                // Not while a package operation runs: the dpkg of an
                // apt-get this command once had to stop at its bound may
                // still be reading one of those directories.
                let swept = if busy { 0 } else { sweep_downloads(env) };
                if swept > 0 {
                    say(format!(
                        "removed {swept} download director{} an earlier run left in {}",
                        if swept == 1 { "y" } else { "ies" },
                        env.state_dir.display()
                    ));
                }
                Some(lock)
            }
            Err(why) if work => {
                say(format!(
                    "WARN nothing was installed: {why}; nothing was changed"
                ));
                return 1;
            }
            Err(_) => None,
        },
        Err(why) if work => {
            say(format!(
                "WARN nothing was installed: {why}; nothing was changed"
            ));
            return 1;
        }
        Err(_) => None,
    };

    let mut code = 0;
    let mut installed_any = false;
    for plan in &plans {
        let who = plan.needed_by.join(", ");
        match &plan.state {
            SetState::NoTarget { os, arch } => {
                say(format!(
                    "WARN {} REFUSED: the catalog lists no packages of this set for {os}/{arch}, so nothing can be installed on this device; {who} will not work here",
                    plan.id
                ));
                code = code.max(3);
            }
            SetState::Invalid(why) => {
                say(format!(
                    "WARN {} REFUSED: the catalog entry is not usable ({why}); nothing was changed",
                    plan.id
                ));
                code = code.max(3);
            }
            SetState::Unknown(why) => {
                say(format!(
                    "WARN {} NOT installed: {why}; nothing was changed",
                    plan.id
                ));
                code = code.max(1);
            }
            SetState::Satisfied | SetState::Missing => {
                if !pk::needs_work(plan, held.as_deref()) {
                    if !args.quiet {
                        say(format!(
                            "{}: already installed{} (needed by {who})",
                            plan.id,
                            match (plan.apt_hold, &held) {
                                (false, _) => "",
                                (true, Some(_)) => " and held",
                                (true, None) => "; the apt hold could not be read",
                            }
                        ));
                    }
                    continue;
                }
                // An interrupted dpkg: apt would refuse after the download.
                // Said first, with the remedy, and nothing is fetched.
                if let (SetState::Missing, Some(_), true) = (&plan.state, &interrupted, busy) {
                    // The signs of an interrupted dpkg while someone holds
                    // the dpkg lock: a package operation is simply running.
                    say(format!(
                        "WARN {} NOT installed: {BUSY} (nothing was downloaded or changed)",
                        plan.id
                    ));
                    code = code.max(1);
                    continue;
                }
                if let (SetState::Missing, Some(how)) = (&plan.state, &interrupted) {
                    say(format!(
                        "WARN {} NOT installed: dpkg was interrupted on this device ({how}). Run `{}` there, then `sudo xorgate-rs camera requirements --install` again; nothing was downloaded or changed",
                        plan.id,
                        pk::DPKG_REMEDY
                    ));
                    code = code.max(1);
                    continue;
                }
                let before = db.clone().unwrap_or_default();
                let job = Job {
                    env,
                    plan,
                    clock: &clock,
                    before: &before,
                    held: held.as_deref(),
                };
                match job.install_set(&mut record, &mut say).await {
                    Ok(installed) => installed_any |= installed,
                    Err(Failure::NotInstalled(why)) => {
                        say(format!(
                            "WARN {} NOT installed: {why}. {who} will not appear until this succeeds: run `sudo xorgate-rs camera requirements --install` again",
                            plan.id
                        ));
                        code = code.max(1);
                    }
                    Err(Failure::NoTime) => {
                        say(format!(
                            "WARN {} NOT installed: {NO_TIME} (the install is only started when it can finish; nothing was changed)",
                            plan.id
                        ));
                        code = code.max(1);
                    }
                    Err(Failure::Stopped) => {
                        say(format!(
                            "WARN {} NOT installed: stopped on request before the install began; nothing was changed",
                            plan.id
                        ));
                        // No further set is started; a set installed
                        // earlier in this run still gets its restart below.
                        code = code.max(1);
                        break;
                    }
                    // The camera works; what is missing is said.
                    Err(Failure::Installed { installed, why }) => {
                        installed_any |= installed;
                        say(format!("WARN {}: {why}", plan.id));
                        code = code.max(1);
                    }
                }
            }
        }
    }
    if installed_any {
        // A camera line that waits for a reboot (the agent's own record of
        // this boot): the packages need no reboot, that line does, and the
        // closing line must not say otherwise.
        let reboot = reboot_still_needed(args, env);
        if args.no_restart {
            say(match &reboot {
                None => "the camera appears when the agent starts next (or at its next camera re-listing, within 5 minutes); no reboot is needed".to_string(),
                Some(why) => format!("the packages are in place (the agent uses them when it starts next, or at its next camera re-listing, within 5 minutes), but a reboot is still needed: {why}"),
            });
        } else {
            let r = tool(
                env,
                &env.systemctl,
                &["try-restart".to_string(), env.service.clone()],
                Duration::from_secs(pk::T_RESTART),
                End::Bound,
                &mut say,
            )
            .await;
            // Read again after the restart: the record is the agent's.
            let reboot = reboot.or_else(|| reboot_still_needed(args, env));
            if r.failed {
                say(format!(
                    "WARN the agent could not be restarted ({}); {}",
                    last_line(&r),
                    match &reboot {
                        None => "the camera appears at its next camera re-listing, within 5 minutes".to_string(),
                        Some(why) => format!("it uses the packages at its next camera re-listing, within 5 minutes, and a reboot is still needed: {why}"),
                    }
                ));
            } else {
                say(match &reboot {
                    None => "the agent was restarted so the camera is listed again; no reboot is needed".to_string(),
                    Some(why) => format!("the packages are in place and the agent was restarted, but a reboot is still needed: {why}"),
                });
            }
        }
    }
    code
}

/// One set's install.
struct Job<'a, 'c> {
    env: &'a PkgEnv<'c>,
    plan: &'a SetPlan,
    clock: &'a Clock,
    /// The dpkg database at the start of the run.
    before: &'a DpkgSnapshot,
    held: Option<&'a [String]>,
}

impl Job<'_, '_> {
    /// Make the set match: `Ok(true)` = packages were installed, `Ok(false)`
    /// = only the hold was missing, `Err` = why not.
    async fn install_set(
        &self,
        record: &mut InstallRecord,
        say: &mut dyn FnMut(String),
    ) -> Result<bool, Failure> {
        let (env, plan) = (self.env, self.plan);
        let (installed, mut trouble) = self.install_packages(record, say).await?;
        // The hold: without it a later apt upgrade replaces the packages.
        if plan.apt_hold {
            let names = plan.names();
            let to_hold: Vec<String> = match self.held {
                Some(held) if !installed => names
                    .iter()
                    .filter(|n| !held.contains(n))
                    .cloned()
                    .collect(),
                // Just installed (an install does not keep a hold in every
                // apt version), or the holds could not be read: set them all.
                _ => names.clone(),
            };
            if !to_hold.is_empty() {
                // After a transaction its time is reserved (`T_AFTER`).
                let bound = if installed {
                    Some(Duration::from_secs(pk::T_HOLD))
                } else {
                    self.clock.plain(pk::T_HOLD)
                };
                let Some(bound) = bound else {
                    return Err(Failure::NoTime);
                };
                let r = tool(
                    env,
                    &env.apt_mark,
                    &pk::apt_mark_args("hold", &to_hold),
                    bound,
                    if installed { End::Bound } else { End::Killable },
                    say,
                )
                .await;
                if r.failed {
                    return Err(Failure::Installed {
                        installed,
                        why: format!(
                            "the packages are installed, but `apt-mark hold {}` failed ({}): an apt upgrade may replace them. Run `sudo xorgate-rs camera requirements --install` again to set the hold",
                            to_hold.join(" "),
                            last_line(&r)
                        ),
                    });
                }
                say(format!(
                    "{}: apt hold set on {}",
                    plan.id,
                    to_hold.join(", ")
                ));
                // Recorded: the holds this command set (one that was there
                // before is not its to release). Unknown = none claimed.
                let newly: Vec<String> = match self.held {
                    Some(held) => to_hold
                        .iter()
                        .filter(|n| !held.contains(n))
                        .cloned()
                        .collect(),
                    None => Vec::new(),
                };
                if !newly.is_empty() {
                    self.keep_record(record, &[], &newly, say);
                }
            }
        }
        match trouble.take() {
            Some(why) => Err(Failure::Installed { installed, why }),
            None => Ok(installed),
        }
    }

    /// Merge what this run did into the record and write it.
    fn keep_record(
        &self,
        record: &mut InstallRecord,
        replaced: &[Replaced],
        held: &[String],
        say: &mut dyn FnMut(String),
    ) {
        let (env, id) = (self.env, &self.plan.id);
        let rec = pk::merge_record(record.sets.get(id), replaced, held, env.now_ms);
        record.v = 1;
        record.sets.insert(id.clone(), rec);
        if let Err(e) = write_record(env, record) {
            say(format!(
                "WARN {id}: could not record what was replaced in {} ({e})",
                record_path(env).display()
            ));
        }
    }

    /// Download, verify and install the packages that are not at their
    /// pinned version: `Ok((true, _))` = installed, `Ok((false, _))` =
    /// nothing was missing; the second part is something that is not as it
    /// should be although the packages are in. `Err` = why not (the
    /// sentence says whether anything changed).
    async fn install_packages(
        &self,
        record: &mut InstallRecord,
        say: &mut dyn FnMut(String),
    ) -> Result<(bool, Option<String>), Failure> {
        let (env, plan, clock) = (self.env, self.plan, self.clock);
        let id = &plan.id;
        let not = |why: String| Failure::NotInstalled(why);
        let was = pk::installed_versions(self.before);
        let missing: Vec<&Package> = plan
            .packages
            .iter()
            .filter(|p| was.get(&p.name) != Some(&p.version))
            .collect();
        if missing.is_empty() {
            return Ok((false, None));
        }
        // Nothing is started that the time limit would cut before the
        // transaction could begin.
        if clock.before(1).is_none() {
            return Err(Failure::NoTime);
        }
        // 1. A directory only root can read (the state directory was
        //    checked and is locked).
        let dir = nix::unistd::mkdtemp(&env.state_dir.join(format!("{DOWNLOAD_PREFIX}XXXXXX")))
            .map_err(|e| {
                not(format!(
                    "cannot create a download directory in {}: {e}; nothing was changed",
                    env.state_dir.display()
                ))
            })?;
        let mut scratch = Scratch { dir, armed: true };
        // 2. Download and verify EVERY file before anything is installed.
        let mut files: Vec<String> = Vec::new();
        for p in &missing {
            let file = pk::check_package(p)
                .map_err(|e| not(format!("{}: {e}; nothing was changed", p.name)))?;
            let dest = scratch.dir.join(&file);
            let url = match &env.url_map {
                Some(map) => map(&p.url),
                None => p.url.clone(),
            };
            let Some(total) = clock.before(pk::T_DOWNLOAD) else {
                return Err(Failure::NoTime);
            };
            let limits = FetchLimits {
                total: total.min(env.limits.total),
                ..env.limits
            };
            let client = fetch_client(&limits).map_err(|e| {
                not(format!(
                    "cannot set up the HTTPS client: {e}; nothing was changed"
                ))
            })?;
            say(format!(
                "{id}: downloading {} {} ({} bytes)",
                p.name, p.version, p.bytes
            ));
            let fetched = tokio::select! {
                r = fetch_pinned(
                    &client,
                    Pinned {
                        url: &url,
                        bytes: p.bytes,
                        sha256: &p.sha256,
                    },
                    &dest,
                    &*env.redirect_ok,
                    pk::MAX_REDIRECTS,
                    &limits,
                ) => r,
                _ = stopped(&env.stop) => return Err(Failure::Stopped),
            };
            fetched
                .map_err(|e| not(format!("{file} was not accepted: {e}; nothing was changed")))?;
            say(format!(
                "{id}: verified {file} ({} bytes, sha256 {}…)",
                p.bytes,
                &p.sha256[..12]
            ));
            files.push(dest.to_string_lossy().into_owned());
        }
        // 3. What would apt do? Exactly the pinned packages, or nothing.
        let Some(bound) = clock.before(pk::T_SIMULATE) else {
            return Err(Failure::NoTime);
        };
        let sim = tool(
            env,
            &env.apt_get,
            &pk::apt_install_args(&files, true),
            bound,
            End::Killable,
            say,
        )
        .await;
        if sim.stopped {
            return Err(Failure::Stopped);
        }
        if sim.failed {
            return Err(not(format!(
                "apt-get could not plan the install ({}); nothing was changed",
                last_line(&sim)
            )));
        }
        let pins: Vec<Package> = missing.iter().map(|p| (*p).clone()).collect();
        pk::check_simulation(&sim.stdout, &pins)
            .map_err(|e| not(format!("{e}; nothing was changed")))?;
        // The database right before the transaction, to see afterwards
        // that nothing outside the pinned packages changed. (The one read
        // at the start stands in when this read fails.)
        let Some(bound) = clock.before(pk::T_DPKG) else {
            return Err(Failure::NoTime);
        };
        let before = snapshot(env, bound)
            .await
            .unwrap_or_else(|| self.before.clone());
        // 4. THE TRANSACTION. Started only when it and what follows it fit
        //    in the time limit; from here nothing ends it but its own bound.
        if clock.before(1).is_none() {
            return Err(Failure::NoTime);
        }
        if *env.stop.borrow() {
            return Err(Failure::Stopped);
        }
        // From here until apt-get has exited by itself, NOTHING removes the
        // files it reads: not an early return, not a panic's unwind.
        scratch.armed = false;
        say(format!(
            "{id}: installing {} with apt-get (never interrupted once begun)",
            missing
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let res = tool(
            env,
            &env.apt_get,
            &pk::apt_install_args(&files, false),
            env.install_bound,
            End::Never,
            say,
        )
        .await;
        // apt-get has exited. By itself: dpkg is done with the files, and
        // they go. Stopped at its bound: dpkg, which apt runs in a session
        // of its own, may live on and still read them; they are left for a
        // later run's sweep (which waits for the dpkg lock to be free).
        scratch.armed = !res.timed_out;
        drop(scratch);
        // What is there now. Its time was reserved; a failure to read is
        // "could not confirm", never "not installed".
        let names = plan.names();
        let after = snapshot(env, Duration::from_secs(pk::T_DPKG)).await;
        let was_before = pk::installed_versions(&before);
        let show = |v: Option<&String>| v.map_or("not installed", String::as_str).to_string();
        let Some(after) = after else {
            if res.failed {
                return Err(not(format!(
                    "apt-get install failed ({}), and the packages could not be read back with dpkg-query: check with `dpkg --audit`",
                    last_line(&res)
                )));
            }
            // apt said it installed them: recorded and held on its word.
            let replaced: Vec<Replaced> = missing
                .iter()
                .map(|p| {
                    (
                        p.name.clone(),
                        p.version.clone(),
                        was_before.get(&p.name).cloned(),
                    )
                })
                .collect();
            self.keep_record(record, &replaced, &[], say);
            say(format!(
                "{id}: installed {} (apt-get reported success; dpkg-query could not confirm it within {} s)",
                missing
                    .iter()
                    .map(|p| format!("{} {} (was {})", p.name, p.version, show(was_before.get(&p.name))))
                    .collect::<Vec<_>>()
                    .join(", "),
                pk::T_DPKG
            ));
            return Ok((true, None));
        };
        let now = pk::installed_versions(&after);
        // Recorded AT ONCE, whatever else happened: every pinned package
        // that is now at its pinned version and was not before.
        let replaced: Vec<Replaced> = missing
            .iter()
            .filter(|p| now.get(&p.name) == Some(&p.version))
            .map(|p| {
                (
                    p.name.clone(),
                    p.version.clone(),
                    was_before.get(&p.name).cloned(),
                )
            })
            .collect();
        if !replaced.is_empty() {
            self.keep_record(record, &replaced, &[], say);
        }
        let wrong: Vec<&Package> = plan
            .packages
            .iter()
            .filter(|p| now.get(&p.name) != Some(&p.version))
            .collect();
        if res.failed || !wrong.is_empty() {
            let why = if res.timed_out {
                format!(
                    "apt-get install did not finish within {} s and was stopped (the one case in which a running install is ended); its files are left in {} until a later run removes them, in case dpkg is still at work",
                    env.install_bound.as_secs(),
                    env.state_dir.display()
                )
            } else if res.failed {
                format!("apt-get install failed ({})", last_line(&res))
            } else {
                "apt-get reported success, but the packages are not at the pinned versions"
                    .to_string()
            };
            let untouched = plan
                .packages
                .iter()
                .all(|p| after.get(&p.name) == before.get(&p.name));
            return Err(not(if untouched {
                format!("{why}; nothing was changed")
            } else {
                format!(
                    "{why}; the packages are now: {} (check with `dpkg --audit`; if dpkg was interrupted, `{}`)",
                    plan.packages
                        .iter()
                        .map(|p| format!("{} {}", p.name, show(now.get(&p.name))))
                        .collect::<Vec<_>>()
                        .join(", "),
                    pk::DPKG_REMEDY
                )
            }));
        }
        say(format!(
            "{id}: installed {}",
            missing
                .iter()
                .map(|p| format!(
                    "{} {} (was {})",
                    p.name,
                    p.version,
                    show(was_before.get(&p.name))
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        // Nothing outside the pinned packages may have changed.
        let outside = pk::changed_outside(&before, &after, &names);
        let trouble = (!outside.is_empty()).then(|| {
            format!(
                "the packages are installed, but packages outside the pinned set changed during the install: {}",
                outside.join(", ")
            )
        });
        Ok((true, trouble))
    }
}
