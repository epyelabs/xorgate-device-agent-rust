//! `xorgate-rs camera apply --from-claim <claim.json>`: write the camera
//! lines of a freshly claimed device (support-other-camera-models Phase 2;
//! RUST-ONLY, no TS counterpart). The installer (`scripts/provision-rs.sh`,
//! step 8b) runs it as root in place of its own `jq` render, so the lines
//! an installer writes and the lines the running agent wants are decided by
//! ONE implementation: the compiled-in sensor catalog,
//! `xorgate_core::camera_sensors::{apply_plan, line_op}` and the
//! camera-line owner `xorgate-camcfg`.
//!
//! What it applies, per camera port (`camera_sensors::apply_plan`):
//! the claim's `cameraSensors` when present, else `hardware.video[].sensor`,
//! else the two `imx290` lines. A port the claim's setting leaves out is
//! not touched. Idempotent: a port whose line already says it is not
//! written, and a re-provision can never leave two lines on a port (the
//! helper's `set` replaces the port's line and deletes any other).
//!
//! It calls the helper directly (it IS root; no sudo), with the helper's
//! own arguments, after `boot_config::camcfg_call`'s checks. It never
//! reboots and writes nothing but through the helper.
//!
//! Exit codes: 0 = every planned port is as declared (the last stdout line
//! says whether config.txt was changed); 1 = some port could not be
//! applied (the others still were); 2 = usage, an unreadable claim, or no
//! config.txt.

use std::path::{Path, PathBuf};

use xorgate_core::boot_config::{
    board_from_model, camcfg_call, parse_scoped_lines, Board, CamCfgOp, BOOT_CONFIG_CANDIDATES,
    CAMCFG_HELPER, PROC_DEVICE_TREE,
};
use xorgate_core::camera_catalog::catalog;
use xorgate_core::camera_sensors::{apply_plan, line_op, port_boot_line, wanted, Wanted};
use xorgate_core::js;
use xorgate_core::settings::clip;

pub const USAGE: &str = "usage: xorgate-rs camera apply --from-claim <claim.json>\n  Writes the camera dtoverlay lines of /boot/firmware/config.txt for the ports the claim\n  response declares (its cameraSensors, else hardware.video[].sensor, else two imx290 lines).\n  Run as root. Never reboots.\n\n       xorgate-rs camera requirements [--install] [--from-claim <claim.json>] [--no-restart] [--quiet]\n  Lists (or, as root with --install, installs) the extra OS packages the declared sensors need.";

/// Marks the line that says a port was left with no camera line at all
/// (the installer turns it into its own WARN).
pub const NO_LINE_MARK: &str = "NO CAMERA LINE";

/// The last stdout line when something was written.
pub const CHANGED_LINE: &str = "config.txt changed: REBOOT required before the cameras stream";
/// The last stdout line when nothing was.
pub const UNCHANGED_LINE: &str = "config.txt unchanged: the camera lines were already as declared";

/// Where the command looks, and how it runs the helper.
pub struct CameraEnv {
    /// config.txt; `None` = the first of [`BOOT_CONFIG_CANDIDATES`] that exists.
    pub boot_config: Option<PathBuf>,
    /// The board, for config.txt's `[section]` filters.
    pub board: Board,
    /// The helper to run.
    pub helper: PathBuf,
    /// Extra environment for the helper (a test seam: the helper's own
    /// seams, per call). Empty in every real run.
    pub helper_env: Vec<(String, String)>,
}

impl CameraEnv {
    /// From this process: the real paths, always, when it runs as root (the
    /// way the installer runs it).
    pub fn from_process() -> Self {
        Self::from_env(nix::unistd::geteuid().is_root(), |k| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
    }

    /// `XORGATE_BOOT_CONFIG`, `XORGATE_CAMCFG_MODEL` and `XORGATE_CAMCFG`
    /// are TEST SEAMS (the first two are the helper's own, which it ignores
    /// under sudo). They are honoured only when the process is NOT root: a
    /// root run never takes the executable it runs, or the file it edits,
    /// from its environment.
    pub fn from_env(is_root: bool, var: impl Fn(&str) -> Option<PathBuf>) -> Self {
        let var = |k: &str| if is_root { None } else { var(k) };
        let board = match var("XORGATE_CAMCFG_MODEL") {
            Some(model) => xorgate_hal::read_lossy::read_text(&model)
                .ok()
                .map(|m| board_from_model(m.trim_end_matches('\0')))
                .unwrap_or(Board::Unknown),
            None => xorgate_hal::boot_config_io::read_board(Path::new(PROC_DEVICE_TREE)),
        };
        Self {
            boot_config: var("XORGATE_BOOT_CONFIG"),
            board,
            helper: var("XORGATE_CAMCFG").unwrap_or_else(|| PathBuf::from(CAMCFG_HELPER)),
            helper_env: Vec::new(),
        }
    }
}

/// `main()` → exit code. `rest` excludes `camera`.
pub fn main(rest: &[String]) -> i32 {
    // `camera requirements …`: the extra OS packages of a sensor.
    if rest.first().map(String::as_str) == Some("requirements") {
        return super::camera_packages::main(&rest[1..]);
    }
    run(rest, &CameraEnv::from_process())
}

pub fn run(rest: &[String], env: &CameraEnv) -> i32 {
    let claim_path = match rest {
        [verb, flag, path] if verb == "apply" && flag == "--from-claim" => path,
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let claim: serde_json::Value = match std::fs::read(claim_path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("xorgate-rs camera apply: cannot read the claim response {claim_path}: {e}");
            return 2;
        }
    };
    let (code, lines) = apply(&claim, env);
    for line in lines {
        if code == 2 {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
    code
}

/// Apply a claim response's camera choice: `(exit code, the lines to
/// print)`. Exit 2 = no config.txt (its one line goes to stderr).
pub fn apply(claim: &serde_json::Value, env: &CameraEnv) -> (i32, Vec<String>) {
    let mut out: Vec<String> = Vec::new();
    macro_rules! say {
        ($($arg:tt)*) => { out.push(format!($($arg)*)) };
    }
    let config = match &env.boot_config {
        Some(p) => Some(p.clone()),
        None => xorgate_hal::boot_config_io::resolve_boot_config_path_fs(BOOT_CONFIG_CANDIDATES)
            .map(PathBuf::from),
    };
    let Some(config) = config.filter(|p| p.is_file()) else {
        return (
            2,
            vec!["xorgate-rs camera apply: no config.txt on this device (looked in /boot/firmware and /boot)".to_string()],
        );
    };
    let c = catalog();
    let (mut failed, mut changed) = (false, false);
    for choice in apply_plan(claim) {
        // A stream key out of the claim is printed clipped.
        let key = &clip(&choice.stream_key);
        let from = choice.source.words();
        // One port: `Ok(true)` = written, `Ok(false)` = already as declared,
        // `Err` = why it could not be applied.
        let mut port_no = None;
        let result: Result<bool, String> = (|| {
            let port = choice
                .port
                .clone()
                .map_err(|why| format!("{why}; config.txt is left as it is"))?;
            port_no = Some(port);
            // Read per port: the helper may just have rewritten the file.
            let text = xorgate_hal::boot_config_io::read_boot_config(&config.to_string_lossy())
                .map_err(|e| format!("cannot read {}: {e}", config.display()))?;
            let want = wanted(&choice.entry, c).map_err(|refusal| refusal.to_string())?;
            let was =
                port_boot_line(&parse_scoped_lines(&text, env.board), port, c).map(|l| l.text);
            // The same decision as the running agent's (`cameraMount` is not
            // known here: a line's own rotation is kept).
            let Some(op) = line_op(&text, env.board, port, &want, None, c)? else {
                match (&want, &was) {
                    (Wanted::Empty, _) => say!(
                        "{key}: no change (no camera line; the port is declared empty by {from})"
                    ),
                    (_, Some(line)) => say!("{key}: no change ({line}; {} by {from})", want.id()),
                    (_, None) => say!("{key}: no change"),
                }
                return Ok(false);
            };
            camcfg_call(op.clone(), c)?;
            let wrote = run_helper(env, &op)?;
            if !wrote {
                say!("{key}: no change");
                return Ok(false);
            }
            let was = was.map(|l| format!(" (was: {l})")).unwrap_or_default();
            match &op {
                CamCfgOp::Remove { .. } => {
                    say!("{key}: camera line removed{was}; the port is declared empty by {from}")
                }
                CamCfgOp::Rotate { rotation, .. } => {
                    say!(
                        "{key}: rotation={rotation} set on the camera line{was}; {} by {from}",
                        want.id()
                    )
                }
                CamCfgOp::Set { .. } => say!(
                    "{key}: dtoverlay={} written{was}; {} by {from}",
                    op.args()[2..]
                        .join(",")
                        .replacen(',', &format!(",{},", port.token()), 1),
                    want.id()
                ),
            }
            Ok(true)
        })();
        match result {
            Ok(wrote) => changed |= wrote,
            Err(why) => {
                failed = true;
                say!("{key}: FAILED: {why}");
                // A port that was to get a camera and now has NO camera line
                // at all: said in so many words, with the way out. (No
                // fallback line is written: a guessed sensor is worse than a
                // port that says it has none.)
                let wants_camera = choice.entry.get("sensor").and_then(|s| s.as_str())
                    != Some(xorgate_core::settings::CAMERA_SENSOR_NONE);
                // Only for a port this device has, whose file can be read.
                let no_line = port_no.is_some_and(|port| {
                    xorgate_hal::boot_config_io::read_boot_config(&config.to_string_lossy())
                        .map(|t| {
                            port_boot_line(&parse_scoped_lines(&t, env.board), port, c).is_none()
                        })
                        .unwrap_or(false)
                });
                if wants_camera && no_line {
                    say!(
                        "{key}: {NO_LINE_MARK}: {key} has NO camera line in {}, so no camera will appear on that port ({why}). To fix it: correct the cause, then `xorgate-rs set cameraSensors.{key}.sensor <{}>` (the agent writes the line within a minute; reboot after), or add the dtoverlay line for {key} by hand and reboot",
                        config.display(),
                        c.sensors.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join("|")
                    );
                }
            }
        }
    }
    out.push(
        if changed {
            CHANGED_LINE
        } else {
            UNCHANGED_LINE
        }
        .to_string(),
    );
    (i32::from(failed), out)
}

/// Run the helper with `op`'s arguments. `Ok(true)` = it changed the file
/// (exit 0), `Ok(false)` = already as asked (exit 3), `Err` = its own
/// sentence (stderr, else stdout, else the exit code).
fn run_helper(env: &CameraEnv, op: &CamCfgOp) -> Result<bool, String> {
    let helper = env.helper.as_path();
    let out = xorgate_hal::child::std_command(helper)
        .args(op.args())
        .envs(env.helper_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run {}: {e}", helper.display()))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(3) => Ok(false),
        code => {
            let (e, o) = (
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&out.stdout),
            );
            let (e, o) = (js::trim(&e), js::trim(&o));
            Err(if !e.is_empty() {
                e.to_string()
            } else if !o.is_empty() {
                o.to_string()
            } else {
                match code {
                    Some(c) => format!("{} exit {c}", helper.display()),
                    None => format!("{} was killed", helper.display()),
                }
            })
        }
    }
}
