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
/// run, reported to the tape as an `exec` record (stdout + stderr + exit
/// code; `None` = timeout / not found / signal).
pub async fn run_list_cameras(tape: &dyn TapeSink) -> RunResult {
    let res = run(LIST_ARGV[0], &LIST_ARGV[1..], Some(LIST_TIMEOUT)).await;
    tape_list_result(tape, &res);
    res
}

/// The `exec` tape record of one listing run. `rpicam-hello` prints
/// `No cameras available!` on STDERR (the "recognizable" rule reads
/// stdout + stderr, `cameras.ts:86-90`), so the record carries stderr too —
/// the replay shim prints it back on stderr (review-wp5 S1).
fn tape_list_result(tape: &dyn TapeSink, res: &RunResult) {
    tape.exec_with_stderr(&LIST_ARGV, &res.stdout, &res.stderr, res.exit_code);
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

    /// argv, stdout, stderr, code of one taped run.
    type ExecRec = (Vec<String>, String, String, Option<i32>);

    #[derive(Default)]
    struct Rec(std::sync::Mutex<Vec<ExecRec>>);
    impl TapeSink for Rec {
        fn exec(&self, _argv: &[&str], _stdout: &str, _code: Option<i32>) {
            panic!("the listing must be taped through exec_with_stderr");
        }
        fn exec_with_stderr(&self, argv: &[&str], stdout: &str, stderr: &str, code: Option<i32>) {
            self.0.lock().unwrap().push((
                argv.iter().map(|a| a.to_string()).collect(),
                stdout.into(),
                stderr.into(),
                code,
            ));
        }
    }

    #[test]
    fn listing_is_taped_with_stderr() {
        let tape = Rec::default();
        let mut r = res("", "No cameras available!\n", false);
        r.exit_code = Some(255);
        tape_list_result(&tape, &r);
        let got = tape.0.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![(
                vec!["rpicam-hello".to_string(), "--list-cameras".to_string()],
                String::new(),
                "No cameras available!\n".to_string(),
                Some(255)
            )]
        );
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
