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
//! 2. **Two entry points, enforced by the compiler** (review-wp3 F1). The
//!    command constant is private to `at_clear.rs`, sent only by
//!    `clear_transport`, which takes a `ClearRequest` BY VALUE. A
//!    `ClearRequest` can be built only inside `xorgate-hal/src/sms_clear_token.rs`
//!    (private fields, no Clone/Copy/Default), and only from a consumed proof:
//!    `AcceptedClearSms` (only for an `Effect::ClearSms`) or `CliConfirmation`
//!    (only for `--yes` / an interactive yes). The greps below pin the four
//!    mint calls to `command_plane.rs` (`run` mints the proof, `run_sms` the
//!    token) and `cli/sms.rs clear_main`, and the delete literal to
//!    `clear_transport` (+ the help text's `delete_all_text`).

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
const THE_LINE: &str = r#"const CMGD_DELETE_ALL: &str = "AT+CMGD=0,4";"#;
const TOKEN: &str = "crates/xorgate-hal/src/sms_clear_token.rs";

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
    assert_eq!(xorgate_hal::at_clear::delete_all_text(), "AT+CMGD=0,4");
    // The constant is private; inside at_clear.rs it is used only by the
    // sender and by the help-text accessor.
    let code = code_of(&text);
    let uses: Vec<String> = code
        .match_indices("CMGD_DELETE_ALL")
        .map(|(at, _)| site(&code, at))
        .collect();
    assert_eq!(uses, ["<top>", "delete_all_text", "clear_transport"]);
    assert!(code.contains("\nconst CMGD_DELETE_ALL"), "not pub");
    // The sender takes the token by value and spends it.
    assert!(code.contains("    token: ClearRequest,\n) -> Result<ClearOutcome, SmsReadError> {\n"));
}

/// Code only: `//` comment lines dropped (docs may point at the module).
fn code_of(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The function a byte offset sits in: the last `fn <name>` before it, or
/// `<use>` for an import line.
fn site(code: &str, at: usize) -> String {
    let line_start = code[..at].rfind('\n').map_or(0, |i| i + 1);
    if code[line_start..].trim_start().starts_with("use ") {
        return "<use>".into();
    }
    let before = &code[..at];
    let mut name = "<top>".to_string();
    let mut i = 0;
    while let Some(j) = before[i..].find("fn ") {
        let k = i + j;
        let boundary = k == 0
            || !before.as_bytes()[k - 1].is_ascii_alphanumeric()
                && before.as_bytes()[k - 1] != b'_';
        if boundary {
            let rest = &before[k + 3..];
            let id: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !id.is_empty() {
                name = id;
            }
        }
        i = k + 3;
    }
    name
}

/// Every production call of the token module's four mint functions and of
/// the help-text accessor, as `file function call`.
fn mint_sites() -> Vec<String> {
    let mut out = Vec::new();
    for (rel, text) in sources() {
        if exempt(&rel) || rel == TOKEN {
            continue;
        }
        let code = code_of(&text);
        let short = rel.trim_start_matches("crates/xorgate-agent/src/");
        for call in [
            "from_effect(",
            "from_gate(",
            "accepted_command(",
            "confirmed_cli(",
            "delete_all_text(",
        ] {
            for (at, _) in code.match_indices(call) {
                if code[..at].ends_with("fn ") {
                    continue; // a definition (at_clear.rs delete_all_text)
                }
                out.push(format!("{short} {} {call}", site(&code, at)));
            }
        }
    }
    out.sort();
    out
}

/// The mint calls, by file and function: a new one — anywhere, however the
/// type is imported or aliased — fails here.
#[test]
fn the_token_is_minted_only_at_the_two_entry_points() {
    assert_eq!(
        mint_sites(),
        [
            // `xorgate-rs sms clear`: help text, then --yes / interactive yes.
            "cli/sms.rs clear_main confirmed_cli(",
            "cli/sms.rs clear_main delete_all_text(",
            "cli/sms.rs clear_main from_gate(",
            // The accepted, ledger-claimed `clear_sms` (Effect::ClearSms).
            "command_plane.rs run from_effect(",
            "command_plane.rs run_sms accepted_command(",
        ]
    );
    // The sender: only the reader's clear body calls it.
    let mut callers = Vec::new();
    for (rel, text) in sources() {
        if exempt(&rel) || rel == AT_CLEAR {
            continue;
        }
        let code = code_of(&text);
        for (at, _) in code.match_indices("clear_transport(") {
            callers.push(format!("{rel} {}", site(&code, at)));
        }
    }
    assert_eq!(callers, ["crates/xorgate-agent/src/ops/sms.rs clear_body"]);
}

/// The token module itself: private fields, no Clone/Copy/Default, every
/// constructor gated on a proof, no other way in.
#[test]
fn the_token_module_is_sealed() {
    let code = code_of(&std::fs::read_to_string(workspace().join(TOKEN)).unwrap());
    for ty in ["AcceptedClearSms", "CliConfirmation", "ClearRequest"] {
        let i = code.find(&format!("pub struct {ty} {{")).expect(ty);
        let derive = code[..i].rsplit("#[derive(").next().unwrap();
        assert!(derive.starts_with("Debug)]"), "{ty}: derives only Debug");
        assert!(
            code[i..].starts_with(&format!("pub struct {ty} {{\n    _sealed: (),\n}}")),
            "{ty}: one private field"
        );
        for t in ["Clone", "Copy", "Default", "From<"] {
            assert!(
                !code.contains(&format!("impl {t} for {ty}")),
                "{ty}: impl {t}"
            );
        }
    }
    // Every `Self { _sealed` literal sits in one of the gated constructors.
    let mints: Vec<String> = code
        .match_indices("Self { _sealed")
        .map(|(at, _)| site(&code, at))
        .collect();
    assert_eq!(
        mints,
        [
            "from_effect",
            "from_gate",
            "accepted_command",
            "confirmed_cli"
        ]
    );
    assert_eq!(
        code.matches("pub fn ").count(),
        4,
        "four public constructors"
    );
}

/// The proofs exist only for the designed decisions.
#[test]
fn proofs_exist_only_for_an_accepted_clear_or_a_confirmed_cli() {
    use xorgate_core::command_flow::{Effect, SmsSource};
    use xorgate_core::sms_cli::ClearGate;
    use xorgate_hal::sms_clear_token::{AcceptedClearSms, CliConfirmation};
    let clear = Effect::ClearSms {
        source: SmsSource::Reader,
        request_id: "r".into(),
        limit: 10,
    };
    let read = Effect::ReadSms {
        source: SmsSource::Reader,
        request_id: "r".into(),
        limit: 10,
    };
    assert!(AcceptedClearSms::from_effect(&clear).is_some());
    assert!(AcceptedClearSms::from_effect(&read).is_none());
    assert!(AcceptedClearSms::from_effect(&Effect::RequestReport).is_none());
    assert!(CliConfirmation::from_gate(ClearGate::Proceed, None).is_some());
    for yes in ["y", "YES", "  Yes \n"] {
        assert!(
            CliConfirmation::from_gate(ClearGate::Ask, Some(yes)).is_some(),
            "{yes:?}"
        );
    }
    for no in ["n", "", "yess", "ye", "no"] {
        assert!(
            CliConfirmation::from_gate(ClearGate::Ask, Some(no)).is_none(),
            "{no:?}"
        );
    }
    assert!(CliConfirmation::from_gate(ClearGate::Ask, None).is_none());
    assert!(CliConfirmation::from_gate(ClearGate::Help, Some("y")).is_none());
    assert!(CliConfirmation::from_gate(ClearGate::Refuse, Some("y")).is_none());
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
