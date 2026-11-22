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
use xorgate_core::camera_sensors::{apply_plan, line_op, port_boot_line, wanted_for_apply, Wanted};
use xorgate_core::js;

pub const USAGE: &str = "usage: xorgate-rs camera apply --from-claim <claim.json>\n  Writes the camera dtoverlay lines of /boot/firmware/config.txt for the ports the claim\n  response declares (its cameraSensors, else hardware.video[].sensor, else two imx290 lines).\n  Run as root. Never reboots.";

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
}

impl CameraEnv {
    /// From this process. The three variables are the helper's own test
    /// seams (`system/xorgate-camcfg`), which it ignores under sudo: with
    /// them unset (every real run) the real paths are used.
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
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
        }
    }
}

/// `main()` → exit code. `rest` excludes `camera`.
pub fn main(rest: &[String]) -> i32 {
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
    let config = match &env.boot_config {
        Some(p) => Some(p.clone()),
        None => xorgate_hal::boot_config_io::resolve_boot_config_path_fs(BOOT_CONFIG_CANDIDATES)
            .map(PathBuf::from),
    };
    let Some(config) = config.filter(|p| p.is_file()) else {
        eprintln!(
            "xorgate-rs camera apply: no config.txt on this device (looked in /boot/firmware and /boot)"
        );
        return 2;
    };
    let c = catalog();
    let (mut failed, mut changed) = (false, false);
    for choice in apply_plan(&claim) {
        let key = &choice.stream_key;
        let port = match &choice.port {
            Ok(p) => *p,
            Err(why) => {
                println!("{key}: FAILED: {why}; config.txt is left as it is");
                failed = true;
                continue;
            }
        };
        // Read per port: the helper may just have rewritten the file.
        let text = match xorgate_hal::boot_config_io::read_boot_config(&config.to_string_lossy()) {
            Ok(t) => t,
            Err(e) => {
                println!("{key}: FAILED: cannot read {}: {e}", config.display());
                failed = true;
                continue;
            }
        };
        let want = match wanted_for_apply(&choice, port, &text, env.board, c) {
            Ok(w) => w,
            Err(refusal) => {
                println!("{key}: FAILED: {refusal}");
                failed = true;
                continue;
            }
        };
        let was = port_boot_line(&parse_scoped_lines(&text, env.board), port, c).map(|l| l.text);
        let from = choice.source.words();
        let Some(op) = line_op(&text, env.board, port, &want, None, c) else {
            match (&want, &was) {
                (Wanted::Empty, _) => {
                    println!("{key}: no change (no camera line; the port is declared empty by {from})")
                }
                (_, Some(line)) => println!("{key}: no change ({line}; {} by {from})", want.id()),
                (_, None) => println!("{key}: no change"),
            }
            continue;
        };
        if let Err(why) = camcfg_call(op.clone(), c) {
            println!("{key}: FAILED: {why}");
            failed = true;
            continue;
        }
        match run_helper(&env.helper, &op) {
            Ok(true) => {
                changed = true;
                let was = was.map(|l| format!(" (was: {l})")).unwrap_or_default();
                match &op {
                    CamCfgOp::Remove { .. } => println!(
                        "{key}: camera line removed{was}; the port is declared empty by {from}"
                    ),
                    _ => println!(
                        "{key}: dtoverlay={} written{was}; {} by {from}",
                        op.args()[2..].join(",").replacen(',', &format!(",{},", port.token()), 1),
                        want.id()
                    ),
                }
            }
            Ok(false) => println!("{key}: no change"),
            Err(why) => {
                println!("{key}: FAILED: {why}");
                failed = true;
            }
        }
    }
    println!(
        "{}",
        if changed {
            CHANGED_LINE
        } else {
            UNCHANGED_LINE
        }
    );
    i32::from(failed)
}

/// Run the helper with `op`'s arguments. `Ok(true)` = it changed the file
/// (exit 0), `Ok(false)` = already as asked (exit 3), `Err` = its own
/// sentence (stderr, else stdout, else the exit code).
fn run_helper(helper: &Path, op: &CamCfgOp) -> Result<bool, String> {
    let out = xorgate_hal::child::std_command(helper)
        .args(op.args())
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
