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

/// Every (file, function, token) in production code that names the clear's
/// token or calls a clear with an argument.
fn clear_sites() -> Vec<String> {
    let mut out = Vec::new();
    for (rel, text) in sources() {
        if exempt(&rel) {
            continue;
        }
        let code = code_of(&text);
        let short = rel.trim_start_matches("crates/xorgate-agent/src/");
        // Every mention of the token type, and every constructor call
        // (however the type is spelled or aliased), outside its definition.
        if rel != "crates/xorgate-agent/src/ops/sms.rs" {
            for (at, _) in code.match_indices("ClearRequest") {
                out.push(format!("{short} {} ClearRequest", site(&code, at)));
            }
        }
        for ctor in ["accepted_command(", "confirmed_cli("] {
            for (at, _) in code.match_indices(ctor) {
                let prefix = &code[..at];
                if prefix.ends_with("fn ") {
                    continue; // the definition
                }
                out.push(format!("{short} {} {ctor}", site(&code, at)));
            }
        }
        // Every `.clear(<arg>)` and UFCS `SmsReader::clear(` in the agent
        // crate — the only crate that can see `SmsReader` (core / hal / cloud
        // do not depend on it; the replay rig is exempt) — is listed.
        if !rel.starts_with("crates/xorgate-agent/") {
            continue;
        }
        for (at, _) in code.match_indices(".clear(") {
            let next = code[at + 7..].trim_start();
            if !next.starts_with(')') {
                out.push(format!("{short} {} .clear(arg)", site(&code, at)));
            }
        }
        for (at, _) in code.match_indices("SmsReader::clear") {
            out.push(format!("{short} {} SmsReader::clear", site(&code, at)));
        }
    }
    out.sort();
    out
}

/// Who calls what: the full list, by file and function, so a new caller —
/// however it spells the token — fails here first.
#[test]
fn the_clear_has_exactly_the_designed_callers() {
    assert_eq!(
        clear_sites(),
        [
            // `xorgate-rs sms clear` after `--yes` / an interactive yes.
            "cli/sms.rs <use> ClearRequest",
            "cli/sms.rs clear_main .clear(arg)",
            "cli/sms.rs clear_main ClearRequest",
            "cli/sms.rs clear_main confirmed_cli(",
            // The accepted, ledger-claimed `clear_sms` (Effect::ClearSms).
            "command_plane.rs <use> ClearRequest",
            "command_plane.rs run_sms .clear(arg)",
            "command_plane.rs run_sms ClearRequest",
            "command_plane.rs run_sms accepted_command(",
        ]
    );
    let mut transport_callers = Vec::new();
    let mut at_clear_users = Vec::new();
    for (rel, text) in sources() {
        if exempt(&rel) || rel == AT_CLEAR {
            continue;
        }
        let code = code_of(&text);
        if code.contains("clear_transport(") {
            transport_callers.push(rel.clone());
        }
        if code.contains("at_clear") && rel != "crates/xorgate-hal/src/lib.rs" {
            at_clear_users.push(rel.clone());
        }
    }
    transport_callers.sort();
    at_clear_users.sort();
    assert_eq!(transport_callers, ["crates/xorgate-agent/src/ops/sms.rs"]);
    assert_eq!(
        at_clear_users,
        [
            // `xorgate_hal::at_clear::delete_all_text` for `sms clear --help`.
            "crates/xorgate-agent/src/cli/sms.rs",
            "crates/xorgate-agent/src/ops/sms.rs",
        ]
    );
    // The CLI only borrows the TEXT of the command.
    let cli =
        std::fs::read_to_string(workspace().join("crates/xorgate-agent/src/cli/sms.rs")).unwrap();
    assert!(cli.contains("use xorgate_hal::at_clear::delete_all_text;"));
    assert!(!cli.contains("clear_transport") && !cli.contains("CMGD"));
    // ops/sms.rs: `clear_transport` only inside `clear_body`, reached only
    // from `run_op(Op::Clear)`, which only `clear()` tracks.
    let ops = code_of(
        &std::fs::read_to_string(workspace().join("crates/xorgate-agent/src/ops/sms.rs")).unwrap(),
    );
    let calls: Vec<String> = ops
        .match_indices("clear_transport(")
        .map(|(at, _)| site(&ops, at))
        .collect();
    assert_eq!(calls, ["clear_body"]);
    let tracks: Vec<String> = ops
        .match_indices("Op::Clear, prev")
        .map(|(at, _)| site(&ops, at))
        .collect();
    assert_eq!(tracks, ["clear"], "only clear() starts a Clear op");
}

/// The token has a private field and no Clone/Copy/Default: only the two
/// constructors make one.
#[test]
fn the_token_is_constructor_only() {
    let ops =
        std::fs::read_to_string(workspace().join("crates/xorgate-agent/src/ops/sms.rs")).unwrap();
    let i = ops.find("pub struct ClearRequest {").expect("struct");
    let head = &ops[..i];
    let derive = head[head.rfind("#[derive(").unwrap()..]
        .lines()
        .next()
        .unwrap();
    assert_eq!(derive, "#[derive(Debug)]");
    assert!(ops[i..].starts_with("pub struct ClearRequest {\n    _entry: Entry,\n}"));
    assert!(
        !ops.contains("impl Clone for ClearRequest")
            && !ops.contains("impl Default for ClearRequest")
    );
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
