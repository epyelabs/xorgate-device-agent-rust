//! Phase 5b WP3 — the SMS-clear safety greps (phase-5b-js-catchup.md §6
//! points 1 and 2; gate row H11). A workspace-wide source test: it reads
//! every `.rs` file under `crates/` and fails the build when the destructive
//! command could be reached any other way than the two designed entry points.
//!
//! 1. **One literal, one file.** `CMGD` is spelled in production code ONLY in
//!    `crates/xorgate-hal/src/at_clear.rs`, and there exactly once as a
//!    command: `pub const CMGD_DELETE_ALL: &str = "AT+CMGD=0,4";` — every
//!    other mention in that file is that constant's NAME. No other delete
//!    flag (`=…,1|2|3`) or single-index delete can be spelled anywhere, nor
//!    the halves of a split spelling. Test files (`*/tests/**`) and the
//!    replay rig (`crates/xorgate-replay/`, the FakeModem's RECEIVING side,
//!    never shipped in the agent) are exempt.
//! 2. **Two entry points, both explicit.** `clear_transport(` is called only
//!    by `ops/sms.rs` (`SmsReader::clear`); `xorgate_hal::at_clear` is
//!    imported only by `ops/sms.rs` and `cli/sms.rs` (the help text); a
//!    `ClearRequest` — the argument `SmsReader::clear` demands — is built
//!    only by the command plane (`AcceptedCommand`, the `Effect::ClearSms`
//!    interpreter, which runs after the ledger claim) and the CLI
//!    (`ConfirmedCli`, after `--yes` / an interactive yes). Nothing else —
//!    read path, reuse window, sweep, startup, shutdown, manage TUI — can.

use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Every `.rs` under `crates/`, as (workspace-relative path, text).
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&p, root, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.push((rel, std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    let root = workspace();
    let mut out = Vec::new();
    walk(&root.join("crates"), &root, &mut out);
    assert!(
        out.len() > 100,
        "walked the workspace ({} files)",
        out.len()
    );
    out
}

fn exempt(rel: &str) -> bool {
    rel.contains("/tests/") || rel.starts_with("crates/xorgate-replay/")
}

const AT_CLEAR: &str = "crates/xorgate-hal/src/at_clear.rs";
const THE_LINE: &str = r#"pub const CMGD_DELETE_ALL: &str = "AT+CMGD=0,4";"#;

#[test]
fn the_delete_command_is_spelled_once_in_at_clear_only() {
    let mut seen_at_clear = false;
    for (rel, text) in sources() {
        if exempt(&rel) {
            continue;
        }
        if rel == AT_CLEAR {
            seen_at_clear = true;
            // Exactly one command literal, on the const line.
            assert_eq!(
                text.matches("\"AT+CMGD").count(),
                1,
                "{rel}: command literals"
            );
            assert_eq!(text.matches(THE_LINE).count(), 1, "{rel}: the const line");
            // Every other CMGD is the constant's name.
            let names = text.matches("CMGD_DELETE_ALL").count();
            assert_eq!(text.matches("CMGD").count(), names + 1, "{rel}: stray CMGD");
            assert!(
                !text.contains("#[cfg(test)]"),
                "{rel}: no test-only code here"
            );
            continue;
        }
        assert!(
            !text.contains("CMGD"),
            "{rel} names the delete command (only {AT_CLEAR} may)"
        );
        // Split spellings / other delete flags.
        for bad in ["GD=0,", "GD=1", "\"AT+CM\"", "\"+CM\"", "\"CM\""] {
            assert!(!text.contains(bad), "{rel} contains {bad:?}");
        }
    }
    assert!(seen_at_clear, "{AT_CLEAR} exists");
}

/// The no-other-flag rule, on the one file allowed to spell the command:
/// nothing like `=0,1`, `=1,4`, `=<index>` follows the command anywhere.
#[test]
fn only_the_delete_all_form_exists() {
    let text = std::fs::read_to_string(workspace().join(AT_CLEAR)).unwrap();
    for (i, _) in text.match_indices("CMGD=") {
        assert_eq!(&text[i..i + 9], "CMGD=0,4\"", "at byte {i}");
    }
    assert_eq!(xorgate_hal::at_clear::CMGD_DELETE_ALL, "AT+CMGD=0,4");
    assert_eq!(
        xorgate_hal::at_clear::delete_all_text(),
        xorgate_hal::at_clear::CMGD_DELETE_ALL
    );
}

/// Who calls what: the full caller list, so a new caller fails here first.
#[test]
fn the_clear_has_exactly_the_designed_callers() {
    let mut transport_callers = Vec::new();
    let mut at_clear_users = Vec::new();
    let mut request_builders = Vec::new();
    for (rel, text) in sources() {
        if exempt(&rel) {
            continue;
        }
        if rel != AT_CLEAR && text.contains("clear_transport(") {
            transport_callers.push(rel.clone());
        }
        // Code lines only: doc comments may point at the module.
        let code = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        if code.contains("at_clear") && rel != AT_CLEAR && rel != "crates/xorgate-hal/src/lib.rs" {
            at_clear_users.push(rel.clone());
        }
        for v in [
            "ClearRequest::AcceptedCommand",
            "ClearRequest::ConfirmedCli",
        ] {
            for _ in text
                .matches(&format!("{v} {{"))
                .chain(text.matches(&format!("{v})")))
            {
                request_builders.push(format!("{rel} {v}"));
            }
        }
    }
    transport_callers.sort();
    at_clear_users.sort();
    request_builders.sort();
    assert_eq!(transport_callers, ["crates/xorgate-agent/src/ops/sms.rs"]);
    assert_eq!(
        at_clear_users,
        [
            // `xorgate_hal::at_clear::delete_all_text` for `sms clear --help`.
            "crates/xorgate-agent/src/cli/sms.rs",
            "crates/xorgate-agent/src/ops/sms.rs",
        ]
    );
    assert_eq!(
        request_builders,
        [
            "crates/xorgate-agent/src/cli/sms.rs ClearRequest::ConfirmedCli",
            "crates/xorgate-agent/src/command_plane.rs ClearRequest::AcceptedCommand",
        ]
    );
    // The CLI only borrows the TEXT of the command.
    let cli =
        std::fs::read_to_string(workspace().join("crates/xorgate-agent/src/cli/sms.rs")).unwrap();
    assert!(cli.contains("use xorgate_hal::at_clear::delete_all_text;"));
    assert!(!cli.contains("clear_transport") && !cli.contains("CMGD"));
    // The command plane builds its request only in the `clear_sms` arm of
    // `run_sms`, which only `Effect::ClearSms` reaches.
    let cp = std::fs::read_to_string(workspace().join("crates/xorgate-agent/src/command_plane.rs"))
        .unwrap();
    let arm = cp.find("(SmsAction::Clear, SmsDecision::Reader, SmsSource::Real(reader))");
    let req = cp.find("ClearRequest::AcceptedCommand {");
    assert!(arm.is_some_and(|a| req.is_some_and(|r| r > a && r - a < 200)));
}

/// Phase 4's read-only rule for the reader module still holds for the files
/// it covers (the at_port grep test itself is unchanged): the reader module
/// names no delete command — it reaches the clear only through `at_clear`.
#[test]
fn the_reader_module_still_names_no_destructive_command() {
    for rel in [
        "crates/xorgate-hal/src/at_port.rs",
        "crates/xorgate-agent/src/ops/sms.rs",
        "crates/xorgate-core/src/sms.rs",
    ] {
        let text = std::fs::read_to_string(workspace().join(rel)).unwrap();
        for bad in ["CMGR=", "CMGL", "CMGD", "CMGW", "CMSS", "CMGS"] {
            assert!(!text.contains(bad), "{rel} names {bad}");
        }
    }
}
