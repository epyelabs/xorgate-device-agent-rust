//! The I/O half of `ts:src/hardware/boot-config.ts` @89b5c64 (Phase 4 WP3,
//! I7): the reads the camera-mount reconciler interleaves per camera —
//! `resolveBootConfigPath` (`:54-61`, `existsSync`), the config.txt read
//! (`:372-381`, `readFileSync(path, "utf8")` + its error text),
//! `cameraNameFor` (`:182-191`), `dtRotationForCamera`'s file read
//! (`:166-179`) and `probeOverlayRotation` (`:206-215`, `dtoverlay -h`).
//! The parsing and every decision are `xorgate_core::boot_config`; the
//! privileged write is `crate::privileged` (`HelperArgv`).
//!
//! Rust-only since the support-other-camera-models plan (Phase 1): the
//! three-way device-tree read ([`dt_rotation_state`]: a node WITHOUT a
//! `rotation` property is not an unreadable one), the board
//! ([`read_board`]) and a probe that also reads the `Params:` line and the
//! default the help text states (bug B1).
//!
//! THE SAFETY INVARIANT carries over: nothing here can reboot the device.

use std::path::Path;
use std::time::Duration;

use xorgate_core::boot_config::{
    board_from_model, dt_rotation_for_camera, dt_rotation_node, resolve_boot_config_path, Board,
    BOOT_CONFIG_CANDIDATES,
};
use xorgate_core::js;
use xorgate_core::video_capture::camera_file_for;

use crate::exec::run;
use crate::io::TapeSink;

/// `probeOverlayRotation`'s bound (`boot-config.ts:207`, `timeoutMs: 10_000`).
pub const DTOVERLAY_TIMEOUT: Duration = Duration::from_secs(10);

/// `resolveBootConfigPath(candidates)` (`boot-config.ts:54-61`) over the real
/// filesystem (`existsSync`: anything that exists, file or not).
pub fn resolve_boot_config_path_fs<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    resolve_boot_config_path(candidates, |p| Path::new(p).exists())
}

/// `readFileSync(path, "utf8")` (`boot-config.ts:374`): bytes decoded lossily
/// (D4; node's utf8 decode also maps invalid sequences to U+FFFD). `Err` is
/// node's error `message` — it lands verbatim in the cloud-visible
/// `cannot read <path>: <message>` detail (`:378`).
pub fn read_boot_config(path: &str) -> Result<String, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => Err(node_read_error(&e, path)),
    }
}

/// The boot config's text, wherever this OS keeps it
/// ([`BOOT_CONFIG_CANDIDATES`]); `None` when there is no such file or it
/// cannot be read. Read-only (the camera report and the mismatch warning).
pub fn read_boot_config_text() -> Option<String> {
    let path = resolve_boot_config_path_fs(BOOT_CONFIG_CANDIDATES)?;
    read_boot_config(path).ok()
}

/// node's `readFileSync` error text for the errnos a config.txt read can
/// realistically hit (same table as `xorgate-cloud::iot_creds`); anything
/// else falls back to the OS text in node's `<text>, open '<path>'` shape.
fn node_read_error(err: &std::io::Error, path: &str) -> String {
    match err.raw_os_error() {
        Some(2) => format!("ENOENT: no such file or directory, open '{path}'"),
        Some(13) => format!("EACCES: permission denied, open '{path}'"),
        Some(21) => "EISDIR: illegal operation on a directory, read".to_string(),
        Some(20) => format!("ENOTDIR: not a directory, open '{path}'"),
        _ => format!("{err}, open '{path}'"),
    }
}

/// `cameraNameFor(pipelineDir, streamKey)` (`boot-config.ts:182-191`): the
/// enumerated camera identity the hardware reconciler cached this boot in
/// `<pipelineDir>/<key>.camera`, trimmed; None when missing or blank.
pub fn camera_name_for(pipeline_dir: &str, stream_key: &str) -> Option<String> {
    let bytes = std::fs::read(camera_file_for(pipeline_dir, stream_key)).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let t = js::trim(&text);
    (!t.is_empty()).then(|| t.to_string())
}

/// `dtRotationForCamera(cameraName, dtRoot)` (`boot-config.ts:166-179`): the
/// big-endian u32 at `<dtRoot>/<node>/rotation`; None when the node or the
/// property is missing or not 4 bytes (never assume 0).
pub fn dt_rotation_at(camera_name: &str, dt_root: &Path) -> Option<u32> {
    dt_rotation_for_camera(camera_name, |node| {
        std::fs::read(dt_root.join(node).join("rotation")).ok()
    })
}

/// What the live device tree says about a camera node's `rotation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtRotation {
    /// The property is there: its big-endian u32.
    Value(u32),
    /// The node is there and has NO `rotation` property: the overlay's
    /// default is in force (an `imx290` node on some kernels; the caller
    /// supplies the default, it is never assumed to be 0 here).
    Absent,
    /// The node is not there, or the property is not 4 bytes: unknown.
    Unknown,
}

/// The [`DtRotation`] of an enumerated camera (`cameraName` = its libcamera
/// path, `/base/…`).
pub fn dt_rotation_state(camera_name: &str, dt_root: &Path) -> DtRotation {
    let Some(node) = dt_rotation_node(camera_name) else {
        return DtRotation::Unknown;
    };
    let dir = dt_root.join(&node);
    match std::fs::read(dir.join("rotation")) {
        Ok(bytes) => match xorgate_core::boot_config::dt_rotation_from_bytes(&bytes) {
            Some(v) => DtRotation::Value(v),
            None => DtRotation::Unknown,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir.is_dir() => DtRotation::Absent,
        Err(_) => DtRotation::Unknown,
    }
}

/// The board, from `<dtRoot>/model` (`/proc/device-tree/model`): the same
/// file `system/xorgate-camcfg` reads, so both judge config.txt's
/// `[section]` filters alike. Unreadable = [`Board::Unknown`].
pub fn read_board(dt_root: &Path) -> Board {
    match std::fs::read(dt_root.join("model")) {
        Ok(bytes) => board_from_model(&String::from_utf8_lossy(&bytes)),
        Err(_) => Board::Unknown,
    }
}

/// `probeOverlayRotation`'s answer (`boot-config.ts:204`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayProbeResult {
    /// false = the probe itself could not run (no `dtoverlay`, unknown overlay).
    pub ok: bool,
    pub supports: bool,
    /// Rust-only: the default the help text states for `rotation`
    /// (`… (0 or 180, default 180)`), when it states 0 or 180.
    pub default_rotation: Option<u16>,
}

/// `probeOverlayRotation(sensor)` (`boot-config.ts:206-215`): `dtoverlay -h
/// <sensor>` through `PATH` with a 10 s bound; `notFound || failed` →
/// `{ok:false}`; otherwise supports = [`help_declares_rotation`] on stdout.
/// The run is reported to the tape as an `exec` record (stdout + stderr +
/// code). Since the sensor catalog the reconciler asks only for an overlay
/// OUTSIDE the catalog (D96).
pub async fn probe_overlay_rotation(sensor: &str, tape: &dyn TapeSink) -> OverlayProbeResult {
    let argv = ["dtoverlay", "-h", sensor];
    let res = run(argv[0], &argv[1..], Some(DTOVERLAY_TIMEOUT)).await;
    tape.exec_with_stderr(&argv, &res.stdout, &res.stderr, res.exit_code);
    if res.not_found || res.failed {
        return OverlayProbeResult {
            ok: false,
            supports: false,
            default_rotation: None,
        };
    }
    OverlayProbeResult {
        ok: true,
        supports: help_declares_rotation(&res.stdout),
        default_rotation: help_rotation_default(&res.stdout),
    }
}

/// The lines of a `dtoverlay -h` text (JS multiline `^` = after `\n`, `\r`,
/// U+2028, U+2029).
fn help_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\n', '\r', '\u{2028}', '\u{2029}'])
}

/// Does this help line DECLARE the `rotation` parameter? After JS
/// whitespace, and after the `Params:` label when the parameter is the
/// first of the list, the line starts with the word `rotation` (`\b`: not
/// followed by `[A-Za-z0-9_]`).
fn declares_rotation(line: &str) -> bool {
    let rest = js::trim_start(line);
    let rest = rest
        .strip_prefix("Params:")
        .map(js::trim_start)
        .unwrap_or(rest);
    rest.strip_prefix("rotation").is_some_and(|after| {
        !after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Does the overlay's help text declare a `rotation` parameter?
///
/// The TS asks `/^\s*rotation\b/m.test(text)` (`boot-config.ts:213`), which
/// misses the parameter when it is the FIRST of the list: `dtoverlay -h`
/// prints that one on the `Params:` line (`imx708`, `imx219`, `imx477`,
/// `ov5647`; bug B1). This reads the `Params:` line too (D96).
pub fn help_declares_rotation(text: &str) -> bool {
    help_lines(text).any(declares_rotation)
}

/// The default the help text states for `rotation`: the `default <n>` of
/// the parameter's description (`Mounting rotation of the camera sensor (0
/// or 180, default 180)`, which may wrap onto the next, deeper-indented
/// lines). `None` when it is not stated, or is neither 0 nor 180.
pub fn help_rotation_default(text: &str) -> Option<u16> {
    let mut lines = help_lines(text).skip_while(|l| !declares_rotation(l));
    let first = lines.next()?;
    // A description continues on lines indented deeper than a parameter
    // name is (8 columns); the next parameter ends it.
    let indent = |l: &str| l.len() - js::trim_start(l).len();
    let mut desc = first.to_string();
    for l in lines {
        if js::trim(l).is_empty() || indent(l) <= 8 {
            break;
        }
        desc.push(' ');
        desc.push_str(js::trim(l));
    }
    let after = &desc[desc.find("default")? + "default".len()..];
    let digits: String = js::trim_start(after)
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    match digits.as_str() {
        "0" => Some(0),
        "180" => Some(180),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_rotation_word() {
        assert!(help_declares_rotation(
            "Name:   imx290\n\nParams: 4lane    Enable 4 lane\n        rotation  Mounting rotation of the camera sensor (0 or 180, default 0)\n"
        ));
        assert!(help_declares_rotation("rotation"));
        assert!(help_declares_rotation("x\r\t rotation=180"));
        assert!(!help_declares_rotation("        rotations   nope\n"));
        assert!(!help_declares_rotation("        rotation_x  nope\n"));
        assert!(!help_declares_rotation("Params: orientation  …\n"));
        assert!(!help_declares_rotation("        cam0 rotation\n"));
        assert!(!help_declares_rotation(""));
        // B1: the first parameter of the list sits on the `Params:` line.
        assert!(help_declares_rotation(
            "Params: rotation                Mounting rotation of the camera sensor (0 or\n"
        ));
        assert!(!help_declares_rotation("Params: rotations  nope\n"));
        assert!(!help_declares_rotation("Info:   rotation of the earth\n"));
    }

    /// The REAL `dtoverlay -h` texts of the benches (identical on the CM4
    /// and both CM5, read 2026-11-21): every camera overlay declares
    /// `rotation`; four of the six print it on the `Params:` line, which
    /// the TS regex missed (B1).
    #[test]
    fn real_help_texts_declare_rotation_and_state_a_default() {
        let all = include_str!("../../../fixtures/camera_line/dtoverlay-help.txt");
        let mut seen = Vec::new();
        for block in all.split("--- dtoverlay -h ").skip(1) {
            let (name, text) = block.split_once('\n').unwrap();
            assert!(help_declares_rotation(text), "{name}");
            let ts_regex_saw_it = text.lines().any(|l| {
                l.trim_start()
                    .strip_prefix("rotation")
                    .is_some_and(|a| a.starts_with(' '))
            });
            seen.push((name, ts_regex_saw_it, help_rotation_default(text)));
        }
        assert_eq!(
            seen,
            [
                ("imx290", true, Some(0)),
                ("imx219", false, Some(180)),
                ("imx708", false, Some(180)),
                // The help text says 0; the overlay gives 180 (measured). The
                // catalog, not this text, is what an `imx335` line is read by.
                ("imx335", true, Some(0)),
                ("imx477", false, Some(180)),
                ("ov5647", false, Some(0)),
            ]
        );
        assert_eq!(help_rotation_default("Params: rotation  no default here\n"), None);
        assert_eq!(
            help_rotation_default("        rotation  Turn (default 90)\n"),
            None
        );
        assert_eq!(
            help_rotation_default("        orientation  x (default 180)\n"),
            None
        );
        // The next parameter ends the description.
        assert_eq!(
            help_rotation_default(
                "        rotation   Mounting rotation\n        other      thing (default 180)\n"
            ),
            None
        );
    }

    #[test]
    fn read_errors_read_like_node() {
        let dir = std::env::temp_dir().join(format!("xg-bcio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.txt");
        let m = missing.to_str().unwrap();
        assert_eq!(
            read_boot_config(m).unwrap_err(),
            format!("ENOENT: no such file or directory, open '{m}'")
        );
        assert_eq!(
            read_boot_config(dir.to_str().unwrap()).unwrap_err(),
            "EISDIR: illegal operation on a directory, read"
        );
        std::fs::write(dir.join("c.txt"), b"dtoverlay=imx290,cam0\xff\n").unwrap();
        assert_eq!(
            read_boot_config(dir.join("c.txt").to_str().unwrap()).unwrap(),
            "dtoverlay=imx290,cam0\u{fffd}\n"
        );
        // cameraNameFor + dtRotationForCamera over a fake tree.
        let pipes = dir.join("pipelines");
        std::fs::create_dir_all(&pipes).unwrap();
        let node = "/base/soc/i2c0mux/i2c@0/imx290@1a";
        std::fs::write(pipes.join("cam0.camera"), format!("{node}\n")).unwrap();
        std::fs::write(pipes.join("cam1.camera"), "  \n").unwrap();
        let dt = dir.join("device-tree");
        let nd = dt.join("soc/i2c0mux/i2c@0/imx290@1a");
        std::fs::create_dir_all(&nd).unwrap();
        std::fs::write(nd.join("rotation"), 180u32.to_be_bytes()).unwrap();
        let p = pipes.to_str().unwrap();
        assert_eq!(camera_name_for(p, "cam0").as_deref(), Some(node));
        assert_eq!(camera_name_for(p, "cam1"), None);
        assert_eq!(camera_name_for(p, "cam2"), None);
        assert_eq!(dt_rotation_at(node, &dt), Some(180));
        assert_eq!(
            dt_rotation_at("/base/soc/i2c0mux/i2c@7/imx290@1a", &dt),
            None
        );
        // The three-way read: a value, a node without the property, unknown.
        assert_eq!(dt_rotation_state(node, &dt), DtRotation::Value(180));
        let bare = "/base/soc/i2c0mux/i2c@1/imx290@1a";
        std::fs::create_dir_all(dt.join("soc/i2c0mux/i2c@1/imx290@1a")).unwrap();
        assert_eq!(dt_rotation_state(bare, &dt), DtRotation::Absent);
        assert_eq!(dt_rotation_at(bare, &dt), None);
        assert_eq!(
            dt_rotation_state("/base/soc/i2c0mux/i2c@7/imx290@1a", &dt),
            DtRotation::Unknown,
            "no such node"
        );
        std::fs::write(dt.join("soc/i2c0mux/i2c@1/imx290@1a/rotation"), [0u8, 180]).unwrap();
        assert_eq!(dt_rotation_state(bare, &dt), DtRotation::Unknown, "2 bytes");
        assert_eq!(dt_rotation_state("", &dt), DtRotation::Unknown);
        assert_eq!(dt_rotation_state("/base/../../etc", &dt), DtRotation::Unknown);
        // The board, from the model file (NUL-terminated on a real device).
        assert_eq!(read_board(&dt), Board::Unknown, "no model file");
        std::fs::write(dt.join("model"), b"Raspberry Pi Compute Module 5 Rev 1.0\0").unwrap();
        assert_eq!(read_board(&dt), Board::Cm5);
        std::fs::write(dt.join("model"), b"Raspberry Pi Compute Module 4 Rev 1.1\0").unwrap();
        assert_eq!(read_board(&dt), Board::Cm4);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
