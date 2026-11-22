//! Phase 5 WP1 (CF68): `fixtures/install_rs/bodies.json` is the golden of the
//! bodies `scripts/install-rs.sh` (and the bench `_lib.sh`) write as bash —
//! the `024` grant and the `/usr/local/bin/xorgate-rs` wrapper. This test ties
//! the golden to `update_plan`, so a conversion writes exactly the bytes the
//! first `xorgate-rs update` would write (it then rewrites nothing).
//! `scripts/tests/install-rs-bodies.sh` ties the same golden to the bash.
//!
//! Phase 8 WP-R1: the native provisioner (`scripts/provision-rs.sh`) writes
//! sudoers `021`/`023` too (the update rail rewrites them on a Rust-only
//! device, `update_plan::asset_policy`), so their golden bodies are tied to
//! `update_plan` here as well; `scripts/tests/provision-rs-bodies.sh` ties
//! every provisioner body to the golden, and the JS-shared ones to the JS
//! install.sh golden.

mod common;

use serde::Deserialize;
use std::collections::BTreeMap;
use xorgate_core::update_plan::{
    camcfg_sudoers_body, camrotate_sudoers_body, clock_sudoers_body, updater_sudoers_body,
    WRAPPER_BODY,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Bodies {
    generated_by: String,
    note: String,
    wrapper: String,
    updater_sudoers: BTreeMap<String, String>,
    dist_dropin: String,
    clock_sudoers: BTreeMap<String, String>,
    camrotate_sudoers: BTreeMap<String, String>,
    camcfg_sudoers: BTreeMap<String, String>,
    power_sudoers: BTreeMap<String, String>,
    user_dropin: BTreeMap<String, String>,
    native_dropin: String,
    native_generator: String,
    native_dist_dropin: String,
    logind_remove_ipc: String,
    uplink_priority: String,
    apt_lock_timeout: String,
}

fn golden() -> Bodies {
    serde_json::from_str(&common::fixture_text("install_rs/bodies.json")).expect("bodies.json")
}

#[test]
fn wrapper_equals_update_plan() {
    assert_eq!(golden().wrapper, WRAPPER_BODY);
}

fn per_user(m: &BTreeMap<String, String>, what: &str, body: fn(&str) -> String) {
    let users: Vec<&str> = m.keys().map(String::as_str).collect();
    assert_eq!(users, ["operator", "pi"], "{what}");
    for (user, b) in m {
        assert_eq!(b, &body(user), "{what} body for {user}");
    }
}

#[test]
fn updater_sudoers_equal_update_plan() {
    per_user(&golden().updater_sudoers, "024", updater_sudoers_body);
}

/// WP-R1: the provisioner's `021`/`023` are the bytes `xorgate-rs update`
/// writes on a Rust-only device, so the first update rewrites nothing.
#[test]
fn clock_and_camrotate_sudoers_equal_update_plan() {
    let g = golden();
    per_user(&g.clock_sudoers, "021", clock_sudoers_body);
    per_user(&g.camrotate_sudoers, "023", camrotate_sudoers_body);
}

/// The camera-line owner's `025` (Rust-only; support-other-camera-models
/// Phase 1): the converter and the provisioner write the bytes `xorgate-rs
/// update` writes on every device.
#[test]
fn camcfg_sudoers_equal_update_plan() {
    per_user(&golden().camcfg_sudoers, "025", camcfg_sudoers_body);
}

#[test]
fn golden_is_bash_readable() {
    // The bash reader decodes only \n and \" : no other escape may appear.
    let raw = common::fixture_text("install_rs/bodies.json");
    let g = golden();
    assert!(!g.generated_by.is_empty() && !g.note.is_empty());
    let mut values = vec![
        g.wrapper.as_str(),
        g.dist_dropin.as_str(),
        g.native_dropin.as_str(),
        g.native_generator.as_str(),
        g.native_dist_dropin.as_str(),
        g.logind_remove_ipc.as_str(),
        g.uplink_priority.as_str(),
        g.apt_lock_timeout.as_str(),
    ];
    for m in [
        &g.updater_sudoers,
        &g.clock_sudoers,
        &g.camrotate_sudoers,
        &g.camcfg_sudoers,
        &g.power_sudoers,
        &g.user_dropin,
    ] {
        values.extend(m.values().map(String::as_str));
    }
    for v in values {
        assert!(
            !v.contains('\\') && !v.contains('\t') && !v.contains('\r'),
            "{v:?}"
        );
        assert!(v.ends_with('\n'));
    }
    // One value per line, as the bash reader expects: a top-level key at a
    // 2-space indent, a per-user value at a 4-space indent inside its object.
    for key in [
        "wrapper",
        "distDropin",
        "nativeDropin",
        "nativeGenerator",
        "nativeDistDropin",
        "logindRemoveIpc",
        "uplinkPriority",
        "aptLockTimeout",
    ] {
        assert_eq!(
            raw.matches(&format!("\n  \"{key}\": \"")).count(),
            1,
            "{key}"
        );
    }
    for obj in [
        "updaterSudoers",
        "clockSudoers",
        "camrotateSudoers",
        "camcfgSudoers",
        "powerSudoers",
        "userDropin",
    ] {
        let start = raw
            .find(&format!("\n  \"{obj}\": {{\n"))
            .unwrap_or_else(|| panic!("{obj}"));
        let body = &raw[start..];
        let body = &body[..body.find("\n  }").expect("object end")];
        for user in ["pi", "operator"] {
            assert_eq!(
                body.matches(&format!("\n    \"{user}\": \"")).count(),
                1,
                "{obj}.{user}"
            );
        }
    }
    // 10-dist.conf (§8): the [Service] override the updater needs (CF63);
    // the native copy says the same (WP-R1).
    for d in [&g.dist_dropin, &g.native_dist_dropin] {
        assert!(d.ends_with("[Service]\nEnvironment=DEVICE_DIST_VERSION=latest\n"));
    }
    // 20-native.conf (D1.3): the condition that lets the JS installer win.
    assert!(g
        .native_dropin
        .ends_with("[Unit]\nConditionPathExists=!/opt/xorgate\n"));
    // The boot-time enablement (WP-R1): wanted only while /opt/xorgate is absent.
    assert!(g.native_generator.starts_with("#!/bin/sh\n"));
    // review-wp-r1 N3: run by hand with no output dir it does nothing.
    assert!(g.native_generator.contains("\n[ -n \"$1\" ] || exit 0\n"));
    assert!(g
        .native_generator
        .contains("\n[ -e /opt/xorgate ] && exit 0\nmkdir -p \"$1/multi-user.target.wants\""));
}
