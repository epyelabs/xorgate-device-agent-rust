//! `xorgate-rs camera requirements [--install] [--from-claim <claim.json>]
//! [--no-restart] [--quiet]`: the extra OS packages a declared camera sensor
//! needs (support-other-camera-models Phase 4; RUST-ONLY, no TS
//! counterpart). Today that is one set: Arducam's libcamera build, which
//! the IMX335 needs.
//!
//! Without `--install` it only LOOKS and lists, for the sensors the device
//! declares (the held `cameraSensors` setting, or a claim response with
//! `--from-claim`), each package set with its required and installed
//! versions. Anyone may run that.
//!
//! With `--install` (ROOT only) it makes the device match, and is
//! idempotent: a set that is already installed and held costs no download
//! and no apt call. What it does, in order, per set that has work:
//!
//! 1. refuses when the catalog lists no packages for this OS release and
//!    architecture, or a pinned entry is not plain
//!    (`camera_packages::check_package`): one sentence, nothing changed;
//! 2. downloads each missing package over HTTPS from the catalog's URL
//!    (`xorgate_cloud::fetch`; the allowed hosts and redirect hosts are
//!    `camera_packages::{PACKAGE_HOSTS, REDIRECT_HOSTS}`) into a new
//!    directory only root can read, and accepts it only when its size and
//!    sha256 are the pinned ones. Nothing has been installed at this point:
//!    a failed download or a wrong checksum leaves the system as it was
//!    (the directory is removed on every path);
//! 3. asks `apt-get` to SIMULATE installing exactly those files and reads
//!    the plan: only the pinned packages in the pinned versions, nothing
//!    else installed, nothing removed, or the install is refused;
//! 4. installs exactly those files with `apt-get install` in one
//!    transaction, checks with `dpkg-query` that the versions are now the
//!    pinned ones, records what was replaced
//!    (`/var/lib/xorgate-packages/camera-packages.json`), and sets
//!    `apt-mark hold` when the set says so;
//! 5. unless `--no-restart`: `systemctl try-restart` of the agent, so the
//!    camera is listed again at once instead of at the agent's next
//!    re-listing (up to 5 minutes).
//!
//! It never runs a script of the package's vendor, never adds an apt
//! source, and never installs anything the compiled-in catalog does not
//! pin. No catalog string reaches a shell: every tool is run with an
//! argument vector and a fixed environment.
//!
//! Who calls it: `scripts/provision-rs.sh` after `camera apply` (with the
//! claim), and `xorgate-rs update` after the new build is in place (the
//! held setting), both with `--no-restart --quiet`. A failure there is a
//! loud `WARN` line and never fails the provisioning or the update.
//!
//! Exit codes: 0 = nothing is needed, or everything is in place; 1 =
//! something is missing (listing), or an install did not succeed; 2 =
//! usage, or an unreadable claim; 3 = refused for this device (no packages
//! for its OS release / architecture, an unusable catalog entry).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use xorgate_cloud::fetch::{fetch_client, fetch_pinned, FetchLimits, Pinned};
use xorgate_core::camera_catalog::{catalog, Catalog, Package};
use xorgate_core::camera_packages::{self as pk, InstallRecord, SetPlan, SetState, LINE_PREFIX};
use xorgate_core::camera_sensors::{apply_plan, os_codename, wanted, HostFacts, Wanted};
use xorgate_core::settings::clip;
use xorgate_hal::exec::RunResult;

pub const USAGE: &str = "usage: xorgate-rs camera requirements [--install] [--from-claim <claim.json>] [--no-restart] [--quiet]\n  Lists the extra OS packages the declared camera sensors need (the held cameraSensors setting,\n  or a claim response), with the required and the installed versions.\n  --install      (root) download the missing packages from the URLs the sensor catalog pins,\n                 check size and sha256, install exactly those files, hold them, restart the agent\n  --no-restart   do not restart the agent after an install\n  --quiet        print only what was changed or what failed\n  Never reboots.";

/// Where the record of what an install replaced lives (root-owned).
pub const STATE_DIR: &str = "/var/lib/xorgate-packages";
pub const RECORD_FILE: &str = "camera-packages.json";

const APT_PLAN_TIMEOUT: Duration = Duration::from_secs(180);
const APT_INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const APT_MARK_TIMEOUT: Duration = Duration::from_secs(60);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(90);

/// Where a pinned URL is fetched from instead (a test seam).
pub type UrlMap = Box<dyn Fn(&str) -> String + Send + Sync>;

/// Checks a redirect target.
pub type RedirectCheck = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

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
    /// The record and the download directories.
    pub state_dir: PathBuf,
    /// settings.json (the held `cameraSensors`).
    pub settings: PathBuf,
    /// The agent's unit.
    pub service: String,
    pub limits: FetchLimits,
    pub redirect_ok: RedirectCheck,
    /// TEST SEAM: where a pinned URL is fetched from instead. `None` in
    /// every real run: the catalog's URL is the one fetched.
    pub url_map: Option<UrlMap>,
    pub now_ms: i64,
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
            state_dir: STATE_DIR.into(),
            settings: xorgate_hal::settings_io::settings_path(&home),
            service: xorgate_core::update_plan::SERVICE.to_string(),
            limits: FetchLimits::default(),
            redirect_ok: Box::new(pk::redirect_allowed),
            url_map: None,
            now_ms: crate::now_ms(),
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
            _ => return None,
        }
    }
    Some(a)
}

/// `main()` → exit code. `rest` excludes `camera requirements`.
pub fn main(rest: &[String]) -> i32 {
    let Some(args) = parse(rest) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("xorgate-rs camera requirements: {e}");
            return 1;
        }
    };
    let env = PkgEnv::from_process();
    // Printed as it happens: a download takes a while.
    rt.block_on(run(&args, &env, &mut |line| println!("{line}")))
}

/// Run a tool to completion: its argument vector, the fixed environment,
/// nothing on stdin, a bound on its time.
async fn tool(env: &PkgEnv<'_>, program: &Path, args: &[String], timeout: Duration) -> RunResult {
    let mut cmd = xorgate_hal::child::command(program);
    cmd.args(args)
        .env_clear()
        .envs(env.tool_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => RunResult {
            stderr: format!("timed out after {} s", timeout.as_secs()),
            failed: true,
            ..Default::default()
        },
        Ok(Err(e)) => RunResult {
            stderr: e.to_string(),
            failed: true,
            not_found: e.kind() == std::io::ErrorKind::NotFound,
            spawn_failed: true,
            ..Default::default()
        },
        Ok(Ok(out)) => RunResult {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            exit_code: out.status.code(),
            failed: !out.status.success(),
            ..Default::default()
        },
    }
}

/// The last non-empty line a failed tool printed (stderr, else stdout),
/// clipped: the reason, for a sentence.
fn last_line(r: &RunResult) -> String {
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

/// `(stream key, sensor id)` of every declared port whose entry names a
/// sensor this build's catalog knows.
fn declared_sensors(args: &Args, env: &PkgEnv<'_>) -> Result<Vec<(String, String)>, String> {
    let entries: Vec<(String, serde_json::Value)> = match &args.from_claim {
        Some(path) => {
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

fn host_facts(env: &PkgEnv<'_>, names: &[String]) -> HostFacts {
    HostFacts {
        os: xorgate_hal::read_lossy::read_text(&env.os_release)
            .ok()
            .as_deref()
            .and_then(os_codename),
        arch: env.arch.clone(),
        packages: xorgate_hal::camera_host::query_packages_with(
            &env.dpkg_query,
            Some(&env.tool_env),
            names,
        ),
    }
}

/// The packages on hold; `None` = `apt-mark` could not be asked.
async fn held_packages(env: &PkgEnv<'_>) -> Option<Vec<String>> {
    let r = tool(
        env,
        &env.apt_mark,
        &["showhold".to_string()],
        APT_MARK_TIMEOUT,
    )
    .await;
    (!r.failed).then(|| pk::parse_showhold(&r.stdout))
}

fn record_path(env: &PkgEnv<'_>) -> PathBuf {
    env.state_dir.join(RECORD_FILE)
}

fn read_record(env: &PkgEnv<'_>) -> InstallRecord {
    xorgate_hal::read_lossy::read_text(&record_path(env))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Why a set is not (fully) as it should be.
enum Failure {
    /// The packages were not installed. The sentence says whether
    /// anything was changed.
    NotInstalled(String),
    /// The packages are in place (`installed`: by this run), but the hold
    /// could not be set.
    HoldFailed { installed: bool, why: String },
}

/// A download directory that is gone when this is dropped, whatever
/// happened in between.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The command. Every line goes to `out` as it is produced.
pub async fn run(args: &Args, env: &PkgEnv<'_>, out: &mut dyn FnMut(String)) -> i32 {
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
    let names = pk::package_names(&declared, env.catalog);
    if names.is_empty() {
        if !args.quiet {
            say("no declared camera sensor needs extra packages".to_string());
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
    let host = host_facts(env, &names);
    let plans = pk::plan(&declared, env.catalog, &host);
    let held = held_packages(env).await;
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
        if code == 1 {
            say("to install what is missing: sudo xorgate-rs camera requirements --install (or sudo xorgate-rs update)".to_string());
        }
        return code;
    }

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
                match install_set(env, plan, &host, held.as_deref(), &mut record, &mut say).await {
                    Ok(installed) => installed_any |= installed,
                    Err(Failure::NotInstalled(why)) => {
                        say(format!(
                            "WARN {} NOT installed: {why}. {who} will not appear until this succeeds: run `sudo xorgate-rs camera requirements --install` again",
                            plan.id
                        ));
                        code = code.max(1);
                    }
                    // The camera works; what is missing is the protection
                    // against a later apt upgrade.
                    Err(Failure::HoldFailed { installed, why }) => {
                        installed_any |= installed;
                        say(format!(
                            "WARN {}: {why}. Run `sudo xorgate-rs camera requirements --install` again to set the hold",
                            plan.id
                        ));
                        code = code.max(1);
                    }
                }
            }
        }
    }
    if installed_any {
        if args.no_restart {
            say("the camera appears when the agent starts next (or at its next camera re-listing, within 5 minutes); no reboot is needed".to_string());
        } else {
            let r = tool(
                env,
                &env.systemctl,
                &["try-restart".to_string(), env.service.clone()],
                SYSTEMCTL_TIMEOUT,
            )
            .await;
            if r.failed {
                say(format!(
                    "WARN the agent could not be restarted ({}); the camera appears at its next camera re-listing, within 5 minutes",
                    last_line(&r)
                ));
            } else {
                say(
                    "the agent was restarted so the camera is listed again; no reboot is needed"
                        .to_string(),
                );
            }
        }
    }
    code
}

/// Make one set match: `Ok(true)` = packages were installed, `Ok(false)` =
/// only the hold was missing, `Err` = why not.
async fn install_set(
    env: &PkgEnv<'_>,
    plan: &SetPlan,
    host: &HostFacts,
    held: Option<&[String]>,
    record: &mut InstallRecord,
    say: &mut dyn FnMut(String),
) -> Result<bool, Failure> {
    let installed = install_packages(env, plan, host, held, record, say)
        .await
        .map_err(Failure::NotInstalled)?;
    // The hold: without it a later apt upgrade replaces the packages.
    if plan.apt_hold {
        let names = plan.names();
        let to_hold: Vec<String> = match held {
            Some(held) if !installed => names
                .iter()
                .filter(|n| !held.contains(n))
                .cloned()
                .collect(),
            // Just installed (an install does not keep a hold in every apt
            // version), or the holds could not be read: set them all.
            _ => names.clone(),
        };
        if !to_hold.is_empty() {
            let r = tool(
                env,
                &env.apt_mark,
                &pk::apt_mark_args("hold", &to_hold),
                APT_MARK_TIMEOUT,
            )
            .await;
            if r.failed {
                return Err(Failure::HoldFailed {
                    installed,
                    why: format!(
                        "the packages are installed, but `apt-mark hold {}` failed ({}): an apt upgrade may replace them",
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
        }
    }
    Ok(installed)
}

/// Download, verify and install the packages of `plan` that are not at
/// their pinned version: `Ok(true)` = installed, `Ok(false)` = nothing was
/// missing, `Err` = why not (the sentence says whether anything changed).
async fn install_packages(
    env: &PkgEnv<'_>,
    plan: &SetPlan,
    host: &HostFacts,
    held: Option<&[String]>,
    record: &mut InstallRecord,
    say: &mut dyn FnMut(String),
) -> Result<bool, String> {
    let id = &plan.id;
    let before: BTreeMap<String, String> = host.packages.clone().unwrap_or_default();
    let missing: Vec<&Package> = plan
        .packages
        .iter()
        .filter(|p| before.get(&p.name) != Some(&p.version))
        .collect();
    let mut installed = false;
    if !missing.is_empty() {
        // 1. A directory only root can read; gone on every path out.
        std::fs::create_dir_all(&env.state_dir).map_err(|e| {
            format!(
                "cannot create {}: {e}; nothing was changed",
                env.state_dir.display()
            )
        })?;
        let dir = nix::unistd::mkdtemp(&env.state_dir.join("download-XXXXXX")).map_err(|e| {
            format!(
                "cannot create a download directory in {}: {e}; nothing was changed",
                env.state_dir.display()
            )
        })?;
        let scratch = Scratch(dir);
        let client = fetch_client(&env.limits)
            .map_err(|e| format!("cannot set up the HTTPS client: {e}; nothing was changed"))?;
        // 2. Download and verify EVERY file before anything is installed.
        let mut files: Vec<String> = Vec::new();
        for p in &missing {
            let file = pk::check_package(p)
                .map_err(|e| format!("{}: {e}; nothing was changed", p.name))?;
            let dest = scratch.0.join(&file);
            let url = match &env.url_map {
                Some(map) => map(&p.url),
                None => p.url.clone(),
            };
            say(format!(
                "{id}: downloading {} {} ({} bytes)",
                p.name, p.version, p.bytes
            ));
            fetch_pinned(
                &client,
                Pinned {
                    url: &url,
                    bytes: p.bytes,
                    sha256: &p.sha256,
                },
                &dest,
                &*env.redirect_ok,
                pk::MAX_REDIRECTS,
                &env.limits,
            )
            .await
            .map_err(|e| format!("{file} was not accepted: {e}; nothing was changed"))?;
            say(format!(
                "{id}: verified {file} ({} bytes, sha256 {}…)",
                p.bytes,
                &p.sha256[..12]
            ));
            files.push(dest.to_string_lossy().into_owned());
        }
        // 3. What would apt do? Only the pinned packages, or nothing at all.
        let sim = tool(
            env,
            &env.apt_get,
            &pk::apt_install_args(&files, true),
            APT_PLAN_TIMEOUT,
        )
        .await;
        if sim.failed {
            return Err(format!(
                "apt-get could not plan the install ({}); nothing was changed",
                last_line(&sim)
            ));
        }
        let pins: Vec<Package> = missing.iter().map(|p| (*p).clone()).collect();
        pk::check_simulation(&sim.stdout, &pins)
            .map_err(|e| format!("{e}; nothing was changed"))?;
        // 4. The one transaction.
        let res = tool(
            env,
            &env.apt_get,
            &pk::apt_install_args(&files, false),
            APT_INSTALL_TIMEOUT,
        )
        .await;
        let names = plan.names();
        let after = xorgate_hal::camera_host::query_packages_with(
            &env.dpkg_query,
            Some(&env.tool_env),
            &names,
        )
        .unwrap_or_default();
        let wrong: Vec<String> = plan
            .packages
            .iter()
            .filter(|p| after.get(&p.name) != Some(&p.version))
            .map(|p| {
                format!(
                    "{} is {}",
                    p.name,
                    after.get(&p.name).map_or("not installed", String::as_str)
                )
            })
            .collect();
        if res.failed || !wrong.is_empty() {
            let why = if res.failed {
                format!("apt-get install failed ({})", last_line(&res))
            } else {
                "apt-get reported success, but the packages are not at the pinned versions"
                    .to_string()
            };
            let untouched = plan
                .packages
                .iter()
                .all(|p| after.get(&p.name) == before.get(&p.name));
            return Err(if untouched {
                format!("{why}; nothing was changed")
            } else {
                format!(
                    "{why}; the packages are now: {} (check with `dpkg --audit`)",
                    plan.packages
                        .iter()
                        .map(|p| format!(
                            "{} {}",
                            p.name,
                            after.get(&p.name).map_or("not installed", String::as_str)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
        }
        installed = true;
        say(format!(
            "{id}: installed {}",
            missing
                .iter()
                .map(|p| format!(
                    "{} {} (was {})",
                    p.name,
                    p.version,
                    before.get(&p.name).map_or("not installed", String::as_str)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        // What was replaced, so it can be put back by hand.
        let rec = pk::set_record(
            record.sets.get(id),
            &plan.packages,
            &before,
            held.unwrap_or(&[]),
            plan.apt_hold,
            env.now_ms,
        );
        record.v = 1;
        record.sets.insert(id.clone(), rec);
        if let Err(e) = xorgate_hal::atomic_write::atomic_write_json(&record_path(env), record) {
            say(format!(
                "WARN {id}: could not record what was replaced in {} ({e}); the packages are installed",
                record_path(env).display()
            ));
        }
    }
    Ok(installed)
}
