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
/// In-file `#[cfg(test)]` modules are NOT cut: code appended after one would
/// otherwise hide from every pin.
fn code_of(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Offsets of `name` as a whole identifier (bare: a fn path in `.map(…)`,
/// a macro argument, a call — all count).
fn idents(code: &str, name: &str) -> Vec<usize> {
    let b = code.as_bytes();
    code.match_indices(name)
        .map(|(at, _)| at)
        .filter(|&at| {
            (at == 0 || !is_ident(b[at - 1]))
                && b.get(at + name.len()).is_none_or(|&c| !is_ident(c))
        })
        .collect()
}

/// The item an offset sits in: the last `fn` / `struct` / `enum` / `trait`
/// header before it, or `<use>` on an import line.
fn site(code: &str, at: usize) -> String {
    let line_start = code[..at].rfind('\n').map_or(0, |i| i + 1);
    if code[line_start..].trim_start().starts_with("use ")
        || code[line_start..].trim_start().starts_with("pub use ")
    {
        return "<use>".into();
    }
    let before = &code[..at];
    let mut best: Option<(usize, String)> = None;
    for kw in ["fn", "struct", "enum", "trait"] {
        for k in idents(before, kw) {
            let rest = before[k + kw.len()..].trim_start();
            let id: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !id.is_empty() && best.as_ref().is_none_or(|(bk, _)| k > *bk) {
                best = Some((k, id));
            }
        }
    }
    best.map_or("<top>".into(), |(_, id)| id)
}

/// Every bare-identifier mention of `names` in production code (outside
/// `skip`), as `file item name`, sorted.
fn mentions(names: &[&str], skip: &[&str], agent_only: bool) -> Vec<String> {
    let mut out = Vec::new();
    for (rel, text) in sources() {
        if exempt(&rel) || skip.contains(&rel.as_str()) {
            continue;
        }
        if agent_only && !rel.starts_with("crates/xorgate-agent/") {
            continue;
        }
        let code = code_of(&text);
        let short = rel.trim_start_matches("crates/");
        for name in names {
            for at in idents(&code, name) {
                let pre = &code[..at];
                if pre.ends_with("fn ") || pre.ends_with("struct ") || pre.ends_with("enum ") {
                    continue; // a definition
                }
                out.push(format!("{short} {} {name}", site(&code, at)));
            }
        }
    }
    out.sort();
    out
}

/// THE pin (review-wp3 F2, threat model in `sms_clear_token.rs`): every
/// mention of the mint functions, the proof/token types and the core claim
/// — as bare identifiers, so fn paths and macro arguments count — and every
/// clear call with an argument in the agent crate, by file and item.
#[test]
fn the_token_is_minted_only_at_the_two_entry_points() {
    let token = mentions(
        &[
            "from_effect",
            "from_gate",
            "accepted_command",
            "confirmed_cli",
            "delete_all_text",
            "AcceptedClearSms",
            "CliConfirmation",
            "ClearRequest",
        ],
        &[TOKEN],
        false,
    );
    assert_eq!(token, EXPECTED_TOKEN_MENTIONS, "{token:#?}");
    // `Effect::ClearSms` can only be built with a `ClearClaim`, which only
    // core `decide_command` makes.
    let claim = mentions(&["ClearClaim", "after_claim"], &[], false);
    assert_eq!(claim, EXPECTED_CLAIM_MENTIONS, "{claim:#?}");
    // Every clear call with an argument (method or path form) in the agent
    // crate — the only crate that sees `SmsReader`.
    let mut calls = Vec::new();
    for (rel, text) in sources() {
        if exempt(&rel) || !rel.starts_with("crates/xorgate-agent/") {
            continue;
        }
        let code = code_of(&text);
        let short = rel.trim_start_matches("crates/");
        for (at, _) in code.match_indices(".clear(") {
            if !code[at + 7..].trim_start().starts_with(')') {
                calls.push(format!("{short} {} .clear(arg)", site(&code, at)));
            }
        }
        for at in idents(&code, "clear") {
            if code[..at].ends_with("::") {
                calls.push(format!("{short} {} ::clear", site(&code, at)));
            }
        }
    }
    calls.sort();
    assert_eq!(
        calls,
        [
            "xorgate-agent/src/cli/sms.rs clear_main .clear(arg)",
            "xorgate-agent/src/command_plane.rs run_sms .clear(arg)",
        ]
    );
    // The sender: only the reader's clear body calls it.
    let sender = mentions(&["clear_transport"], &[AT_CLEAR], false);
    assert_eq!(
        sender,
        [
            "xorgate-agent/src/ops/sms.rs <use> clear_transport",
            "xorgate-agent/src/ops/sms.rs clear_body clear_transport",
        ]
    );
}

const EXPECTED_TOKEN_MENTIONS: &[&str] = &[
    "xorgate-agent/src/cli/sms.rs <use> ClearRequest",
    "xorgate-agent/src/cli/sms.rs <use> CliConfirmation",
    "xorgate-agent/src/cli/sms.rs <use> delete_all_text",
    "xorgate-agent/src/cli/sms.rs clear_main ClearRequest",
    "xorgate-agent/src/cli/sms.rs clear_main CliConfirmation",
    "xorgate-agent/src/cli/sms.rs clear_main confirmed_cli",
    "xorgate-agent/src/cli/sms.rs clear_main delete_all_text",
    "xorgate-agent/src/cli/sms.rs clear_main from_gate",
    "xorgate-agent/src/command_plane.rs <use> AcceptedClearSms",
    "xorgate-agent/src/command_plane.rs <use> ClearRequest",
    "xorgate-agent/src/command_plane.rs run AcceptedClearSms",
    "xorgate-agent/src/command_plane.rs run from_effect",
    "xorgate-agent/src/command_plane.rs run_sms AcceptedClearSms",
    "xorgate-agent/src/command_plane.rs run_sms ClearRequest",
    "xorgate-agent/src/command_plane.rs run_sms accepted_command",
    "xorgate-agent/src/ops/sms.rs <use> ClearRequest",
    "xorgate-agent/src/ops/sms.rs Work ClearRequest",
    "xorgate-agent/src/ops/sms.rs clear ClearRequest",
    "xorgate-agent/src/ops/sms.rs clear_body ClearRequest",
    "xorgate-hal/src/at_clear.rs <use> ClearRequest",
    "xorgate-hal/src/at_clear.rs clear_transport ClearRequest",
];

const EXPECTED_CLAIM_MENTIONS: &[&str] = &[
    // `impl ClearClaim` (its private `after_claim` is a definition).
    "xorgate-core/src/command_flow.rs ClearClaim ClearClaim",
    "xorgate-core/src/command_flow.rs Effect ClearClaim",
    // core's own unit test (in-file) compares the effect it expects.
    "xorgate-core/src/command_flow.rs clear_sms_acks_then_clears_and_a_duplicate_never_clears ClearClaim",
    "xorgate-core/src/command_flow.rs clear_sms_acks_then_clears_and_a_duplicate_never_clears after_claim",
    "xorgate-core/src/command_flow.rs decide_command ClearClaim",
    "xorgate-core/src/command_flow.rs decide_command after_claim",
];

/// No ordinary code can talk to the modem except the SMS modules
/// (containment for raw writes; a deliberately escaped byte string written
/// through them is out of scope — hostile code, see the threat model).
#[test]
fn the_at_port_api_is_used_only_by_the_sms_modules() {
    let mut files: Vec<String> = mentions(
        &[
            "AtTransport",
            "AtSession",
            "open_serial_transport",
            "open_serial_transport_taped",
        ],
        &[],
        false,
    )
    .into_iter()
    .map(|m| m.split(' ').next().unwrap().to_string())
    .collect();
    files.dedup();
    assert_eq!(
        files,
        [
            "xorgate-agent/src/ops/sms.rs",
            "xorgate-hal/src/at_clear.rs",
            "xorgate-hal/src/at_port.rs",
        ]
    );
}

/// `#![deny(unsafe_code)]` on hal + agent: `transmute` / `zeroed` minting no
/// longer compiles. The exceptions are exactly today's FFI sites.
#[test]
fn unsafe_code_is_denied_except_at_todays_ffi_sites() {
    for root in [
        "crates/xorgate-hal/src/lib.rs",
        "crates/xorgate-agent/src/lib.rs",
        "crates/xorgate-agent/src/main.rs",
    ] {
        let t = std::fs::read_to_string(workspace().join(root)).unwrap();
        assert!(t.contains("\n#![deny(unsafe_code)]\n"), "{root}");
    }
    let mut allows = Vec::new();
    for (rel, text) in sources() {
        if rel.contains("/tests/") {
            continue;
        }
        if !(rel.starts_with("crates/xorgate-hal/") || rel.starts_with("crates/xorgate-agent/")) {
            continue;
        }
        assert!(
            !text.contains("#![allow(unsafe_code)]"),
            "{rel}: crate/module-wide allow"
        );
        assert!(
            !text.contains("allow(unsafe_code)]\nmod"),
            "{rel}: module allow"
        );
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for (at, _) in code.match_indices("#[allow(unsafe_code)]") {
            // The item the attribute sits on: the next `fn` header.
            let rest = &code[at..];
            let k = idents(rest, "fn")[0];
            let id: String = rest[k + 2..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            allows.push(format!("{} {id}", rel.trim_start_matches("crates/")));
        }
    }
    allows.sort();
    assert_eq!(
        allows,
        [
            "xorgate-agent/src/alloc.rs tune",
            "xorgate-agent/src/cli/manage/format.rs clock",
            "xorgate-agent/src/cli/sms.rs stdout_columns",
            // Phase 7c WP3: the IMU reader's one timed signal wait.
            "xorgate-agent/src/imu_reader/sys.rs wait_sigint",
            "xorgate-agent/src/recording/capture/backend.rs renice",
            "xorgate-hal/src/atomic_write.rs is_root",
            "xorgate-hal/src/atomic_write.rs preserve_ownership",
            "xorgate-hal/src/boot_id.rs monotonic_ns",
            "xorgate-hal/src/child.rs drop_kills_a_running_child",
            "xorgate-hal/src/child.rs spawn",
            "xorgate-hal/src/fragment_stat.rs hrtime_ns",
            "xorgate-hal/src/fs_watch.rs pump",
            "xorgate-hal/src/fs_watch.rs watch_dir",
            // Phase 7c WP3: the i2c-dev transport (one libc call each).
            "xorgate-hal/src/i2c_dev.rs read_once",
            "xorgate-hal/src/i2c_dev.rs select_device",
            "xorgate-hal/src/i2c_dev.rs strerror",
            "xorgate-hal/src/i2c_dev.rs write_once",
            "xorgate-hal/src/ledger_file.rs is_root",
            "xorgate-hal/src/ledger_file.rs preserve_ownership",
            "xorgate-hal/src/nmea_tty.rs a_stuck_open_strands_at_most_one_thread",
            "xorgate-hal/src/nmea_tty.rs fifo_streams_chunks_and_stop_closes",
            "xorgate-hal/src/nmea_tty.rs reader_thread",
            "xorgate-hal/src/system.rs os_mem_fallback",
            "xorgate-hal/src/tape.rs host_mono_ms",
        ]
    );
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
    // The only way to get an `Effect::ClearSms`: core's accepted path.
    let cmd = xorgate_core::commands::parse_command(
        br#"{"v":1,"requestId":"r","command":"clear_sms","ts":1750000000000,"expiresAt":1750000060000}"#,
    )
    .unwrap();
    let d = xorgate_core::command_flow::decide_command(
        Some(&cmd),
        1_750_000_001_000,
        &xorgate_core::commands::Ledger::empty(),
        xorgate_core::command_flow::RefreshWindow { now: 0, last: None },
        xorgate_core::telemetry::RunMode::Real,
        false,
    );
    let clear = d
        .effects
        .iter()
        .find(|e| matches!(e, Effect::ClearSms { .. }))
        .expect("accepted clear_sms yields the effect");
    let read = Effect::ReadSms {
        source: SmsSource::Reader,
        request_id: "r".into(),
        limit: 10,
    };
    assert!(AcceptedClearSms::from_effect(clear).is_some());
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

/// Phase 5b WP5 (review-wp3 N5): the accepted-clear claim cannot be COPIED.
/// `ClearClaim` (and so `Effect` and `CommandDecision`, which carry it) is not
/// `Clone`, so a second `Effect::ClearSms` can never be built by cloning the
/// claim of a real accepted clear (review-wp3's remaining survivor (2)). The
/// check is at compile time: if `ClearClaim: Clone`, the `_` below has two
/// candidate impls and the call is ambiguous (E0283), so this file fails to
/// build.
#[test]
fn the_clear_claim_is_not_clone() {
    trait AmbiguousIfClone<A> {
        fn probe() {}
    }
    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    #[allow(dead_code)]
    struct IsClone;
    impl<T: Clone> AmbiguousIfClone<IsClone> for T {}
    <xorgate_core::command_flow::ClearClaim as AmbiguousIfClone<_>>::probe();
    <xorgate_core::command_flow::Effect as AmbiguousIfClone<_>>::probe();
    <xorgate_core::command_flow::CommandDecision as AmbiguousIfClone<_>>::probe();
}
