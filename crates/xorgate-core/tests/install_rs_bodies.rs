//! Phase 5 WP1 (CF68): `fixtures/install_rs/bodies.json` is the golden of the
//! bodies `scripts/install-rs.sh` (and the bench `_lib.sh`) write as bash —
//! the `024` grant and the `/usr/local/bin/xorgate-rs` wrapper. This test ties
//! the golden to `update_plan`, so a conversion writes exactly the bytes the
//! first `xorgate-rs update` would write (it then rewrites nothing).
//! `scripts/tests/install-rs-bodies.sh` ties the same golden to the bash.

mod common;

use serde::Deserialize;
use std::collections::BTreeMap;
use xorgate_core::update_plan::{updater_sudoers_body, WRAPPER_BODY};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Bodies {
    generated_by: String,
    note: String,
    wrapper: String,
    updater_sudoers: BTreeMap<String, String>,
    dist_dropin: String,
}

fn golden() -> Bodies {
    serde_json::from_str(&common::fixture_text("install_rs/bodies.json")).expect("bodies.json")
}

#[test]
fn wrapper_equals_update_plan() {
    assert_eq!(golden().wrapper, WRAPPER_BODY);
}

#[test]
fn updater_sudoers_equal_update_plan() {
    let g = golden();
    let users: Vec<&str> = g.updater_sudoers.keys().map(String::as_str).collect();
    assert_eq!(users, ["operator", "pi"]);
    for (user, body) in &g.updater_sudoers {
        assert_eq!(body, &updater_sudoers_body(user), "024 body for {user}");
    }
}

#[test]
fn golden_is_bash_readable() {
    // The bash reader decodes only \n and \" : no other escape may appear.
    let raw = common::fixture_text("install_rs/bodies.json");
    let g = golden();
    assert!(!g.generated_by.is_empty() && !g.note.is_empty());
    let mut values = vec![g.wrapper.as_str(), g.dist_dropin.as_str()];
    values.extend(g.updater_sudoers.values().map(String::as_str));
    for v in values {
        assert!(!v.contains('\\') && !v.contains('\t') && !v.contains('\r'), "{v:?}");
        assert!(v.ends_with('\n'));
    }
    // One value per line, as the bash reader expects.
    for key in ["wrapper", "pi", "operator", "distDropin"] {
        assert_eq!(raw.matches(&format!("\"{key}\": \"")).count(), 1, "{key}");
    }
    // 10-dist.conf (§8): the [Service] override the updater needs (CF63).
    assert!(g
        .dist_dropin
        .ends_with("[Service]\nEnvironment=DEVICE_DIST_VERSION=latest\n"));
}
