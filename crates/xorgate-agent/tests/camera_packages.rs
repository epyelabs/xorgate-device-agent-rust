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
use xorgate_agent::cli::camera_packages::{parse, run, Args, PkgEnv, RECORD_FILE};
use xorgate_cloud::fetch::FetchLimits;
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
}

const FAKE_DPKG_QUERY: &str = r#"echo "$*" >> "$D/dpkg-query.log"
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
for f in $files; do
  [ -r "$f" ] || { echo "E: cannot read $f" >&2; exit 100; }
  b=$(basename "$f" .deb); n=${b%%_*}; v=${b#*_}; v=${v%_*}
  awk -F'\t' -v n="$n" '$1 != n' "$D/installed.tsv" > "$D/installed.new" 2>/dev/null
  printf '%s\t%s\tii \n' "$n" "$v" >> "$D/installed.new"
  mv "$D/installed.new" "$D/installed.tsv"
  cp "$f" "$D/seen-$(basename "$f")"
done
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
            state_dir: dir.join("packages"),
            settings: dir.join("settings.json"),
            service: "xorgate-agent-rs".to_string(),
            limits: FetchLimits {
                connect: Duration::from_secs(5),
                read: Duration::from_secs(5),
                total: Duration::from_secs(20),
            },
            // The REAL policy: https and the pinned hosts only.
            redirect_ok: Box::new(pk::redirect_allowed),
            url_map: Some(Box::new(move |url: &str| url.replacen(BASE, &to, 1))),
            now_ms: 1_795_000_000_000,
        };
        let dev = Self {
            dir,
            state,
            stub,
            mode,
            env,
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
    /// download directory left behind).
    fn kept(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.env.state_dir)
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
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
            quiet: true
        })
    );
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
    let common = "-o DPkg::Lock::Timeout=120 install -y --no-install-recommends --allow-downgrades --allow-change-held-packages";
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
        json!({"v": 1, "sets": {"arducam-libcamera": {"at": 1_795_000_000_000i64, "hold": true, "packages": [
            {"name": "libcamera0.5", "installed": V, "replaced": STOCK, "wasHeld": false},
            {"name": "libcamera-ipa", "installed": V, "replaced": STOCK, "wasHeld": false}
        ]}}})
    );

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
            "  to go back: sudo apt-get install --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
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
            "camera packages: arducam-libcamera: installed libcamera-ipa 0.5.2+rpt20250909-1 (was 0.5.2+rpt20250903-1)",
            "camera packages: arducam-libcamera: apt hold set on libcamera0.5, libcamera-ipa",
            "camera packages: the camera appears when the agent starts next (or at its next camera re-listing, within 5 minutes); no reboot is needed",
        ]
    );
    // --no-restart: systemctl was not run.
    assert_eq!(dev.read("systemctl.log"), "");
    // What was "replaced" for the one that was already there is itself.
    assert_eq!(dev.record()["sets"][SET]["packages"][0]["replaced"], V);

    // Both installed by hand, no hold: only the hold is set. No download,
    // no apt-get, no restart, no record.
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
    assert!(dev.kept().is_empty());

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
        lines[5..],
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
    // The catalog pins bookworm/arm64 only.
    let mut dev = Dev::new("os").await;
    std::fs::write(&dev.env.os_release, "VERSION_CODENAME=trixie\n").unwrap();
    assert_eq!(
        dev.run(&["--install"]).await,
        (
            3,
            vec!["camera packages: WARN arducam-libcamera REFUSED: the catalog lists no packages of this set for trixie/arm64, so nothing can be installed on this device; imx335 (cam0) will not work here".to_string()]
        )
    );
    let (code, lines) = dev.run(&[]).await;
    assert_eq!(code, 3);
    assert_eq!(
        lines[1],
        "  REFUSED: the catalog lists no packages of this set for trixie/arm64; nothing can be installed on this device"
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
            {"name": name_a, "installed": "1.0-1", "replaced": "0.9", "wasHeld": false},
            {"name": name_b, "installed": "1.0-1", "replaced": null, "wasHeld": false}
        ])
    );
    // No download directory is left; the record is the only file.
    let kept: Vec<String> = std::fs::read_dir(dir.join("packages"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
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
