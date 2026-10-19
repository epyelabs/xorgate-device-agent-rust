//! `xorgate-rs recording reset-counters` — port of
//! `src/commands/recording.ts:1-40`: zero the persistent `droppedSegments`
//! counter (`<home>/recordings/counters.json`). The write is atomic
//! (`atomicWriteJson`, bytes `{"v":1,"droppedSegments":0}` — core
//! `disk_plan::counters_json`) and the running agent re-reads the file on
//! every status tick, so the reset shows within ~5 s without a restart (true
//! for the Rust agent too: `counters_file::load_dropped` on every `stats()`;
//! Q9 keeps the line verbatim).

use std::path::Path;

use super::pipeline::{Console, StdConsole};

/// The usage line (`recording.ts:24-26`), verbatim.
pub const USAGE: &str =
    "usage: xorgate recording reset-counters   (zero the droppedSegments counter)";

/// `xorgate-rs recording …` (raw args after the verb) → exit code.
pub fn main(args: &[String]) -> i32 {
    run(args, &crate::env::runtime_home(), &mut StdConsole)
}

/// `recordingCommand(args)` (`recording.ts:21-40`) for the home `home`
/// (`recordingsRoot()` = `join(runtimeHome(), "recordings")`); a write error
/// is `main().catch`'s `err.message` + exit 1.
pub fn run(args: &[String], home: &Path, c: &mut dyn Console) -> i32 {
    let sub = args.iter().find(|a| !a.starts_with('-'));
    if sub.map(String::as_str) != Some("reset-counters") {
        c.error(USAGE);
        return 1;
    }
    let root = home.join("recordings");
    if !root.exists() {
        c.log("no recordings tree — counters are already 0");
        return 0;
    }
    if let Err(e) = xorgate_hal::counters_file::save_dropped(&root, 0.0) {
        c.error(&e.node_message());
        return 1;
    }
    c.log("✓ droppedSegments reset to 0");
    c.log("  the agent reflects it in state/telemetry within ~5 s (no restart needed)");
    0
}

#[cfg(test)]
mod tests {
    //! The `recording reset-counters` golden (tree absent / present / usage).
    use super::*;
    use crate::cli::pipeline::Captured;

    fn home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("xg-rec-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn tree_absent_present_and_usage() {
        let h = home("golden");
        let mut c = Captured::default();
        assert_eq!(run(&s(&["reset-counters"]), &h, &mut c), 0);
        assert_eq!(c.stdout, "no recordings tree — counters are already 0\n");
        assert!(!h.join("recordings").exists());

        std::fs::create_dir_all(h.join("recordings")).unwrap();
        std::fs::write(
            h.join("recordings/counters.json"),
            r#"{"v":1,"droppedSegments":17}"#,
        )
        .unwrap();
        let mut c = Captured::default();
        assert_eq!(run(&s(&["--x", "reset-counters"]), &h, &mut c), 0);
        assert_eq!(
            c.stdout,
            "✓ droppedSegments reset to 0\n  the agent reflects it in state/telemetry within ~5 s (no restart needed)\n"
        );
        assert_eq!(
            std::fs::read_to_string(h.join("recordings/counters.json")).unwrap(),
            r#"{"v":1,"droppedSegments":0}"#
        );

        for bad in [&[][..], &["reset"][..], &["-h"][..]] {
            let mut c = Captured::default();
            assert_eq!(run(&s(bad), &h, &mut c), 1);
            assert_eq!(c.stderr, format!("{USAGE}\n"));
            assert_eq!(c.stdout, "");
        }
    }
}
