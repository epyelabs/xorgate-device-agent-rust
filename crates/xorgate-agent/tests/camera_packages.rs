//! `xorgate-rs camera requirements [--install]` (support-other-camera-models
//! Phase 4): the extra OS packages of a camera sensor, end to end.
//!
//! - The plan is the catalog's (a TEST catalog here: the compiled-in one
//!   with the package set's target replaced by two tiny files a local
//!   server hands out).
//! - The download is the real one (`xorgate_cloud::fetch`) against that
//!   local server; the pinned URL is `https://github.com/…` as in
//!   production and reaches the server through the test seam `url_map`.
//! - `apt-get`, `apt-mark`, `dpkg-query` and `systemctl` are FAKES (shell
//!   scripts that keep their state in files and log their arguments), so
//!   every refusal can be shown to have changed nothing.
//! - One test at the end uses the REAL `dpkg-deb`, `apt-get`, `apt-mark`
//!   and `dpkg-query` on two tiny packages built on the spot. It runs only
//!   as root inside a container (the gate's cargo container), and removes
//!   what it installed.
//!
//! NOT covered here (it needs the real packages on a Raspberry Pi): that
//! Arducam's two `.deb` files install over the stock ones with no other
//! package pulled in or removed, and that the IMX335 then lists.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use xorgate_agent::cli::camera_packages::{parse, run, Args, PkgEnv, LOCK_FILE, RECORD_FILE};
use xorgate_cloud::fetch::FetchLimits;
use xorgate_core::boot_config::{Board, CamPort};
use xorgate_core::camera_catalog::{catalog, Catalog, Package, PackageTarget};
use xorgate_core::camera_packages as pk;

#[path = "../../xorgate-cloud/tests/support/http_stub.rs"]
mod http_stub;
use http_stub::{serve, HttpStub, Reply};

const V: &str = "0.5.2+rpt20250909-1";
const STOCK: &str = "0.5.2+rpt20250903-1";
const A: &[u8] = b"the first tiny package: libcamera0.5\n";
const B: &[u8] = b"the second tiny package: libcamera-ipa, a little longer\n";
const SET: &str = "arducam-libcamera";
const BASE: &str = "https://github.com/test/pkgs/releases/download/v1/";
/// The boot the device is in.
const BOOT: &str = "boot-1";

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn file_a() -> String {
    format!("libcamera0.5_{V}_arm64.deb")
}

fn file_b() -> String {
    format!("libcamera-ipa_{V}_arm64.deb")
}

/// Trixie (catalog 4): Arducam's build of the stock 0.8.0, other package
/// names than Bookworm's (`libcamera0.8`; `libcamera-ipa` is one name on
/// both), and the stock version it replaces.
const V8: &str = "0.8.0+rpt20261106-2";
const STOCK8: &str = "0.8.0+rpt20261104-1";
const C: &[u8] = b"the third tiny package: libcamera0.8, for trixie\n";
const D: &[u8] = b"the fourth tiny package: libcamera-ipa 0.8.0, for trixie\n";
/// Debian 13's `/etc/os-release` (the bench reads `VERSION_CODENAME=trixie`).
const TRIXIE_OS_RELEASE: &str = "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nNAME=\"Debian GNU/Linux\"\nVERSION_ID=\"13\"\nVERSION=\"13 (trixie)\"\nVERSION_CODENAME=trixie\nID=debian\n";

fn file_c() -> String {
    format!("libcamera0.8_{V8}_arm64.deb")
}

fn file_d() -> String {
    format!("libcamera-ipa_{V8}_arm64.deb")
}

/// The compiled-in catalog's SHAPE for the IMX335's set: one target per OS
/// release (bookworm and trixie, arm64), each with its own pair, pointing
/// at the tiny files.
fn two_release_catalog() -> &'static Catalog {
    let mut c = test_catalog(
        "bookworm",
        "arm64",
        &[
            ("libcamera0.5", V, file_a(), A),
            ("libcamera-ipa", V, file_b(), B),
        ],
    )
    .clone();
    let real: Vec<(String, String, Vec<String>)> = catalog()
        .package_set(SET)
        .unwrap()
        .targets
        .iter()
        .map(|t| {
            (
                t.os.clone(),
                t.arch.clone(),
                t.packages.iter().map(|p| p.name.clone()).collect(),
            )
        })
        .collect();
    // The test's targets follow the real ones: names, OS releases, order.
    assert_eq!(
        real,
        [
            (
                "bookworm".to_string(),
                "arm64".to_string(),
                vec!["libcamera0.5".to_string(), "libcamera-ipa".to_string()]
            ),
            (
                "trixie".to_string(),
                "arm64".to_string(),
                vec!["libcamera0.8".to_string(), "libcamera-ipa".to_string()]
            ),
        ]
    );
    let set = c.package_sets.iter_mut().find(|s| s.id == SET).unwrap();
    set.targets.push(PackageTarget {
        os: "trixie".to_string(),
        arch: "arm64".to_string(),
        packages: [
            ("libcamera0.8", file_c(), C),
            ("libcamera-ipa", file_d(), D),
        ]
        .into_iter()
        .map(|(name, file, bytes)| Package {
            name: name.to_string(),
            version: V8.to_string(),
            url: format!("{BASE}{file}"),
            sha256: sha(bytes),
            bytes: bytes.len() as u64,
        })
        .collect(),
    });
    Box::leak(Box::new(c))
}

/// The compiled-in catalog with the IMX335's package set pointing at two
/// tiny files (`packages`: name, version, file name, bytes).
fn test_catalog(
    os: &str,
    arch: &str,
    packages: &[(&str, &str, String, &[u8])],
) -> &'static Catalog {
    let mut c = catalog().clone();
    let set = c
        .package_sets
        .iter_mut()
        .find(|s| s.id == SET)
        .expect("the set of the imx335");
    set.targets = vec![PackageTarget {
        os: os.to_string(),
        arch: arch.to_string(),
        packages: packages
            .iter()
            .map(|(name, version, file, bytes)| Package {
                name: name.to_string(),
                version: version.to_string(),
                url: format!("{BASE}{file}"),
                sha256: sha(bytes),
                bytes: bytes.len() as u64,
            })
            .collect(),
    }];
    Box::leak(Box::new(c))
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xg-campkg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_exec(path: &Path, body: &str) {
    xorgate_hal::test_support::write_executable(path, &format!("#!/bin/sh\n{body}\n")).unwrap();
}

/// What the local server does for each file: `ok`, or a named fault.
type Mode = Arc<Mutex<&'static str>>;

async fn package_server(mode: &Mode) -> HttpStub {
    let mode = mode.clone();
    serve(move |path, _port| {
        let (a, b) = (format!("/{}", file_a()), format!("/{}", file_b()));
        let body: &[u8] = if path == a {
            A
        } else if path == b {
            B
        } else if path == format!("/{}", file_c()) {
            return Reply::ok(C);
        } else if path == format!("/{}", file_d()) {
            return Reply::ok(D);
        } else if path.starts_with("/libfoo9_") {
            // The file of a second package set: its server never answers.
            return Reply::Hang;
        } else {
            return Reply::status(404);
        };
        // The fault hits the SECOND file: the first one is already on disk
        // when it strikes, and must be gone afterwards too.
        if path == a {
            return Reply::ok(body);
        }
        match *mode.lock().unwrap() {
            "wrong-hash" => Reply::ok(&body.iter().map(|x| x ^ 1).collect::<Vec<u8>>()),
            "wrong-size" => Reply::ok(&body[..10]),
            "truncated" => Reply::announcing(&body[..10], body.len() as u64),
            "foreign-redirect" => Reply::redirect("https://evil.example/libcamera-ipa.deb"),
            "plain-http-redirect" => Reply::redirect("http://github.com/x.deb"),
            "hang" => Reply::Hang,
            "gone" => Reply::status(404),
            _ => Reply::ok(body),
        }
    })
    .await
}

/// A device made of files: fake tools, their state, a settings.json.
struct Dev {
    dir: PathBuf,
    state: PathBuf,
    stub: HttpStub,
    mode: Mode,
    env: PkgEnv<'static>,
    /// Makes the stop request (SIGTERM, Ctrl-C in production).
    stop: tokio::sync::watch::Sender<bool>,
}

const FAKE_DPKG_QUERY: &str = r#"echo "$*" >> "$D/dpkg-query.log"
[ -f "$D/dpkg.fail" ] && exit 2
cat "$D/installed.tsv" 2>/dev/null
exit 0"#;

/// `apt-get`: `-s` prints the plan for the `.deb` files it was given (plus
/// what `apt.mode` asks for); without `-s` it "installs" them by rewriting
/// `installed.tsv`.
const FAKE_APT_GET: &str = r#"echo "$*" >> "$D/apt.log"
mode=$(cat "$D/apt.mode" 2>/dev/null || echo ok)
sim=0; files=""
for a in "$@"; do case "$a" in -s) sim=1 ;; *.deb) files="$files $a" ;; esac; done
if [ "$sim" = 1 ]; then
  [ "$mode" = sim-fail ] && { echo "E: Unable to correct problems, you have held broken packages." >&2; exit 100; }
  # A simulation that hangs, with a process of its own that must go with it.
  [ "$mode" = sim-hang ] && { sleep 60 & echo $! > "$D/bg.pid"; wait; }
  echo "NOTE: This is only a simulation!"
  for f in $files; do
    b=$(basename "$f" .deb); n=${b%%_*}; v=${b#*_}; v=${v%_*}
    echo "Inst $n [old] ($v local-deb [arm64])"
  done
  [ "$mode" = sim-extra ] && echo "Inst libextra1 (9 Debian:12 [arm64])"
  [ "$mode" = sim-remove ] && echo "Remv gstreamer1.0-libcamera [0.5.2]"
  exit 0
fi
env > "$D/apt.env"
[ "$mode" = install-fail ] && { echo "E: Sub-process /usr/bin/dpkg returned an error code (1)" >&2; exit 100; }
[ "$mode" = install-noop ] && exit 0
[ "$mode" = install-slow ] && sleep 2
# An install that hangs, with a "dpkg" of its own (apt runs dpkg in a session
# of its own, so stopping apt-get's process group does not reach it).
[ "$mode" = install-hangs ] && { setsid sh -c 'echo $$ > "$D/dpkg.pid"; exec sleep 60' & sleep 60; }
# Something the package's script leaves running, with apt's output still open.
[ "$mode" = install-lingers ] && { sleep 20 & }
# Another package changes with the transaction.
[ "$mode" = install-extra ] && sed -i 's/^bash\t5.2/bash\t5.3/' "$D/installed.tsv"
count=0
for f in $files; do
  # The first package goes in, then dpkg fails.
  [ "$mode" = install-partial ] && [ "$count" = 1 ] && { echo "E: Sub-process /usr/bin/dpkg returned an error code (1)" >&2; exit 100; }
  count=$((count + 1))
  [ -r "$f" ] || { echo "E: cannot read $f" >&2; exit 100; }
  b=$(basename "$f" .deb); n=${b%%_*}; v=${b#*_}; v=${v%_*}
  awk -F'\t' -v n="$n" '$1 != n' "$D/installed.tsv" > "$D/installed.new" 2>/dev/null
  printf '%s\t%s\tii \n' "$n" "$v" >> "$D/installed.new"
  mv "$D/installed.new" "$D/installed.tsv"
  cp "$f" "$D/seen-$(basename "$f")"
done
# The database cannot be read back afterwards.
[ "$mode" = confirm-fails ] && : > "$D/dpkg.fail"
exit 0"#;

const FAKE_APT_MARK: &str = r#"echo "$*" >> "$D/mark.log"
case "$1" in
  showhold) [ -f "$D/showhold.fail" ] && exit 100; cat "$D/held" 2>/dev/null; exit 0 ;;
  hold) shift; [ -f "$D/hold.fail" ] && { echo "E: could not open lock file" >&2; exit 100; }
        for n in "$@"; do echo "$n" >> "$D/held"; done; exit 0 ;;
esac
exit 2"#;

const FAKE_SYSTEMCTL: &str = r#"echo "$*" >> "$D/systemctl.log"
[ -f "$D/systemctl.fail" ] && { echo "Failed to try-restart: Unit not found." >&2; exit 5; }
exit 0"#;

impl Dev {
    async fn new(tag: &str) -> Self {
        let dir = scratch(tag);
        let state = dir.join("state");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(&bin.join("dpkg-query"), FAKE_DPKG_QUERY);
        write_exec(&bin.join("apt-get"), FAKE_APT_GET);
        write_exec(&bin.join("apt-mark"), FAKE_APT_MARK);
        write_exec(&bin.join("systemctl"), FAKE_SYSTEMCTL);
        std::fs::write(
            dir.join("os-release"),
            "PRETTY_NAME=\"Debian\"\nVERSION_CODENAME=bookworm\n",
        )
        .unwrap();
        let mode: Mode = Arc::new(Mutex::new("ok"));
        let stub = package_server(&mode).await;
        let catalog = test_catalog(
            "bookworm",
            "arm64",
            &[
                ("libcamera0.5", V, file_a(), A),
                ("libcamera-ipa", V, file_b(), B),
            ],
        );
        let to = stub.url("/");
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let env = PkgEnv {
            catalog,
            is_root: true,
            apt_get: bin.join("apt-get"),
            apt_mark: bin.join("apt-mark"),
            dpkg_query: bin.join("dpkg-query"),
            systemctl: bin.join("systemctl"),
            tool_env: vec![
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("D".to_string(), state.display().to_string()),
            ],
            os_release: dir.join("os-release"),
            arch: Some("arm64".to_string()),
            dpkg_updates: dir.join("dpkg-updates"),
            dpkg_locks: vec![dir.join("dpkg-lock-frontend"), dir.join("dpkg-lock")],
            state_dir: dir.join("packages"),
            // The tests run as whoever runs them (root in the gate).
            state_owner: nix::unistd::geteuid().as_raw(),
            settings: dir.join("settings.json"),
            // The agent's snapshot of this boot's camera lines and
            // config.txt: absent unless a test writes them.
            boot_lines: dir.join("camera-boot-lines.json"),
            boot_id: BOOT.to_string(),
            boot_config: Some(dir.join("config.txt")),
            board: Board::Cm5,
            service: "xorgate-agent-rs".to_string(),
            limits: FetchLimits {
                connect: Duration::from_secs(5),
                read: Duration::from_secs(5),
                total: Duration::from_secs(20),
            },
            install_bound: Duration::from_secs(pk::T_INSTALL),
            // The REAL policy: https and the pinned hosts only.
            redirect_ok: Box::new(pk::redirect_allowed),
            url_map: Some(Box::new(move |url: &str| url.replacen(BASE, &to, 1))),
            now_ms: 1_795_000_000_000,
            stop: stop_rx,
        };
        let dev = Self {
            dir,
            state,
            stub,
            mode,
            env,
            stop: stop_tx,
        };
        dev.settings(json!({"cameraSensors": {"cameras": {"cam0": {"sensor": "imx335"}, "cam1": {"sensor": "imx219"}}}}));
        dev.installed(&[("libcamera0.5", STOCK), ("libcamera-ipa", STOCK)]);
        dev
    }

    fn settings(&self, v: Value) {
        std::fs::write(&self.env.settings, v.to_string()).unwrap();
    }

    fn installed(&self, pkgs: &[(&str, &str)]) {
        let text: String = pkgs
            .iter()
            .map(|(n, v)| format!("{n}\t{v}\tii \n"))
            .collect();
        std::fs::write(self.state.join("installed.tsv"), text).unwrap();
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.state.join(name)).unwrap_or_default()
    }

    /// The agent's side of a pending reboot: it took its snapshot of the
    /// camera lines in boot `boot` when config.txt read `booted`, and
    /// config.txt reads `now` since. Returns the snapshot file's bytes.
    fn camera_lines(&self, boot: &str, booted: &str, now: &str) -> Vec<u8> {
        let snap = xorgate_core::camera_sensors::snapshot_boot_lines(
            boot,
            booted,
            Board::Cm5,
            catalog(),
            &[CamPort::Cam0, CamPort::Cam1],
        );
        let bytes = serde_json::to_vec(&snap).unwrap();
        std::fs::write(&self.env.boot_lines, &bytes).unwrap();
        std::fs::write(self.env.boot_config.as_ref().unwrap(), now).unwrap();
        bytes
    }

    fn set(&self, name: &str, text: &str) {
        std::fs::write(self.state.join(name), text).unwrap();
    }

    /// `(exit code, lines)` of `camera requirements <args>`.
    async fn run(&self, args: &[&str]) -> (i32, Vec<String>) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let args = parse(&args).expect("usage");
        let mut lines = Vec::new();
        let dir = self.dir.display().to_string();
        let code = run(&args, &self.env, &mut |l| {
            lines.push(l.replace(&dir, "<dir>"))
        })
        .await;
        (code, lines)
    }

    /// The names under the packages directory (the record, and any
    /// download directory left behind); the run's lock file aside.
    fn kept(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.env.state_dir)
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .filter(|n| n != LOCK_FILE)
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn record(&self) -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(self.env.state_dir.join(RECORD_FILE)).unwrap(),
        )
        .unwrap()
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_command_line() {
    let p = |a: &[&str]| parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(p(&[]), Some(Args::default()));
    assert_eq!(
        p(&[
            "--install",
            "--from-claim",
            "/c.json",
            "--no-restart",
            "--quiet"
        ]),
        Some(Args {
            install: true,
            from_claim: Some("/c.json".into()),
            no_restart: true,
            quiet: true,
            time_limit: None,
        })
    );
    assert_eq!(
        p(&["--install", "--time-limit", "1320"]).and_then(|a| a.time_limit),
        Some(1320)
    );
    for bad in ["", "-5", "12s", "1e3", "+7", " 9"] {
        assert_eq!(p(&["--install", "--time-limit", bad]), None, "{bad:?}");
    }
    assert_eq!(p(&["--time-limit"]), None);
    for bad in [
        &["--from-claim"][..],
        &["--remove"],
        &["install"],
        &["--install", "x"],
    ] {
        assert_eq!(p(bad), None, "{bad:?}");
    }
}

#[tokio::test]
async fn the_listing_only_looks() {
    let dev = Dev::new("list").await;
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 1, "something is missing");
    assert_eq!(
        lines,
        [
            "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), needed by imx335 (cam0):",
            "  libcamera0.5   required 0.5.2+rpt20250909-1  installed 0.5.2+rpt20250903-1  MISSING",
            "  libcamera-ipa  required 0.5.2+rpt20250909-1  installed 0.5.2+rpt20250903-1  MISSING",
            "to install what is missing: sudo xorgate-rs camera requirements --install (or sudo xorgate-rs update)",
        ]
    );
    // It asked dpkg and apt-mark, and nothing else: no download, no apt-get.
    assert_eq!(dev.read("dpkg-query.log").matches("-W").count(), 1);
    assert_eq!(dev.read("mark.log"), "showhold\n");
    assert_eq!(dev.read("apt.log"), "");
    assert_eq!(dev.stub.hits(), 0);
    assert!(dev.kept().is_empty());

    // Sensors that need nothing: one sentence (none with --quiet), and no
    // tool is run at all.
    let plain = Dev::new("list-plain").await;
    plain.settings(json!({"cameraSensors": {"cameras": {"cam0": {"sensor": "imx708"}, "cam1": {"sensor": "none"}}}}));
    assert_eq!(
        plain.run(&[]).await,
        (
            0,
            vec!["no declared camera sensor needs extra packages".to_string()]
        )
    );
    assert_eq!(plain.run(&["--install", "--quiet"]).await, (0, vec![]));
    assert_eq!(
        plain.run(&["--install"]).await,
        (
            0,
            vec!["camera packages: no declared camera sensor needs extra packages".to_string()]
        )
    );
    assert_eq!(plain.read("dpkg-query.log"), "");
    assert_eq!(plain.read("mark.log"), "");
    // No settings.json at all (a device that has not started yet).
    std::fs::remove_file(&plain.env.settings).unwrap();
    assert_eq!(plain.run(&["--install", "--quiet"]).await, (0, vec![]));
}

#[tokio::test]
async fn install_downloads_verifies_installs_holds_and_restarts() {
    let mut dev = Dev::new("install").await;
    // Not root: refused before anything is run.
    dev.env.is_root = false;
    assert_eq!(
        dev.run(&["--install"]).await,
        (
            1,
            vec!["camera packages: installing packages needs root: sudo xorgate-rs camera requirements --install".to_string()]
        )
    );
    assert_eq!(dev.read("apt.log"), "");
    assert_eq!(dev.read("mark.log"), "");
    assert_eq!(dev.read("dpkg-query.log"), "");
    assert_eq!(dev.stub.hits(), 0);
    dev.env.is_root = true;

    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines,
        [
            "camera packages: arducam-libcamera: downloading libcamera0.5 0.5.2+rpt20250909-1 (37 bytes)".to_string(),
            format!("camera packages: arducam-libcamera: verified libcamera0.5_0.5.2+rpt20250909-1_arm64.deb (37 bytes, sha256 {}…)", &sha(A)[..12]),
            "camera packages: arducam-libcamera: downloading libcamera-ipa 0.5.2+rpt20250909-1 (56 bytes)".to_string(),
            format!("camera packages: arducam-libcamera: verified libcamera-ipa_0.5.2+rpt20250909-1_arm64.deb (56 bytes, sha256 {}…)", &sha(B)[..12]),
            "camera packages: arducam-libcamera: installing libcamera0.5, libcamera-ipa with apt-get (never interrupted once begun)".to_string(),
            "camera packages: arducam-libcamera: installed libcamera0.5 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1), libcamera-ipa 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1)".to_string(),
            "camera packages: arducam-libcamera: apt hold set on libcamera0.5, libcamera-ipa".to_string(),
            "camera packages: the agent was restarted so the camera is listed again; no reboot is needed".to_string(),
        ]
    );
    // apt-get was asked twice with the SAME two files: the simulation,
    // then the one transaction. Nothing else was ever passed to it.
    let apt = dev.read("apt.log");
    let calls: Vec<&str> = apt.lines().collect();
    assert_eq!(calls.len(), 2, "{apt}");
    let dir = dev.dir.display().to_string();
    let common = "-o DPkg::Lock::Timeout=120 -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold install -y --no-install-recommends --no-remove --allow-downgrades --allow-change-held-packages";
    let files: Vec<&str> = calls[1][common.len() + 1..].split(' ').collect();
    assert_eq!(files.len(), 2);
    for (f, name) in files.iter().zip([file_a(), file_b()]) {
        assert!(
            f.starts_with(&format!("{dir}/packages/download-")) && f.ends_with(&format!("/{name}")),
            "{f}"
        );
    }
    assert_eq!(calls[0], format!("{common} -s {}", files.join(" ")));
    assert_eq!(calls[1], format!("{common} {}", files.join(" ")));
    // The files apt installed were the pinned bytes.
    assert_eq!(
        std::fs::read(dev.state.join(format!("seen-{}", file_a()))).unwrap(),
        A
    );
    assert_eq!(
        std::fs::read(dev.state.join(format!("seen-{}", file_b()))).unwrap(),
        B
    );
    // The tools ran with the fixed environment only (nothing inherited).
    let env = dev.read("apt.env");
    assert!(env.contains(&format!("D={}", dev.state.display())), "{env}");
    for leaked in ["HOME=", "CARGO", "USER=", "RUST"] {
        assert!(
            !env.lines().any(|l| l.starts_with(leaked)),
            "{leaked} leaked: {env}"
        );
    }
    // The device now: both at the pinned version, both held, the agent
    // restarted (only if it runs: try-restart), the record written, the
    // download directory gone.
    assert_eq!(
        dev.read("installed.tsv"),
        format!("libcamera0.5\t{V}\tii \nlibcamera-ipa\t{V}\tii \n")
    );
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    assert_eq!(
        dev.read("mark.log"),
        "showhold\nhold libcamera0.5 libcamera-ipa\n"
    );
    assert_eq!(dev.read("systemctl.log"), "try-restart xorgate-agent-rs\n");
    assert_eq!(dev.kept(), [RECORD_FILE]);
    assert_eq!(
        dev.record(),
        json!({"v": 1, "sets": {"arducam-libcamera": {"at": 1_795_000_000_000i64, "packages": [
            {"name": "libcamera0.5", "installed": V, "replaced": STOCK},
            {"name": "libcamera-ipa", "installed": V, "replaced": STOCK}
        ], "held": ["libcamera0.5", "libcamera-ipa"]}}})
    );
    // The state directory is root's alone to write and everyone's to read
    // (the unprivileged listing prints the way back from the record).
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&dev.env.state_dir), 0o755);
    assert_eq!(mode(&dev.env.state_dir.join(RECORD_FILE)), 0o644);

    // IDEMPOTENT: a second run downloads nothing, runs no apt-get, restarts
    // nothing, and with --quiet says nothing.
    let hits = dev.stub.hits();
    assert_eq!(dev.run(&["--install", "--quiet"]).await, (0, vec![]));
    assert_eq!(
        dev.run(&["--install"]).await,
        (
            0,
            vec!["camera packages: arducam-libcamera: already installed and held (needed by imx335 (cam0))".to_string()]
        )
    );
    assert_eq!(dev.stub.hits(), hits);
    assert_eq!(dev.read("apt.log").lines().count(), 2);
    assert_eq!(dev.read("systemctl.log").lines().count(), 1);
    // The listing now shows the versions, the hold, and how to go back.
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 0);
    assert_eq!(
        lines[1..],
        [
            "  libcamera0.5   required 0.5.2+rpt20250909-1  installed 0.5.2+rpt20250909-1  ok",
            "  libcamera-ipa  required 0.5.2+rpt20250909-1  installed 0.5.2+rpt20250909-1  ok",
            "  apt hold: set on every package",
            "  installed by xorgate-rs; it replaced: libcamera0.5 0.5.2+rpt20250903-1, libcamera-ipa 0.5.2+rpt20250903-1",
            "  to go back: sudo apt-mark unhold libcamera0.5 libcamera-ipa",
            "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
            "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
            "  (going back does not last while a port is set to a sensor that needs these packages: the next `xorgate-rs update` installs them again. Set the port to another sensor or to `none` first)",
        ]
    );
}

#[tokio::test]
async fn only_what_is_missing_is_done() {
    // One of the two already at the pinned version: one download, one file.
    let dev = Dev::new("partial").await;
    dev.installed(&[("libcamera0.5", V), ("libcamera-ipa", STOCK)]);
    let (code, lines) = dev.run(&["--install", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(dev.stub.hits(), 1);
    let apt = dev.read("apt.log");
    assert_eq!(apt.lines().count(), 2);
    assert!(
        !apt.contains("libcamera0.5_") && apt.contains(&file_b()),
        "{apt}"
    );
    assert_eq!(
        lines[2..],
        [
            "camera packages: arducam-libcamera: installing libcamera-ipa with apt-get (never interrupted once begun)",
            "camera packages: arducam-libcamera: installed libcamera-ipa 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1)",
            "camera packages: arducam-libcamera: apt hold set on libcamera0.5, libcamera-ipa",
            "camera packages: the camera appears when the agent starts next (or at its next camera re-listing, within 5 minutes); no reboot is needed",
        ]
    );
    // --no-restart: systemctl was not run.
    assert_eq!(dev.read("systemctl.log"), "");
    // The package that was already at the pinned version is not recorded
    // as replaced by anything: only what this run installed is.
    assert_eq!(
        dev.record()["sets"][SET],
        json!({"at": 1_795_000_000_000i64, "packages": [
            {"name": "libcamera-ipa", "installed": V, "replaced": STOCK}
        ], "held": ["libcamera0.5", "libcamera-ipa"]})
    );

    // Both installed by hand, no hold: only the hold is set. No download,
    // no apt-get, no restart. The record says which hold is this
    // command's, and nothing about packages it did not install.
    let dev = Dev::new("hold-only").await;
    dev.installed(&[("libcamera0.5", V), ("libcamera-ipa", V)]);
    dev.set("held", "libcamera0.5\n");
    assert_eq!(
        dev.run(&["--install", "--quiet"]).await,
        (
            0,
            vec!["camera packages: arducam-libcamera: apt hold set on libcamera-ipa".to_string()]
        )
    );
    assert_eq!(dev.read("mark.log"), "showhold\nhold libcamera-ipa\n");
    assert_eq!((dev.stub.hits(), dev.read("apt.log")), (0, String::new()));
    assert_eq!(dev.read("systemctl.log"), "");
    assert_eq!(
        dev.record()["sets"][SET],
        json!({"at": 1_795_000_000_000i64, "packages": [], "held": ["libcamera-ipa"]})
    );
    let (_, lines) = dev.run(&[]).await;
    assert_eq!(
        lines[3..],
        [
            "  apt hold: set on every package",
            "  to go back: sudo apt-mark unhold libcamera-ipa",
            "  (going back does not last while a port is set to a sensor that needs these packages: the next `xorgate-rs update` installs them again. Set the port to another sensor or to `none` first)",
        ]
    );

    // The agent could not be restarted: said, and not a failure.
    let dev = Dev::new("restart-fails").await;
    dev.set("systemctl.fail", "");
    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        lines.last().unwrap(),
        "camera packages: WARN the agent could not be restarted (Failed to try-restart: Unit not found.); the camera appears at its next camera re-listing, within 5 minutes"
    );
}

/// A failed download or checksum is a loud WARN and the system is EXACTLY
/// as it was: apt-get never ran, no package changed, no hold, no restart,
/// no file of the download left.
#[tokio::test]
async fn a_download_that_is_not_the_pinned_file_changes_nothing() {
    let cases: [(&str, &str); 7] = [
        ("wrong-hash", "sha256 mismatch: the file is "),
        ("wrong-size", "the server announces 10 bytes, the catalog pins 56"),
        ("truncated", "the download broke off after "),
        ("foreign-redirect", "a redirect to a host that is not allowed (\"evil.example\"; allowed: github.com, release-assets.githubusercontent.com, objects.githubusercontent.com)"),
        ("plain-http-redirect", "a redirect to a URL that is not https://"),
        ("gone", "the server answered HTTP 404"),
        ("hang", "the request failed: timed out"),
    ];
    for (mode, why) in cases {
        let mut dev = Dev::new(&format!("bad-{mode}")).await;
        *dev.mode.lock().unwrap() = mode;
        if mode == "hang" {
            dev.env.limits.read = Duration::from_millis(300);
        }
        let before = dev.read("installed.tsv");
        let (code, lines) = dev.run(&["--install"]).await;
        assert_eq!(code, 1, "{mode}: {lines:?}");
        // The first file was fine; the second was refused.
        assert_eq!(lines.len(), 4, "{mode}: {lines:?}");
        let warn = &lines[3];
        let head = format!(
            "camera packages: WARN arducam-libcamera NOT installed: {} was not accepted: {why}",
            file_b()
        );
        assert!(warn.starts_with(&head), "{mode}: {warn}");
        assert!(
            warn.ends_with("; nothing was changed. imx335 (cam0) will not appear until this succeeds: run `sudo xorgate-rs camera requirements --install` again"),
            "{mode}: {warn}"
        );
        assert_eq!(dev.read("apt.log"), "", "{mode}: apt-get never ran");
        assert_eq!(dev.read("installed.tsv"), before, "{mode}");
        assert_eq!(dev.read("held"), "", "{mode}");
        assert_eq!(dev.read("mark.log"), "showhold\n", "{mode}");
        assert_eq!(dev.read("systemctl.log"), "", "{mode}");
        // Nothing of the download is left: not the bad file, not the good
        // one fetched before it, not the directory.
        assert!(dev.kept().is_empty(), "{mode}: {:?}", dev.kept());
        // --quiet does not silence a failure.
        let (code, lines) = dev.run(&["--install", "--quiet", "--no-restart"]).await;
        assert_eq!(code, 1);
        assert!(
            lines.last().unwrap().starts_with(&head),
            "{mode}: {lines:?}"
        );
        // The fault goes away: the same command then succeeds.
        *dev.mode.lock().unwrap() = "ok";
        let (code, _) = dev.run(&["--install", "--quiet"]).await;
        assert_eq!(code, 0, "{mode}");
        assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n", "{mode}");
    }
}

/// apt may install the pinned packages and nothing else; when it fails or
/// wants more, nothing is installed and the sentence says so.
#[tokio::test]
async fn apt_is_checked_before_and_after() {
    let cases: [(&str, &str, usize); 5] = [
        ("sim-extra", "apt would also install libextra1, which the catalog does not pin; nothing was changed", 1),
        ("sim-remove", "apt would remove gstreamer1.0-libcamera, which the catalog does not ask for; nothing was changed", 1),
        ("sim-fail", "apt-get could not plan the install (E: Unable to correct problems, you have held broken packages.); nothing was changed", 1),
        ("install-fail", "apt-get install failed (E: Sub-process /usr/bin/dpkg returned an error code (1)); nothing was changed", 2),
        ("install-noop", "apt-get reported success, but the packages are not at the pinned versions; nothing was changed", 2),
    ];
    for (mode, why, apt_calls) in cases {
        let dev = Dev::new(&format!("apt-{mode}")).await;
        dev.set("apt.mode", mode);
        let before = dev.read("installed.tsv");
        let (code, lines) = dev.run(&["--install"]).await;
        assert_eq!(code, 1, "{mode}");
        assert_eq!(
            lines.last().unwrap(),
            &format!("camera packages: WARN arducam-libcamera NOT installed: {why}. imx335 (cam0) will not appear until this succeeds: run `sudo xorgate-rs camera requirements --install` again"),
            "{mode}"
        );
        assert_eq!(dev.read("apt.log").lines().count(), apt_calls, "{mode}");
        assert_eq!(dev.read("installed.tsv"), before, "{mode}");
        assert_eq!(dev.read("held"), "", "{mode}: nothing is held");
        assert_eq!(dev.read("systemctl.log"), "", "{mode}");
        assert!(dev.kept().is_empty(), "{mode}: {:?}", dev.kept());
    }
    // The packages went in, the hold did not: said as that (the camera
    // works; an upgrade may undo it), exit 1, and the next run sets it.
    let dev = Dev::new("apt-hold-fails").await;
    dev.set("hold.fail", "");
    let (code, lines) = dev.run(&["--install", "--no-restart"]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines[6..],
        [
            "camera packages: WARN arducam-libcamera: the packages are installed, but `apt-mark hold libcamera0.5 libcamera-ipa` failed (E: could not open lock file): an apt upgrade may replace them. Run `sudo xorgate-rs camera requirements --install` again to set the hold",
            "camera packages: the camera appears when the agent starts next (or at its next camera re-listing, within 5 minutes); no reboot is needed",
        ],
        "{lines:?}"
    );
    std::fs::remove_file(dev.state.join("hold.fail")).unwrap();
    assert_eq!(
        dev.run(&["--install", "--quiet"]).await,
        (
            0,
            vec![
                "camera packages: arducam-libcamera: apt hold set on libcamera0.5, libcamera-ipa"
                    .to_string()
            ]
        )
    );
}

#[tokio::test]
async fn another_os_release_or_architecture_is_refused_in_a_sentence() {
    // The catalog pins bookworm/arm64 and trixie/arm64: a Trixie device of
    // another architecture is refused (the Trixie plan itself:
    // `the_trixie_pair_is_its_own_plan`).
    let mut dev = Dev::new("os").await;
    dev.env.catalog = two_release_catalog();
    std::fs::write(&dev.env.os_release, TRIXIE_OS_RELEASE).unwrap();
    dev.env.arch = Some("armhf".to_string());
    assert_eq!(
        dev.run(&["--install"]).await,
        (
            3,
            vec!["camera packages: WARN arducam-libcamera REFUSED: the catalog lists no packages of this set for trixie/armhf, so nothing can be installed on this device; imx335 (cam0) will not work here".to_string()]
        )
    );
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 3);
    assert_eq!(
        lines[1],
        "  REFUSED: the catalog lists no packages of this set for trixie/armhf; nothing can be installed on this device"
    );
    // An OS release the catalog does not pin at all.
    dev.env.arch = Some("arm64".to_string());
    std::fs::write(&dev.env.os_release, "VERSION_CODENAME=bullseye\n").unwrap();
    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 3);
    assert!(lines[0].contains("for bullseye/arm64"), "{lines:?}");
    dev.env.catalog = test_catalog(
        "bookworm",
        "arm64",
        &[
            ("libcamera0.5", V, file_a(), A),
            ("libcamera-ipa", V, file_b(), B),
        ],
    );
    std::fs::write(&dev.env.os_release, "VERSION_CODENAME=bookworm\n").unwrap();
    dev.env.arch = Some("armhf".to_string());
    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 3);
    assert!(lines[0].contains("for bookworm/armhf"), "{lines:?}");
    // The OS release cannot be read: nothing is concluded, nothing is done.
    dev.env.arch = Some("arm64".to_string());
    std::fs::remove_file(&dev.env.os_release).unwrap();
    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines,
        ["camera packages: WARN arducam-libcamera NOT installed: the OS release or the package architecture of this device could not be read; nothing was changed"]
    );
    assert_eq!(dev.read("apt.log"), "");
    assert_eq!(dev.stub.hits(), 0);
    assert!(dev.kept().is_empty());

    // A catalog entry that is not plain (here: a host that is not allowed)
    // is refused whole.
    let mut dev = Dev::new("pin").await;
    let mut c = dev.env.catalog.clone();
    let set = c.package_sets.iter_mut().find(|s| s.id == SET).unwrap();
    set.targets[0].packages[1].url = format!("https://example.org/{}", file_b());
    dev.env.catalog = Box::leak(Box::new(c));
    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 3);
    assert_eq!(
        lines,
        ["camera packages: WARN arducam-libcamera REFUSED: the catalog entry is not usable (libcamera-ipa: its URL names a host that is not allowed (allowed: github.com)); nothing was changed"]
    );
    assert_eq!((dev.stub.hits(), dev.read("apt.log")), (0, String::new()));
}

/// Catalog 4, a Trixie device: the IMX335's set has a Trixie target whose
/// package names differ from Bookworm's (`libcamera0.8` + `libcamera-ipa`
/// 0.8.0). The listing, the install (simulation, the one transaction, the
/// hold, the record), the "to go back" lines and the block of packages no
/// sensor needs any more all name the Trixie pair and the versions it
/// replaced; Bookworm's pair is never asked for.
#[tokio::test]
async fn the_trixie_pair_is_its_own_plan() {
    let mut dev = Dev::new("trixie").await;
    dev.env.catalog = two_release_catalog();
    std::fs::write(&dev.env.os_release, TRIXIE_OS_RELEASE).unwrap();
    // Stock Trixie libcamera (an upgraded device may still list Bookworm's
    // libcamera0.5 as removed, `rc`: not installed).
    std::fs::write(
        dev.state.join("installed.tsv"),
        format!("libcamera0.8\t{STOCK8}\tii \nlibcamera-ipa\t{STOCK8}\tii \nlibcamera0.5\t{STOCK}\trc \n"),
    )
    .unwrap();
    let header = "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), needed by imx335 (cam0):";
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines,
        [
            header,
            "  libcamera0.8   required 0.8.0+rpt20261106-2  installed 0.8.0+rpt20261104-1  MISSING",
            "  libcamera-ipa  required 0.8.0+rpt20261106-2  installed 0.8.0+rpt20261104-1  MISSING",
            "to install what is missing: sudo xorgate-rs camera requirements --install (or sudo xorgate-rs update)",
        ]
    );
    assert_eq!(dev.stub.hits(), 0);

    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines,
        [
            format!("camera packages: arducam-libcamera: downloading libcamera0.8 0.8.0+rpt20261106-2 ({} bytes)", C.len()),
            format!("camera packages: arducam-libcamera: verified libcamera0.8_0.8.0+rpt20261106-2_arm64.deb ({} bytes, sha256 {}…)", C.len(), &sha(C)[..12]),
            format!("camera packages: arducam-libcamera: downloading libcamera-ipa 0.8.0+rpt20261106-2 ({} bytes)", D.len()),
            format!("camera packages: arducam-libcamera: verified libcamera-ipa_0.8.0+rpt20261106-2_arm64.deb ({} bytes, sha256 {}…)", D.len(), &sha(D)[..12]),
            "camera packages: arducam-libcamera: installing libcamera0.8, libcamera-ipa with apt-get (never interrupted once begun)".to_string(),
            "camera packages: arducam-libcamera: installed libcamera0.8 0.8.0+rpt20261106-2 (was 0.8.0+rpt20261104-1), libcamera-ipa 0.8.0+rpt20261106-2 (was 0.8.0+rpt20261104-1)".to_string(),
            "camera packages: arducam-libcamera: apt hold set on libcamera0.8, libcamera-ipa".to_string(),
            "camera packages: the agent was restarted so the camera is listed again; no reboot is needed".to_string(),
        ]
    );
    // The simulation, then the one transaction, with the Trixie files only.
    let apt = dev.read("apt.log");
    let calls: Vec<&str> = apt.lines().collect();
    assert_eq!(calls.len(), 2, "{apt}");
    assert!(calls[0].contains(" -s "), "{apt}");
    for call in &calls {
        let files: Vec<&str> = call.split(' ').filter(|a| a.ends_with(".deb")).collect();
        assert_eq!(files.len(), 2, "{call}");
        assert!(files[0].ends_with(&format!("/{}", file_c())), "{call}");
        assert!(files[1].ends_with(&format!("/{}", file_d())), "{call}");
    }
    assert_eq!(
        std::fs::read(dev.state.join(format!("seen-{}", file_c()))).unwrap(),
        C
    );
    assert_eq!(
        std::fs::read(dev.state.join(format!("seen-{}", file_d()))).unwrap(),
        D
    );
    assert_eq!(dev.read("held"), "libcamera0.8\nlibcamera-ipa\n");
    assert_eq!(
        dev.read("mark.log"),
        "showhold\nshowhold\nhold libcamera0.8 libcamera-ipa\n"
    );
    assert_eq!(dev.read("systemctl.log"), "try-restart xorgate-agent-rs\n");
    assert_eq!(
        dev.record(),
        json!({"v": 1, "sets": {"arducam-libcamera": {"at": 1_795_000_000_000i64, "packages": [
            {"name": "libcamera0.8", "installed": V8, "replaced": STOCK8},
            {"name": "libcamera-ipa", "installed": V8, "replaced": STOCK8}
        ], "held": ["libcamera0.8", "libcamera-ipa"]}}})
    );
    // The listing now: the pinned versions, the hold, the way back.
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 0);
    assert_eq!(
        lines[1..],
        [
            "  libcamera0.8   required 0.8.0+rpt20261106-2  installed 0.8.0+rpt20261106-2  ok",
            "  libcamera-ipa  required 0.8.0+rpt20261106-2  installed 0.8.0+rpt20261106-2  ok",
            "  apt hold: set on every package",
            "  installed by xorgate-rs; it replaced: libcamera0.8 0.8.0+rpt20261104-1, libcamera-ipa 0.8.0+rpt20261104-1",
            "  to go back: sudo apt-mark unhold libcamera0.8 libcamera-ipa",
            "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.8=0.8.0+rpt20261104-1 libcamera-ipa=0.8.0+rpt20261104-1",
            "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
            "  (going back does not last while a port is set to a sensor that needs these packages: the next `xorgate-rs update` installs them again. Set the port to another sensor or to `none` first)",
        ]
    );
    // No sensor needs the set any more: the leftover block names the
    // Trixie pair.
    dev.settings(json!({"cameraSensors": {"cameras": {"cam0": {"sensor": "imx708"}, "cam1": {"sensor": "imx219"}}}}));
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 0);
    assert_eq!(
        lines,
        [
            "no declared camera sensor needs extra packages",
            "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), the set imx335 needs: no declared sensor needs it any more, but what xorgate-rs installed for it is still in place:",
            "  libcamera0.8   installed 0.8.0+rpt20261106-2  held  (installed by xorgate-rs; it replaced 0.8.0+rpt20261104-1)",
            "  libcamera-ipa  installed 0.8.0+rpt20261106-2  held  (installed by xorgate-rs; it replaced 0.8.0+rpt20261104-1)",
            "  while they are held, apt does not update these packages: the OS's own updates of them do not reach this device",
            "  to go back: sudo apt-mark unhold libcamera0.8 libcamera-ipa",
            "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.8=0.8.0+rpt20261104-1 libcamera-ipa=0.8.0+rpt20261104-1",
            "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
        ]
    );
}

/// The Trixie bench of 2026-11-23: Arducam's Trixie pair was installed BY
/// HAND, at the pinned version, and not held, and there is no record. The
/// listing reads true (the packages are there, the hold is not), and
/// `--install` sets the hold and nothing else.
#[tokio::test]
async fn the_trixie_pair_installed_by_hand_lacks_only_its_hold() {
    let mut dev = Dev::new("trixie-hand").await;
    dev.env.catalog = two_release_catalog();
    std::fs::write(&dev.env.os_release, TRIXIE_OS_RELEASE).unwrap();
    dev.installed(&[("libcamera0.8", V8), ("libcamera-ipa", V8)]);
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 1, "the hold is to be set");
    assert_eq!(
        lines[1..],
        [
            "  libcamera0.8   required 0.8.0+rpt20261106-2  installed 0.8.0+rpt20261106-2  ok",
            "  libcamera-ipa  required 0.8.0+rpt20261106-2  installed 0.8.0+rpt20261106-2  ok",
            "  apt hold: MISSING on libcamera0.8, libcamera-ipa (an apt upgrade may replace them)",
            "to set the hold again: sudo xorgate-rs camera requirements --install (or sudo xorgate-rs update)",
        ]
    );
    assert_eq!(
        dev.run(&["--install", "--quiet"]).await,
        (
            0,
            vec![
                "camera packages: arducam-libcamera: apt hold set on libcamera0.8, libcamera-ipa"
                    .to_string()
            ]
        )
    );
    assert_eq!(dev.stub.hits(), 0);
    assert_eq!(dev.read("apt.log"), "");
    assert_eq!(dev.read("systemctl.log"), "");
    assert_eq!(dev.read("held"), "libcamera0.8\nlibcamera-ipa\n");
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 0);
    assert_eq!(lines[3], "  apt hold: set on every package");
}

/// The installer's path: the sensors of a claim response, before any
/// settings.json exists.
#[tokio::test]
async fn the_sensors_of_a_claim_response() {
    let dev = Dev::new("claim").await;
    std::fs::remove_file(&dev.env.settings).unwrap();
    let claim = dev.dir.join("claim.json");
    let path = claim.display().to_string();
    // The claim's own setting.
    std::fs::write(
        &claim,
        json!({
            "hardware": {"video": [{"key": "cam0", "connector": "csi0", "sensor": "imx290"}]},
            "cameraSensors": {"cameras": {"cam0": {"sensor": "imx335"}}}
        })
        .to_string(),
    )
    .unwrap();
    let (code, lines) = dev.run(&["--from-claim", &path]).await;
    assert_eq!(code, 1);
    assert!(lines[0].ends_with("needed by imx335 (cam0):"), "{lines:?}");
    // No setting: the model's sensors.
    std::fs::write(
        &claim,
        json!({"hardware": {"video": [
            {"key": "cam0", "connector": "csi0", "sensor": "imx290"},
            {"key": "cam1", "connector": "csi1", "sensor": "imx335"}
        ]}})
        .to_string(),
    )
    .unwrap();
    let (code, lines) = dev
        .run(&[
            "--install",
            "--no-restart",
            "--quiet",
            "--from-claim",
            &path,
        ])
        .await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    assert_eq!(dev.read("systemctl.log"), "");
    // A claim that names no such sensor: nothing, quietly.
    std::fs::write(&claim, json!({"deviceId": "x"}).to_string()).unwrap();
    assert_eq!(
        dev.run(&["--install", "--quiet", "--from-claim", &path])
            .await,
        (0, vec![])
    );
    // An unreadable claim is a usage error (exit 2), in a sentence.
    std::fs::write(&claim, "{not json").unwrap();
    let (code, lines) = dev.run(&["--install", "--from-claim", &path]).await;
    assert_eq!(code, 2);
    assert!(
        lines[0].starts_with("camera packages: cannot read the claim response <dir>/claim.json: "),
        "{lines:?}"
    );
}

/// Bench findings (2026-11-22, bench 2): the "to go back" lines must work
/// as printed, say what else is needed, and be printed only for what is
/// still in place; with only the hold missing the listing says so, and
/// `--install` sets it without a download or an apt-get.
#[tokio::test]
async fn going_back_and_the_hold_alone() {
    let dev = Dev::new("go-back").await;
    assert_eq!(
        dev.run(&["--install", "--quiet", "--no-restart"]).await.0,
        0
    );
    let (hits, apt_calls) = (dev.stub.hits(), dev.read("apt.log").lines().count());

    // Someone released the hold: the packages are fine, the hold is not.
    dev.set("held", "");
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 1, "the hold is to be set");
    assert_eq!(
        lines[3..],
        [
            "  apt hold: MISSING on libcamera0.5, libcamera-ipa (an apt upgrade may replace them)",
            "  installed by xorgate-rs; it replaced: libcamera0.5 0.5.2+rpt20250903-1, libcamera-ipa 0.5.2+rpt20250903-1",
            // No hold exists: no "unhold" line.
            "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
            "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
            "  (going back does not last while a port is set to a sensor that needs these packages: the next `xorgate-rs update` installs them again. Set the port to another sensor or to `none` first)",
            // Not "to install what is missing": no package is.
            "to set the hold again: sudo xorgate-rs camera requirements --install (or sudo xorgate-rs update)",
        ]
    );
    // --install in that state sets the hold and nothing else: no download,
    // no apt-get, no restart of the agent.
    assert_eq!(
        dev.run(&["--install", "--quiet"]).await,
        (
            0,
            vec![
                "camera packages: arducam-libcamera: apt hold set on libcamera0.5, libcamera-ipa"
                    .to_string()
            ]
        )
    );
    assert_eq!(dev.stub.hits(), hits);
    assert_eq!(dev.read("apt.log").lines().count(), apt_calls);
    assert_eq!(dev.read("systemctl.log"), "");
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    assert_eq!(dev.run(&[]).await.0, 0);

    // Gone back by hand: the hold released, the stock versions installed.
    // The record still tells what this command once did, but nothing of it
    // is in place any more, so no "to go back" line is printed.
    dev.set("held", "");
    dev.installed(&[("libcamera0.5", STOCK), ("libcamera-ipa", STOCK)]);
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines[1..],
        [
            "  libcamera0.5   required 0.5.2+rpt20250909-1  installed 0.5.2+rpt20250903-1  MISSING",
            "  libcamera-ipa  required 0.5.2+rpt20250909-1  installed 0.5.2+rpt20250903-1  MISSING",
            "to install what is missing: sudo xorgate-rs camera requirements --install (or sudo xorgate-rs update)",
        ]
    );
    // Half-way back (the hold released, one package put back): a line only
    // for the package that is still this command's.
    dev.installed(&[("libcamera0.5", STOCK), ("libcamera-ipa", V)]);
    let (_, lines) = dev.run(&[]).await;
    assert_eq!(
        lines[3..6],
        [
            "  installed by xorgate-rs; it replaced: libcamera-ipa 0.5.2+rpt20250903-1",
            "  to go back: sudo apt-get install -y --allow-downgrades libcamera-ipa=0.5.2+rpt20250903-1",
            "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)"
        ]
    );
}

/// Bench finding (2026-11-22, CM4): the update ended with "the agent was
/// restarted so the camera is listed again; no reboot is needed" while the
/// port read `reboot-required` (its camera line had been changed since
/// boot). The closing line goes by the agent's own snapshot of this boot's
/// camera lines, the fact behind that verdict, and says which port waits.
#[tokio::test]
async fn the_closing_line_does_not_deny_a_pending_reboot() {
    const WAS: &str = "dtoverlay=imx708,cam0,rotation=180\ndtoverlay=imx219,cam1,rotation=180\n";
    const NOW: &str = "dtoverlay=imx335,cam0,rotation=180\ndtoverlay=imx219,cam1,rotation=180\n";
    const CAM0: &str =
        "the camera line of cam0 in config.txt was changed since this device booted (sudo reboot)";
    let last = |lines: &[String]| lines.last().cloned().unwrap_or_default();

    // The update's case: installed, agent restarted, cam0 waits.
    let dev = Dev::new("pending").await;
    let snapshot = dev.camera_lines(BOOT, WAS, NOW);
    let (code, lines) = dev
        .run(&["--install", "--quiet", "--time-limit", "1320"])
        .await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        last(&lines),
        format!("camera packages: the packages are in place and the agent was restarted, but a reboot is still needed: {CAM0}")
    );
    assert!(
        !lines.iter().any(|l| l.contains("no reboot is needed")),
        "{lines:?}"
    );
    assert_eq!(dev.read("systemctl.log"), "try-restart xorgate-agent-rs\n");
    // The snapshot is the agent's: read, never written.
    assert_eq!(std::fs::read(&dev.env.boot_lines).unwrap(), snapshot);

    // By hand with --no-restart.
    let dev = Dev::new("pending-norestart").await;
    dev.camera_lines(BOOT, WAS, NOW);
    let (code, lines) = dev.run(&["--install", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        last(&lines),
        format!("camera packages: the packages are in place (the agent uses them when it starts next, or at its next camera re-listing, within 5 minutes), but a reboot is still needed: {CAM0}")
    );

    // The restart failed: still a WARN, and the reboot is not forgotten.
    let dev = Dev::new("pending-restart-fails").await;
    dev.camera_lines(BOOT, WAS, NOW);
    dev.set("systemctl.fail", "");
    let (code, lines) = dev.run(&["--install", "--quiet"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        last(&lines),
        format!("camera packages: WARN the agent could not be restarted (Failed to try-restart: Unit not found.); it uses the packages at its next camera re-listing, within 5 minutes, and a reboot is still needed: {CAM0}")
    );

    // Both ports wait (the other one's line was edited by hand: the
    // report says `reboot-required` for it too, managed or not).
    let dev = Dev::new("pending-both").await;
    dev.camera_lines(
        BOOT,
        WAS,
        "dtoverlay=imx335,cam0,rotation=180\ndtoverlay=imx219,cam1,rotation=0\n",
    );
    let (_, lines) = dev.run(&["--install", "--quiet"]).await;
    assert_eq!(
        last(&lines),
        "camera packages: the packages are in place and the agent was restarted, but a reboot is still needed: the camera lines of cam0 and cam1 in config.txt were changed since this device booted (sudo reboot)"
    );

    // NOTHING WAITS: today's line, in each of these.
    const TODAY: &str = "camera packages: the agent was restarted so the camera is listed again; no reboot is needed";
    // The device was rebooted since: the lines are as booted (another
    // spelling of the same line is not a change).
    let dev = Dev::new("pending-none").await;
    dev.camera_lines(
        BOOT,
        "dtoverlay=imx335,cam0\ndtoverlay=imx219,cam1,rotation=180\n",
        NOW,
    );
    assert_eq!(last(&dev.run(&["--install", "--quiet"]).await.1), TODAY);
    // A snapshot of ANOTHER boot says nothing about this one (the agent
    // has not run in this boot; when it does, it takes its snapshot from
    // config.txt as it is and reports no pending change either).
    let dev = Dev::new("pending-other-boot").await;
    dev.camera_lines("boot-0", WAS, NOW);
    assert_eq!(last(&dev.run(&["--install", "--quiet"]).await.1), TODAY);
    // A snapshot that is not a small regular JSON file, or no config.txt.
    let dev = Dev::new("pending-unreadable").await;
    dev.camera_lines(BOOT, WAS, NOW);
    std::fs::write(&dev.env.boot_lines, "{not json").unwrap();
    assert_eq!(last(&dev.run(&["--install", "--quiet"]).await.1), TODAY);
    let dev = Dev::new("pending-no-config").await;
    dev.camera_lines(BOOT, WAS, NOW);
    std::fs::remove_file(dev.env.boot_config.as_ref().unwrap()).unwrap();
    assert_eq!(last(&dev.run(&["--install", "--quiet"]).await.1), TODAY);

    // THE INSTALLER (`--from-claim --no-restart`, before the agent first
    // starts) keeps the line it replaces with its own: the question is not
    // asked there, whatever an earlier agent left on the device.
    let dev = Dev::new("pending-claim").await;
    dev.camera_lines(BOOT, WAS, NOW);
    let claim = dev.dir.join("claim.json");
    std::fs::write(
        &claim,
        json!({"cameraSensors": {"cameras": {"cam0": {"sensor": "imx335"}}}}).to_string(),
    )
    .unwrap();
    let (code, lines) = dev
        .run(&[
            "--install",
            "--no-restart",
            "--quiet",
            "--from-claim",
            &claim.display().to_string(),
        ])
        .await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        last(&lines),
        "camera packages: the camera appears when the agent starts next (or at its next camera re-listing, within 5 minutes); no reboot is needed"
    );
}

/// Bench finding (2026-11-22, bench 2): once the IMX335 had left the
/// device, the listing said only "no declared camera sensor needs extra
/// packages" while the Arducam packages stayed installed and held. The
/// listing now says what is still there and how to go back; the exit code
/// stays 0, and `--install` (so `update`) still does nothing.
#[tokio::test]
async fn packages_no_sensor_needs_any_more_are_still_listed() {
    let dev = Dev::new("leftover").await;
    assert_eq!(
        dev.run(&["--install", "--quiet", "--no-restart"]).await.0,
        0
    );
    // The sensor that needed them is gone.
    dev.settings(json!({"cameraSensors": {"cameras": {"cam0": {"sensor": "imx708"}, "cam1": {"sensor": "imx219"}}}}));
    for log in ["dpkg-query.log", "mark.log", "apt.log", "systemctl.log"] {
        dev.set(log, "");
    }
    let hits = dev.stub.hits();
    let record = dev.record();
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 0, "nothing is asked for");
    assert_eq!(
        lines,
        [
            "no declared camera sensor needs extra packages",
            "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), the set imx335 needs: no declared sensor needs it any more, but what xorgate-rs installed for it is still in place:",
            "  libcamera0.5   installed 0.5.2+rpt20250909-1  held  (installed by xorgate-rs; it replaced 0.5.2+rpt20250903-1)",
            "  libcamera-ipa  installed 0.5.2+rpt20250909-1  held  (installed by xorgate-rs; it replaced 0.5.2+rpt20250903-1)",
            "  while they are held, apt does not update these packages: the OS's own updates of them do not reach this device",
            "  to go back: sudo apt-mark unhold libcamera0.5 libcamera-ipa",
            "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
            "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
        ]
    );
    // It looked (dpkg and the holds, once each), and that is all.
    assert_eq!(dev.read("dpkg-query.log").matches("-W").count(), 1);
    assert_eq!(dev.read("mark.log"), "showhold\n");
    // --quiet prints nothing; --install and the update's call do nothing
    // and run no tool.
    dev.set("dpkg-query.log", "");
    dev.set("mark.log", "");
    assert_eq!(dev.run(&["--quiet"]).await, (0, vec![]));
    assert_eq!(
        dev.run(&["--install"]).await,
        (
            0,
            vec!["camera packages: no declared camera sensor needs extra packages".to_string()]
        )
    );
    assert_eq!(
        dev.run(&["--install", "--quiet", "--time-limit", "1320"])
            .await,
        (0, vec![])
    );
    assert_eq!(dev.read("dpkg-query.log"), "");
    assert_eq!(dev.read("mark.log"), "");
    assert_eq!(dev.read("apt.log"), "");
    assert_eq!(dev.read("systemctl.log"), "");
    assert_eq!(dev.stub.hits(), hits);
    assert_eq!(dev.record(), record);
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");

    // The hold released by hand: the packages are still said, without the
    // hold line and without an unhold.
    dev.set("held", "");
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 6, "{lines:?}");
    assert!(lines[2].contains("  not held  "), "{lines:?}");
    assert!(lines[4].starts_with("  to go back: sudo apt-get install -y --allow-downgrades "));
    // Gone back all the way: one line again, as on a device that never had
    // them.
    dev.installed(&[("libcamera0.5", STOCK), ("libcamera-ipa", STOCK)]);
    assert_eq!(
        dev.run(&[]).await,
        (
            0,
            vec!["no declared camera sensor needs extra packages".to_string()]
        )
    );
}

/// Review N1 + N6: the state directory is root's own real directory or it
/// is not used; a second run is kept out; what a killed run left behind is
/// swept at the start of the next.
#[tokio::test]
async fn the_state_directory_is_checked_locked_and_swept() {
    use std::os::unix::fs::PermissionsExt;
    // What a run that was killed (or cut by a power loss) left behind.
    let dev = Dev::new("sweep").await;
    let stale = dev.env.state_dir.join("download-Ab12Cd");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join(file_a()), b"half a file").unwrap();
    std::fs::set_permissions(&dev.env.state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Not a directory of that name: left alone (a link is never followed).
    let elsewhere = dev.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("keep"), b"x").unwrap();
    std::os::unix::fs::symlink(&elsewhere, dev.env.state_dir.join("download-link")).unwrap();
    let (code, lines) = dev.run(&["--install", "--quiet", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines[0],
        "camera packages: removed 1 download directory an earlier run left in <dir>/packages"
    );
    assert!(!stale.exists());
    assert!(elsewhere.join("keep").exists(), "the link was not followed");
    assert_eq!(dev.kept(), [RECORD_FILE, "download-link"]);
    // Also swept by a run that has nothing to install.
    std::fs::create_dir_all(&stale).unwrap();
    let (code, lines) = dev.run(&["--install", "--quiet"]).await;
    assert_eq!(
        (code, lines),
        (
            0,
            vec!["camera packages: removed 1 download directory an earlier run left in <dir>/packages".to_string()]
        )
    );
    // The listing does not sweep (it is not root's run) and says nothing.
    std::fs::create_dir_all(&stale).unwrap();
    let (code, _) = dev.run(&[]).await;
    assert_eq!(code, 0);
    assert!(stale.exists());

    // A second run while one holds the lock: kept out, nothing done.
    let dev = Dev::new("locked").await;
    std::fs::create_dir_all(&dev.env.state_dir).unwrap();
    std::fs::set_permissions(&dev.env.state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let other = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dev.env.state_dir.join(LOCK_FILE))
        .unwrap();
    other.lock().unwrap();
    assert_eq!(
        dev.run(&["--install"]).await,
        (
            1,
            vec!["camera packages: WARN nothing was installed: another `xorgate-rs camera requirements --install` is running on this device; nothing was changed".to_string()]
        )
    );
    assert_eq!((dev.stub.hits(), dev.read("apt.log")), (0, String::new()));
    drop(other);
    assert_eq!(
        dev.run(&["--install", "--quiet", "--no-restart"]).await.0,
        0
    );

    // The directory must be a REAL directory, owned by root, that nobody
    // else can write. Otherwise it is not used and nothing is installed.
    type Spoil = fn(&Dev);
    let cases: [(&str, Spoil, &str); 3] = [
        (
            "link",
            |d| {
                let real = d.dir.join("real-dir");
                std::fs::create_dir_all(&real).unwrap();
                std::os::unix::fs::symlink(&real, &d.env.state_dir).unwrap();
            },
            "it is a symbolic link",
        ),
        (
            "file",
            |d| std::fs::write(&d.env.state_dir, b"x").unwrap(),
            "it is not a directory",
        ),
        (
            "writable",
            |d| {
                std::fs::create_dir_all(&d.env.state_dir).unwrap();
                std::fs::set_permissions(&d.env.state_dir, std::fs::Permissions::from_mode(0o777))
                    .unwrap();
            },
            "its mode is 777",
        ),
    ];
    for (name, spoil, what) in cases {
        let dev = Dev::new(&format!("dir-{name}")).await;
        spoil(&dev);
        let (code, lines) = dev.run(&["--install"]).await;
        assert_eq!(code, 1, "{name}");
        assert_eq!(
            lines,
            [format!("camera packages: WARN nothing was installed: <dir>/packages is not a directory owned by root that only root can write ({what}); it is not used; nothing was changed")],
            "{name}"
        );
        assert_eq!(
            (dev.stub.hits(), dev.read("apt.log")),
            (0, String::new()),
            "{name}"
        );
    }
    // Another owner than the one required.
    let mut dev = Dev::new("dir-owner").await;
    std::fs::create_dir_all(&dev.env.state_dir).unwrap();
    std::fs::set_permissions(&dev.env.state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    dev.env.state_owner += 1;
    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 1);
    assert!(lines[0].contains("(its owner is uid "), "{lines:?}");
}

/// Review N2: an interrupted dpkg is seen BEFORE anything is downloaded,
/// and said with its remedy.
#[tokio::test]
async fn an_interrupted_dpkg_is_said_before_anything_is_downloaded() {
    // A package left half-installed (a power cut during an install).
    let dev = Dev::new("dpkg-half").await;
    dev.set(
        "installed.tsv",
        &format!("libcamera0.5\t{V}\tiU \nlibcamera-ipa\t{STOCK}\tii \nbash\t5.2\tii \n"),
    );
    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines,
        ["camera packages: WARN arducam-libcamera NOT installed: dpkg was interrupted on this device (libcamera0.5 is unpacked, not configured). Run `sudo dpkg --configure -a` there, then `sudo xorgate-rs camera requirements --install` again; nothing was downloaded or changed"]
    );
    assert_eq!((dev.stub.hits(), dev.read("apt.log")), (0, String::new()));
    assert_eq!(dev.read("mark.log"), "showhold\n");
    // The listing says it too.
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines.last().unwrap(),
        "dpkg was interrupted on this device (libcamera0.5 is unpacked, not configured): nothing can be installed until `sudo dpkg --configure -a` has run"
    );
    // ANY package left half-way counts (apt would refuse all the same),
    // and so does dpkg's journal of an unfinished run.
    let dev = Dev::new("dpkg-other").await;
    dev.set(
        "installed.tsv",
        &format!("libcamera0.5\t{STOCK}\tii \nlibcamera-ipa\t{STOCK}\tii \nfoo\t1\tiF \n"),
    );
    let (code, lines) = dev.run(&["--install", "--quiet"]).await;
    assert_eq!(code, 1);
    assert!(lines[0].contains("(foo is half-configured)"), "{lines:?}");
    let dev = Dev::new("dpkg-journal").await;
    std::fs::create_dir_all(&dev.env.dpkg_updates).unwrap();
    std::fs::write(dev.env.dpkg_updates.join("0001"), b"x").unwrap();
    let (code, lines) = dev.run(&["--install", "--quiet"]).await;
    assert_eq!(code, 1);
    assert!(
        lines[0].contains("(its journal of an unfinished run is not empty)"),
        "{lines:?}"
    );
    assert_eq!(dev.stub.hits(), 0);
    // Put right: the same command installs.
    std::fs::remove_file(dev.env.dpkg_updates.join("0001")).unwrap();
    assert_eq!(
        dev.run(&["--install", "--quiet", "--no-restart"]).await.0,
        0
    );
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
}

/// Review N3: the record stays truthful across a partial install. Run 1:
/// apt upgrades the first package and fails on the second. Run 2 installs
/// the second. The way back names the versions that were there BEFORE,
/// for both.
#[tokio::test]
async fn the_record_is_truthful_across_a_partial_install() {
    let dev = Dev::new("two-runs").await;
    dev.set("apt.mode", "install-partial");
    let (code, lines) = dev.run(&["--install", "--no-restart"]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines.last().unwrap(),
        "camera packages: WARN arducam-libcamera NOT installed: apt-get install failed (E: Sub-process /usr/bin/dpkg returned an error code (1)); the packages are now: libcamera0.5 0.5.2+rpt20250909-1, libcamera-ipa 0.5.2+rpt20250903-1 (check with `dpkg --audit`; if dpkg was interrupted, `sudo dpkg --configure -a`). imx335 (cam0) will not appear until this succeeds: run `sudo xorgate-rs camera requirements --install` again"
    );
    // What WAS replaced is on record at once, although the run failed.
    assert_eq!(
        dev.record()["sets"][SET],
        json!({"at": 1_795_000_000_000i64, "packages": [
            {"name": "libcamera0.5", "installed": V, "replaced": STOCK}
        ], "held": []})
    );
    assert_eq!(dev.read("held"), "", "no hold on a half-done set");
    // Run 2: only the second package is fetched and installed.
    dev.set("apt.mode", "ok");
    let hits = dev.stub.hits();
    let (code, lines) = dev.run(&["--install", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(dev.stub.hits(), hits + 1);
    assert_eq!(
        dev.record()["sets"][SET]["packages"],
        json!([
            {"name": "libcamera0.5", "installed": V, "replaced": STOCK},
            {"name": "libcamera-ipa", "installed": V, "replaced": STOCK}
        ])
    );
    let (_, lines) = dev.run(&[]).await;
    assert_eq!(
        lines[4..],
        [
            "  installed by xorgate-rs; it replaced: libcamera0.5 0.5.2+rpt20250903-1, libcamera-ipa 0.5.2+rpt20250903-1",
            "  to go back: sudo apt-mark unhold libcamera0.5 libcamera-ipa",
            "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
            "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
            "  (going back does not last while a port is set to a sensor that needs these packages: the next `xorgate-rs update` installs them again. Set the port to another sensor or to `none` first)",
        ]
    );
}

/// Review N4 + N7: after the transaction the whole dpkg database is read
/// back. A package outside the pinned set that changed is said loudly; a
/// database that cannot be read is "could not confirm", never "not
/// installed".
#[tokio::test]
async fn the_database_is_read_back_after_the_transaction() {
    let dev = Dev::new("outside").await;
    dev.set(
        "installed.tsv",
        &format!("libcamera0.5\t{STOCK}\tii \nlibcamera-ipa\t{STOCK}\tii \nbash\t5.2\tii \n"),
    );
    dev.set("apt.mode", "install-extra");
    let (code, lines) = dev.run(&["--install", "--no-restart"]).await;
    assert_eq!(code, 1, "{lines:?}");
    assert_eq!(
        lines[5..],
        [
            "camera packages: arducam-libcamera: installed libcamera0.5 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1), libcamera-ipa 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1)",
            "camera packages: arducam-libcamera: apt hold set on libcamera0.5, libcamera-ipa",
            "camera packages: WARN arducam-libcamera: the packages are installed, but packages outside the pinned set changed during the install: bash 5.2 (ii) -> 5.3 (ii)",
            "camera packages: the camera appears when the agent starts next (or at its next camera re-listing, within 5 minutes); no reboot is needed",
        ]
    );
    // The packages ARE installed, held and recorded.
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    assert_eq!(
        dev.record()["sets"][SET]["packages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // dpkg-query cannot answer after the install: apt's word is taken, the
    // hold and the record are written, and the line says what it is.
    let dev = Dev::new("confirm").await;
    dev.set("apt.mode", "confirm-fails");
    let (code, lines) = dev.run(&["--install", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines[5..7],
        [
            "camera packages: arducam-libcamera: installed libcamera0.5 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1), libcamera-ipa 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1) (apt-get reported success; dpkg-query could not confirm it within 30 s)",
            "camera packages: arducam-libcamera: apt hold set on libcamera0.5, libcamera-ipa",
        ]
    );
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    assert_eq!(
        dev.record()["sets"][SET],
        json!({"at": 1_795_000_000_000i64, "packages": [
            {"name": "libcamera0.5", "installed": V, "replaced": STOCK},
            {"name": "libcamera-ipa", "installed": V, "replaced": STOCK}
        ], "held": ["libcamera0.5", "libcamera-ipa"]})
    );
}

/// Review S1: with a time limit, nothing is started that could not finish,
/// and the transaction is started only when it fits. A killable step that
/// overruns is ended WITH what it started.
#[tokio::test]
async fn the_time_limit_never_cuts_a_transaction() {
    // Less than the transaction and what follows it (600 + 150 s): nothing
    // is even downloaded.
    for limit in ["0", "100", "751"] {
        let dev = Dev::new(&format!("limit-{limit}")).await;
        let (code, lines) = dev
            .run(&["--install", "--quiet", "--time-limit", limit])
            .await;
        assert_eq!(code, 1, "{limit}");
        let want = if limit == "0" {
            "camera packages: WARN nothing was looked at: not enough time left in this run; run `sudo xorgate-rs camera requirements --install`"
        } else {
            "camera packages: WARN arducam-libcamera NOT installed: not enough time left in this run; run `sudo xorgate-rs camera requirements --install` (the install is only started when it can finish; nothing was changed)"
        };
        assert_eq!(lines, [want], "{limit}");
        assert_eq!(
            (dev.stub.hits(), dev.read("apt.log")),
            (0, String::new()),
            "{limit}"
        );
        assert!(dev.kept().is_empty(), "{limit}");
    }
    // The command's own worst case fits: it installs.
    let dev = Dev::new("limit-ok").await;
    let (code, _) = dev
        .run(&[
            "--install",
            "--quiet",
            "--no-restart",
            "--time-limit",
            "1320",
        ])
        .await;
    assert_eq!(code, 0);
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    // A hold alone needs no transaction: a short limit is enough.
    let dev = Dev::new("limit-hold").await;
    dev.installed(&[("libcamera0.5", V), ("libcamera-ipa", V)]);
    let (code, lines) = dev
        .run(&["--install", "--quiet", "--time-limit", "100"])
        .await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    // Nothing to do at all: quiet, whatever the limit.
    assert_eq!(
        dev.run(&["--install", "--quiet", "--time-limit", "100"])
            .await,
        (0, vec![])
    );

    // A simulation that hangs is ended at its bound (here 6 s are left
    // before the transaction would have to start), together with the
    // process it started; nothing was changed.
    let dev = Dev::new("limit-hang").await;
    dev.set("apt.mode", "sim-hang");
    let t0 = std::time::Instant::now();
    let (code, lines) = dev
        .run(&["--install", "--quiet", "--time-limit", "756"])
        .await;
    assert_eq!(code, 1);
    let last = lines.last().unwrap();
    assert!(
        last.starts_with("camera packages: WARN arducam-libcamera NOT installed: apt-get could not plan the install (it did not finish within ")
            && last.contains(" s and was stopped); nothing was changed."),
        "{last}"
    );
    assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());
    let bg: i32 = dev.read("bg.pid").trim().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{bg}")).exists()
            || std::fs::read_to_string(format!("/proc/{bg}/stat"))
                .is_ok_and(|s| s.contains(") Z ")),
        "the simulation's own process {bg} is still running"
    );
    assert_eq!(
        dev.read("apt.log").lines().count(),
        1,
        "the real install never ran"
    );
    assert!(dev.kept().is_empty());
}

/// A finished install is finished when apt-get has exited, whatever a
/// package's script left running with apt's output still open: it must not
/// be taken for an install that hangs (and then be "stopped" at its bound).
#[tokio::test]
async fn a_process_left_behind_by_the_install_does_not_hold_it_up() {
    let dev = Dev::new("lingers").await;
    dev.set("apt.mode", "install-lingers");
    let t0 = std::time::Instant::now();
    let (code, lines) = dev.run(&["--install", "--quiet", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert!(t0.elapsed() < Duration::from_secs(12), "{:?}", t0.elapsed());
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    assert!(
        lines
            .iter()
            .any(|l| l.contains(": installed libcamera0.5 ")),
        "{lines:?}"
    );

    // The install's bound runs out while only its OUTPUT is still being
    // collected (apt-get itself exited long before): a tool that has
    // finished is not "timed out" and is not signalled (review F6).
    let mut dev = Dev::new("lingers-bound").await;
    dev.set("apt.mode", "install-lingers");
    dev.env.install_bound = Duration::from_millis(1000);
    let (code, lines) = dev.run(&["--install", "--quiet", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains("did not finish")),
        "{lines:?}"
    );
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
}

/// Another process holding the dpkg lock (`fcntl`), as a running apt or
/// dpkg does. `None` where there is no perl to hold it with.
struct LockHolder(std::process::Child);

impl LockHolder {
    fn hold(path: &Path) -> Option<Self> {
        use std::io::BufRead;
        if !Path::new("/usr/bin/perl").exists() {
            eprintln!("SKIPPED the dpkg-lock part: no perl to hold the lock with");
            return None;
        }
        // struct flock on 64-bit Linux: l_type, l_whence, pad, l_start, l_len, l_pid, pad.
        let script = r#"use Fcntl; open(my $f, ">", $ARGV[0]) or die "open: $!"; my $l = pack("s s x4 q q i x4", F_WRLCK, 0, 0, 0, 0); fcntl($f, F_SETLK, $l) or die "lock: $!"; $| = 1; print "locked\n"; sleep 120;"#;
        let mut child = xorgate_hal::child::std_command("/usr/bin/perl")
            .args(["-e", script])
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "locked\n", "the lock holder");
        Some(Self(child))
    }
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Review F5: while another package operation RUNS, dpkg's journal is not
/// empty and packages sit half-installed: the signs of an interrupted
/// dpkg, but passing ones. The root command asks the dpkg lock and says
/// which it is.
#[tokio::test]
async fn a_running_package_operation_is_not_called_an_interrupted_dpkg() {
    let dev = Dev::new("busy").await;
    dev.set(
        "installed.tsv",
        &format!("libcamera0.5\t{STOCK}\tii \nlibcamera-ipa\t{STOCK}\tii \nfoo\t1\tiU \n"),
    );
    std::fs::create_dir_all(&dev.env.dpkg_updates).unwrap();
    std::fs::write(dev.env.dpkg_updates.join("0003"), b"x").unwrap();
    // Nobody holds the lock (the lock files do not even exist): interrupted.
    let (code, lines) = dev.run(&["--install", "--quiet"]).await;
    assert_eq!(code, 1);
    assert!(
        lines[0].contains("dpkg was interrupted on this device (foo is unpacked, not configured)"),
        "{lines:?}"
    );
    let Some(holder) = LockHolder::hold(&dev.env.dpkg_locks[1]) else {
        return;
    };
    assert_eq!(
        xorgate_hal::camera_host::dpkg_lock_held(&dev.env.dpkg_locks),
        Some(true)
    );
    let (code, lines) = dev.run(&["--install", "--quiet"]).await;
    assert_eq!(
        (code, lines),
        (
            1,
            vec!["camera packages: WARN arducam-libcamera NOT installed: another package operation is running on this device; try again when it has finished (nothing was downloaded or changed)".to_string()]
        )
    );
    assert_eq!((dev.stub.hits(), dev.read("apt.log")), (0, String::new()));
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines.last().unwrap(),
        "another package operation is running on this device; try again when it has finished"
    );
    // The operation ends and the signs are still there: THEN it is an
    // interrupted dpkg.
    drop(holder);
    assert_eq!(
        xorgate_hal::camera_host::dpkg_lock_held(&dev.env.dpkg_locks),
        Some(false)
    );
    let (_, lines) = dev.run(&[]).await;
    assert!(
        lines
            .last()
            .unwrap()
            .starts_with("dpkg was interrupted on this device ("),
        "{lines:?}"
    );
    // No lock file can be opened: it cannot be told.
    assert_eq!(
        xorgate_hal::camera_host::dpkg_lock_held(&[dev.dir.join("no-such-lock")]),
        None
    );
}

/// The download directory is never removed while the real install may
/// still be alive. An apt-get that had to be stopped at its bound (the
/// last resort) leaves the files where they are: its dpkg, which runs in
/// a session of its own, may still be reading them. A later run removes
/// them, but not while a package operation holds the dpkg lock.
#[tokio::test]
async fn the_files_of_an_install_that_had_to_be_stopped_are_left_for_the_sweep() {
    let mut dev = Dev::new("bound").await;
    dev.set("apt.mode", "install-hangs");
    dev.env.install_bound = Duration::from_millis(1500);
    let t0 = std::time::Instant::now();
    let (code, lines) = dev.run(&["--install", "--quiet", "--no-restart"]).await;
    assert_eq!(code, 1);
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    assert_eq!(
        lines.last().unwrap(),
        "camera packages: WARN arducam-libcamera NOT installed: apt-get install did not finish within 1 s and was stopped (the one case in which a running install is ended); its files are left in <dir>/packages until a later run removes them, in case dpkg is still at work; nothing was changed. imx335 (cam0) will not appear until this succeeds: run `sudo xorgate-rs camera requirements --install` again"
    );
    // The "dpkg" (its own session) outlived the stop of apt-get's group…
    let dpkg: i32 = dev.read("dpkg.pid").trim().parse().unwrap();
    assert!(
        Path::new(&format!("/proc/{dpkg}")).exists(),
        "the stand-in dpkg is gone"
    );
    // …and the files it may be reading are still there.
    let left: Vec<String> = dev
        .kept()
        .into_iter()
        .filter(|n| n.starts_with("download-"))
        .collect();
    assert_eq!(left.len(), 1, "{:?}", dev.kept());
    let dir = dev.env.state_dir.join(&left[0]);
    assert!(dir.join(file_a()).exists() && dir.join(file_b()).exists());

    // A later run while a package operation still holds the dpkg lock: the
    // directory is NOT swept.
    dev.set("apt.mode", "ok");
    dev.env.install_bound = Duration::from_secs(pk::T_INSTALL);
    if let Some(holder) = LockHolder::hold(&dev.env.dpkg_locks[0]) {
        let (code, lines) = dev.run(&["--install", "--quiet", "--no-restart"]).await;
        assert_eq!(code, 0, "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("removed ")), "{lines:?}");
        assert!(
            dir.join(file_a()).exists(),
            "swept under a running package operation"
        );
        drop(holder);
    }
    // The lock is free: the next run removes it.
    let (code, lines) = dev.run(&["--install", "--quiet", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines[0],
        "camera packages: removed 1 download directory an earlier run left in <dir>/packages"
    );
    assert!(!dir.exists());
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(dpkg),
        nix::sys::signal::Signal::SIGKILL,
    );
}

/// Review F6: a stop request during a LATER set does not skip the agent's
/// restart for a set installed earlier in the same run.
#[tokio::test]
async fn a_stop_during_a_later_set_keeps_the_restart_for_an_earlier_one() {
    let mut dev = Dev::new("two-sets").await;
    // A second package set, needed by the imx219 on cam1; its one file
    // never arrives.
    let c: &[u8] = b"a third tiny package\n";
    let mut cat = dev.env.catalog.clone();
    cat.package_sets
        .push(xorgate_core::camera_catalog::PackageSet {
            id: "extra-set".into(),
            label: "a second set".into(),
            apt_hold: false,
            targets: vec![PackageTarget {
                os: "bookworm".into(),
                arch: "arm64".into(),
                packages: vec![Package {
                    name: "libfoo9".into(),
                    version: "1.0".into(),
                    url: format!("{BASE}libfoo9_1.0_arm64.deb"),
                    sha256: sha(c),
                    bytes: c.len() as u64,
                }],
            }],
        });
    cat.sensors
        .iter_mut()
        .find(|s| s.id == "imx219")
        .unwrap()
        .package_set = Some("extra-set".into());
    dev.env.catalog = Box::leak(Box::new(cat));
    let (stop, hits) = (dev.stop.clone(), dev.stub.hits.clone());
    tokio::spawn(async move {
        // Once the second set's download has begun (the third request).
        for _ in 0..500 {
            if hits.load(std::sync::atomic::Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = stop.send(true);
    });
    let (code, lines) = dev.run(&["--install"]).await;
    assert_eq!(code, 1, "{lines:?}");
    let n = lines.len();
    assert_eq!(
        lines[n - 3..],
        [
            "camera packages: extra-set: downloading libfoo9 1.0 (21 bytes)",
            "camera packages: WARN extra-set NOT installed: stopped on request before the install began; nothing was changed",
            "camera packages: the agent was restarted so the camera is listed again; no reboot is needed",
        ],
        "{lines:?}"
    );
    // The first set IS installed and held, and the agent was restarted.
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
    assert_eq!(dev.read("systemctl.log"), "try-restart xorgate-agent-rs\n");
}

/// Review S1: a stop request (SIGTERM, Ctrl-C) ends a killable step at
/// once and leaves nothing; during the transaction it is noted and the
/// command WAITS: a running apt-get install is never interrupted.
#[tokio::test]
async fn a_stop_request_ends_a_download_and_waits_for_a_transaction() {
    // During a download (the server never answers).
    let dev = Dev::new("stop-download").await;
    *dev.mode.lock().unwrap() = "hang";
    let stop = dev.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = stop.send(true);
    });
    let t0 = std::time::Instant::now();
    let (code, lines) = dev.run(&["--install", "--quiet"]).await;
    assert_eq!(code, 1);
    assert_eq!(
        lines.last().unwrap(),
        "camera packages: WARN arducam-libcamera NOT installed: stopped on request before the install began; nothing was changed"
    );
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
    assert_eq!(dev.read("apt.log"), "");
    assert!(dev.kept().is_empty(), "{:?}", dev.kept());

    // During the transaction (apt-get takes 2 s here).
    let dev = Dev::new("stop-install").await;
    dev.set("apt.mode", "install-slow");
    let (stop, state) = (dev.stop.clone(), dev.state.clone());
    tokio::spawn(async move {
        // Once the real install has begun (its environment file exists).
        for _ in 0..200 {
            if state.join("apt.env").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = stop.send(true);
    });
    let (code, lines) = dev.run(&["--install", "--no-restart"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines[4..],
        [
            "camera packages: arducam-libcamera: installing libcamera0.5, libcamera-ipa with apt-get (never interrupted once begun)",
            "camera packages: a stop was requested: waiting for apt-get to finish its transaction, which is never interrupted",
            "camera packages: arducam-libcamera: installed libcamera0.5 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1), libcamera-ipa 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1)",
            "camera packages: arducam-libcamera: apt hold set on libcamera0.5, libcamera-ipa",
            "camera packages: the camera appears when the agent starts next (or at its next camera re-listing, within 5 minutes); no reboot is needed",
        ]
    );
    assert_eq!(
        dev.read("installed.tsv"),
        format!("libcamera0.5\t{V}\tii \nlibcamera-ipa\t{V}\tii \n")
    );
    assert_eq!(dev.read("held"), "libcamera0.5\nlibcamera-ipa\n");
}

/// THE REAL TOOLS: two tiny `.deb` files built here with `dpkg-deb`, served
/// by the local server, installed by the real `apt-get` (simulation, then
/// the transaction), held by the real `apt-mark`, read back by the real
/// `dpkg-query`. Only as root inside a container (the gate's cargo
/// container, which is thrown away); everything is removed again.
#[tokio::test]
async fn real_apt_installs_two_tiny_packages_and_holds_them() {
    let tools = [
        "/usr/bin/dpkg-deb",
        "/usr/bin/apt-get",
        "/usr/bin/apt-mark",
        "/usr/bin/dpkg-query",
        "/usr/bin/dpkg",
    ];
    let in_container = Path::new("/.dockerenv").exists();
    if !nix::unistd::geteuid().is_root()
        || !in_container
        || tools.iter().any(|t| !Path::new(t).exists())
    {
        eprintln!("SKIPPED real_apt_installs_two_tiny_packages_and_holds_them: needs root, a container and the Debian tools");
        return;
    }
    let dir = scratch("real");
    let arch = xorgate_hal::camera_host::package_arch().expect("a Debian architecture");
    let os = xorgate_core::camera_sensors::os_codename(
        &std::fs::read_to_string("/etc/os-release").unwrap(),
    )
    .expect("VERSION_CODENAME");
    let (name_a, name_b) = ("xorgate-test-pkg-a", "xorgate-test-pkg-b");
    let sh = |cmd: &str, args: &[&str]| {
        let out = xorgate_hal::child::std_command(cmd)
            .args(args)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{cmd} {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // Start clean (a rerun in the same container).
    let _ = xorgate_hal::child::std_command("/usr/bin/apt-mark")
        .args(["unhold", name_a, name_b])
        .output();
    let _ = xorgate_hal::child::std_command("/usr/bin/dpkg")
        .args(["--purge", name_a, name_b])
        .output();
    let build = |name: &str, version: &str| -> Vec<u8> {
        let root = dir.join(format!("{name}-{version}"));
        std::fs::create_dir_all(root.join("DEBIAN")).unwrap();
        std::fs::create_dir_all(root.join(format!("usr/share/{name}"))).unwrap();
        std::fs::write(root.join(format!("usr/share/{name}/version")), version).unwrap();
        std::fs::write(
            root.join("DEBIAN/control"),
            format!("Package: {name}\nVersion: {version}\nArchitecture: {arch}\nMaintainer: xorgate tests <nobody@example.invalid>\nDescription: a tiny test package\n"),
        )
        .unwrap();
        let deb = dir.join(format!("{name}_{version}_{arch}.deb"));
        sh(
            "/usr/bin/dpkg-deb",
            &[
                "--build",
                "--root-owner-group",
                root.to_str().unwrap(),
                deb.to_str().unwrap(),
            ],
        );
        std::fs::read(&deb).unwrap()
    };
    // "Stock": an older version of package A is installed; B is not.
    build(name_a, "0.9");
    sh(
        "/usr/bin/dpkg",
        &[
            "-i",
            dir.join(format!("{name_a}_0.9_{arch}.deb"))
                .to_str()
                .unwrap(),
        ],
    );
    let (deb_a, deb_b) = (build(name_a, "1.0-1"), build(name_b, "1.0-1"));
    let (file_a, file_b) = (
        format!("{name_a}_1.0-1_{arch}.deb"),
        format!("{name_b}_1.0-1_{arch}.deb"),
    );
    let bodies = Arc::new([
        (format!("/{file_a}"), deb_a.clone()),
        (format!("/{file_b}"), deb_b.clone()),
    ]);
    let served = bodies.clone();
    let stub = serve(
        move |path, _| match served.iter().find(|(p, _)| p == path) {
            Some((_, body)) => Reply::ok(body),
            None => Reply::status(404),
        },
    )
    .await;
    let catalog = test_catalog(
        &os,
        &arch,
        &[
            (name_a, "1.0-1", file_a.clone(), &deb_a[..]),
            (name_b, "1.0-1", file_b.clone(), &deb_b[..]),
        ],
    );
    std::fs::write(
        dir.join("settings.json"),
        json!({"cameraSensors": {"cameras": {"cam0": {"sensor": "imx335"}}}}).to_string(),
    )
    .unwrap();
    // No agent to restart in a container: a systemctl that says so.
    write_exec(
        &dir.join("systemctl"),
        "echo \"$*\" >> \"$(dirname \"$0\")/systemctl.log\"; exit 0",
    );
    let to = stub.url("/");
    let mut env = PkgEnv::from_process();
    env.catalog = catalog;
    env.systemctl = dir.join("systemctl");
    env.state_dir = dir.join("packages");
    env.settings = dir.join("settings.json");
    env.url_map = Some(Box::new(move |url: &str| url.replacen(BASE, &to, 1)));
    env.now_ms = 7;
    assert!(env.is_root);
    assert_eq!(env.apt_get, Path::new("/usr/bin/apt-get"));

    let run_cmd = |args: &'static [&'static str]| {
        let env = &env;
        async move {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let mut lines = Vec::new();
            let code = run(&parse(&args).unwrap(), env, &mut |l| lines.push(l)).await;
            (code, lines)
        }
    };
    // Before: A is at 0.9, B is not installed.
    let (code, lines) = run_cmd(&[]).await;
    assert_eq!(code, 1, "{lines:?}");
    assert!(
        lines[1].ends_with("required 1.0-1  installed 0.9  MISSING"),
        "{lines:?}"
    );
    assert!(
        lines[2].ends_with("required 1.0-1  installed -  MISSING"),
        "{lines:?}"
    );

    let (code, lines) = run_cmd(&["--install"]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines[4..],
        [
            format!("camera packages: arducam-libcamera: installing {name_a}, {name_b} with apt-get (never interrupted once begun)"),
            format!("camera packages: arducam-libcamera: installed {name_a} 1.0-1 (was 0.9), {name_b} 1.0-1 (was not installed)"),
            format!("camera packages: arducam-libcamera: apt hold set on {name_a}, {name_b}"),
            "camera packages: the agent was restarted so the camera is listed again; no reboot is needed".to_string(),
        ],
        "{lines:?}"
    );
    // The real dpkg and apt-mark agree.
    let q = sh(
        "/usr/bin/dpkg-query",
        &[
            "-W",
            "-f=${Package} ${Version} ${db:Status-Abbrev}\n",
            name_a,
            name_b,
        ],
    );
    // `hi`: wanted = hold, status = installed. (This is what a held
    // package looks like to dpkg; `parse_dpkg_query` reads it as installed.)
    assert_eq!(q, format!("{name_a} 1.0-1 hi \n{name_b} 1.0-1 hi \n"));
    let held = sh("/usr/bin/apt-mark", &["showhold"]);
    assert!(
        held.lines().any(|l| l == name_a) && held.lines().any(|l| l == name_b),
        "{held}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(format!("{name_a}-1.0-1/usr/share/{name_a}/version")))
            .unwrap(),
        "1.0-1"
    );
    assert_eq!(
        std::fs::read_to_string(format!("/usr/share/{name_a}/version")).unwrap(),
        "1.0-1"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("systemctl.log")).unwrap(),
        "try-restart xorgate-agent-rs\n"
    );
    // The record: what was there before.
    let rec: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("packages").join(RECORD_FILE)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        rec["sets"][SET]["packages"],
        json!([
            {"name": name_a, "installed": "1.0-1", "replaced": "0.9"},
            {"name": name_b, "installed": "1.0-1", "replaced": null}
        ])
    );
    // No download directory is left; the record is the only file.
    let kept: Vec<String> = std::fs::read_dir(dir.join("packages"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != LOCK_FILE)
        .collect();
    assert_eq!(kept, [RECORD_FILE]);
    // Idempotent against the real tools: nothing to do, nothing fetched.
    let hits = stub.hits();
    assert_eq!(run_cmd(&["--install", "--quiet"]).await, (0, vec![]));
    assert_eq!(stub.hits(), hits);
    // A held package is installed OVER when the pin moves (a newer build
    // of the same set): `--allow-change-held-packages`.
    // (Covered by the arguments; here the hold is only checked to survive.)
    let (code, lines) = run_cmd(&[]).await;
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(lines[3], "  apt hold: set on every package");
    assert_eq!(
        lines[5],
        format!("  to go back: sudo apt-mark unhold {name_a} {name_b}")
    );

    // Clean up what this test installed.
    sh("/usr/bin/apt-mark", &["unhold", name_a, name_b]);
    sh("/usr/bin/dpkg", &["--purge", name_a, name_b]);
    let _ = std::fs::remove_dir_all(&dir);
}
