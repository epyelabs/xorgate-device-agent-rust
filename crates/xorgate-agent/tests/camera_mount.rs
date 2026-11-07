//! Camera-mount reconciler (Phase 4 WP3, I7) against
//! `fixtures/camera_mount/camera-mount.json`, dumped from the REAL TS
//! `reconcileCameraMount` @89b5c64 + the REAL `system/xorgate-camrotate`
//! (`wp3-tools/dump-camera-mount.mts`, node:20-bookworm, mawk):
//!
//! - `test/boot-config.test.ts` reconcileCameraMount ×14 (`:261 … :481`) + 8
//!   extras (every cloud-visible `detail` branch): same fixture device (config
//!   .txt, pipeline dir, fake device tree), the production
//!   [`HelperRotationWriter`] over a runner that EXECUTES the vendored
//!   `system/xorgate-camrotate` with `XORGATE_BOOT_CONFIG` (the TS test's
//!   `helperWriter`); per pass: state, log lines, probe calls, helper calls,
//!   config.txt bytes;
//! - the privileged writer ×6 (`:210 :216 :225 :233 :242 :252`) by executing
//!   the vendored helper directly;
//! - `CameraMountReconciler` (`worker.ts:171-200`): summary-change → report
//!   request, the failure catch;
//! - invariant 4: no reboot / power-off / systemctl identifier in
//!   `ops/camera_mount.rs`; `system/SHA256SUMS` matches the vendored files.
#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use xorgate_agent::ctx::BoxFut;
use xorgate_agent::ops::camera_mount::{
    reconcile_camera_mount, BootConfigPath, CameraMountReconciler, CameraMountReconcilerOptions,
    HelperRotationWriter, OverlayProbe, ReconcileCameraMountOptions, RotationResult,
    RotationWriter,
};
use xorgate_core::clock::{Clock, FixedClock};
use xorgate_core::clock_step::EventLevel;
use xorgate_core::commands::RunResult;
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::settings::CameraMountSettings;
use xorgate_core::types::CameraRotationDeg;
use xorgate_hal::boot_config_io::OverlayProbeResult;
use xorgate_hal::privileged::{HelperArgv, PrivilegedArgv, PrivilegedRunner, RunFut};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn helper() -> PathBuf {
    repo().join("system/xorgate-camrotate")
}

fn fixture() -> Value {
    let p = repo().join("fixtures/camera_mount/camera-mount.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn scratch(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "xg-bootcfg-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `device()` (`boot-config.test.ts:72-97`).
struct Device {
    dir: PathBuf,
    boot_config: PathBuf,
    pipeline_dir: PathBuf,
    dt_root: PathBuf,
}

impl Device {
    fn new(tag: &str, config: &str, in_force: &serde_json::Map<String, Value>) -> Self {
        let dir = scratch(tag);
        let boot_config = dir.join("config.txt");
        std::fs::write(&boot_config, config).unwrap();
        let pipeline_dir = dir.join("pipelines");
        std::fs::create_dir_all(&pipeline_dir).unwrap();
        let dt_root = dir.join("device-tree");
        for (key, deg) in in_force {
            let node = format!("/base/soc/i2c0mux/i2c@{}/imx290@1a", &key[3..]);
            std::fs::write(
                pipeline_dir.join(format!("{key}.camera")),
                format!("{node}\n"),
            )
            .unwrap();
            let node_dir = dt_root.join(node.replacen("/base/", "", 1));
            std::fs::create_dir_all(&node_dir).unwrap();
            let deg = deg.as_u64().unwrap() as u32;
            std::fs::write(node_dir.join("rotation"), deg.to_be_bytes()).unwrap();
        }
        Self {
            dir,
            boot_config,
            pipeline_dir,
            dt_root,
        }
    }
    fn read(&self) -> String {
        std::fs::read_to_string(&self.boot_config).unwrap()
    }
    fn redact(&self, s: &str) -> String {
        s.replace(self.dir.to_str().unwrap(), "<dir>")
    }
}

/// Runs `sudo -n /usr/local/bin/xorgate-camrotate …` as the vendored helper
/// itself with `XORGATE_BOOT_CONFIG` (the TS `helperWriter`; never sudo).
struct HelperExecRunner {
    config: PathBuf,
    calls: Mutex<Vec<Vec<String>>>,
}

impl PrivilegedRunner for HelperExecRunner {
    fn run<'a>(&'a self, _call: &'a PrivilegedArgv) -> RunFut<'a> {
        panic!("the camera mount never runs a command argv")
    }
    fn run_helper<'a>(&'a self, call: &'a HelperArgv) -> RunFut<'a> {
        self.calls.lock().unwrap().push(call.argv().to_vec());
        let argv = call.argv().to_vec();
        assert_eq!(
            argv[..3],
            ["sudo", "-n", "/usr/local/bin/xorgate-camrotate"]
        );
        let config = self.config.clone();
        Box::pin(async move { exec_helper(&argv[3..], &config).await })
    }
}

async fn exec_helper(args: &[String], config: &Path) -> RunResult {
    let o = tokio::process::Command::new(helper())
        .args(args)
        .env("XORGATE_BOOT_CONFIG", config)
        .output()
        .await
        .unwrap();
    RunResult {
        failed: !o.status.success(),
        not_found: false,
        exit_code: o.status.code().map(i64::from),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

struct FakeProbe {
    answer: OverlayProbeResult,
    calls: Mutex<Vec<String>>,
}
impl OverlayProbe for FakeProbe {
    fn probe<'a>(&'a self, sensor: &'a str) -> BoxFut<'a, OverlayProbeResult> {
        self.calls.lock().unwrap().push(sensor.to_string());
        let a = self.answer;
        Box::pin(async move { a })
    }
}

/// Records `[sensor, cam, deg]`, delegates to the helper writer or answers a
/// fixed result (the TS scenario's `applyRotation`).
struct Writer {
    inner: Option<HelperRotationWriter>,
    fixed: Option<RotationResult>,
    calls: Mutex<Vec<Value>>,
}
impl RotationWriter for Writer {
    fn apply<'a>(
        &'a self,
        sensor: &'a str,
        cam_token: &'a str,
        deg: CameraRotationDeg,
    ) -> BoxFut<'a, RotationResult> {
        self.calls
            .lock()
            .unwrap()
            .push(serde_json::json!([sensor, cam_token, deg.degrees()]));
        Box::pin(async move {
            match (&self.inner, &self.fixed) {
                (Some(w), _) => w.apply(sensor, cam_token, deg).await,
                (None, Some(r)) => r.clone(),
                _ => unreachable!(),
            }
        })
    }
}

fn level_str(l: EventLevel) -> &'static str {
    match l {
        EventLevel::Info => "info",
        EventLevel::Warn => "warn",
    }
}

#[tokio::test]
async fn reconcile_camera_mount_matches_the_ts_pass_by_pass() {
    let f = fixture();
    let k = &f["constants"];
    let config_txt = k["CONFIG_TXT"].as_str().unwrap();
    let hardware: DeviceHardwareBlock = serde_json::from_value(k["HARDWARE"].clone()).unwrap();
    let now = k["NOW"].as_i64().unwrap();
    let cases = f["reconcile"].as_array().unwrap();
    assert_eq!(cases.len(), 22); // 14 it() + 8 extras
    let mut passes_total = 0;
    for (ci, c) in cases.iter().enumerate() {
        let name = c["name"].as_str().unwrap();
        let empty = serde_json::Map::new();
        let d = Device::new(
            &format!("r{ci}"),
            c["config"].as_str().unwrap_or(config_txt),
            c["inForce"].as_object().unwrap_or(&empty),
        );
        if c["chmod444"].as_bool() == Some(true) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&d.boot_config, std::fs::Permissions::from_mode(0o444))
                .unwrap();
        }
        let boot_config_path = match &c["bootConfigPath"] {
            Value::Null if c.get("bootConfigPath").is_some() => BootConfigPath::Fixed(None),
            Value::String(s) if s == "missing" => {
                BootConfigPath::Fixed(Some(d.dir.join("nope/config.txt").display().to_string()))
            }
            Value::String(s) if s == "dir" => {
                BootConfigPath::Fixed(Some(d.pipeline_dir.display().to_string()))
            }
            _ => BootConfigPath::Fixed(Some(d.boot_config.display().to_string())),
        };
        let settings: Option<CameraMountSettings> = match c.get("settings") {
            None | Some(Value::Null) => None,
            Some(v) => Some(serde_json::from_value(v.clone()).unwrap()),
        };
        let hw = (c["hardware"] == "HARDWARE").then(|| hardware.clone());
        let probe_answer = OverlayProbeResult {
            ok: c["probe"]["ok"].as_bool().unwrap(),
            supports: c["probe"]["supports"].as_bool().unwrap(),
        };
        let expected = c["passes"].as_array().unwrap();
        assert_eq!(expected.len() as u64, c["passCount"].as_u64().unwrap());
        for (pi, want) in expected.iter().enumerate() {
            passes_total += 1;
            let logs = Arc::new(Mutex::new(Vec::<Value>::new()));
            let l2 = logs.clone();
            let dir = d.dir.display().to_string();
            let probe = Arc::new(FakeProbe {
                answer: probe_answer,
                calls: Mutex::new(vec![]),
            });
            let runner = Arc::new(HelperExecRunner {
                config: d.boot_config.clone(),
                calls: Mutex::new(vec![]),
            });
            let writer = Arc::new(if c["apply"] == "helper" {
                Writer {
                    inner: Some(HelperRotationWriter {
                        runner: runner.clone(),
                    }),
                    fixed: None,
                    calls: Mutex::new(vec![]),
                }
            } else {
                Writer {
                    inner: None,
                    fixed: Some(RotationResult {
                        ok: c["apply"]["ok"].as_bool().unwrap(),
                        changed: c["apply"]["changed"].as_bool().unwrap(),
                        detail: c["apply"]["detail"].as_str().unwrap().to_string(),
                    }),
                    calls: Mutex::new(vec![]),
                }
            });
            let state = reconcile_camera_mount(ReconcileCameraMountOptions {
                settings: settings.clone(),
                hardware: hw.clone(),
                pipeline_dir: d.pipeline_dir.display().to_string(),
                boot_config_path: boot_config_path.clone(),
                dt_root: d.dt_root.clone(),
                now,
                log: Arc::new(move |level, msg| {
                    l2.lock().unwrap().push(serde_json::json!({
                        "level": level_str(level),
                        "msg": msg.replace(&dir, "<dir>"),
                    }))
                }),
                probe: probe.clone(),
                apply_rotation: writer.clone(),
            })
            .await;
            let state_json: Value = match &state {
                None => Value::Null,
                Some(s) => {
                    serde_json::from_str(&d.redact(&serde_json::to_string(s).unwrap())).unwrap()
                }
            };
            let ctx = format!("{name} — pass {pi}");
            assert_eq!(state_json, want["state"], "{ctx}: state");
            assert_eq!(
                Value::Array(logs.lock().unwrap().clone()),
                want["logs"],
                "{ctx}: logs"
            );
            assert_eq!(
                serde_json::to_value(probe.calls.lock().unwrap().clone()).unwrap(),
                want["probes"],
                "{ctx}: probes"
            );
            assert_eq!(
                Value::Array(writer.calls.lock().unwrap().clone()),
                want["applies"],
                "{ctx}: applies"
            );
            assert_eq!(
                d.read(),
                want["file"].as_str().unwrap(),
                "{ctx}: config.txt"
            );
            // Every helper run went through the minted argv, nothing else.
            for argv in runner.calls.lock().unwrap().iter() {
                assert_eq!(
                    argv[..3],
                    ["sudo", "-n", "/usr/local/bin/xorgate-camrotate"]
                );
            }
        }
        let _ = std::fs::remove_dir_all(&d.dir);
    }
    assert_eq!(passes_total, 26);
}

/// The privileged writer ×6 (`boot-config.test.ts:209-258`) — the vendored
/// helper run directly, as the TS test does (`helperWriter`).
#[tokio::test]
async fn the_vendored_privileged_writer_matches_the_ts_run() {
    let f = fixture();
    let config_txt = f["constants"]["CONFIG_TXT"].as_str().unwrap();
    let cases = f["writer"].as_array().unwrap();
    assert_eq!(cases.len(), 5);
    for (i, c) in cases.iter().enumerate() {
        let d = Device::new(&format!("w{i}"), config_txt, &serde_json::Map::new());
        let mut results = Vec::new();
        for call in c["calls"].as_array().unwrap() {
            let args: Vec<String> = call
                .as_array()
                .unwrap()
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    n => n.to_string(),
                })
                .collect();
            let r = exec_helper(&args, &d.boot_config).await;
            // `helperWriter` (`boot-config.test.ts:100-112`).
            let (ok, changed, detail) = if !r.failed {
                (true, true, String::new())
            } else if r.exit_code == Some(3) {
                (true, false, String::new())
            } else {
                (false, false, d.redact(r.stderr.trim()))
            };
            results.push(serde_json::json!({ "ok": ok, "changed": changed, "detail": detail }));
        }
        let name = c["name"].as_str().unwrap();
        assert_eq!(Value::Array(results), c["results"], "{name}: results");
        assert_eq!(d.read(), c["file"].as_str().unwrap(), "{name}: file");
        let bak = std::fs::read_to_string(d.boot_config.with_extension("txt.bak-camrotate")).ok();
        assert_eq!(
            bak.map(Value::String).unwrap_or(Value::Null),
            c["bak"],
            "{name}: .bak-camrotate"
        );
        let _ = std::fs::remove_dir_all(&d.dir);
    }
}

/// `boot-config.test.ts:252` "never contains a reboot", on the vendored file.
#[test]
fn the_vendored_helper_never_contains_a_reboot() {
    assert_eq!(fixture()["constants"]["helperNeverReboots"], true);
    let body = std::fs::read_to_string(helper()).unwrap();
    // /^\s*(sudo\s+)?(systemctl\s+reboot|reboot|shutdown)\b/m
    for line in body.lines() {
        let mut w: Vec<&str> = line.split_whitespace().collect();
        if w.first() == Some(&"sudo") {
            w.remove(0);
        }
        let word = |s: Option<&&str>, want: &str| {
            s.is_some_and(|t| {
                t.strip_prefix(want).is_some_and(|r| {
                    !r.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                })
            })
        };
        let bad = (w.first() == Some(&"systemctl") && word(w.get(1), "reboot"))
            || word(w.first(), "reboot")
            || word(w.first(), "shutdown");
        assert!(!bad, "reboot-shaped line in the helper: {line}");
    }
}

/// Invariant 4 (I7): the module's CODE (comments and string literals
/// stripped) names no reboot / power-off / systemctl identifier and never
/// builds a command argv — `run_helper` with a minted `HelperArgv` only.
#[test]
fn camera_mount_module_has_no_reboot_symbol() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ops/camera_mount.rs"),
    )
    .unwrap();
    let code = strip_comments_and_strings(&src);
    let forbidden = [
        "reboot",
        "poweroff",
        "power_off",
        "systemctl",
        "shutdown",
        "PrivilegedArgv",
        "execute_command",
        "DeviceCommandName",
        "Command",
        "exec",
        "run_argv",
    ];
    for tok in code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        let low = tok.to_ascii_lowercase();
        for f in forbidden {
            assert!(
                low != f.to_ascii_lowercase(),
                "forbidden identifier `{tok}` in ops/camera_mount.rs"
            );
        }
    }
    // The positive half: the one privileged path is there.
    assert!(code.contains("run_helper(&HelperArgv::from(&call))"));
    assert!(code.contains("camrotate_call("));
}

fn strip_comments_and_strings(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push_str("\"\"");
        } else if b[i] == b'\'' && b.get(i + 2) == Some(&b'\'') {
            i += 3; // a char literal like '_'
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// `system/SHA256SUMS` (= the JS repo's blobs @89b5c64) matches the files.
#[test]
fn system_sha256sums_match_the_vendored_files() {
    use sha2::{Digest, Sha256};
    let sums = std::fs::read_to_string(repo().join("system/SHA256SUMS")).unwrap();
    let mut n = 0;
    for line in sums.lines() {
        let (hex, name) = line.split_once("  ").unwrap();
        let bytes = std::fs::read(repo().join("system").join(name)).unwrap();
        let got: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(got, hex, "{name}");
        n += 1;
    }
    assert_eq!(n, 3);
}

/// `reconcileCameraMountNow` (`worker.ts:171-200`): the report is requested
/// when the summary moves — first answer, a verdict change — and not on a
/// settled re-pass; a panic in the pass is caught and the state kept.
#[tokio::test]
async fn reconciler_requests_a_report_only_when_the_summary_moves() {
    let f = fixture();
    let config_txt = f["constants"]["CONFIG_TXT"].as_str().unwrap();
    let mut in_force = serde_json::Map::new();
    in_force.insert("cam0".into(), Value::from(0));
    let d = Device::new("rec", config_txt, &in_force);
    let deg = Arc::new(Mutex::new(Some(180u32)));
    let panic_now = Arc::new(Mutex::new(false));
    let (dg, pn) = (deg.clone(), panic_now.clone());
    let runner = Arc::new(HelperExecRunner {
        config: d.boot_config.clone(),
        calls: Mutex::new(vec![]),
    });
    let reports = Arc::new(AtomicUsize::new(0));
    let r2 = reports.clone();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FixedClock::new(1_785_326_400_000, 0));
    let rec = CameraMountReconciler::new(
        CameraMountReconcilerOptions {
            settings: Box::new(move || {
                if *pn.lock().unwrap() {
                    panic!("settings exploded");
                }
                dg.lock().unwrap().map(|v| {
                    serde_json::from_value(
                        serde_json::json!({ "cameras": { "cam0": { "rotationDeg": v } } }),
                    )
                    .unwrap()
                })
            }),
            hardware: Box::new(|| None),
            pipeline_dir: d.pipeline_dir.display().to_string(),
            boot_config_path: BootConfigPath::Fixed(Some(d.boot_config.display().to_string())),
            dt_root: d.dt_root.clone(),
            probe: Arc::new(FakeProbe {
                answer: OverlayProbeResult {
                    ok: true,
                    supports: true,
                },
                calls: Mutex::new(vec![]),
            }),
            apply_rotation: Arc::new(HelperRotationWriter {
                runner: runner.clone(),
            }),
            log: Arc::new(|_, _| {}),
            clock,
        },
        Box::new(move || {
            r2.fetch_add(1, Ordering::SeqCst);
        }),
    );
    assert!(rec.state().is_none());
    rec.now().await; // null → pending-reboot
    assert_eq!(reports.load(Ordering::SeqCst), 1);
    assert!(rec.state().unwrap().reboot_required);
    rec.now().await; // settled: same summary (wrote/at ignored)
    rec.now().await;
    assert_eq!(reports.load(Ordering::SeqCst), 1);
    assert_eq!(runner.calls.lock().unwrap().len(), 1);
    *deg.lock().unwrap() = Some(0); // back to 0 → applied (in force 0)
    rec.now().await;
    assert_eq!(reports.load(Ordering::SeqCst), 2);
    assert_eq!(d.read(), config_txt);
    *panic_now.lock().unwrap() = true; // the TS catch: logged, state kept
    rec.now().await;
    assert_eq!(reports.load(Ordering::SeqCst), 2);
    assert!(rec.state().is_some());
    *panic_now.lock().unwrap() = false;
    *deg.lock().unwrap() = None; // namespace removed → null state → report
    rec.now().await;
    assert_eq!(reports.load(Ordering::SeqCst), 3);
    assert!(rec.state().is_none());
    let _ = std::fs::remove_dir_all(&d.dir);
}
