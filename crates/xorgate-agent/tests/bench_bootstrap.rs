//! WP6 (CF47 bench half): the bench bootstrap's `024_xorgate-update-rs`
//! (`scripts/bench/_lib.sh` `update_sudoers_body`, written by install.sh 3b)
//! is byte-for-byte WP4's `updater_sudoers_body` — one body, two writers
//! (the bench bootstrap, then every `xorgate-rs update`).

use xorgate_core::update_plan::updater_sudoers_body;

#[test]
fn bench_bootstrap_024_body_equals_the_update_rail_body() {
    let lib = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/bench/_lib.sh"
    ))
    .expect("scripts/bench/_lib.sh");
    let start = lib
        .find("update_sudoers_body() {")
        .expect("update_sudoers_body in _lib.sh");
    let func = &lib[start..start + lib[start..].find("\n}\n").expect("function end")];
    // The bash function prints each quoted line with `printf '%s\n'`, `$1` = the user.
    let printed: Vec<&str> = func
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let l = l.strip_prefix('"')?;
            l.trim_end_matches('\\').trim_end().strip_suffix('"')
        })
        .collect();
    let want = updater_sudoers_body("$1");
    let want: Vec<&str> = want.lines().collect();
    assert_eq!(printed, want);
}
