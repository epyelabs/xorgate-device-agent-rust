//! review-wp3 N3: `HelperCall::mint` (I5, `pub(crate)`) has EXACTLY two
//! callers — `clock_step::setclock_call` and `boot_config::camrotate_call` —
//! and no other module of the crate builds a `HelperCall` literal. A source
//! scan, so a new mint site fails here instead of silently widening the sudo
//! surface (invariant 4).

use std::path::Path;

#[test]
fn helper_call_mint_has_exactly_the_two_audited_callers() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut mint_calls: Vec<(String, usize)> = Vec::new();
    let mut literals: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let n = text.matches("HelperCall::mint(").count() + text.matches("Self::mint(").count();
        if n > 0 {
            mint_calls.push((name.clone(), n));
        }
        if text.contains("HelperCall {") && name != "commands.rs" {
            literals.push(name);
        }
    }
    mint_calls.sort();
    assert_eq!(
        mint_calls,
        [
            ("boot_config.rs".to_string(), 1),
            ("clock_step.rs".to_string(), 1)
        ]
    );
    assert!(
        literals.is_empty(),
        "HelperCall literal outside commands.rs: {literals:?}"
    );
    // Inside commands.rs the only literal is `mint`'s own body.
    let commands = std::fs::read_to_string(src.join("commands.rs")).unwrap();
    assert_eq!(commands.matches("Self { kind }").count(), 1);
    assert_eq!(commands.matches("pub(crate) fn mint(").count(), 1);
}
