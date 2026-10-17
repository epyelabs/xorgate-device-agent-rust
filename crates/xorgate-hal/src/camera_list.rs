//! Camera enumeration exec — the I/O half of `ts:src/hardware/cameras.ts`
//! `enumerateCameras` (:74-92): `rpicam-hello --list-cameras` through `PATH`
//! with a 10 s timeout (`defaultExec`, :66-67). The listing parser and the
//! "recognizable output" rule are `xorgate_core::cameras`.

use std::time::Duration;

use xorgate_core::cameras::{enumeration_recognizable, parse_camera_list, EnumeratedCamera};
use xorgate_core::hardware_block::Platform;

use crate::exec::{run, RunResult};
use crate::io::TapeSink;

/// `run("rpicam-hello", ["--list-cameras"], { timeoutMs: 10_000 })`.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// The argv, program first (what the `exec` tape kind records).
pub const LIST_ARGV: [&str; 2] = ["rpicam-hello", "--list-cameras"];

/// `defaultExec()` (`cameras.ts:66-67`): one `rpicam-hello --list-cameras`
/// run, reported to the tape as an `exec` record (stdout + exit code; `None`
/// = timeout / not found / signal).
pub async fn run_list_cameras(tape: &dyn TapeSink) -> RunResult {
    let res = run(LIST_ARGV[0], &LIST_ARGV[1..], Some(LIST_TIMEOUT)).await;
    tape.exec(&LIST_ARGV, &res.stdout, res.exit_code);
    res
}

/// `enumerateCameras(platform, exec)` (`cameras.ts:79-92`) over a finished
/// run: `None` = cannot enumerate (tool missing, or an error WITHOUT any
/// recognizable listing output); `Some(vec![])` = enumerated zero cameras.
///
/// The TS `notFound` is always false under execa 9 + `reject:false` (see
/// `crate::child`), so a missing tool reaches the "unrecognizable" branch
/// there; both routes end in `null`, as here.
pub fn enumerate_from(platform: Platform, res: &RunResult) -> Option<Vec<EnumeratedCamera>> {
    if res.not_found {
        return None;
    }
    let cams = parse_camera_list(&res.stdout, platform);
    if cams.is_empty() {
        let text = format!("{}{}", res.stdout, res.stderr);
        if !enumeration_recognizable(&text) {
            return None;
        }
    }
    Some(cams)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(stdout: &str, stderr: &str, not_found: bool) -> RunResult {
        RunResult {
            stdout: stdout.into(),
            stderr: stderr.into(),
            not_found,
            ..Default::default()
        }
    }

    #[test]
    fn enumerate_rules() {
        assert_eq!(enumerate_from(Platform::RpiCm5, &res("", "", true)), None);
        assert_eq!(
            enumerate_from(Platform::RpiCm5, &res("", "No cameras available!\n", false)),
            Some(vec![])
        );
        assert_eq!(
            enumerate_from(Platform::RpiCm5, &res("", "libcamera: broken\n", false)),
            None
        );
    }
}
