//! The I/O half of `ts:src/hardware/boot-config.ts` @89b5c64 (Phase 4 WP3,
//! I7): the reads the camera-mount reconciler interleaves per camera —
//! `resolveBootConfigPath` (`:54-61`, `existsSync`), the config.txt read
//! (`:372-381`, `readFileSync(path, "utf8")` + its error text),
//! `cameraNameFor` (`:182-191`), `dtRotationForCamera`'s file read
//! (`:166-179`) and `probeOverlayRotation` (`:206-215`, `dtoverlay -h`).
//! The parsing and every decision are `xorgate_core::boot_config`; the
//! privileged write is `crate::privileged` (`HelperArgv`).
//!
//! THE SAFETY INVARIANT carries over: nothing here can reboot the device.

use std::path::Path;
use std::time::Duration;

use xorgate_core::boot_config::{dt_rotation_for_camera, resolve_boot_config_path};
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

/// `readInForce(opts, streamKey)` (`boot-config.ts:459-463`).
pub fn read_in_force(pipeline_dir: &str, stream_key: &str, dt_root: &Path) -> Option<f64> {
    let name = camera_name_for(pipeline_dir, stream_key)?;
    dt_rotation_at(&name, dt_root).map(f64::from)
}

/// `probeOverlayRotation`'s answer (`boot-config.ts:204`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayProbeResult {
    /// false = the probe itself could not run (no `dtoverlay`, unknown overlay).
    pub ok: bool,
    pub supports: bool,
}

/// `probeOverlayRotation(sensor)` (`boot-config.ts:206-215`): `dtoverlay -h
/// <sensor>` through `PATH` with a 10 s bound; `notFound || failed` →
/// `{ok:false}`; otherwise supports = `/^\s*rotation\b/m` on stdout. The run
/// is reported to the tape as an `exec` record (stdout + stderr + code).
pub async fn probe_overlay_rotation(sensor: &str, tape: &dyn TapeSink) -> OverlayProbeResult {
    let argv = ["dtoverlay", "-h", sensor];
    let res = run(argv[0], &argv[1..], Some(DTOVERLAY_TIMEOUT)).await;
    tape.exec_with_stderr(&argv, &res.stdout, &res.stderr, res.exit_code);
    if res.not_found || res.failed {
        return OverlayProbeResult {
            ok: false,
            supports: false,
        };
    }
    OverlayProbeResult {
        ok: true,
        supports: help_declares_rotation(&res.stdout),
    }
}

/// `/^\s*rotation\b/m.test(text)` (`boot-config.ts:213`): some line (JS
/// multiline `^` = after `\n`, `\r`, U+2028, U+2029) starts — after JS
/// whitespace — with the word `rotation` (`\b`: not followed by `[A-Za-z0-9_]`).
pub fn help_declares_rotation(text: &str) -> bool {
    text.split(['\n', '\r', '\u{2028}', '\u{2029}'])
        .any(|line| {
            let rest = js::trim_start(line);
            rest.strip_prefix("rotation").is_some_and(|after| {
                !after
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        })
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
        assert_eq!(read_in_force(p, "cam0", &dt), Some(180.0));
        assert_eq!(read_in_force(p, "cam1", &dt), None);
        assert_eq!(
            dt_rotation_at("/base/soc/i2c0mux/i2c@7/imx290@1a", &dt),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
