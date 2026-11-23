//! `xorgate-rs camera apply --from-claim` (the installer's step 8b,
//! support-other-camera-models Phase 2; RUST-ONLY) through the REAL
//! camera-line owner `system/xorgate-camcfg`, on files:
//!
//! - a fresh stock config.txt (no camera line, `camera_auto_detect=1`);
//! - a file that already has lines (the same sensor: nothing written; other
//!   sensors: replaced, never doubled);
//! - the three benches' real config.txt files (`fixtures/camera_line/`);
//! - a re-provision (the command run again, with the same and with another
//!   choice): one line per port, always;
//! - the three sources of the choice (the claim's `cameraSensors`, the
//!   model's sensors, the two `imx290` lines) and what cannot be applied.
#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};
use xorgate_agent::cli::camera::{apply, run, CameraEnv, CHANGED_LINE, UNCHANGED_LINE};
use xorgate_core::boot_config::{board_from_model, Board};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bench_config(name: &str) -> String {
    std::fs::read_to_string(repo().join("fixtures/camera_line").join(name)).unwrap()
}

const MODEL_CM4: &str = "Raspberry Pi Compute Module 4 Rev 1.1";
const MODEL_CM5: &str = "Raspberry Pi Compute Module 5 Rev 1.0";

/// A device's boot partition in a scratch directory.
struct Dev {
    dir: PathBuf,
    config: PathBuf,
    board: Board,
}

impl Dev {
    fn new(config: &str, model: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "xg-camapply-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("overlays")).unwrap();
        for o in ["imx290", "imx219", "imx708", "imx335", "imx662"] {
            std::fs::write(dir.join("overlays").join(format!("{o}.dtbo")), b"").unwrap();
        }
        std::fs::write(dir.join("model"), format!("{model}\0")).unwrap();
        let config_path = dir.join("config.txt");
        std::fs::write(&config_path, config).unwrap();
        Self {
            dir,
            config: config_path,
            board: board_from_model(model),
        }
    }

    fn env(&self) -> CameraEnv {
        let p = |p: PathBuf| p.display().to_string();
        CameraEnv {
            boot_config: Some(self.config.clone()),
            board: self.board,
            helper: repo().join("system/xorgate-camcfg"),
            helper_env: vec![
                ("XORGATE_BOOT_CONFIG".into(), p(self.config.clone())),
                (
                    "XORGATE_CAMCFG_OVERLAYS".into(),
                    p(repo().join("system/xorgate-camcfg.overlays")),
                ),
                ("XORGATE_CAMCFG_MODEL".into(), p(self.dir.join("model"))),
                ("XORGATE_CAMCFG_LOCK".into(), p(self.dir.join("lock"))),
            ],
        }
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.config).unwrap()
    }

    fn apply(&self, claim: &Value) -> (i32, Vec<String>) {
        apply(claim, &self.env())
    }

    fn camera_lines(&self) -> Vec<String> {
        self.read()
            .lines()
            .filter(|l| l.starts_with("dtoverlay=imx"))
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The hardware block of a two-camera model, as a claim carries it.
fn hardware(sensor0: &str, sensor1: &str) -> Value {
    json!({
        "v": 2, "platform": "rpi-cm5",
        "video": [
            {"key": "cam0", "connector": "csi0", "sensor": sensor0, "codec": "h264"},
            {"key": "cam1", "connector": "csi1", "sensor": sensor1, "codec": "h264"}
        ]
    })
}

/// A stock Raspberry Pi OS config.txt: no camera line, auto-detect on.
const STOCK: &str = "# For more options and information see\n# http://rptl.io/configtxt\ndtparam=audio=on\ncamera_auto_detect=1\ndisplay_auto_detect=1\nauto_initramfs=1\ndtoverlay=vc4-kms-v3d\nmax_framebuffers=2\n\n[cm4]\notg_mode=1\n\n[cm5]\ndtoverlay=dwc2,dr_mode=host\n\n[all]\n";

#[test]
fn a_fresh_stock_config_gets_the_claims_lines() {
    // 1. The claim carries the setting (the Register dialog's choice).
    let d = Dev::new(STOCK, MODEL_CM5);
    let claim = json!({
        "hardware": hardware("imx290", "imx290"),
        "cameraSensors": {"cameras": {
            "cam0": {"sensor": "imx708", "options": {"linkFrequency": 447000000}},
            "cam1": {"sensor": "none"}
        }}
    });
    let (code, lines) = d.apply(&claim);
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines,
        [
            "cam0: dtoverlay=imx708,cam0,link-frequency=447000000,rotation=180 written; imx708 by the camera sensor setting",
            "cam1: no change (no camera line; the port is declared empty by the camera sensor setting)",
            CHANGED_LINE,
        ]
    );
    // One line added under [all], auto-detect turned off, nothing else.
    assert_eq!(
        d.read(),
        format!(
            "{}dtoverlay=imx708,cam0,link-frequency=447000000,rotation=180\n",
            STOCK.replace("camera_auto_detect=1", "camera_auto_detect=0")
        )
    );
    assert_eq!(
        std::fs::read_to_string(d.dir.join("config.txt.bak-camcfg")).unwrap(),
        STOCK,
        "the helper's one-time backup is the file as found"
    );

    // 2. No setting in the claim (an older backend): the model's sensors.
    let d = Dev::new(STOCK, MODEL_CM5);
    let (code, lines) = d.apply(&json!({"hardware": hardware("imx290", "imx290")}));
    assert_eq!(code, 0);
    assert_eq!(
        lines,
        [
            "cam0: dtoverlay=imx290,cam0,rotation=0 written; imx290 by the device model",
            "cam1: dtoverlay=imx290,cam1,rotation=0 written; imx290 by the device model",
            CHANGED_LINE,
        ]
    );
    assert_eq!(
        d.camera_lines(),
        [
            "dtoverlay=imx290,cam0,rotation=0",
            "dtoverlay=imx290,cam1,rotation=0"
        ]
    );

    // 3. No hardware block either: the two imx290 lines of every installer.
    let d = Dev::new(STOCK, MODEL_CM4);
    let (code, lines) = d.apply(&json!({"deviceId": "x", "hardware": null}));
    assert_eq!(code, 0);
    assert_eq!(
        lines,
        [
            "cam0: dtoverlay=imx290,cam0,rotation=0 written; imx290 by the default",
            "cam1: dtoverlay=imx290,cam1,rotation=0 written; imx290 by the default",
            CHANGED_LINE,
        ]
    );
}

/// The lines an installer writes are the lines the running agent wants: the
/// agent's own decision (`camera_sensors::line_op`) over the written file,
/// for the setting the backend seeded, asks for nothing.
#[test]
fn what_the_installer_writes_the_agent_does_not_rewrite() {
    use xorgate_core::boot_config::CamPort;
    use xorgate_core::camera_catalog::catalog;
    use xorgate_core::camera_sensors::{line_op, wanted};
    let settings = [
        json!({"cam0": {"sensor": "imx290"}, "cam1": {"sensor": "imx290"}}),
        json!({"cam0": {"sensor": "imx708"}, "cam1": {"sensor": "none"}}),
        json!({"cam0": {"sensor": "imx335", "options": {"linkFrequency": 445500000}}, "cam1": {"sensor": "imx219"}}),
        json!({"cam0": {"sensor": "imx290", "options": {"clockFrequency": 74250000}}}),
    ];
    for cameras in settings {
        let d = Dev::new(STOCK, MODEL_CM5);
        let (code, lines) = d.apply(&json!({
            "hardware": hardware("imx290", "imx290"),
            "cameraSensors": {"cameras": cameras}
        }));
        assert_eq!(code, 0, "{lines:?}");
        let text = d.read();
        for (key, entry) in cameras.as_object().unwrap() {
            let port = if key == "cam0" {
                CamPort::Cam0
            } else {
                CamPort::Cam1
            };
            let want = wanted(entry, catalog()).unwrap();
            // With `cameraMount` absent, and with it at 0 (upright).
            for mount in [None, Some(0)] {
                assert_eq!(
                    line_op(&text, d.board, port, &want, mount, catalog()),
                    Ok(None),
                    "{cameras} {key} mount {mount:?}\n{text}"
                );
            }
        }
    }
}

#[test]
fn a_file_that_already_has_the_lines_is_not_written() {
    // The benches' real files, each with the sensors it really has.
    let cases = [
        (
            "config-bench1-imx290.txt",
            MODEL_CM5,
            json!({"cam0": {"sensor": "imx290"}, "cam1": {"sensor": "imx290"}}),
        ),
        (
            "config-cm4-imx708.txt",
            MODEL_CM4,
            json!({"cam0": {"sensor": "imx708"}}),
        ),
        (
            "config-bench2-imx335-imx219.txt",
            MODEL_CM5,
            json!({"cam0": {"sensor": "imx335"}, "cam1": {"sensor": "imx219"}}),
        ),
    ];
    for (fixture, model, cameras) in cases {
        let config = bench_config(fixture);
        let d = Dev::new(&config, model);
        let (code, lines) = d.apply(&json!({
            "hardware": hardware("imx290", "imx290"),
            "cameraSensors": {"cameras": cameras}
        }));
        assert_eq!(code, 0, "{fixture}: {lines:?}");
        assert_eq!(
            lines.last().map(String::as_str),
            Some(UNCHANGED_LINE),
            "{fixture}: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .take(lines.len() - 1)
                .all(|l| l.contains(": no change (")),
            "{fixture}: {lines:?}"
        );
        assert_eq!(d.read(), config, "{fixture}");
        assert!(
            !d.dir.join("config.txt.bak-camcfg").exists(),
            "{fixture}: no write, no backup"
        );
    }
    // Bench 1 through the model's sensors (no setting in the claim): its two
    // token-less imx290 lines are the model's lines already.
    let config = bench_config("config-bench1-imx290.txt");
    let d = Dev::new(&config, MODEL_CM5);
    let (code, lines) = d.apply(&json!({"hardware": hardware("imx290", "imx290")}));
    assert_eq!(code, 0);
    assert_eq!(
        lines,
        [
            "cam0: no change (dtoverlay=imx290,cam0; imx290 by the device model)",
            "cam1: no change (dtoverlay=imx290,cam1; imx290 by the device model)",
            UNCHANGED_LINE,
        ]
    );
    assert_eq!(d.read(), config);
    // A model's choice names the sensor only: a hand-set parameter and a
    // hand-set rotation on a line of that sensor survive a re-provision.
    let hand = "camera_auto_detect=0\ndtoverlay=imx290,cam0,clock-frequency=74250000,rotation=180\ndtoverlay=imx290,cam1\n";
    let d = Dev::new(hand, MODEL_CM5);
    let (code, lines) = d.apply(&json!({"hardware": hardware("imx290", "imx290")}));
    assert_eq!(
        (code, lines.last().map(String::as_str)),
        (0, Some(UNCHANGED_LINE))
    );
    assert_eq!(d.read(), hand);
}

/// A re-provision never leaves two lines on a port, whatever was there and
/// whatever is chosen now (the append-only render it replaces did).
#[test]
fn a_reprovision_leaves_one_line_per_port() {
    let d = Dev::new(STOCK, MODEL_CM5);
    let first = json!({"hardware": hardware("imx290", "imx290")});
    assert_eq!(d.apply(&first).0, 0);
    let after_first = d.read();
    // The same claim again: nothing written.
    let (code, lines) = d.apply(&first);
    assert_eq!(
        (code, lines.last().map(String::as_str)),
        (0, Some(UNCHANGED_LINE))
    );
    assert_eq!(d.read(), after_first);
    // Another sensor on cam0, cam1 declared empty.
    let second = json!({
        "hardware": hardware("imx290", "imx290"),
        "cameraSensors": {"cameras": {"cam0": {"sensor": "imx219"}, "cam1": {"sensor": "none"}}}
    });
    let (code, lines) = d.apply(&second);
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        lines,
        [
            "cam0: dtoverlay=imx219,cam0,rotation=180 written (was: dtoverlay=imx290,cam0,rotation=0); imx219 by the camera sensor setting",
            "cam1: camera line removed (was: dtoverlay=imx290,cam1,rotation=0); the port is declared empty by the camera sensor setting",
            CHANGED_LINE,
        ]
    );
    assert_eq!(d.camera_lines(), ["dtoverlay=imx219,cam0,rotation=180"]);
    // Back to the model's sensors (a claim without the setting).
    assert_eq!(d.apply(&first).0, 0);
    assert_eq!(
        d.camera_lines(),
        [
            "dtoverlay=imx290,cam0,rotation=0",
            "dtoverlay=imx290,cam1,rotation=0"
        ]
    );
    // A file an older installer left with TWO lines on a port (a sensor
    // swap under the append-only render): collapsed into the chosen one.
    let doubled = "camera_auto_detect=0\ndtoverlay=imx290,cam0\ndtoverlay=imx290,cam1\ndtoverlay=imx708,cam0\n";
    let d = Dev::new(doubled, MODEL_CM5);
    let (code, _) = d.apply(&json!({
        "hardware": hardware("imx290", "imx290"),
        "cameraSensors": {"cameras": {"cam0": {"sensor": "imx708"}, "cam1": {"sensor": "imx290"}}}
    }));
    assert_eq!(code, 0);
    assert_eq!(
        d.read(),
        "camera_auto_detect=0\ndtoverlay=imx290,cam1\ndtoverlay=imx708,cam0,rotation=180\n"
    );
    // A port the setting leaves out is not touched.
    let d = Dev::new(doubled, MODEL_CM5);
    let (code, lines) = d.apply(&json!({
        "hardware": hardware("imx290", "imx290"),
        "cameraSensors": {"cameras": {"cam1": {"sensor": "imx219"}}}
    }));
    assert_eq!(code, 0, "{lines:?}");
    assert_eq!(
        d.read(),
        "camera_auto_detect=0\ndtoverlay=imx290,cam0\ndtoverlay=imx219,cam1,rotation=180\ndtoverlay=imx708,cam0\n"
    );
}

/// What cannot be applied fails that port only (exit 1) and says why.
#[test]
fn a_port_that_cannot_be_applied_fails_alone() {
    let d = Dev::new(STOCK, MODEL_CM5);
    let (code, lines) = d.apply(&json!({
        "hardware": hardware("imx290", "imx290"),
        "cameraSensors": {"cameras": {
            "cam0": {"sensor": "imx999"},
            "cam1": {"sensor": "imx219"},
            "cam5": {"sensor": "imx290"}
        }}
    }));
    assert_eq!(code, 1);
    let dir = d.dir.to_str().unwrap().to_string();
    let lines: Vec<String> = lines.iter().map(|l| l.replace(&dir, "<dir>")).collect();
    assert_eq!(
        lines,
        [
            "cam0: FAILED: this agent build does not know the camera sensor \"imx999\" (it knows: imx290, imx219, imx708, imx335, imx662); update the agent or choose another sensor. config.txt is left as it is",
            // The port was to get a camera and has no line at all: said in
            // so many words, with the way out (the installer's WARN).
            "cam0: NO CAMERA LINE: cam0 has NO camera line in <dir>/config.txt, so no camera will appear on that port (this agent build does not know the camera sensor \"imx999\" (it knows: imx290, imx219, imx708, imx335, imx662); update the agent or choose another sensor. config.txt is left as it is). To fix it: correct the cause, then `xorgate-rs set cameraSensors.cam0.sensor <imx290|imx219|imx708|imx335|imx662>` (the agent writes the line within a minute; reboot after), or add the dtoverlay line for cam0 by hand and reboot",
            "cam1: dtoverlay=imx219,cam1,rotation=180 written; imx219 by the camera sensor setting",
            // A key the device has no port for: nothing to say about a line.
            "cam5: FAILED: the device model declares no camera by that stream key; config.txt is left as it is",
            CHANGED_LINE,
        ]
    );
    // A port that FAILS but still has a line (its old one) is not "no line".
    let d2 = Dev::new("camera_auto_detect=0\ndtoverlay=imx290,cam0\n", MODEL_CM5);
    let (code, lines2) = d2.apply(&json!({
        "hardware": hardware("imx290", "imx290"),
        "cameraSensors": {"cameras": {"cam0": {"sensor": "imx999"}}}
    }));
    assert_eq!(code, 1);
    assert_eq!(lines2.len(), 2, "{lines2:?}");
    assert!(lines2[0].starts_with("cam0: FAILED: "));
    assert_eq!(lines2[1], UNCHANGED_LINE);
    // A port declared empty never gets the warning.
    assert!(!lines
        .iter()
        .chain(&lines2)
        .any(|l| l.starts_with("cam5: NO CAMERA LINE")));
    assert_eq!(d.camera_lines(), ["dtoverlay=imx219,cam1,rotation=180"]);
    // The helper's own refusal (no overlay file on the device), in its words.
    let d = Dev::new(STOCK, MODEL_CM5);
    std::fs::remove_file(d.dir.join("overlays/imx335.dtbo")).unwrap();
    let (code, lines) = d.apply(&json!({
        "hardware": hardware("imx335", "imx290"),
    }));
    assert_eq!(code, 1);
    assert_eq!(
        lines[0].replace(d.dir.to_str().unwrap(), "<dir>"),
        "cam0: FAILED: no overlay file <dir>/overlays/imx335.dtbo on this device"
    );
    assert_eq!(d.camera_lines(), ["dtoverlay=imx290,cam1,rotation=0"]);
}

/// A re-provision resets nothing by itself: a line of the same sensor
/// keeps what the claim does not name (a hand-set option, a parameter the
/// catalog does not list, its rotation), whichever source the choice has.
/// A write that would drop an unlisted parameter is refused, not made.
#[test]
fn a_reprovision_keeps_what_the_claim_does_not_name() {
    let hand = "camera_auto_detect=0\ndtoverlay=imx290,cam0,4lane,rotation=180\ndtoverlay=imx290,cam1,clock-frequency=74250000\n";
    for claim in [
        json!({"hardware": hardware("imx290", "imx290")}),
        json!({
            "hardware": hardware("imx290", "imx290"),
            "cameraSensors": {"cameras": {"cam0": {"sensor": "imx290"}, "cam1": {"sensor": "imx290"}}}
        }),
    ] {
        let d = Dev::new(hand, MODEL_CM5);
        let (code, lines) = d.apply(&claim);
        assert_eq!(
            (code, lines.last().map(String::as_str)),
            (0, Some(UNCHANGED_LINE)),
            "{lines:?}"
        );
        assert_eq!(d.read(), hand);
    }
    // A NAMED option: cam1's line holds only catalog tokens and is written;
    // cam0's would lose `4lane` and is refused (it keeps its line).
    let d = Dev::new(hand, MODEL_CM5);
    let (code, lines) = d.apply(&json!({
        "hardware": hardware("imx290", "imx290"),
        "cameraSensors": {"cameras": {
            "cam0": {"sensor": "imx290", "options": {"clockFrequency": 74250000}},
            "cam1": {"sensor": "imx290", "options": {"clockFrequency": 37125000}}
        }}
    }));
    assert_eq!(code, 1);
    assert_eq!(
        lines,
        [
            "cam0: FAILED: the camera line for cam0 holds \"4lane\", which the sensor catalog does not list for imx290: rewriting the line would drop it, so the line is left as it is. Edit the line by hand, or remove \"4lane\" from it",
            "cam1: dtoverlay=imx290,cam1,clock-frequency=37125000,rotation=0 written (was: dtoverlay=imx290,cam1,clock-frequency=74250000); imx290 by the camera sensor setting",
            CHANGED_LINE,
        ]
    );
    assert_eq!(
        d.read(),
        "camera_auto_detect=0\ndtoverlay=imx290,cam0,4lane,rotation=180\ndtoverlay=imx290,cam1,clock-frequency=37125000,rotation=0\n"
    );
}

/// The test seams of the command (its config.txt, its board file, the
/// helper it runs) are read from the environment only when it is NOT root:
/// a root run, which is how the installer runs it, uses the real paths.
#[test]
fn a_root_run_ignores_the_environment_seams() {
    use xorgate_core::boot_config::CAMCFG_HELPER;
    let seams = |k: &str| match k {
        "XORGATE_CAMCFG" => Some(PathBuf::from("/tmp/evil-helper")),
        "XORGATE_BOOT_CONFIG" => Some(PathBuf::from("/tmp/other-config.txt")),
        _ => None,
    };
    let root = CameraEnv::from_env(true, seams);
    assert_eq!(root.helper, PathBuf::from(CAMCFG_HELPER));
    assert_eq!(root.boot_config, None);
    assert!(root.helper_env.is_empty());
    let user = CameraEnv::from_env(false, seams);
    assert_eq!(user.helper, PathBuf::from("/tmp/evil-helper"));
    assert_eq!(
        user.boot_config,
        Some(PathBuf::from("/tmp/other-config.txt"))
    );
}

#[test]
fn usage_and_unreadable_inputs_exit_2() {
    let d = Dev::new(STOCK, MODEL_CM5);
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(run(&args(&[]), &d.env()), 2);
    assert_eq!(run(&args(&["apply"]), &d.env()), 2);
    assert_eq!(run(&args(&["apply", "--from-claim"]), &d.env()), 2);
    assert_eq!(run(&args(&["status"]), &d.env()), 2);
    let missing = d.dir.join("nope.json").display().to_string();
    assert_eq!(
        run(&args(&["apply", "--from-claim", &missing]), &d.env()),
        2
    );
    let bad = d.dir.join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    assert_eq!(
        run(
            &args(&["apply", "--from-claim", bad.to_str().unwrap()]),
            &d.env()
        ),
        2
    );
    // No config.txt on the device.
    let mut env = d.env();
    env.boot_config = Some(d.dir.join("absent/config.txt"));
    assert_eq!(apply(&json!({}), &env).0, 2);
    assert_eq!(d.read(), STOCK, "nothing was written by any of them");
    // A readable claim through `run`.
    let ok = d.dir.join("claim.json");
    std::fs::write(
        &ok,
        json!({"hardware": hardware("imx290", "imx290")}).to_string(),
    )
    .unwrap();
    assert_eq!(
        run(
            &args(&["apply", "--from-claim", ok.to_str().unwrap()]),
            &d.env()
        ),
        0
    );
    assert_eq!(d.camera_lines().len(), 2);
}

/// The installer's half of the wire: its claim body declares the agent's
/// own capability string (the backend seeds `cameraSensors` only then), and
/// step 8b hands the claim response to this command.
#[test]
fn the_provisioner_declares_the_capability_and_runs_camera_apply() {
    use xorgate_core::config_report::{AGENT_CAPABILITIES, CAPABILITY_CAMERA_SENSORS};
    let script = std::fs::read_to_string(repo().join("scripts/provision-rs.sh")).unwrap();
    let code: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(AGENT_CAPABILITIES.contains(&CAPABILITY_CAMERA_SENSORS));
    assert_eq!(
        code.matches(&format!("capabilities:[\"{CAPABILITY_CAMERA_SENSORS}\"]"))
            .count(),
        1,
        "the claim body carries the capability, once"
    );
    assert_eq!(
        code.matches("\"$RS_PREFIX/bin/xorgate-rs\" camera apply --from-claim \"$RESP\"")
            .count(),
        1
    );
    // The jq render it replaces is gone: no second writer of camera lines.
    assert!(!code.contains("dtoverlay=\\(.sensor)"));
    assert!(!code.contains(">> \"$CONFIG_TXT\"; CAM_OVERLAY_ADDED"));
    // The convert-in-place installer gains no sensor selection: a converted
    // device stays unmanaged.
    let convert = std::fs::read_to_string(repo().join("scripts/install-rs.sh")).unwrap();
    assert!(!convert.contains("camera apply"));
    assert!(!convert.contains("capabilities"));
}
