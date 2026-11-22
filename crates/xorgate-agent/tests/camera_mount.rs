//! Camera-mount reconciler (Phase 4 WP3, I7; reworked in Phase 1 of the
//! support-other-camera-models plan):
//!
//! - the TS golden `fixtures/camera_mount/camera-mount.json` (dumped from
//!   the REAL TS `reconcileCameraMount` @89b5c64, 22 cases, pass by pass)
//!   run through the Rust reconciler over the REAL `system/xorgate-camcfg`.
//!   Where the Rust agent answers differently BY DESIGN the case is named
//!   in `fixtures/camera_mount/rust-divergences.json` (accepted divergences
//!   D95-D104, D106); every other field of every case must equal the TS;
//! - the rule "device-tree rotation = the overlay's default + the mount" on
//!   the three benches' real `config.txt`: zero writes and zero helper calls
//!   on an IMX290 device, both rotations reachable on IMX708 / IMX219, an
//!   IMX335 that cannot rotate;
//! - B7: the port's line is not edited when the kernel and libcamera do not
//!   confirm its sensor, when two lines configure the port, or under a
//!   `[section]` the agent cannot evaluate;
//! - the update window (the helper not installed yet) and a failing write:
//!   one clear status, no loop;
//! - the JS agent's `system/xorgate-camrotate`, still shipped byte for byte,
//!   against the TS run (`writer` ×5);
//! - `CameraMountReconciler` (`worker.ts:171-200`): summary-change → report
//!   request, the failure catch;
//! - invariant 4: no reboot / power-off / systemctl identifier in
//!   `ops/camera_mount.rs`; `system/SHA256SUMS` matches the vendored files.
#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use xorgate_agent::ctx::BoxFut;
use xorgate_agent::ops::camera_mount::{
    reconcile_camera_mount, reconcile_cameras, BootConfigPath, BootLineStore,
    CameraMountReconciler, CameraMountReconcilerOptions, CameraStates, HelperRotationWriter,
    OverlayProbe, PortEvidence, ReconcileCameraMountOptions, RotationResult, RotationWriter,
    WriteMemo, HELPER_NOT_INSTALLED, RETRY_FAILED_WRITE_MS,
};
use xorgate_core::boot_config::CamCfgOp;
use xorgate_core::camera_catalog::catalog;
use xorgate_core::camera_truth::PortNode;
use xorgate_core::cameras::EnumeratedCamera;
use xorgate_core::clock::{Clock, FixedClock};
use xorgate_core::clock_step::EventLevel;
use xorgate_core::commands::RunResult;
use xorgate_core::hardware_block::DeviceHardwareBlock;
use xorgate_core::settings::CameraMountSettings;
use xorgate_core::types::{CameraMountState, CameraMountVerdict};
use xorgate_hal::boot_config_io::OverlayProbeResult;
use xorgate_hal::privileged::{HelperArgv, PrivilegedArgv, PrivilegedRunner, RunFut};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The JS agent's helper (still shipped; the Rust agent no longer calls it).
fn camrotate() -> PathBuf {
    repo().join("system/xorgate-camrotate")
}

/// The camera-line owner the Rust agent calls.
fn camcfg() -> PathBuf {
    repo().join("system/xorgate-camcfg")
}

fn fixture() -> Value {
    let p = repo().join("fixtures/camera_mount/camera-mount.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn divergences() -> Value {
    let p = repo().join("fixtures/camera_mount/rust-divergences.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn bench_config(name: &str) -> String {
    std::fs::read_to_string(repo().join("fixtures/camera_line").join(name)).unwrap()
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

const MODEL_CM4: &str = "Raspberry Pi Compute Module 4 Rev 1.1\0";
const MODEL_CM5: &str = "Raspberry Pi Compute Module 5 Rev 1.0\0";

/// The libcamera path of a camera on a CM5 port (`cameras.rs`: CAM0 is
/// `i2c@88000`, CAM1 is `i2c@70000`).
fn cm5_path(key: &str, node: &str) -> String {
    let bus = if key == "cam0" { "88000" } else { "70000" };
    format!("/base/axi/pcie@1000120000/rp1/i2c@{bus}/{node}")
}

/// `device()` (`boot-config.test.ts:72-97`), plus what the Rust reconciler
/// also reads: the board's model file and nodes of any sensor.
struct Device {
    dir: PathBuf,
    boot_config: PathBuf,
    pipeline_dir: PathBuf,
    dt_root: PathBuf,
}

impl Device {
    /// The TS fixture device: an `imx290@1a` node per camera of `in_force`,
    /// with that `rotation`; no model file.
    fn new(tag: &str, config: &str, in_force: &serde_json::Map<String, Value>) -> Self {
        let d = Self::bare(tag, config);
        for (key, deg) in in_force {
            let node = format!("/base/soc/i2c0mux/i2c@{}/imx290@1a", &key[3..]);
            d.camera(key, &node, Some(deg.as_u64().unwrap() as u32));
        }
        d
    }

    fn bare(tag: &str, config: &str) -> Self {
        let dir = scratch(tag);
        let boot_config = dir.join("config.txt");
        std::fs::write(&boot_config, config).unwrap();
        // The helper's `set` wants the overlay files; `rotate` does not.
        std::fs::create_dir_all(dir.join("overlays")).unwrap();
        let pipeline_dir = dir.join("pipelines");
        std::fs::create_dir_all(&pipeline_dir).unwrap();
        let dt_root = dir.join("device-tree");
        std::fs::create_dir_all(&dt_root).unwrap();
        Self {
            dir,
            boot_config,
            pipeline_dir,
            dt_root,
        }
    }

    /// A bench-shaped device: a model file and the given config.txt.
    fn bench(tag: &str, config: &str, model: &str) -> Self {
        let d = Self::bare(tag, config);
        std::fs::write(d.dt_root.join("model"), model).unwrap();
        d
    }

    /// The enumerated camera of a stream (`<key>.camera`) and its node in
    /// the live device tree; `rotation: None` = the node has NO `rotation`
    /// property.
    fn camera(&self, key: &str, name: &str, rotation: Option<u32>) {
        std::fs::write(
            self.pipeline_dir.join(format!("{key}.camera")),
            format!("{name}\n"),
        )
        .unwrap();
        let node_dir = self.dt_root.join(name.replacen("/base/", "", 1));
        std::fs::create_dir_all(&node_dir).unwrap();
        let file = node_dir.join("rotation");
        match rotation {
            Some(deg) => std::fs::write(file, deg.to_be_bytes()).unwrap(),
            None => {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.boot_config).unwrap()
    }
    fn redact(&self, s: &str) -> String {
        s.replace(self.dir.to_str().unwrap(), "<dir>")
    }
    fn md5(&self) -> String {
        md5_hex(&std::fs::read(&self.boot_config).unwrap())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Runs `sudo -n /usr/local/bin/xorgate-camcfg …` as the vendored helper
/// itself, against the device's files (never sudo). `fail` answers instead
/// of running (a missing grant).
struct HelperExecRunner {
    device_dir: PathBuf,
    calls: Mutex<Vec<Vec<String>>>,
    fail: Mutex<Option<RunResult>>,
}

impl HelperExecRunner {
    fn new(d: &Device) -> Arc<Self> {
        Arc::new(Self {
            device_dir: d.dir.clone(),
            calls: Mutex::new(vec![]),
            fail: Mutex::new(None),
        })
    }
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
    /// The helper arguments of every call (`rotate cam0 imx290 180`).
    fn asked(&self) -> Vec<String> {
        self.calls().iter().map(|c| c[3..].join(" ")).collect()
    }
}

impl PrivilegedRunner for HelperExecRunner {
    fn run<'a>(&'a self, _call: &'a PrivilegedArgv) -> RunFut<'a> {
        panic!("the camera mount never runs a command argv")
    }
    fn run_helper<'a>(&'a self, call: &'a HelperArgv) -> RunFut<'a> {
        let argv = call.argv().to_vec();
        // Every helper run goes through the minted argv, nothing else, and
        // never to the JS agent's helper.
        assert_eq!(argv[..3], ["sudo", "-n", "/usr/local/bin/xorgate-camcfg"]);
        assert_eq!(call.timeout(), Some(std::time::Duration::from_secs(20)));
        self.calls.lock().unwrap().push(argv.clone());
        let fail = self.fail.lock().unwrap().clone();
        let dir = self.device_dir.clone();
        Box::pin(async move {
            match fail {
                Some(r) => r,
                None => exec_camcfg(&argv[3..], &dir).await,
            }
        })
    }
}

async fn exec_camcfg(args: &[String], device_dir: &Path) -> RunResult {
    let o = tokio::process::Command::new("/bin/sh")
        .arg(camcfg())
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("XORGATE_BOOT_CONFIG", device_dir.join("config.txt"))
        .env(
            "XORGATE_CAMCFG_OVERLAYS",
            repo().join("system/xorgate-camcfg.overlays"),
        )
        .env("XORGATE_CAMCFG_MODEL", device_dir.join("device-tree/model"))
        .env("XORGATE_CAMCFG_LOCK", device_dir.join("lock"))
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

/// The JS agent's helper, run directly (`helperWriter`, the TS test).
async fn exec_camrotate(args: &[String], config: &Path) -> RunResult {
    let o = tokio::process::Command::new(camrotate())
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
    answer: Mutex<OverlayProbeResult>,
    calls: Mutex<Vec<String>>,
}
impl FakeProbe {
    fn new(answer: OverlayProbeResult) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            calls: Mutex::new(vec![]),
        })
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}
impl OverlayProbe for FakeProbe {
    fn probe<'a>(&'a self, overlay: &'a str) -> BoxFut<'a, OverlayProbeResult> {
        self.calls.lock().unwrap().push(overlay.to_string());
        let a = *self.answer.lock().unwrap();
        Box::pin(async move { a })
    }
}

const PROBE_OK: OverlayProbeResult = OverlayProbeResult {
    ok: true,
    supports: true,
    default_rotation: None,
};

/// Records `[overlay, cam, rotation]`, then delegates to the helper writer
/// or answers a fixed result (the TS scenario's `applyRotation`).
struct Writer {
    inner: Option<HelperRotationWriter>,
    fixed: Option<RotationResult>,
    calls: Mutex<Vec<Value>>,
}
impl RotationWriter for Writer {
    fn unavailable(&self) -> Option<String> {
        self.inner.as_ref().and_then(|w| w.unavailable())
    }
    fn apply(&self, op: CamCfgOp) -> BoxFut<'_, RotationResult> {
        // The TS scenario's `applyRotation(overlay, cam, rotation)` record.
        if let CamCfgOp::Rotate {
            port,
            overlay,
            rotation,
        } = &op
        {
            self.calls
                .lock()
                .unwrap()
                .push(json!([overlay, port.token(), rotation]));
        }
        Box::pin(async move {
            match (&self.inner, &self.fixed) {
                (Some(w), _) => w.apply(op).await,
                (None, Some(r)) => r.clone(),
                _ => unreachable!(),
            }
        })
    }
}

fn helper_writer(
    runner: &Arc<HelperExecRunner>,
    installed: &Arc<AtomicBool>,
) -> HelperRotationWriter {
    let i = installed.clone();
    HelperRotationWriter {
        runner: runner.clone(),
        catalog: catalog(),
        installed: Box::new(move || i.load(Ordering::SeqCst)),
    }
}

fn level_str(l: EventLevel) -> &'static str {
    match l {
        EventLevel::Info => "info",
        EventLevel::Warn => "warn",
    }
}

fn md5_hex(bytes: &[u8]) -> String {
    // The control bench's baseline is an md5 (`md5sum /boot/firmware/config.txt`).
    let o = std::process::Command::new("md5sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(bytes)?;
            c.wait_with_output()
        })
        .unwrap();
    String::from_utf8_lossy(&o.stdout)[..32].to_string()
}

#[tokio::test]
async fn the_ts_golden_pass_by_pass_with_the_named_divergences() {
    let f = fixture();
    let div = divergences();
    let k = &f["constants"];
    let config_txt = k["CONFIG_TXT"].as_str().unwrap();
    let hardware: DeviceHardwareBlock = serde_json::from_value(k["HARDWARE"].clone()).unwrap();
    let now = k["NOW"].as_i64().unwrap();
    let cases = f["reconcile"].as_array().unwrap();
    assert_eq!(cases.len(), 22); // 14 it() + 8 extras
    let named = div["cases"].as_object().unwrap();
    for name in named.keys() {
        assert!(
            cases.iter().any(|c| c["name"] == name.as_str()),
            "rust-divergences.json names a case the TS golden does not have: {name}"
        );
    }
    assert_eq!(named.len(), 11);
    let (mut passes_total, mut fields_differing) = (0, 0);
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
            default_rotation: None,
        };
        let expected = c["passes"].as_array().unwrap();
        assert_eq!(expected.len() as u64, c["passCount"].as_u64().unwrap());
        // One memo per case: the passes of a case are one agent run.
        let memo: Arc<Mutex<WriteMemo>> = Arc::default();
        let boot_lines = memory_boot_lines(&Arc::new(Mutex::new("boot-1".to_string())));
        for (pi, ts) in expected.iter().enumerate() {
            passes_total += 1;
            let logs = Arc::new(Mutex::new(Vec::<Value>::new()));
            let l2 = logs.clone();
            let dir = d.dir.display().to_string();
            let probe = FakeProbe::new(probe_answer);
            let runner = HelperExecRunner::new(&d);
            let installed = Arc::new(AtomicBool::new(true));
            let writer = Arc::new(if c["apply"] == "helper" {
                Writer {
                    inner: Some(helper_writer(&runner, &installed)),
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
                sensors: None,
                boot_lines: boot_lines.clone(),
                hardware: hw.clone(),
                pipeline_dir: d.pipeline_dir.display().to_string(),
                boot_config_path: boot_config_path.clone(),
                dt_root: d.dt_root.clone(),
                now,
                log: Arc::new(move |level, msg| {
                    l2.lock().unwrap().push(json!({
                        "level": level_str(level),
                        "msg": msg.replace(&dir, "<dir>"),
                    }))
                }),
                probe: probe.clone(),
                apply_rotation: writer.clone(),
                catalog: catalog(),
                // The TS has no such input: with none the B7 gate is open.
                evidence: Arc::new(PortEvidence::default),
                memo: memo.clone(),
            })
            .await;
            let state_json: Value = match &state {
                None => Value::Null,
                Some(s) => {
                    serde_json::from_str(&d.redact(&serde_json::to_string(s).unwrap())).unwrap()
                }
            };
            let got = json!({
                "state": state_json,
                "logs": Value::Array(logs.lock().unwrap().clone()),
                "applies": Value::Array(writer.calls.lock().unwrap().clone()),
                "file": d.read(),
            });
            let ctx = format!("{name} — pass {pi}");
            let over = named.get(name).map(|n| &n["passes"][pi]);
            for field in ["state", "logs", "applies", "file"] {
                match over.and_then(|o| o.get(field)) {
                    Some(rust) => {
                        assert_ne!(
                            rust, &ts[field],
                            "{ctx}: {field} is named as a divergence but equals the TS"
                        );
                        assert_eq!(&got[field], rust, "{ctx}: {field} (the named Rust answer)");
                        fields_differing += 1;
                    }
                    None => assert_eq!(got[field], ts[field], "{ctx}: {field} (must equal the TS)"),
                }
            }
            // The probe: only for an overlay outside the catalog (D96).
            let want_probes = over
                .and_then(|o| o.get("probes"))
                .cloned()
                .unwrap_or_else(|| json!([]));
            assert_eq!(json!(probe.calls()), want_probes, "{ctx}: probes");
            for call in runner.calls() {
                assert_eq!(call[3], "rotate", "{ctx}: the only verb this phase uses");
            }
        }
    }
    assert_eq!(passes_total, 26);
    // state ×6, logs ×9, applies ×5, file ×5 across the 11 named cases
    // (4 of them differ in their log only: D115, a rejection is logged once
    // per change, and a write back to the rotation in force needs no reboot).
    assert_eq!(fields_differing, 25);
}

// ---------------------------------------------------------------------------
// The rule on real devices (Rust-only tests; no TS counterpart)
// ---------------------------------------------------------------------------

/// A device under the Rust reconciler: the real helper, a recording probe,
/// the evidence of the B7 gate, one memo for all its passes.
struct Rig {
    d: Device,
    runner: Arc<HelperExecRunner>,
    probe: Arc<FakeProbe>,
    installed: Arc<AtomicBool>,
    logs: Arc<Mutex<Vec<String>>>,
    memo: Arc<Mutex<WriteMemo>>,
    hardware: Option<DeviceHardwareBlock>,
    evidence: PortEvidence,
    /// How often a pass asked for the evidence (the sysfs scan and the
    /// listing copy): only when a line is about to be edited.
    evidence_reads: Arc<AtomicUsize>,
    now: i64,
    /// The kernel's boot id as the pass sees it; [`Rig::reboot`] moves it.
    boot_id: Arc<Mutex<String>>,
    boot_lines: Arc<BootLineStore>,
}

const NOW: i64 = 1_785_326_400_000;

/// A [`BootLineStore`] kept in memory, reading the boot id from `id`.
fn memory_boot_lines(id: &Arc<Mutex<String>>) -> Arc<BootLineStore> {
    let id = id.clone();
    Arc::new(BootLineStore::new(
        None,
        Box::new(move || id.lock().unwrap().clone()),
    ))
}

/// Two cameras on a CM5 whose MODEL says imx290 on both ports (as every
/// device model does today): the sensor that counts is config.txt's.
fn two_camera_block() -> DeviceHardwareBlock {
    serde_json::from_value(json!({
        "v": 2, "platform": "rpi-cm5", "encoder": "sw-h264", "rails": true,
        "statusLed": null, "imu": "bno085",
        "video": [
            {"key": "cam0", "connector": "csi0", "sensor": "imx290", "codec": "h264"},
            {"key": "cam1", "connector": "csi1", "sensor": "imx290", "codec": "h264"}
        ]
    }))
    .unwrap()
}

fn listed(key: &str, sensor: &str, node: &str) -> EnumeratedCamera {
    EnumeratedCamera {
        camera_name: cm5_path(key, node),
        sensor: sensor.to_string(),
        connector: Some(if key == "cam0" { "csi0" } else { "csi1" }.to_string()),
    }
}

fn node(key: &str, node: &str, driver: Option<&str>) -> PortNode {
    PortNode {
        connector: if key == "cam0" { "csi0" } else { "csi1" }.to_string(),
        node: node.to_string(),
        driver: driver.map(str::to_string),
    }
}

impl Rig {
    fn new(d: Device) -> Self {
        let runner = HelperExecRunner::new(&d);
        let boot_id = Arc::new(Mutex::new("boot-1".to_string()));
        Self {
            d,
            runner,
            probe: FakeProbe::new(PROBE_OK),
            installed: Arc::new(AtomicBool::new(true)),
            logs: Arc::default(),
            memo: Arc::default(),
            hardware: Some(two_camera_block()),
            evidence: PortEvidence::default(),
            evidence_reads: Arc::default(),
            now: NOW,
            boot_lines: memory_boot_lines(&boot_id),
            boot_id,
        }
    }

    /// The device reboots: a new boot id (the caller moves the device tree
    /// to what the file now says, as the firmware would).
    fn reboot(&self) {
        let mut id = self.boot_id.lock().unwrap();
        *id = format!("{id}+");
    }

    /// One pass over both namespaces: `cameraMount` = `mounts` (`None` =
    /// the namespace is absent), `cameraSensors` = `sensors` (the
    /// namespace's `cameras` value; `None` = absent).
    async fn pass_both(
        &self,
        mounts: Option<&[(&str, u32)]>,
        sensors: Option<Value>,
    ) -> CameraStates {
        let settings: Option<CameraMountSettings> = mounts.map(|mounts| {
            let cameras: serde_json::Map<String, Value> = mounts
                .iter()
                .map(|(k, deg)| (k.to_string(), json!({ "rotationDeg": deg })))
                .collect();
            serde_json::from_value(json!({ "cameras": cameras })).unwrap()
        });
        let sensors = sensors.map(|cameras| {
            xorgate_core::settings::parse_camera_sensors_settings(&json!({ "cameras": cameras }))
                .expect("a cameras object")
        });
        reconcile_cameras(self.options(settings, sensors)).await
    }

    fn options(
        &self,
        settings: Option<CameraMountSettings>,
        sensors: Option<xorgate_core::settings::CameraSensorsSettings>,
    ) -> ReconcileCameraMountOptions {
        let (logs, dir) = (self.logs.clone(), self.d.dir.display().to_string());
        ReconcileCameraMountOptions {
            settings,
            sensors,
            boot_lines: self.boot_lines.clone(),
            hardware: self.hardware.clone(),
            pipeline_dir: self.d.pipeline_dir.display().to_string(),
            boot_config_path: BootConfigPath::Fixed(Some(self.d.boot_config.display().to_string())),
            dt_root: self.d.dt_root.clone(),
            now: self.now,
            log: Arc::new(move |level, msg| {
                logs.lock().unwrap().push(format!(
                    "{} {}",
                    level_str(level),
                    msg.replace(&dir, "<dir>")
                ))
            }),
            probe: self.probe.clone(),
            apply_rotation: Arc::new(helper_writer(&self.runner, &self.installed)),
            catalog: catalog(),
            evidence: {
                let (e, reads) = (self.evidence.clone(), self.evidence_reads.clone());
                Arc::new(move || {
                    reads.fetch_add(1, Ordering::SeqCst);
                    e.clone()
                })
            },
            memo: self.memo.clone(),
        }
    }

    /// One reconcile pass with `cameraMount` = `mounts` (stream key → deg).
    async fn pass(&self, mounts: &[(&str, u32)]) -> CameraMountState {
        self.pass_both(Some(mounts), None)
            .await
            .mount
            .expect("the namespace is present")
    }

    fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().clone()
    }
}

/// One camera of a state, as `(verdict, inForceDeg, bootDeg, detail)`.
fn cam(
    state: &CameraMountState,
    key: &str,
) -> (CameraMountVerdict, Option<f64>, Option<f64>, String) {
    let c = state
        .cameras
        .iter()
        .find(|c| c.stream_key == key)
        .unwrap_or_else(|| panic!("no {key} in {state:?}"));
    (
        c.verdict,
        c.in_force_deg,
        c.boot_deg,
        c.detail.clone().unwrap_or_default(),
    )
}

use CameraMountVerdict::{Applied, PendingReboot, Unconfirmed, Unsupported, WriteFailed};

/// THE CONTROL: bench 1, two token-less IMX290 lines. With mount 0 on both
/// cameras the agent writes NOTHING and calls NO helper: config.txt keeps
/// its md5 (`0180bbaa978d8ab4e4f610d13e2ad092`, read on the bench).
#[tokio::test]
async fn imx290_token_less_lines_with_mount_0_get_zero_writes() {
    const BENCH1_MD5: &str = "0180bbaa978d8ab4e4f610d13e2ad092";
    let config = bench_config("config-bench1-imx290.txt");
    assert_eq!(md5_hex(config.as_bytes()), BENCH1_MD5);
    // The bench as read on 2026-11-21: both nodes carry `rotation = 0`.
    // On other kernels the property is ABSENT on such a line; both read
    // "in force = the overlay's default" = mount 0.
    for rotation in [Some(0u32), None] {
        let d = Device::bench("b1", &config, MODEL_CM5);
        d.camera("cam0", &cm5_path("cam0", "imx290@1a"), rotation);
        d.camera("cam1", &cm5_path("cam1", "imx290@1a"), rotation);
        let mut rig = Rig::new(d);
        rig.evidence = PortEvidence {
            cameras: Some(vec![
                listed("cam0", "imx290", "imx290@1a"),
                listed("cam1", "imx290", "imx290@1a"),
            ]),
            relisted: false,
            nodes: Some(vec![
                node("cam0", "imx290@1a", Some("imx290")),
                node("cam1", "imx290@1a", Some("imx290")),
            ]),
        };
        for _ in 0..3 {
            let s = rig.pass(&[("cam0", 0), ("cam1", 0)]).await;
            for key in ["cam0", "cam1"] {
                assert_eq!(
                    cam(&s, key),
                    (Applied, Some(0.0), Some(0.0), String::new()),
                    "{rotation:?}"
                );
            }
            assert!(!s.wrote && !s.reboot_required);
        }
        assert_eq!(
            rig.runner.calls(),
            Vec::<Vec<String>>::new(),
            "zero helper calls"
        );
        assert_eq!(rig.probe.calls(), Vec::<String>::new(), "zero probes");
        assert_eq!(rig.logs(), Vec::<String>::new());
        assert_eq!(rig.d.md5(), BENCH1_MD5, "config.txt untouched");
        assert!(
            !rig.d.dir.join("config.txt.bak-camcfg").exists(),
            "no backup either"
        );
        // The gate does not matter when nothing needs writing: with no
        // evidence at all, or with a port that is not detected, still nothing.
        for evidence in [
            PortEvidence::default(),
            PortEvidence {
                cameras: Some(vec![]),
                relisted: true,
                nodes: Some(vec![]),
            },
        ] {
            rig.evidence = evidence;
            let s = rig.pass(&[("cam0", 0), ("cam1", 0)]).await;
            assert_eq!(cam(&s, "cam0").0, Applied);
        }
        assert_eq!(rig.runner.calls().len(), 0);
        assert_eq!(rig.d.md5(), BENCH1_MD5);
        assert_eq!(
            rig.evidence_reads.load(Ordering::SeqCst),
            0,
            "a pass that writes nothing does not scan sysfs"
        );
    }
}

/// An IMX290 line `…,rotation=180` with mount 180 gets zero writes, and an
/// IMX290 behaves as before in both directions (the mount IS the rotation).
#[tokio::test]
async fn imx290_keeps_behaving_as_before() {
    let config = bench_config("config-bench1-imx290.txt").replace(
        "dtoverlay=imx290,cam0\n",
        "dtoverlay=imx290,cam0,rotation=180\n",
    );
    let d = Device::bench("b1r", &config, MODEL_CM5);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(180));
    let rig = Rig::new(d);
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(180.0), Some(180.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), config);
    // 180 → 0: one write, the line now says rotation=0 (D98), pending.
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (PendingReboot, Some(180.0), Some(0.0), String::new())
    );
    assert!(s.wrote && s.reboot_required);
    assert_eq!(rig.runner.asked(), ["rotate cam0 imx290 0"]);
    assert_eq!(
        rig.d.read(),
        config.replace(
            "dtoverlay=imx290,cam0,rotation=180\n",
            "dtoverlay=imx290,cam0,rotation=0\n"
        )
    );
    assert_eq!(
        rig.logs(),
        ["info cam0: imx290/cam0 rotation set to 0° in <dir>/config.txt — REBOOT required before the image changes (the agent never reboots itself)"]
    );
    // After the reboot the kernel runs 0 (property 0, or no property).
    rig.d.camera("cam0", &cm5_path("cam0", "imx290@1a"), None);
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 1, "settled: no second call");
}

/// The CM4 with an IMX708 (`dtoverlay=imx708,cam0,rotation=180`, default
/// 180): the mount is how the camera is MOUNTED. 0 keeps `rotation=180`
/// (no write); 180 writes `rotation=0`; and back.
#[tokio::test]
async fn imx708_reaches_both_pictures() {
    let config = bench_config("config-cm4-imx708.txt");
    let d = Device::bench("cm4", &config, MODEL_CM4);
    let name = "/base/soc/i2c0mux/i2c@0/imx708@1a";
    d.camera("cam0", name, Some(180));
    let mut rig = Rig::new(d);
    rig.hardware = Some(
        serde_json::from_value(json!({
            "v": 2, "platform": "rpi-cm4", "encoder": "hw-h264", "rails": false,
            "statusLed": null, "imu": "bno085",
            "video": [{"key": "cam0", "connector": "csi0", "sensor": "imx290", "codec": "h264"}]
        }))
        .unwrap(),
    );
    rig.evidence = PortEvidence {
        cameras: Some(vec![EnumeratedCamera {
            camera_name: name.into(),
            sensor: "imx708".into(),
            connector: Some("csi0".into()),
        }]),
        relisted: false,
        nodes: Some(vec![node("cam0", "imx708@1a", Some("imx708"))]),
    };
    // Mount 0 = the overlay's default (180): the line already gives it.
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), config);
    // Mount 180: rotation 0.
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (PendingReboot, Some(0.0), Some(180.0), String::new())
    );
    assert!(s.wrote && s.reboot_required);
    assert_eq!(rig.runner.asked(), ["rotate cam0 imx708 0"]);
    assert_eq!(
        rig.d.read(),
        config.replace(
            "dtoverlay=imx708,cam0,rotation=180\n",
            "dtoverlay=imx708,cam0,rotation=0\n"
        )
    );
    assert_eq!(
        rig.logs(),
        ["info cam0: imx708/cam0 mount set to 180° (rotation=0 written: the imx708 overlay defaults to 180) in <dir>/config.txt — REBOOT required before the image changes (the agent never reboots itself)"]
    );
    assert_eq!(
        rig.probe.calls(),
        Vec::<String>::new(),
        "B1: the catalog, not the probe"
    );
    // The reboot: the device tree now says 0.
    rig.d.camera("cam0", name, Some(0));
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(180.0), Some(180.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 1);
    // And back to mount 0: rotation=180 again, the file as it was.
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(cam(&s, "cam0").0, PendingReboot);
    assert_eq!(
        rig.runner.asked(),
        ["rotate cam0 imx708 0", "rotate cam0 imx708 180"]
    );
    assert_eq!(rig.d.read(), config);

    // A token-less imx708 line is 180 too (bug B3): mount 0 writes nothing,
    // mount 180 writes rotation=0.
    let bare = config.replace(
        "dtoverlay=imx708,cam0,rotation=180\n",
        "dtoverlay=imx708,cam0\n",
    );
    let d = Device::bench("cm4b", &bare, MODEL_CM4);
    d.camera("cam0", name, Some(180));
    let rig = Rig::new(d);
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), bare);
    rig.pass(&[("cam0", 180)]).await;
    assert_eq!(
        rig.d.read(),
        bare.replace(
            "dtoverlay=imx708,cam0\n",
            "dtoverlay=imx708,cam0,rotation=0\n"
        )
    );
}

/// Bench 2: an IMX335 on CAM0 (cannot flip) and an IMX219 on CAM1
/// (`rotation=180`, default 180).
#[tokio::test]
async fn imx335_cannot_rotate_and_imx219_reaches_both_pictures() {
    let config = bench_config("config-bench2-imx335-imx219.txt");
    let d = Device::bench("b2", &config, MODEL_CM5);
    d.camera("cam0", &cm5_path("cam0", "imx335@1a"), Some(180));
    d.camera("cam1", &cm5_path("cam1", "imx219@10"), Some(180));
    let mut rig = Rig::new(d);
    rig.evidence = PortEvidence {
        cameras: Some(vec![
            listed("cam0", "imx335", "imx335@1a"),
            listed("cam1", "imx219", "imx219@10"),
        ]),
        relisted: false,
        nodes: Some(vec![
            node("cam0", "imx335@1a", Some("imx335")),
            node("cam1", "imx219@10", Some("imx219")),
        ]),
    };
    // Mount 0 on both: applied, nothing written.
    let s = rig.pass(&[("cam0", 0), ("cam1", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert_eq!(
        cam(&s, "cam1"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 0);
    // Mount 180 on both: the IMX335 says it cannot; the IMX219 is written.
    let s = rig.pass(&[("cam0", 180), ("cam1", 180)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (
            Unsupported,
            Some(0.0),
            Some(0.0),
            "the imx335 sensor cannot be rotated: mount the camera the right way up".to_string()
        )
    );
    assert_eq!(
        cam(&s, "cam1"),
        (PendingReboot, Some(0.0), Some(180.0), String::new())
    );
    assert_eq!(
        rig.runner.asked(),
        ["rotate cam1 imx219 0"],
        "no call for the IMX335"
    );
    assert_eq!(rig.probe.calls(), Vec::<String>::new());
    assert_eq!(
        rig.d.read(),
        config.replace(
            "dtoverlay=imx219,cam1,rotation=180\n",
            "dtoverlay=imx219,cam1,rotation=0\n"
        )
    );
    // Whatever an imx335 line says, mount 0 is what the sensor shows.
    for line in [
        "dtoverlay=imx335,cam0,rotation=0\n",
        "dtoverlay=imx335,cam0,rotation=180\n",
    ] {
        let cfg = config.replace("dtoverlay=imx335,cam0\n", line);
        let d = Device::bench("b2x", &cfg, MODEL_CM5);
        let rig = Rig::new(d);
        // No camera was enumerated on the port: the verdicts stand, and no
        // rotation is reported that nothing was read for.
        let s = rig.pass(&[("cam0", 0)]).await;
        assert_eq!(cam(&s, "cam0"), (Applied, None, None, String::new()));
        let s = rig.pass(&[("cam0", 180)]).await;
        assert_eq!(
            cam(&s, "cam0"),
            (
                Unsupported,
                None,
                None,
                "the imx335 sensor cannot be rotated: mount the camera the right way up"
                    .to_string()
            )
        );
        // With the camera there (device tree 0 or 180: it changes no pixel).
        rig.d
            .camera("cam0", &cm5_path("cam0", "imx335@1a"), Some(0));
        let s = rig.pass(&[("cam0", 0)]).await;
        assert_eq!(
            cam(&s, "cam0"),
            (Applied, Some(0.0), Some(0.0), String::new())
        );
        assert_eq!(rig.runner.calls().len(), 0);
        assert_eq!(rig.d.read(), cfg);
    }
}

/// B7: the port's line is edited only when the kernel and libcamera agree
/// that the line's sensor is what answered on the port.
#[tokio::test]
async fn b7_a_port_that_is_not_confirmed_is_not_edited() {
    // The line says imx708; the mount (180) would need a write.
    let config = "dtoverlay=vc4-kms-v3d\n[all]\ndtoverlay=imx708,cam0,rotation=180\n";
    let name = cm5_path("cam0", "imx708@1a");
    let confirmed = PortEvidence {
        cameras: Some(vec![listed("cam0", "imx708", "imx708@1a")]),
        relisted: false,
        nodes: Some(vec![node("cam0", "imx708@1a", Some("imx708"))]),
    };
    let blocked = |evidence: PortEvidence, want: &'static str| {
        let name = name.clone();
        async move {
            let d = Device::bench("b7", config, MODEL_CM5);
            d.camera("cam0", &name, Some(180));
            let mut rig = Rig::new(d);
            rig.evidence = evidence;
            for _ in 0..2 {
                let s = rig.pass(&[("cam0", 180)]).await;
                let (verdict, in_force, boot, detail) = cam(&s, "cam0");
                assert_eq!(verdict, Unsupported, "{want}");
                assert_eq!((in_force, boot), (Some(0.0), Some(0.0)));
                assert_eq!(
                    detail,
                    format!("the camera line for cam0 is left as it is, because the camera on this port is not confirmed: {want}"),
                );
                assert!(!s.wrote && !s.reboot_required);
            }
            assert_eq!(rig.runner.calls().len(), 0, "{want}");
            assert_eq!(rig.d.read(), config, "{want}");
            // The refusal is in the journal once, not once per pass (D115).
            assert_eq!(
                rig.logs(),
                [format!("warn cam0: rotation not applied — the camera line for cam0 is left as it is, because the camera on this port is not confirmed: {want}")]
            );
            // The same device once the port is confirmed: written.
            rig.evidence = confirmed_evidence();
            let s = rig.pass(&[("cam0", 180)]).await;
            assert_eq!(cam(&s, "cam0").0, PendingReboot, "{want}");
            assert_eq!(rig.runner.asked(), ["rotate cam0 imx708 0"]);
            // The agent's OWN pending rotation write (config.txt now says
            // rotation=0, the kernel still runs 180) does not trip the
            // gate: the overlay is the same, the port is still confirmed,
            // and the setting can be flipped back without a reboot.
            let s = rig.pass(&[("cam0", 0)]).await;
            assert_eq!(cam(&s, "cam0").0, Applied, "{want}");
            assert_eq!(
                rig.runner.asked(),
                ["rotate cam0 imx708 0", "rotate cam0 imx708 180"]
            );
            assert_eq!(rig.d.read(), config);
        }
    };
    fn confirmed_evidence() -> PortEvidence {
        PortEvidence {
            cameras: Some(vec![listed("cam0", "imx708", "imx708@1a")]),
            relisted: false,
            nodes: Some(vec![node("cam0", "imx708@1a", Some("imx708"))]),
        }
    }
    assert_eq!(
        format!("{confirmed:?}"),
        format!("{:?}", confirmed_evidence())
    );

    // not-detected: the driver did not bind (a wrong overlay, or no sensor).
    blocked(
        PortEvidence {
            cameras: Some(vec![]),
            relisted: true,
            nodes: Some(vec![node("cam0", "imx708@1a", None)]),
        },
        "csi0 is declared as imx708 by config.txt, but no imx708 answered on this port (the kernel driver did not bind). Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached, then reboot.",
    )
    .await;
    // not-detected: bound, nothing listed yet (the agent is re-checking).
    blocked(
        PortEvidence {
            cameras: Some(vec![]),
            relisted: false,
            nodes: Some(vec![node("cam0", "imx708@1a", Some("imx708"))]),
        },
        "The imx708 driver is bound on csi0, but libcamera did not list a camera there when the agent started. The agent is checking again.",
    )
    .await;
    // A stale overlay: config.txt was edited (imx290 → imx708), no reboot.
    blocked(
        PortEvidence {
            cameras: Some(vec![listed("cam0", "imx290", "imx290@1a")]),
            relisted: false,
            nodes: Some(vec![node("cam0", "imx290@1a", Some("imx290"))]),
        },
        "config.txt now declares imx708 on csi0, but the running kernel still has the imx290 overlay loaded. Reboot to apply the config.txt change.",
    )
    .await;
    // …the same where cameras cannot be listed: the kernel alone says so.
    blocked(
        PortEvidence {
            cameras: None,
            relisted: false,
            nodes: Some(vec![node("cam0", "imx290@1a", Some("imx290"))]),
        },
        "config.txt now declares imx708 on csi0, but the running kernel still has the imx290 overlay loaded. Reboot to apply the config.txt change.",
    )
    .await;
    blocked(
        PortEvidence {
            cameras: None,
            relisted: false,
            nodes: Some(vec![node("cam0", "imx708@1a", None)]),
        },
        "csi0 is declared as imx708 by config.txt, but no imx708 answered on this port (the kernel driver did not bind). Check the ribbon cable and that the dtoverlay line in config.txt names the sensor that is really attached, then reboot.",
    )
    .await;
    // The overlay is not loaded at all.
    blocked(
        PortEvidence {
            cameras: Some(vec![]),
            relisted: true,
            nodes: Some(vec![]),
        },
        "csi0 is declared as imx708 by config.txt, but that overlay is not loaded in the running kernel. Reboot if config.txt was edited; otherwise check the dtoverlay line.",
    )
    .await;

    // What does NOT block: cameras cannot be listed but the kernel has the
    // line's driver bound; or the agent cannot look at all (no evidence).
    for evidence in [
        PortEvidence {
            cameras: None,
            relisted: false,
            nodes: Some(vec![node("cam0", "imx708@1a", Some("imx708"))]),
        },
        PortEvidence::default(),
    ] {
        let d = Device::bench("b7ok", config, MODEL_CM5);
        d.camera("cam0", &name, Some(180));
        let mut rig = Rig::new(d);
        rig.evidence = evidence;
        let s = rig.pass(&[("cam0", 180)]).await;
        assert_eq!(cam(&s, "cam0").0, PendingReboot);
        assert_eq!(rig.runner.asked(), ["rotate cam0 imx708 0"]);
    }
}

/// Two camera lines on one port, and a line under a section the agent
/// cannot evaluate: reported, never edited. Under the board's own section
/// the line is the port's and is edited; the other board's line is not.
#[tokio::test]
async fn two_lines_and_sections() {
    let two = "dtoverlay=imx290,cam0\ndtoverlay=imx708,cam0\n";
    let rig = Rig::new(Device::bench("two", two, MODEL_CM5));
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (
            Unsupported,
            None,
            // The LAST line is the port's (Q5): imx708 with no token = mount 0.
            Some(0.0),
            "config.txt has 2 camera lines for cam0; the agent changes a rotation only when exactly one line configures the port: remove the stale ones, then reboot".to_string()
        )
    );
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), two);
    // Two lines that both already give the wanted rotation need no write.
    let agree = "dtoverlay=imx290,cam0\ndtoverlay=imx708,cam0,rotation=180\n";
    let rig = Rig::new(Device::bench("two2", agree, MODEL_CM5));
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(cam(&s, "cam0").0, Unconfirmed);
    assert_eq!(rig.runner.calls().len(), 0);

    let gpio = "[gpio4=1]\ndtoverlay=imx290,cam0\n[all]\n";
    let rig = Rig::new(Device::bench("gpio", gpio, MODEL_CM5));
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(
        cam(&s, "cam0").3,
        "the camera line for cam0 in <dir>/config.txt sits under [gpio4=1], a section the agent cannot evaluate for this board: it is left as it is; edit it by hand"
            .replace("<dir>", rig.d.dir.to_str().unwrap())
    );
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), gpio);

    // The classic two-section file on each board.
    let text = "[cm4]\ndtoverlay=imx708,cam0\n[cm5]\ndtoverlay=imx290,cam0\n[all]\n";
    let rig = Rig::new(Device::bench("sec5", text, MODEL_CM5));
    rig.pass(&[("cam0", 180)]).await;
    assert_eq!(rig.runner.asked(), ["rotate cam0 imx290 180"]);
    assert_eq!(
        rig.d.read(),
        "[cm4]\ndtoverlay=imx708,cam0\n[cm5]\ndtoverlay=imx290,cam0,rotation=180\n[all]\n"
    );
    let rig = Rig::new(Device::bench("sec4", text, MODEL_CM4));
    rig.pass(&[("cam0", 180)]).await;
    assert_eq!(rig.runner.asked(), ["rotate cam0 imx708 0"]);
    assert_eq!(
        rig.d.read(),
        "[cm4]\ndtoverlay=imx708,cam0,rotation=0\n[cm5]\ndtoverlay=imx290,cam0\n[all]\n"
    );
    // No model file: neither section is known to apply.
    let rig = Rig::new(Device::bare("sec0", text));
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(cam(&s, "cam0").0, Unsupported);
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), text);
}

/// "Change only the rotation token": through the reconciler and the real
/// helper, every other token survives verbatim and in order.
#[tokio::test]
async fn every_other_token_of_the_line_is_preserved() {
    let line =
        "dtoverlay=imx290,clock-frequency=74250000,cam0,4lane,media-controller=0,future-thing";
    let config = format!("# head\n[all]\n{line}\ndtoverlay=imx219\n# tail");
    let rig = Rig::new(Device::bench("tok", &config, MODEL_CM5));
    rig.pass(&[("cam0", 180), ("cam1", 180)]).await;
    assert_eq!(
        rig.d.read(),
        format!("# head\n[all]\n{line},rotation=180\ndtoverlay=imx219,rotation=0\n# tail")
    );
    assert_eq!(
        rig.runner.asked(),
        ["rotate cam0 imx290 180", "rotate cam1 imx219 0"]
    );
    // A token the helper could not write back: the agent refuses the line
    // itself and never runs the helper.
    let odd = "dtoverlay=imx290,cam0,x=a b\n";
    let rig = Rig::new(Device::bench("odd", odd, MODEL_CM5));
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(cam(&s, "cam0").0, Unsupported);
    assert!(cam(&s, "cam0").3.ends_with(
        "cannot be rewritten: \"x=a b\" holds characters the camera line owner does not write"
    ));
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), odd);
}

/// The update window: this build runs, the helper is not installed yet
/// (it arrives with the next `xorgate-rs update`). One clear status, one
/// warning, no sudo call, however many passes; then it heals by itself.
#[tokio::test]
async fn the_update_window_is_a_clear_status_not_a_loop() {
    let config = bench_config("config-bench1-imx290.txt");
    let d = Device::bench("win", &config, MODEL_CM5);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    d.camera("cam1", &cm5_path("cam1", "imx290@1a"), Some(0));
    let rig = Rig::new(d);
    rig.installed.store(false, Ordering::SeqCst);
    // An IMX290 device that needs no write behaves exactly as before.
    let s = rig.pass(&[("cam0", 0), ("cam1", 0)]).await;
    assert_eq!(cam(&s, "cam0").0, Applied);
    assert_eq!(cam(&s, "cam1").0, Applied);
    assert_eq!(rig.logs(), Vec::<String>::new());
    // One that needs a write says what is missing.
    for _ in 0..5 {
        let s = rig.pass(&[("cam0", 180), ("cam1", 0)]).await;
        assert_eq!(
            cam(&s, "cam0"),
            (
                WriteFailed,
                Some(0.0),
                Some(0.0),
                format!(
                    "could not edit {}: {HELPER_NOT_INSTALLED}",
                    rig.d.boot_config.display()
                )
            )
        );
        assert_eq!(cam(&s, "cam1").0, Applied);
        assert!(!s.wrote && !s.reboot_required);
    }
    assert_eq!(
        HELPER_NOT_INSTALLED,
        "the xorgate-camcfg helper is not installed on this device yet — run `sudo xorgate-rs update` once more"
    );
    assert_eq!(
        rig.runner.calls().len(),
        0,
        "sudo is never run for a helper that is not there"
    );
    assert_eq!(
        rig.logs(),
        [format!(
            "warn cam0: boot config rotation edit failed — {HELPER_NOT_INSTALLED}"
        )],
        "one warning, not one per pass"
    );
    assert_eq!(rig.d.read(), config);
    // The helper arrives: the next pass writes.
    rig.installed.store(true, Ordering::SeqCst);
    let s = rig.pass(&[("cam0", 180), ("cam1", 0)]).await;
    assert_eq!(cam(&s, "cam0").0, PendingReboot);
    assert_eq!(rig.runner.asked(), ["rotate cam0 imx290 180"]);
}

/// A write that fails (the helper is there, the sudoers grant is not) is
/// reported and NOT repeated every 60 s: once per 10 minutes, or at once
/// when what is asked changes.
#[tokio::test]
async fn a_failing_write_is_not_retried_every_pass() {
    let config = bench_config("config-bench1-imx290.txt");
    let d = Device::bench("fail", &config, MODEL_CM5);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    let mut rig = Rig::new(d);
    *rig.runner.fail.lock().unwrap() = Some(RunResult {
        failed: true,
        not_found: false,
        exit_code: Some(1),
        stdout: String::new(),
        stderr: "sudo: a password is required\n".into(),
    });
    let want = format!(
        "could not edit {}: sudo: a password is required (the agent needs the xorgate-camcfg helper — run `sudo xorgate-rs update`)",
        rig.d.boot_config.display()
    );
    for minute in 0..10 {
        rig.now = NOW + minute * 60_000;
        let s = rig.pass(&[("cam0", 180)]).await;
        assert_eq!(
            cam(&s, "cam0"),
            (WriteFailed, Some(0.0), Some(0.0), want.clone())
        );
    }
    assert_eq!(
        rig.runner.calls().len(),
        1,
        "ten passes in ten minutes: one sudo call"
    );
    assert_eq!(
        rig.logs(),
        ["warn cam0: boot config rotation edit failed — sudo: a password is required"]
    );
    // Ten minutes on: one more try.
    rig.now = NOW + RETRY_FAILED_WRITE_MS;
    rig.pass(&[("cam0", 180)]).await;
    assert_eq!(rig.runner.calls().len(), 2);
    // The request changes (cam1 now): tried at once, per stream.
    rig.pass(&[("cam0", 180), ("cam1", 180)]).await;
    assert_eq!(
        rig.runner.asked(),
        [
            "rotate cam0 imx290 180",
            "rotate cam0 imx290 180",
            "rotate cam1 imx290 180"
        ]
    );
    // The grant arrives: the remembered failure does not outlive it by
    // more than the retry time, and a success clears it.
    *rig.runner.fail.lock().unwrap() = None;
    rig.now += RETRY_FAILED_WRITE_MS;
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(cam(&s, "cam0").0, PendingReboot);
    assert_eq!(
        rig.d.read(),
        config.replace(
            "dtoverlay=imx290,cam0\n",
            "dtoverlay=imx290,cam0,rotation=180\n"
        )
    );
    assert_eq!(config.len() + ",rotation=180".len(), rig.d.read().len());

    // The update hint is for sudo's own refusals (no grant, no helper). A
    // refusal by the helper itself is reported as the helper worded it:
    // an update would change nothing.
    let d = Device::bench("refused", &config, MODEL_CM5);
    let rig = Rig::new(d);
    for (stderr, hinted) in [
        ("sudo: a password is required\n", true),
        (
            "sudo: /usr/local/bin/xorgate-camcfg: command not found\n",
            true,
        ),
        (
            "2 camera lines configure cam0; remove the stale ones first (/boot/firmware/config.txt not changed)\n",
            false,
        ),
        ("could not keep a backup of /boot/firmware/config.txt beside it; not written\n", false),
        ("another xorgate-camcfg is still running\n", false),
    ] {
        *rig.runner.fail.lock().unwrap() = Some(RunResult {
            failed: true,
            not_found: false,
            exit_code: Some(5),
            stdout: String::new(),
            stderr: stderr.into(),
        });
        *rig.memo.lock().unwrap() = WriteMemo::default();
        let s = rig.pass(&[("cam0", 180)]).await;
        let detail = cam(&s, "cam0").3;
        let hint = " (the agent needs the xorgate-camcfg helper — run `sudo xorgate-rs update`)";
        assert_eq!(detail.ends_with(hint), hinted, "{stderr}");
        assert!(
            detail.starts_with(&format!(
                "could not edit {}: {}",
                rig.d.boot_config.display(),
                stderr.trim()
            )),
            "{detail}"
        );
    }
}

/// Q6: an overlay OUTSIDE the catalog is never rendered by the agent. Its
/// rotation token may be set when `dtoverlay -h` says the overlay takes
/// the parameter, with the default that help text states.
#[tokio::test]
async fn an_overlay_outside_the_catalog_is_rotated_only_through_the_probe() {
    let config = "dtoverlay=imx477,cam0\n";
    let name = cm5_path("cam0", "imx477@1a");
    let d = Device::bench("o477", config, MODEL_CM5);
    d.camera("cam0", &name, Some(180));
    let rig = Rig::new(d);
    *rig.probe.answer.lock().unwrap() = OverlayProbeResult {
        ok: true,
        supports: true,
        default_rotation: Some(180),
    };
    // Mount 0 = the default the help text states: no write.
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 0);
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(cam(&s, "cam0").0, PendingReboot);
    assert_eq!(rig.runner.asked(), ["rotate cam0 imx477 0"]);
    assert_eq!(rig.d.read(), "dtoverlay=imx477,cam0,rotation=0\n");
    assert_eq!(rig.probe.calls(), ["imx477"], "asked once, then remembered");

    // No default stated: 0 is assumed, as the TS always did.
    let d = Device::bench("o5647", "dtoverlay=ov5647,cam0\n", MODEL_CM5);
    let rig = Rig::new(d);
    rig.pass(&[("cam0", 180)]).await;
    assert_eq!(rig.d.read(), "dtoverlay=ov5647,cam0,rotation=180\n");

    // The probe cannot run: no write, and it says why (the TS sentence). It
    // is asked ONCE per agent run, not on every 60 s pass.
    let d = Device::bench("onop", config, MODEL_CM5);
    let rig = Rig::new(d);
    *rig.probe.answer.lock().unwrap() = OverlayProbeResult {
        ok: false,
        supports: false,
        default_rotation: None,
    };
    for _ in 0..3 {
        let s = rig.pass(&[("cam0", 180)]).await;
        assert_eq!(
            cam(&s, "cam0"),
            (
                Unsupported,
                None,
                Some(0.0),
                "could not ask dtoverlay whether imx477 supports rotation".to_string()
            )
        );
    }
    assert_eq!(
        rig.probe.calls(),
        ["imx477"],
        "a failed probe is remembered"
    );
    assert_eq!(rig.runner.calls().len(), 0);
    // While it fails the default is ASSUMED 0. A line that looks settled
    // under that assumption (no token, mount 0) is not written; and a
    // device tree that disagrees (an imx477 really runs 180) is not turned
    // into "REBOOT REQUIRED" on the strength of the assumption.
    rig.d.camera("cam0", &name, Some(180));
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (
            Unsupported,
            Some(180.0),
            Some(0.0),
            "could not ask dtoverlay whether imx477 supports rotation".to_string()
        )
    );
    assert!(!s.reboot_required);
    // One that agrees is reported as it is.
    rig.d.camera("cam0", &name, Some(0));
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert_eq!(rig.probe.calls(), ["imx477"]);
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), config);

    // The overlay has no rotation parameter: 0 is what it shows.
    let d = Device::bench("onorot", config, MODEL_CM5);
    let rig = Rig::new(d);
    *rig.probe.answer.lock().unwrap() = OverlayProbeResult {
        ok: true,
        supports: false,
        default_rotation: None,
    };
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (
            Unsupported,
            None,
            None,
            "the imx477 overlay on this device has no \"rotation\" parameter".to_string()
        )
    );
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(cam(&s, "cam0").0, Applied);
    assert_eq!(rig.runner.calls().len(), 0);
    // Without a port token such a line is nobody's.
    let d = Device::bench("otok", "dtoverlay=imx477\n", MODEL_CM5);
    let rig = Rig::new(d);
    let s = rig.pass(&[("cam1", 180)]).await;
    assert_eq!(cam(&s, "cam1").0, CameraMountVerdict::NoOverlay);
    assert_eq!(rig.probe.calls(), Vec::<String>::new());
}

/// The device tree, three ways: a value, a node WITHOUT the property (read
/// as the kernel reads it: rotation 0, never "could not read"), and a node
/// that is not there.
#[tokio::test]
async fn a_node_without_a_rotation_property_is_rotation_0() {
    let config = "dtoverlay=imx290,cam0\ndtoverlay=imx219,cam1\n";
    let d = Device::bench("dt", config, MODEL_CM5);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), None);
    d.camera("cam1", &cm5_path("cam1", "imx219@10"), None);
    let rig = Rig::new(d);
    let s = rig.pass(&[("cam0", 0), ("cam1", 0)]).await;
    // IMX290 (default 0): rotation 0 is mount 0, the case seen on a device.
    assert_eq!(
        cam(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    // A default-180 sensor whose node had no property would be running
    // rotation 0, which is mount 180: not "applied" at mount 0. (The stock
    // imx219 / imx708 / imx335 overlays always set the property.) Status
    // only: the line already gives the wanted rotation, nothing is written.
    assert_eq!(
        cam(&s, "cam1"),
        (PendingReboot, Some(180.0), Some(0.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), config);
    // The camera file names a node the live tree does not have (a stale
    // name after a sensor change): unconfirmed, as before.
    std::fs::remove_dir_all(rig.d.dt_root.join("axi")).unwrap();
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(
        cam(&s, "cam0"),
        (
            Unconfirmed,
            None,
            Some(0.0),
            "could not read the rotation currently in force from the device tree".to_string()
        )
    );
    assert_eq!(rig.runner.calls().len(), 0);
}

/// D106: which port a camera line WITHOUT a port token belongs to, and a
/// line carrying both tokens. The firmware's rule (`cam0` is the only port
/// selector): no `cam0` = CAM1; `cam0,cam1` = CAM0 only. The TS matched the
/// literal `camN` token, so the first was nobody's and the second both's.
#[tokio::test]
async fn a_catalog_line_without_a_port_token_is_cam1s() {
    let config = "dtoverlay=vc4-kms-v3d\ndtoverlay=imx290,clock-frequency=74250000\n";
    let d = Device::bench("notok", config, MODEL_CM5);
    d.camera("cam1", &cm5_path("cam1", "imx290@1a"), Some(0));
    let rig = Rig::new(d);
    // cam1: the line is its own (the TS said "no dtoverlay line for cam1").
    let s = rig.pass(&[("cam0", 0), ("cam1", 0)]).await;
    assert_eq!(
        cam(&s, "cam1"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    // cam0 has no line.
    assert_eq!(cam(&s, "cam0").0, CameraMountVerdict::NoOverlay);
    assert_eq!(rig.runner.calls().len(), 0);
    let s = rig.pass(&[("cam1", 180)]).await;
    assert_eq!(cam(&s, "cam1").0, PendingReboot);
    assert_eq!(rig.runner.asked(), ["rotate cam1 imx290 180"]);
    assert_eq!(
        rig.d.read(),
        "dtoverlay=vc4-kms-v3d\ndtoverlay=imx290,clock-frequency=74250000,rotation=180\n"
    );
    // `cam0,cam1` on one line: CAM0's only (the TS counted it for both).
    let both = "dtoverlay=imx290,cam0,cam1\n";
    let rig = Rig::new(Device::bench("both", both, MODEL_CM5));
    let s = rig.pass(&[("cam0", 180), ("cam1", 180)]).await;
    assert_eq!(cam(&s, "cam1").0, CameraMountVerdict::NoOverlay);
    assert_eq!(
        cam(&s, "cam1").3,
        format!(
            "no dtoverlay line for cam1 in {} — the camera overlay is missing",
            rig.d.boot_config.display()
        )
    );
    assert_eq!(rig.runner.asked(), ["rotate cam0 imx290 180"]);
    assert_eq!(rig.d.read(), "dtoverlay=imx290,cam0,cam1,rotation=180\n");
}

/// C2: a single `rotation=` token that reads as the wanted value through
/// the TS `Number()` is settled, however a hand spelled it: no write, as in
/// v0.1.1. A value that is not a number, another number, or two tokens are
/// rewritten.
#[tokio::test]
async fn a_hand_written_rotation_spelling_is_not_rewritten() {
    for (token, mount) in [
        ("rotation=0x0", 0),
        ("rotation=00", 0),
        ("rotation=0.0", 0),
        ("rotation=", 0),
        ("rotation=180.0", 180),
        ("rotation=1.8e2", 180),
    ] {
        let config = format!("dtoverlay=imx290,cam0,{token}\n");
        let d = Device::bench("spell", &config, MODEL_CM5);
        d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(mount));
        let rig = Rig::new(d);
        let s = rig.pass(&[("cam0", mount)]).await;
        assert_eq!(
            cam(&s, "cam0"),
            (
                Applied,
                Some(f64::from(mount)),
                Some(f64::from(mount)),
                String::new()
            ),
            "{token}"
        );
        assert_eq!(rig.runner.calls().len(), 0, "{token}");
        assert_eq!(rig.d.read(), config, "{token}");
    }
    for (line, mount, asked, want) in [
        (
            "dtoverlay=imx290,cam0,rotation=90",
            0,
            "rotate cam0 imx290 0",
            "dtoverlay=imx290,cam0,rotation=0",
        ),
        (
            "dtoverlay=imx290,cam0,rotation=upside-down",
            180,
            "rotate cam0 imx290 180",
            "dtoverlay=imx290,cam0,rotation=180",
        ),
        (
            "dtoverlay=imx290,rotation=0,cam0,rotation=0",
            0,
            "rotate cam0 imx290 0",
            "dtoverlay=imx290,rotation=0,cam0",
        ),
        // A spelling that reads as the OTHER value is rewritten too.
        (
            "dtoverlay=imx290,cam0,rotation=0x0",
            180,
            "rotate cam0 imx290 180",
            "dtoverlay=imx290,cam0,rotation=180",
        ),
    ] {
        let rig = Rig::new(Device::bench("respell", &format!("{line}\n"), MODEL_CM5));
        rig.pass(&[("cam0", mount)]).await;
        assert_eq!(rig.runner.asked(), [asked], "{line}");
        assert_eq!(rig.d.read(), format!("{want}\n"), "{line}");
    }
}

/// The JS agent's privileged writer ×5 (`boot-config.test.ts:209-258`): the
/// vendored `xorgate-camrotate` run directly, as the TS test does
/// (`helperWriter`). The Rust agent no longer calls it (D95); it is still
/// shipped and installed, byte for byte, for a device reverted to the JS
/// agent, and this keeps it honest.
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
            let r = exec_camrotate(&args, &d.boot_config).await;
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
    }
}

/// `boot-config.test.ts:252` "never contains a reboot", on the vendored file.
#[test]
fn the_vendored_helper_never_contains_a_reboot() {
    assert_eq!(fixture()["constants"]["helperNeverReboots"], true);
    let body = std::fs::read_to_string(camrotate()).unwrap();
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
    assert!(code.contains("camcfg_call("));
    // …and it is the camera-line owner: the JS agent's helper is not named.
    assert!(!code.contains("camrotate"));
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

/// `system/SHA256SUMS` matches the files: the three JS-shared ones (= the
/// JS repo's blobs @89b5c64, unchanged) and the Rust-only camera-line owner
/// with its allow-list.
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
    assert_eq!(n, 5);
    // The JS-shared helpers are the JS repo's bytes, still.
    for (name, hex) in [
        (
            "xorgate-camrotate",
            "9ae4f79b1764122a6ac2186d8feeb708f6d70e00830200b674e5a2bd57ccf8ac",
        ),
        (
            "xorgate-fake-hwclock.cron",
            "a05745a855e54ece2e7d38ff7448ef8c0b9bdd71f97f7d708c7c11581bf51999",
        ),
        (
            "xorgate-setclock",
            "d69de4d496ad1104f14b02f5ca46cedf4daea324398d01e415430401fe72bda1",
        ),
    ] {
        assert!(sums.contains(&format!("{hex}  {name}\n")), "{name}");
    }
    assert!(sums.contains("  xorgate-camcfg\n") && sums.contains("  xorgate-camcfg.overlays\n"));
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
    let runner = HelperExecRunner::new(&d);
    let installed = Arc::new(AtomicBool::new(true));
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
            sensors: Box::new(|| None),
            boot_lines: memory_boot_lines(&Arc::new(Mutex::new("boot-1".to_string()))),
            hardware: Box::new(|| None),
            pipeline_dir: d.pipeline_dir.display().to_string(),
            boot_config_path: BootConfigPath::Fixed(Some(d.boot_config.display().to_string())),
            dt_root: d.dt_root.clone(),
            probe: FakeProbe::new(PROBE_OK),
            apply_rotation: Arc::new(helper_writer(&runner, &installed)),
            catalog: catalog(),
            evidence: Box::new(|_| PortEvidence::default()),
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
    assert_eq!(runner.calls().len(), 1);
    *deg.lock().unwrap() = Some(0); // back to 0 → applied (in force 0)
    rec.now().await;
    assert_eq!(reports.load(Ordering::SeqCst), 2);
    // An explicit rotation=0 (D98), where the TS dropped the token.
    assert_eq!(
        d.read(),
        config_txt.replace(
            "dtoverlay=imx290,cam0,4lane\n",
            "dtoverlay=imx290,cam0,4lane,rotation=0\n"
        )
    );
    assert_eq!(
        runner.asked(),
        ["rotate cam0 imx290 180", "rotate cam0 imx290 0"]
    );
    *panic_now.lock().unwrap() = true; // the TS catch: logged, state kept
    rec.now().await;
    assert_eq!(reports.load(Ordering::SeqCst), 2);
    assert!(rec.state().is_some());
    *panic_now.lock().unwrap() = false;
    *deg.lock().unwrap() = None; // namespace removed → null state → report
    rec.now().await;
    assert_eq!(reports.load(Ordering::SeqCst), 3);
    assert!(rec.state().is_none());
}

// ---------------------------------------------------------------------------
// Phase 2: the sensor setting (`cameraSensors`), through the REAL helper
// ---------------------------------------------------------------------------

use xorgate_core::types::CameraSensorsVerdict as SV;

impl Device {
    /// The overlay files the helper's `set` wants to see on the device.
    fn overlays(&self, names: &[&str]) {
        for n in names {
            std::fs::write(self.dir.join("overlays").join(format!("{n}.dtbo")), b"").unwrap();
        }
    }

    /// A camera node in the LIVE device tree (with its `rotation`), and
    /// nothing else: the `<key>.camera` cache file is not touched.
    fn dt(&self, name: &str, rotation: u32) {
        let node_dir = self.dt_root.join(name.replacen("/base/", "", 1));
        std::fs::create_dir_all(&node_dir).unwrap();
        std::fs::write(node_dir.join("rotation"), rotation.to_be_bytes()).unwrap();
    }

    /// What `<key>.camera` holds: the name the hardware reconcile cached the
    /// last time a camera WAS listed on the port. Production never deletes
    /// it, so after a sensor change, a removal or a wrong declaration it is
    /// STALE. The reconciler must not conclude anything from it.
    fn cache(&self, key: &str) -> String {
        std::fs::read_to_string(self.pipeline_dir.join(format!("{key}.camera")))
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

impl Rig {
    /// What is on the ports NOW: the cameras this pass's listing has
    /// (`(key, listed sensor, node)`, each with its driver bound), which is
    /// also what the live device tree holds. The `<key>.camera` cache files
    /// stay as they are (stale), as on a device.
    fn live(&mut self, cams: &[(&str, &str, &str)]) {
        self.evidence = PortEvidence {
            cameras: Some(cams.iter().map(|(k, s, n)| listed(k, s, n)).collect()),
            relisted: false,
            nodes: Some(cams.iter().map(|(k, s, n)| node(k, n, Some(s))).collect()),
        };
    }
}

/// One port of the `cameraSensors` half, as `(verdict, detail)`.
fn sens(states: &CameraStates, key: &str) -> (SV, String) {
    let s = states.sensors.as_ref().expect("the namespace is present");
    let c = s
        .cameras
        .iter()
        .find(|c| c.stream_key == key)
        .unwrap_or_else(|| panic!("no {key} in {s:?}"));
    (c.verdict, c.detail.clone().unwrap_or_default())
}

/// The `cameraMount` half of one port.
fn mnt(states: &CameraStates, key: &str) -> (CameraMountVerdict, Option<f64>, Option<f64>, String) {
    cam(states.mount.as_ref().expect("cameraMount is present"), key)
}

const ALL_OVERLAYS: [&str; 4] = ["imx290", "imx219", "imx708", "imx335"];

/// THE ZERO-WRITE PROOF, Phase 2: a device whose config.txt already holds
/// the declared sensor's line gets NO write and NO helper call when it
/// becomes managed. That is every freshly provisioned device (the installer
/// wrote the line the claim seeded) and every bench after someone saves the
/// matching sensor. With `cameraMount` absent, present at 0, or at the
/// mount the line already gives; with a hand-set option the entry does not
/// name; and with a STALE `<key>.camera` cache naming another sensor.
#[tokio::test]
async fn unmanaged_to_managed_with_the_same_sensor_writes_nothing() {
    // (config, model, [(key, listed sensor, node, dt rotation)], setting, mounts)
    type Cams = &'static [(&'static str, &'static str, &'static str, u32)];
    type Case = (
        String,
        &'static str,
        Cams,
        Value,
        &'static [(&'static str, u32)],
    );
    let cases: [Case; 5] = [
        (
            bench_config("config-bench1-imx290.txt"),
            MODEL_CM5,
            &[("cam0", "imx290", "imx290@1a", 0), ("cam1", "imx290", "imx290@1a", 0)],
            json!({"cam0": {"sensor": "imx290"}, "cam1": {"sensor": "imx290"}}),
            &[("cam0", 0), ("cam1", 0)],
        ),
        (
            bench_config("config-cm4-imx708.txt"),
            MODEL_CM4,
            &[("cam0", "imx708_wide", "imx708@1a", 180)],
            json!({"cam0": {"sensor": "imx708"}}),
            &[("cam0", 0)],
        ),
        (
            bench_config("config-bench2-imx335-imx219.txt"),
            MODEL_CM5,
            &[("cam0", "imx335", "imx335@1a", 180), ("cam1", "imx219", "imx219@10", 180)],
            json!({"cam0": {"sensor": "imx335"}, "cam1": {"sensor": "imx219"}}),
            &[("cam0", 0), ("cam1", 0)],
        ),
        // An option at its default, written out in the setting.
        (
            bench_config("config-bench1-imx290.txt"),
            MODEL_CM5,
            &[("cam0", "imx290", "imx290@1a", 0), ("cam1", "imx290", "imx290@1a", 0)],
            json!({"cam0": {"sensor": "imx290", "options": {"clockFrequency": 37125000}}}),
            &[],
        ),
        // A hand-set option and a parameter the catalog does not list, which
        // the entry does not name: as found, no write.
        (
            "camera_auto_detect=0\ndtoverlay=imx290,cam0,4lane\ndtoverlay=imx290,cam1,clock-frequency=74250000\n".to_string(),
            MODEL_CM5,
            &[("cam0", "imx290", "imx290@1a", 0), ("cam1", "imx290", "imx290@1a", 0)],
            json!({"cam0": {"sensor": "imx290"}, "cam1": {"sensor": "imx290"}}),
            &[("cam0", 0), ("cam1", 0)],
        ),
    ];
    for (ci, (config, model, cams, setting, mounts)) in cases.into_iter().enumerate() {
        let d = Device::bench("p2zero", &config, model);
        d.overlays(&ALL_OVERLAYS);
        // EVERY port's cache file names a camera that is no longer there
        // (an IMX477 the port had before): stale, as on bench 2.
        for (key, _, _, _) in cams {
            d.camera(key, &cm5_path(key, "imx477@1a"), Some(0));
        }
        for (key, _, node, rot) in cams {
            d.dt(&cm5_path(key, node), *rot);
        }
        let mut rig = Rig::new(d);
        rig.live(
            &cams
                .iter()
                .map(|(k, s, n, _)| (*k, *s, *n))
                .collect::<Vec<_>>(),
        );
        let md5 = rig.d.md5();
        // The namespace arrives; passes repeat; `cameraMount` comes and goes.
        for mount in [None, Some(mounts), None] {
            for _ in 0..2 {
                let s = rig.pass_both(mount, Some(setting.clone())).await;
                for key in setting.as_object().unwrap().keys() {
                    assert_eq!(
                        sens(&s, key),
                        (SV::Applied, String::new()),
                        "case {ci} {key}"
                    );
                }
                let st = s.sensors.as_ref().unwrap();
                assert!(!st.wrote && !st.reboot_required, "case {ci}");
                if let Some(m) = &s.mount {
                    assert!(!m.wrote && !m.reboot_required, "case {ci}");
                    for c in &m.cameras {
                        assert_eq!(c.verdict, Applied, "case {ci} {}", c.stream_key);
                        // The rotation in force is read at the LISTED
                        // camera's node, not at the cached name's.
                        assert_eq!(c.in_force_deg, Some(0.0), "case {ci} {}", c.stream_key);
                    }
                }
            }
        }
        assert_eq!(rig.runner.calls(), Vec::<Vec<String>>::new(), "case {ci}");
        assert_eq!(rig.d.md5(), md5, "case {ci}");
        assert_eq!(rig.d.read(), config, "case {ci}");
        assert_eq!(rig.logs(), Vec::<String>::new(), "case {ci}");
        assert!(!rig.d.boot_config.with_extension("txt.bak-camcfg").exists());
        assert!(
            rig.d.cache("cam0").ends_with("imx477@1a"),
            "the cache stayed stale"
        );
    }
}

/// A sensor change by setting: ONE helper call edits ONE line, both
/// namespaces say "reboot required", nothing more is written while it
/// waits, and after the reboot both say applied. The `<key>.camera` cache
/// keeps naming the OLD camera throughout (nothing rewrites it here), and
/// must not matter.
#[tokio::test]
async fn a_sensor_change_is_one_write_then_a_reboot() {
    let config = bench_config("config-bench1-imx290.txt");
    let d = Device::bench("p2swap", &config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    d.camera("cam1", &cm5_path("cam1", "imx290@1a"), Some(0));
    let mut rig = Rig::new(d);
    rig.live(&[
        ("cam0", "imx290", "imx290@1a"),
        ("cam1", "imx290", "imx290@1a"),
    ]);
    let setting = json!({"cam0": {"sensor": "imx708"}, "cam1": {"sensor": "imx290"}});
    let mounts: &[(&str, u32)] = &[("cam0", 0), ("cam1", 0)];

    let s = rig.pass_both(Some(mounts), Some(setting.clone())).await;
    assert_eq!(rig.runner.asked(), ["set cam0 imx708 rotation=180"]);
    assert_eq!(sens(&s, "cam0"), (SV::PendingReboot, String::new()));
    assert_eq!(sens(&s, "cam1"), (SV::Applied, String::new()));
    let st = s.sensors.as_ref().unwrap();
    assert!(st.wrote && st.reboot_required);
    // `cameraMount` does not claim the new sensor's rotation as in force:
    // the camera running on the port is still the IMX290.
    assert_eq!(
        mnt(&s, "cam0"),
        (PendingReboot, None, Some(0.0), String::new())
    );
    assert_eq!(
        mnt(&s, "cam1"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert!(s.mount.as_ref().unwrap().reboot_required);
    // Exactly one line of the file changed.
    let after = rig.d.read();
    let diff: Vec<(&str, &str)> = config
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(
        diff,
        [(
            "dtoverlay=imx290,cam0",
            "dtoverlay=imx708,cam0,rotation=180"
        )]
    );
    assert_eq!(config.lines().count(), after.lines().count());
    assert_eq!(
        rig.logs(),
        ["info cam0: camera line for cam0 in <dir>/config.txt is now the imx708 line (imx708,rotation=180) — REBOOT required before the camera changes (the agent never reboots itself)"]
    );

    // Waiting: no further call, the same answer, however often it passes.
    for _ in 0..3 {
        let s = rig.pass_both(Some(mounts), Some(setting.clone())).await;
        assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
        assert!(!s.sensors.as_ref().unwrap().wrote);
    }
    assert_eq!(rig.runner.calls().len(), 1);
    assert_eq!(rig.d.read(), after);

    // The reboot: the kernel loads the imx708 overlay with rotation 180 and
    // the camera is listed. cam0's cache file still names the IMX290.
    rig.reboot();
    rig.d.dt(&cm5_path("cam0", "imx708@1a"), 180);
    rig.live(&[
        ("cam0", "imx708_wide", "imx708@1a"),
        ("cam1", "imx290", "imx290@1a"),
    ]);
    assert!(rig.d.cache("cam0").ends_with("imx290@1a"));
    let s = rig.pass_both(Some(mounts), Some(setting.clone())).await;
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    assert_eq!(
        mnt(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert!(!s.sensors.as_ref().unwrap().reboot_required);
    assert!(!s.mount.as_ref().unwrap().reboot_required);
    assert_eq!(rig.runner.calls().len(), 1);

    // Changed back before a reboot: written again and waiting; changed to
    // the booted sensor once more: the line is again what this boot started
    // with, and nothing waits.
    let back = json!({"cam0": {"sensor": "imx290"}, "cam1": {"sensor": "imx290"}});
    let s = rig.pass_both(Some(mounts), Some(back)).await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    let s = rig.pass_both(Some(mounts), Some(setting)).await;
    assert_eq!(
        rig.runner.asked(),
        [
            "set cam0 imx708 rotation=180",
            "set cam0 imx290 rotation=0",
            "set cam0 imx708 rotation=180"
        ]
    );
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
}

/// S1 (review): a WRONG declaration. The line is written, the device
/// reboots, nothing answers on the port. The line is as declared and
/// booted, so `cameraSensors` is `applied` and nothing waits for a reboot,
/// for ever after (the port's verdict says `not-detected`:
/// `camera_truth::tests::a_managed_port_that_is_not_detected_points_at_the_selection`).
/// The stale cache file, which still names the camera that used to work,
/// must not turn that into "REBOOT REQUIRED".
#[tokio::test]
async fn a_wrong_declaration_is_applied_after_its_reboot() {
    let config = "camera_auto_detect=0\n[all]\ndtoverlay=imx708,cam0,rotation=180\n";
    let d = Device::bench("p2wrong", config, MODEL_CM4);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx708@1a"), Some(180));
    let mut rig = Rig::new(d);
    rig.live(&[("cam0", "imx708_wide", "imx708@1a")]);
    let setting = json!({"cam0": {"sensor": "imx219"}});
    let s = rig
        .pass_both(Some(&[("cam0", 0)]), Some(setting.clone()))
        .await;
    assert_eq!(rig.runner.asked(), ["set cam0 imx219 rotation=180"]);
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    assert_eq!(mnt(&s, "cam0").0, PendingReboot);

    // Rebooted: the imx219 overlay is loaded, its driver did not bind (no
    // such chip), libcamera lists nothing. The cache still says imx708.
    rig.reboot();
    rig.evidence = PortEvidence {
        cameras: Some(vec![]),
        relisted: true,
        nodes: Some(vec![node("cam0", "imx219@10", None)]),
    };
    assert!(rig.d.cache("cam0").ends_with("imx708@1a"));
    for _ in 0..3 {
        let s = rig
            .pass_both(Some(&[("cam0", 0)]), Some(setting.clone()))
            .await;
        assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
        assert!(!s.sensors.as_ref().unwrap().reboot_required);
        // `cameraMount`: no camera to read a rotation from, as on any port
        // without a camera; it is not "reboot required" either.
        let (verdict, in_force, boot, _) = mnt(&s, "cam0");
        assert_eq!((verdict, in_force, boot), (Unconfirmed, None, Some(0.0)));
        assert!(!s.mount.as_ref().unwrap().reboot_required);
    }
    assert_eq!(rig.runner.calls().len(), 1);

    // The recovery: the right sensor, one write, a reboot, applied.
    let right = json!({"cam0": {"sensor": "imx708"}});
    let s = rig
        .pass_both(Some(&[("cam0", 0)]), Some(right.clone()))
        .await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    rig.reboot();
    rig.live(&[("cam0", "imx708_wide", "imx708@1a")]);
    let s = rig.pass_both(Some(&[("cam0", 0)]), Some(right)).await;
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    assert_eq!(
        mnt(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    assert_eq!(
        rig.runner.asked(),
        [
            "set cam0 imx219 rotation=180",
            "set cam0 imx708 rotation=180"
        ]
    );
}

/// Rotation and sensor changed together are ONE write: the line carries the
/// new sensor's default plus the mount. A rotation change alone, on a line
/// of the declared sensor, is the in-place `rotate`.
#[tokio::test]
async fn rotation_and_sensor_changed_together_are_one_write() {
    let config = bench_config("config-bench1-imx290.txt");
    let d = Device::bench("p2both", &config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    d.camera("cam1", &cm5_path("cam1", "imx290@1a"), Some(0));
    let mut rig = Rig::new(d);
    rig.live(&[
        ("cam0", "imx290", "imx290@1a"),
        ("cam1", "imx290", "imx290@1a"),
    ]);
    let setting = json!({"cam0": {"sensor": "imx219"}, "cam1": {"sensor": "imx290"}});
    let mounts: &[(&str, u32)] = &[("cam0", 180), ("cam1", 180)];
    let s = rig.pass_both(Some(mounts), Some(setting.clone())).await;
    // imx219: default 180 + mount 180 = 0, with the line. imx290: the line
    // is the sensor's already, only its rotation moves.
    assert_eq!(
        rig.runner.asked(),
        ["set cam0 imx219 rotation=0", "rotate cam1 imx290 180"]
    );
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    // cam1: the same sensor with another rotation: the line changed since
    // boot, and the rotation in force is still the old one.
    assert_eq!(sens(&s, "cam1").0, SV::PendingReboot);
    assert_eq!(
        mnt(&s, "cam1"),
        (PendingReboot, Some(0.0), Some(180.0), String::new())
    );
    assert_eq!(
        mnt(&s, "cam0"),
        (PendingReboot, None, Some(180.0), String::new())
    );
    assert!(rig
        .d
        .read()
        .contains("\ndtoverlay=imx219,cam0,rotation=0\ndtoverlay=imx290,cam1,rotation=180\n"));
    assert_eq!(
        rig.logs()[1],
        "info cam1: camera line for cam1 in <dir>/config.txt: rotation=180 set on the imx290 line, the rest of the line kept — REBOOT required before the camera changes (the agent never reboots itself)"
    );
    // After the reboot.
    rig.reboot();
    rig.d.dt(&cm5_path("cam0", "imx219@10"), 0);
    rig.d.dt(&cm5_path("cam1", "imx290@1a"), 180);
    rig.live(&[
        ("cam0", "imx219", "imx219@10"),
        ("cam1", "imx290", "imx290@1a"),
    ]);
    let s = rig.pass_both(Some(mounts), Some(setting)).await;
    assert_eq!(
        mnt(&s, "cam0"),
        (Applied, Some(180.0), Some(180.0), String::new())
    );
    assert_eq!(
        mnt(&s, "cam1"),
        (Applied, Some(180.0), Some(180.0), String::new())
    );
    assert_eq!(rig.runner.calls().len(), 2);
}

/// A port declared empty: its line is removed; `cameraMount` has nothing to
/// rotate there and says so by saying nothing. After the reboot nothing is
/// on the port, the cache file still names the camera that was (S1), and
/// the port is `applied` with nothing pending.
#[tokio::test]
async fn a_port_declared_empty_loses_its_line() {
    let config = bench_config("config-bench1-imx290.txt");
    let d = Device::bench("p2none", &config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    d.camera("cam1", &cm5_path("cam1", "imx290@1a"), Some(0));
    let mut rig = Rig::new(d);
    rig.live(&[
        ("cam0", "imx290", "imx290@1a"),
        ("cam1", "imx290", "imx290@1a"),
    ]);
    let setting = json!({"cam1": {"sensor": "none"}});
    let s = rig
        .pass_both(Some(&[("cam1", 180)]), Some(setting.clone()))
        .await;
    assert_eq!(rig.runner.asked(), ["remove cam1"]);
    assert_eq!(sens(&s, "cam1"), (SV::PendingReboot, String::new()));
    assert_eq!(mnt(&s, "cam1"), (Applied, None, None, String::new()));
    assert_eq!(rig.d.read(), config.replace("dtoverlay=imx290,cam1\n", ""));
    assert_eq!(
        rig.logs(),
        ["info cam1: camera line for cam1 removed from <dir>/config.txt (the port is declared empty) — REBOOT required before the camera changes (the agent never reboots itself)"]
    );
    // cam0 is not in the setting: its line is as found.
    assert!(rig.d.read().contains("dtoverlay=imx290,cam0\n"));
    // Rebooted: no overlay and no camera on cam1 any more. Its cache file
    // is as it was: nothing deletes it on a device.
    rig.reboot();
    rig.live(&[("cam0", "imx290", "imx290@1a")]);
    assert!(rig.d.cache("cam1").ends_with("imx290@1a"));
    for _ in 0..3 {
        let s = rig
            .pass_both(Some(&[("cam1", 180)]), Some(setting.clone()))
            .await;
        assert_eq!(sens(&s, "cam1"), (SV::Applied, String::new()));
        assert!(!s.sensors.as_ref().unwrap().reboot_required);
    }
    assert_eq!(rig.runner.calls().len(), 1);
}

/// An option change rewrites the line once, keeps the rotation the line
/// gives when `cameraMount` does not manage the port, and waits for a
/// reboot although the overlay is the same.
#[tokio::test]
async fn an_option_change_is_one_write_and_waits_for_a_reboot() {
    let config = "camera_auto_detect=0\n[all]\ndtoverlay=imx708,cam0,rotation=0\n";
    let d = Device::bench("p2opt", config, MODEL_CM4);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx708@1a"), Some(0));
    let mut rig = Rig::new(d);
    rig.live(&[("cam0", "imx708_wide", "imx708@1a")]);
    let setting = json!({"cam0": {"sensor": "imx708", "options": {"linkFrequency": 447000000}}});
    let s = rig.pass_both(None, Some(setting.clone())).await;
    assert!(s.mount.is_none());
    assert_eq!(
        rig.runner.asked(),
        ["set cam0 imx708 link-frequency=447000000 rotation=0"]
    );
    assert_eq!(sens(&s, "cam0"), (SV::PendingReboot, String::new()));
    assert_eq!(
        rig.d.read(),
        "camera_auto_detect=0\n[all]\ndtoverlay=imx708,cam0,link-frequency=447000000,rotation=0\n"
    );
    let s = rig.pass_both(None, Some(setting.clone())).await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    rig.reboot();
    let s = rig.pass_both(None, Some(setting)).await;
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    assert_eq!(rig.runner.calls().len(), 1);
    // The option is dropped from the entry: it is no longer named, so the
    // line keeps it ("as found") and nothing is written.
    let s = rig
        .pass_both(None, Some(json!({"cam0": {"sensor": "imx708"}})))
        .await;
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    assert_eq!(rig.runner.calls().len(), 1);
    // Named at its default: enforced.
    let s = rig
        .pass_both(
            None,
            Some(json!({"cam0": {"sensor": "imx708", "options": {"linkFrequency": 450000000}}})),
        )
        .await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    assert_eq!(
        rig.runner.asked()[1],
        "set cam0 imx708 link-frequency=450000000 rotation=0"
    );
}

/// NEVER LOSE A HAND-SET PARAMETER SILENTLY, through the real helper: a
/// token the catalog does not list survives a rotation change (`rotate`
/// keeps every token), and a write that would drop it is refused with a
/// sentence that names it.
#[tokio::test]
async fn a_hand_set_parameter_is_never_dropped_silently() {
    let config = "camera_auto_detect=0\ndtoverlay=imx290,cam0,4lane\ndtoverlay=imx290,cam1,clock-frequency=74250000\n";
    let d = Device::bench("p2hand", config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    d.camera("cam1", &cm5_path("cam1", "imx290@1a"), Some(0));
    let mut rig = Rig::new(d);
    rig.live(&[
        ("cam0", "imx290", "imx290@1a"),
        ("cam1", "imx290", "imx290@1a"),
    ]);
    let plain = json!({"cam0": {"sensor": "imx290"}, "cam1": {"sensor": "imx290"}});
    // Managed with the same sensor: nothing is written, both lines keep
    // what they have.
    let s = rig.pass_both(None, Some(plain.clone())).await;
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    assert_eq!(sens(&s, "cam1"), (SV::Applied, String::new()));
    assert_eq!(rig.runner.calls().len(), 0);
    // A mount: the rotation is set in place, every other token kept.
    let s = rig
        .pass_both(Some(&[("cam0", 180), ("cam1", 180)]), Some(plain.clone()))
        .await;
    assert_eq!(
        rig.runner.asked(),
        ["rotate cam0 imx290 180", "rotate cam1 imx290 180"]
    );
    assert_eq!(
        rig.d.read(),
        "camera_auto_detect=0\ndtoverlay=imx290,cam0,4lane,rotation=180\ndtoverlay=imx290,cam1,clock-frequency=74250000,rotation=180\n"
    );
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    // A named option on the line with the unlisted token: NOT written.
    let before = rig.d.read();
    let named = json!({
        "cam0": {"sensor": "imx290", "options": {"clockFrequency": 74250000}},
        "cam1": {"sensor": "imx290", "options": {"clockFrequency": 37125000}}
    });
    let s = rig
        .pass_both(Some(&[("cam0", 180), ("cam1", 180)]), Some(named.clone()))
        .await;
    let refusal = "the camera line for cam0 holds \"4lane\", which the sensor catalog does not list for imx290: rewriting the line would drop it, so the line is left as it is. Edit the line by hand, or remove \"4lane\" from it";
    assert_eq!(sens(&s, "cam0"), (SV::Rejected, refusal.to_string()));
    assert_eq!(
        mnt(&s, "cam0"),
        (Unsupported, None, None, refusal.to_string())
    );
    // ... while cam1, whose line holds only catalog tokens, gets the named
    // option in one `set` that keeps its rotation.
    assert_eq!(
        rig.runner.asked()[2..],
        ["set cam1 imx290 clock-frequency=37125000 rotation=180"]
    );
    assert_eq!(
        rig.d.read(),
        before.replace("clock-frequency=74250000", "clock-frequency=37125000")
    );
    // Said once in the journal, not once per pass; no further call.
    rig.pass_both(Some(&[("cam0", 180), ("cam1", 180)]), Some(named))
        .await;
    assert_eq!(rig.runner.calls().len(), 3);
    assert_eq!(
        rig.logs()
            .iter()
            .filter(|l| l.contains("camera sensor setting not applied"))
            .count(),
        1
    );
    // A sensor CHANGE replaces the line: the old sensor's tokens go with it.
    let s = rig
        .pass_both(None, Some(json!({"cam0": {"sensor": "imx708"}})))
        .await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    assert_eq!(rig.runner.asked()[3], "set cam0 imx708 rotation=180");
}

/// What cannot be applied is refused PER PORT, with a sentence, and
/// config.txt is left alone for that port; the other ports are applied.
#[tokio::test]
async fn a_port_that_cannot_be_applied_is_refused_and_the_others_are_applied() {
    let config = bench_config("config-bench1-imx290.txt");
    let d = Device::bench("p2refuse", &config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    d.camera("cam1", &cm5_path("cam1", "imx290@1a"), Some(0));
    // (cam0's entry, its sentence)
    let cases = [
        (
            json!({"sensor": "imx999"}),
            "this agent build does not know the camera sensor \"imx999\" (it knows: imx290, imx219, imx708, imx335); update the agent or choose another sensor. config.txt is left as it is",
        ),
        (
            json!({"sensor": "imx290", "options": {"lanes": 4}}),
            "the imx290 sensor has no option \"lanes\" in this agent build (its options: clockFrequency); config.txt is left as it is",
        ),
        (
            json!({"sensor": "imx290", "options": {"clockFrequency": 1}}),
            "option \"clockFrequency\" of the imx290 sensor must be one of 37125000, 74250000 (got 1); config.txt is left as it is",
        ),
        (
            json!(42),
            "the camera sensor setting of this port is not an object; config.txt is left as it is",
        ),
        (
            json!({"options": {}}),
            "the camera sensor setting of this port has no \"sensor\" name; config.txt is left as it is",
        ),
    ];
    drop(d);
    for (entry, sentence) in cases {
        let rig = Rig::new(Device::bench("p2refuse-x", &config, MODEL_CM5));
        rig.d.overlays(&ALL_OVERLAYS);
        rig.d
            .camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
        rig.d
            .camera("cam1", &cm5_path("cam1", "imx290@1a"), Some(0));
        let s = rig
            .pass_both(
                Some(&[("cam0", 180)]),
                Some(json!({
                    "cam0": entry,
                    "cam1": {"sensor": "imx219"},
                    // A port this device does not have (a model reassignment).
                    "cam7": {"sensor": "imx290"},
                })),
            )
            .await;
        assert_eq!(
            sens(&s, "cam0"),
            (SV::Rejected, sentence.to_string()),
            "{entry}"
        );
        assert_eq!(
            sens(&s, "cam7"),
            (
                SV::Rejected,
                "this device has no camera \"cam7\"; config.txt is left as it is".to_string()
            )
        );
        // The good port is still applied: one call, for cam1 only.
        assert_eq!(sens(&s, "cam1").0, SV::PendingReboot, "{entry}");
        assert_eq!(
            rig.runner.asked(),
            ["set cam1 imx219 rotation=180"],
            "{entry}"
        );
        let after = rig.d.read();
        assert!(
            after.contains("dtoverlay=imx290,cam0\n"),
            "cam0's line is as found: {after}"
        );
        // `cameraMount` writes nothing on a port whose sensor entry is refused.
        assert_eq!(
            mnt(&s, "cam0"),
            (
                Unsupported,
                None,
                None,
                "the camera sensor setting of this port is not applied (see cameraSensors), so its camera line is left as it is".to_string()
            ),
            "{entry}"
        );
        // The same every pass, without a helper call.
        let again = rig
            .pass_both(
                Some(&[("cam0", 180)]),
                Some(json!({"cam0": entry, "cam1": {"sensor": "imx219"}, "cam7": {"sensor": "imx290"}})),
            )
            .await;
        assert_eq!(sens(&again, "cam0").0, SV::Rejected);
        assert_eq!(rig.runner.calls().len(), 1, "{entry}");
    }
}

/// The helper's refusals come back as the status, in the helper's words,
/// and a refused call is not repeated every pass.
#[tokio::test]
async fn a_helper_refusal_is_the_status_and_is_not_retried_every_pass() {
    // A `dtparam=` line follows the camera line: `set` to another overlay
    // would hand it to that overlay (the helper's exit 5).
    let config = "camera_auto_detect=0\ndtoverlay=imx290,cam0\ndtparam=i2c_arm=on\n";
    let d = Device::bench("p2helper", config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    let mut rig = Rig::new(d);
    let setting = json!({"cam0": {"sensor": "imx708"}});
    let s = rig
        .pass_both(Some(&[("cam0", 0)]), Some(setting.clone()))
        .await;
    let refusal = "could not edit <dir>/config.txt: a dtparam= line follows the camera line of cam0 and would then apply to another overlay; move it into the dtoverlay line or remove it first (<dir>/config.txt not changed)";
    let (v, detail) = sens(&s, "cam0");
    assert_eq!(
        (v, rig.d.redact(&detail)),
        (SV::WriteFailed, refusal.to_string())
    );
    let (v, _, _, detail) = mnt(&s, "cam0");
    assert_eq!(
        (v, rig.d.redact(&detail)),
        (WriteFailed, refusal.to_string())
    );
    assert_eq!(rig.d.read(), config);
    assert_eq!(rig.runner.calls().len(), 1);
    assert_eq!(rig.logs().len(), 1);
    assert!(rig.logs()[0]
        .starts_with("warn cam0: boot config camera line edit failed — a dtparam= line follows"));
    // Not repeated: the next passes answer from memory.
    for _ in 0..3 {
        let s = rig
            .pass_both(Some(&[("cam0", 0)]), Some(setting.clone()))
            .await;
        assert_eq!(sens(&s, "cam0").0, SV::WriteFailed);
    }
    assert_eq!(rig.runner.calls().len(), 1);
    // ... until ten minutes have passed, or what is asked changes.
    rig.now += RETRY_FAILED_WRITE_MS;
    rig.pass_both(Some(&[("cam0", 0)]), Some(setting)).await;
    assert_eq!(rig.runner.calls().len(), 2);
    rig.pass_both(
        Some(&[("cam0", 0)]),
        Some(json!({"cam0": {"sensor": "imx219"}})),
    )
    .await;
    assert_eq!(rig.runner.calls().len(), 3);

    // No overlay file on the device: the helper's exit 2, in its words.
    let d = Device::bench("p2nodtbo", "dtoverlay=imx290,cam0\n", MODEL_CM5);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    let rig = Rig::new(d);
    let s = rig
        .pass_both(None, Some(json!({"cam0": {"sensor": "imx335"}})))
        .await;
    let (v, detail) = sens(&s, "cam0");
    assert_eq!(v, SV::WriteFailed);
    assert_eq!(
        rig.d.redact(&detail),
        "could not edit <dir>/config.txt: no overlay file <dir>/overlays/imx335.dtbo on this device"
    );

    // A line under a section the agent cannot evaluate: refused without a
    // sudo call (the helper would exit 5).
    let d = Device::bench(
        "p2section",
        "[pi3]\ndtoverlay=imx290,cam0\n[all]\n",
        MODEL_CM5,
    );
    d.overlays(&ALL_OVERLAYS);
    let rig = Rig::new(d);
    let s = rig
        .pass_both(None, Some(json!({"cam0": {"sensor": "imx708"}})))
        .await;
    let (v, detail) = sens(&s, "cam0");
    assert_eq!(v, SV::Rejected);
    assert_eq!(
        rig.d.redact(&detail),
        "a camera line for cam0 in <dir>/config.txt sits under [pi3], a section the agent cannot evaluate for this board: it is left as it is; edit it by hand"
    );
    assert_eq!(rig.runner.calls().len(), 0);

    // Two lines on the port: `set` collapses them into the declared one.
    let d = Device::bench(
        "p2two",
        "dtoverlay=imx290,cam0\ndtoverlay=imx219,cam0\ndtoverlay=imx290,cam1\n",
        MODEL_CM5,
    );
    d.overlays(&ALL_OVERLAYS);
    let rig = Rig::new(d);
    let s = rig
        .pass_both(None, Some(json!({"cam0": {"sensor": "imx219"}})))
        .await;
    // The line in force was already the LAST one (imx219): deleting the
    // stale line and spelling out the default rotation changes nothing the
    // kernel booted with, so nothing waits for a reboot.
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    assert_eq!(rig.runner.asked(), ["set cam0 imx219 rotation=180"]);
    assert_eq!(
        rig.d.read(),
        "dtoverlay=imx219,cam0,rotation=180\ndtoverlay=imx290,cam1\n"
    );
}

/// The update window: this build runs, the helper is not installed yet. A
/// managed port that needs a write says so once, calls no sudo, and is
/// applied by itself once the helper is there.
#[tokio::test]
async fn the_update_window_holds_for_a_managed_port() {
    let config = bench_config("config-bench1-imx290.txt");
    let d = Device::bench("p2window", &config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    let rig = Rig::new(d);
    rig.installed.store(false, Ordering::SeqCst);
    let setting = json!({"cam0": {"sensor": "imx708"}});
    for _ in 0..3 {
        let s = rig
            .pass_both(Some(&[("cam0", 0)]), Some(setting.clone()))
            .await;
        let (v, detail) = sens(&s, "cam0");
        assert_eq!(v, SV::WriteFailed);
        assert_eq!(
            rig.d.redact(&detail),
            format!("could not edit <dir>/config.txt: {HELPER_NOT_INSTALLED}")
        );
        assert_eq!(mnt(&s, "cam0").0, WriteFailed);
    }
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), config);
    assert_eq!(rig.logs().len(), 1, "one warning, not one per pass");
    // A managed port that needs NO write is unaffected by the window.
    let s = rig
        .pass_both(None, Some(json!({"cam0": {"sensor": "imx290"}})))
        .await;
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    // The helper arrives (the second update).
    rig.installed.store(true, Ordering::SeqCst);
    let s = rig.pass_both(Some(&[("cam0", 0)]), Some(setting)).await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    assert_eq!(rig.runner.asked(), ["set cam0 imx708 rotation=180"]);
}

/// What `cameraMount` says on a managed port whose sensor cannot flip, and
/// that a managed port and an unmanaged port live in one pass: the
/// unmanaged one goes through `rotate`, exactly as before.
#[tokio::test]
async fn camera_mount_on_managed_ports_and_beside_them() {
    let config = bench_config("config-bench2-imx335-imx219.txt");
    let d = Device::bench("p2mount", &config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx335@1a"), Some(180));
    d.camera("cam1", &cm5_path("cam1", "imx219@10"), Some(180));
    let mut rig = Rig::new(d);
    rig.evidence = PortEvidence {
        cameras: Some(vec![
            listed("cam0", "imx335", "imx335@1a"),
            listed("cam1", "imx219", "imx219@10"),
        ]),
        relisted: false,
        nodes: Some(vec![
            node("cam0", "imx335@1a", Some("imx335")),
            node("cam1", "imx219@10", Some("imx219")),
        ]),
    };
    // cam0 managed (imx335, cannot flip), cam1 unmanaged with a mount.
    let setting = json!({"cam0": {"sensor": "imx335"}});
    let s = rig
        .pass_both(Some(&[("cam0", 180), ("cam1", 180)]), Some(setting.clone()))
        .await;
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    assert_eq!(
        mnt(&s, "cam0"),
        (
            Unsupported,
            Some(0.0),
            Some(0.0),
            "the imx335 sensor cannot be rotated: mount the camera the right way up".to_string()
        )
    );
    // cam1: unmanaged, the Phase 1b `rotate` (180 + 180 = 0).
    assert_eq!(rig.runner.asked(), ["rotate cam1 imx219 0"]);
    assert_eq!(
        mnt(&s, "cam1"),
        (PendingReboot, Some(0.0), Some(180.0), String::new())
    );
    assert!(s
        .sensors
        .as_ref()
        .unwrap()
        .cameras
        .iter()
        .all(|c| c.stream_key != "cam1"));
    // The imx335 line was not touched for the mount it cannot take.
    assert!(rig.d.read().contains("dtoverlay=imx335,cam0\n"));
    // Mount 0 on the sensor that cannot flip: applied.
    let s = rig
        .pass_both(Some(&[("cam0", 0), ("cam1", 180)]), Some(setting))
        .await;
    assert_eq!(
        mnt(&s, "cam0"),
        (Applied, Some(0.0), Some(0.0), String::new())
    );
    // The states come out in stream-key order, managed or not.
    let keys: Vec<&str> = s
        .mount
        .as_ref()
        .unwrap()
        .cameras
        .iter()
        .map(|c| c.stream_key.as_str())
        .collect();
    assert_eq!(keys, ["cam0", "cam1"]);
}

/// The namespace states: absent = `None` and nothing read; present and
/// empty = an empty state; the boot snapshot survives an agent restart in
/// the same boot through its file.
#[tokio::test]
async fn namespace_presence_and_the_boot_snapshot_file() {
    let config = bench_config("config-bench1-imx290.txt");
    let d = Device::bench("p2ns", &config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    let mut rig = Rig::new(d);
    let s = rig.pass_both(None, None).await;
    assert_eq!(s, CameraStates::default());
    let s = rig.pass_both(None, Some(json!({}))).await;
    assert!(s.mount.is_none());
    let st = s.sensors.unwrap();
    assert!(st.cameras.is_empty() && !st.wrote && !st.reboot_required);
    assert_eq!(rig.runner.calls().len(), 0);

    // A store on a file: the first pass of the boot writes it.
    let file = rig.d.dir.join("camera-boot-lines.json");
    let id = rig.boot_id.clone();
    let store = |file: &Path| {
        let id = id.clone();
        Arc::new(BootLineStore::new(
            Some(file.to_path_buf()),
            Box::new(move || id.lock().unwrap().clone()),
        ))
    };
    rig.boot_lines = store(&file);
    let setting = json!({"cam0": {"sensor": "imx708"}});
    let s = rig.pass_both(None, Some(setting.clone())).await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(kept["bootId"], "boot-1");
    assert_eq!(kept["ports"]["cam0"]["text"], "dtoverlay=imx290,cam0");
    assert_eq!(kept["ports"]["cam1"]["text"], "dtoverlay=imx290,cam1");
    // The agent restarts in the same boot: a new store, the same file. The
    // written line still waits for a reboot, on the strength of the file
    // alone (this rig's pass cannot look at the ports: no listing, no
    // sysfs scan).
    rig.boot_lines = store(&file);
    let s = rig.pass_both(None, Some(setting.clone())).await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    // A reboot: the file is another boot's and is replaced.
    rig.reboot();
    rig.boot_lines = store(&file);
    let s = rig.pass_both(None, Some(setting)).await;
    assert_eq!(sens(&s, "cam0"), (SV::Applied, String::new()));
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(kept["bootId"], "boot-1+");
    assert_eq!(
        kept["ports"]["cam0"]["text"],
        "dtoverlay=imx708,cam0,rotation=180"
    );
    assert_eq!(rig.runner.calls().len(), 1);
}

/// Bench finding (Phase 1b run): a rejection was in the status only. Each
/// one now reaches the journal ONCE PER CHANGE: when it appears and when
/// its sentence changes, never once per pass.
#[tokio::test]
async fn a_rejection_is_logged_once_per_change() {
    // `cameraMount`, unmanaged ports: a sensor that cannot flip (cam0) and
    // a line under a section the agent cannot evaluate (cam1).
    let config = "camera_auto_detect=0\ndtoverlay=imx335,cam0\n[x-unknown]\ndtoverlay=imx219,cam1,rotation=180\n[all]\n";
    let d = Device::bench("rejlog", config, MODEL_CM5);
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx335@1a"), Some(180));
    d.camera("cam1", &cm5_path("cam1", "imx219@10"), Some(180));
    let rig = Rig::new(d);
    let cannot = "warn cam0: rotation not applied — the imx335 sensor cannot be rotated: mount the camera the right way up";
    let section = "warn cam1: rotation not applied — the camera line for cam1 in <dir>/config.txt sits under [x-unknown], a section the agent cannot evaluate for this board: it is left as it is; edit it by hand";
    for _ in 0..3 {
        let s = rig.pass(&[("cam0", 180), ("cam1", 180)]).await;
        assert_eq!(cam(&s, "cam0").0, Unsupported);
        assert_eq!(cam(&s, "cam1").0, Unsupported);
    }
    assert_eq!(
        rig.logs(),
        [cannot, section],
        "once each, not once per pass"
    );
    // cam0 goes back to a mount it can take: nothing is said; asked again,
    // the rejection is a new one and is said again.
    rig.pass(&[("cam0", 0), ("cam1", 180)]).await;
    assert_eq!(rig.logs().len(), 2);
    rig.pass(&[("cam0", 180), ("cam1", 180)]).await;
    rig.pass(&[("cam0", 180), ("cam1", 180)]).await;
    assert_eq!(rig.logs(), [cannot, section, cannot]);
    assert_eq!(rig.runner.calls().len(), 0);
    assert_eq!(rig.d.read(), config);

    // `cameraSensors`: a refused entry, then another sentence for the same
    // port, then a usable one.
    let d = Device::bench(
        "rejlog2",
        &bench_config("config-bench1-imx290.txt"),
        MODEL_CM5,
    );
    d.overlays(&ALL_OVERLAYS);
    let rig = Rig::new(d);
    for _ in 0..2 {
        rig.pass_both(
            Some(&[("cam0", 180)]),
            Some(json!({"cam0": {"sensor": "imx999"}})),
        )
        .await;
    }
    rig.pass_both(Some(&[("cam0", 180)]), Some(json!({"cam0": 7})))
        .await;
    rig.pass_both(None, Some(json!({"cam0": {"sensor": "imx290"}})))
        .await;
    assert_eq!(
        rig.logs(),
        [
            // One line per refusal; the `cameraMount` consequence on the
            // same port is not a second line.
            "warn cam0: camera sensor setting not applied — this agent build does not know the camera sensor \"imx999\" (it knows: imx290, imx219, imx708, imx335); update the agent or choose another sensor. config.txt is left as it is",
            "warn cam0: camera sensor setting not applied — the camera sensor setting of this port is not an object; config.txt is left as it is",
        ]
    );
    assert_eq!(rig.runner.calls().len(), 0);
}

/// Bench finding: the journal said "REBOOT required" for a write that
/// returned the line to what the device already runs. It now says that no
/// reboot is needed, as the status (`applied`) does.
#[tokio::test]
async fn a_write_back_to_what_runs_says_no_reboot_is_needed() {
    // Unmanaged: 0 -> 180 -> 0 without a reboot.
    let d = Device::bench(
        "noreboot",
        &bench_config("config-bench1-imx290.txt"),
        MODEL_CM5,
    );
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    let mut rig = Rig::new(d);
    rig.evidence = PortEvidence {
        cameras: Some(vec![listed("cam0", "imx290", "imx290@1a")]),
        relisted: false,
        nodes: Some(vec![node("cam0", "imx290@1a", Some("imx290"))]),
    };
    let s = rig.pass(&[("cam0", 180)]).await;
    assert_eq!(cam(&s, "cam0").0, PendingReboot);
    let s = rig.pass(&[("cam0", 0)]).await;
    assert_eq!(cam(&s, "cam0").0, Applied);
    assert_eq!(
        rig.logs(),
        [
            "info cam0: imx290/cam0 rotation set to 180° in <dir>/config.txt — REBOOT required before the image changes (the agent never reboots itself)",
            "info cam0: imx290/cam0 rotation set to 0° in <dir>/config.txt — no reboot needed: the device already runs this rotation",
        ]
    );

    // Managed: the sensor changed and changed back before a reboot.
    let d = Device::bench(
        "noreboot2",
        &bench_config("config-bench1-imx290.txt"),
        MODEL_CM5,
    );
    d.overlays(&ALL_OVERLAYS);
    d.camera("cam0", &cm5_path("cam0", "imx290@1a"), Some(0));
    let rig = Rig::new(d);
    let s = rig
        .pass_both(None, Some(json!({"cam0": {"sensor": "imx708"}})))
        .await;
    assert_eq!(sens(&s, "cam0").0, SV::PendingReboot);
    let s = rig
        .pass_both(None, Some(json!({"cam0": {"sensor": "imx290"}})))
        .await;
    assert_eq!(sens(&s, "cam0").0, SV::Applied);
    assert_eq!(
        rig.logs(),
        [
            "info cam0: camera line for cam0 in <dir>/config.txt is now the imx708 line (imx708,rotation=180) — REBOOT required before the camera changes (the agent never reboots itself)",
            "info cam0: camera line for cam0 in <dir>/config.txt is now the imx290 line (imx290,rotation=0) — no reboot needed: this is the line the device booted with",
        ]
    );
}
