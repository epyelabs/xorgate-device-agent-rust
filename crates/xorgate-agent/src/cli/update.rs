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
//! the I/O. Subprocesses go through `xorgate_hal::exec::run` with bounded
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
        println!("{}{line}", plan::LOG_PREFIX);
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
        eprintln!("{}{}", plan::FAIL_PREFIX, plan::NOT_ROOT_MESSAGE);
        return 1;
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{}{e}", plan::FAIL_PREFIX);
            return 1;
        }
    };
    let env = |k: &str| std::env::var(k).ok();
    match rt.block_on(update(argv, &env, &UpdatePaths::system(), &mut Stdout)) {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("{}{msg}", plan::FAIL_PREFIX);
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

/// `copyFileSync(from, to)` + `chmodSync(to, mode)`, the error worded like
/// node's (`EROFS: … copyfile 'a' -> 'b'`) for the rollback line.
fn install_file(from: &Path, to: &Path, mode: u32) -> Result<(), String> {
    fs::copy(from, to)
        .map_err(|e| format!("{e}, copyfile '{}' -> '{}'", from.display(), to.display()))?;
    chmod(to, mode)
}

fn chmod(p: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(p, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("{e}, chmod '{}'", p.display()))
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
    if let Err(e) = install(p, policy, out).await {
        // Roll back to the previous build (`update.ts:351-356`).
        rm_rf(&live);
        if had_prev {
            if let Err(re) = fs::rename(&backup, &live) {
                return Err(format!("install step failed, rollback FAILED ({re}): {e}"));
            }
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
    Ok(())
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
    install_file(&live.join(plan::UNIT_SRC), &unit_dest, 0o644)?;
    let wrapper = p.abs(plan::WRAPPER);
    rm_rf(&wrapper); // break any prior symlink before writing
    write_file(&wrapper, plan::WRAPPER_BODY, 0o755)?;
    chmod(&wrapper, 0o755)?;

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
                install_file(&from, &p.abs(dest), *mode)?;
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
                install_file(&from, &p.abs(dest), *mode)?;
            }
            install_sudoers(p, SudoersGrant::Clock, out).await?;
            if exists(&p.abs(plan::CAMROTATE_BIN)) {
                install_sudoers(p, SudoersGrant::CamRotate, out).await?;
            }
        }
    }

    // `update.ts:341-350`: the remote rail (Rust-only, never shared).
    let updater_unit = live.join(plan::UPDATER_UNIT_SRC);
    if exists(&updater_unit) {
        install_file(&updater_unit, &p.abs(plan::UPDATER_UNIT_DEST), 0o644)?;
        install_sudoers(p, SudoersGrant::UpdateRs, out).await?;
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
/// goes to a mkdtemp draft (0440), `visudo -cf` must pass, then copy + chmod
/// 0440; a refusal is a warning, never fatal; the draft dir is always removed.
/// No sudoers file is ever written without a passing `visudo -cf`.
async fn install_sudoers(
    p: &UpdatePaths,
    grant: SudoersGrant,
    out: &mut dyn UpdateLog,
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
        install_file(&draft, &dest, 0o440)
    }
    .await;
    rm_rf(&dir);
    res
}

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
        assert!(directives.contains(&"TimeoutStartSec=20min"));
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
        let tb = plan::tarball_name(version);
        let tgz = r.dir.join("bucket").join(&tb);
        let st = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&tree)
            .args(["bin", "systemd", "build-info.json"])
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
}
