//! `config-sync.json` I/O — `src/agent/config-sync.ts:192-246`
//! (`configSyncPath`, `loadConfigSyncState`, `saveConfigSyncState`) and the
//! I/O half of `ensureConfigSyncBaseline` (`:328-341`). The validating parse,
//! the byte shape and the baseline decision are `xorgate_core::config_report`
//! ([`parse_config_sync_state`], [`ConfigSyncState::to_file_string`],
//! [`ensure_config_sync_baseline`](xorgate_core::config_report::ensure_config_sync_baseline)).
//!
//! The sync bookkeeping lives in its OWN file beside settings.json, never
//! inside it (the allowlist rewriter would destroy it; the modem daemon greps
//! settings.json by exact path).

use std::path::{Path, PathBuf};

use xorgate_core::config_report::{self, parse_config_sync_state, ConfigSyncState};

use crate::atomic_write::atomic_write_bytes;
use crate::read_lossy::{read_text, FileText};
use crate::settings_io::sha256_file;

/// `configSyncPath()` (`config-sync.ts:192-194`): `<XORGATE_HOME>/config-sync.json`.
pub fn config_sync_path(home: &Path) -> PathBuf {
    home.join("config-sync.json")
}

/// `loadConfigSyncState(path)` (`config-sync.ts:201-230`): the sync state, or
/// zeros when absent / unreadable / invalid. NEVER fails — a lost file must
/// degrade to "never applied anything". Bytes are decoded lossily (CF21).
pub fn load_config_sync_state(path: &Path) -> ConfigSyncState {
    match read_text(path) {
        FileText::Text(t) => parse_config_sync_state(Some(&t)),
        _ => parse_config_sync_state(None),
    }
}

/// `saveConfigSyncState(state, path)` (`config-sync.ts:237-246`):
/// `atomicWriteJson` of the state (compact `JSON.stringify`, JS key order);
/// `false` instead of an error when the write fails — an unwritable home must
/// not break config delivery, only reset the rev guard on the next boot.
pub fn save_config_sync_state(state: &ConfigSyncState, path: &Path) -> bool {
    atomic_write_bytes(path, state.to_file_string().as_bytes()).is_ok()
}

/// `ensureConfigSyncBaseline(statePath, file)` (`config-sync.ts:328-341`):
/// give the sync file a namespace baseline from settings.json as it stands
/// right now, if it has none. `loadSettings` semantics for the file (`{}` —
/// `AgentSettings::default()` — when absent / unreadable / invalid, CF15) and
/// `readSettingsHash` (`""` when unreadable). Call at startup BEFORE anything
/// else writes settings.json, and never later.
pub fn ensure_config_sync_baseline(state_path: &Path, settings_file: &Path) -> ConfigSyncState {
    let state = load_config_sync_state(state_path);
    if state.namespaces.is_some() {
        return state;
    }
    // RUST-ONLY guard (D118): no baseline is made from a settings.json that
    // EXISTS but cannot be read or parsed. The TS reads it as `{}` here
    // (CF15), so when the file reads again every namespace it holds looks
    // like a local change and is pushed over the cloud's values. Without a
    // baseline nothing is reported as changed; the watcher adopts the file
    // silently at the first tick on which it reads
    // (`config_sync_rules::check_local_change`, "never baselined").
    let read = crate::settings_io::read_settings_file(settings_file);
    if matches!(
        read,
        Err(xorgate_core::settings::SettingsReadFailure::Unreadable
            | xorgate_core::settings::SettingsReadFailure::Invalid)
    ) {
        return state;
    }
    let (next, save) = config_report::ensure_config_sync_baseline(
        state,
        &read.unwrap_or_default(),
        &sha256_file(settings_file),
    );
    if save {
        save_config_sync_state(&next, state_path);
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_core::config_report::SyncPatch;

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-csf-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// D118 (RUST-ONLY): no baseline is made from a settings.json that
    /// exists but cannot be read or parsed (the TS makes an empty one, and
    /// every namespace then looks locally changed once the file reads). It
    /// waits: made at the next call on which the file reads, or by the
    /// watcher's first readable tick. An absent file still baselines `{}`.
    #[test]
    fn no_baseline_from_a_file_that_cannot_be_read_or_parsed() {
        let h = home("d118");
        let (state, settings) = (config_sync_path(&h), h.join("settings.json"));
        for bad in ["{not json", "[1]", ""] {
            std::fs::write(&settings, bad).unwrap();
            let s = ensure_config_sync_baseline(&state, &settings);
            assert!(s.namespaces.is_none(), "{bad:?}");
            assert!(!state.exists(), "{bad:?}: nothing was persisted");
        }
        // Unreadable: a directory in its place.
        std::fs::remove_file(&settings).unwrap();
        std::fs::create_dir(&settings).unwrap();
        assert!(ensure_config_sync_baseline(&state, &settings)
            .namespaces
            .is_none());
        assert!(!state.exists());
        std::fs::remove_dir(&settings).unwrap();
        // The file reads: the baseline is what it holds.
        std::fs::write(&settings, r#"{"gnssAntBias":true,"mode":"real"}"#).unwrap();
        let s = ensure_config_sync_baseline(&state, &settings);
        assert_eq!(
            serde_json::to_string(&s.namespaces).unwrap(),
            r#"{"gnssAntBias":true}"#
        );
        assert!(state.exists());
        // Absent, on a device that never had one: an empty baseline, as before.
        let h2 = home("d118-absent");
        let s = ensure_config_sync_baseline(&config_sync_path(&h2), &h2.join("settings.json"));
        assert_eq!(serde_json::to_string(&s.namespaces).unwrap(), "{}");
    }

    /// config-sync.test.ts:47 "round-trips and defaults to zeros when absent"
    /// + :53 "treats a corrupt or mistyped file as never-applied" (file half).
    #[test]
    fn round_trip_and_zeros() {
        let h = home("rt");
        let p = config_sync_path(&h);
        assert_eq!(load_config_sync_state(&p), ConfigSyncState::default());
        let mut s = ConfigSyncState::default();
        s.merge([SyncPatch::LastAppliedRev(42), SyncPatch::LastReportTs(1700)]);
        assert!(save_config_sync_state(&s, &p));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            r#"{"lastAppliedRev":42,"lastReportTs":1700}"#
        );
        assert_eq!(load_config_sync_state(&p), s);
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(load_config_sync_state(&p), ConfigSyncState::default());
        std::fs::write(&p, r#"{"lastAppliedRev":"12"}"#).unwrap();
        assert_eq!(load_config_sync_state(&p), ConfigSyncState::default());
        let _ = std::fs::remove_dir_all(&h);
    }

    /// config-sync.test.ts:60 "lives beside settings.json, never inside it".
    #[test]
    fn lives_beside_settings_json() {
        let h = Path::new("/h");
        assert_eq!(config_sync_path(h), PathBuf::from("/h/config-sync.json"));
        assert_ne!(config_sync_path(h), crate::settings_io::settings_path(h));
    }

    /// config-sync.test.ts:67 "reports a failed persist instead of throwing".
    #[test]
    fn failed_persist_is_false() {
        let mut s = ConfigSyncState::default();
        s.merge([SyncPatch::LastAppliedRev(1)]);
        assert!(!save_config_sync_state(
            &s,
            Path::new("/nope/does/not/exist.json")
        ));
    }

    /// config-sync.test.ts ensureConfigSyncBaseline "adopts the current file
    /// once, and never re-adopts it" + "leaves a Phase-3 sync file's rev intact"
    /// (file half) — CF15: an unreadable settings.json baselines as `{}`.
    #[test]
    fn baseline_adopts_once() {
        let h = home("bl");
        let (p, s) = (config_sync_path(&h), h.join("settings.json"));
        std::fs::write(&p, r#"{"lastAppliedRev":12,"lastReportTs":5}"#).unwrap();
        std::fs::write(&s, r#"{"mode":"real","gnssAntBias":true}"#).unwrap();
        let first = ensure_config_sync_baseline(&p, &s);
        assert_eq!(first.last_applied_rev, 12);
        assert_eq!(
            first.namespaces.as_ref().unwrap(),
            serde_json::json!({"gnssAntBias": true})
                .as_object()
                .unwrap()
        );
        std::fs::write(&s, r#"{"mode":"real","gnssAntBias":false}"#).unwrap();
        // Re-loaded from the file: same values; the key order is the
        // loader's (`settingsHash` before `namespaces`, `config-sync.ts:220-226`).
        let again = ensure_config_sync_baseline(&p, &s);
        assert_eq!(again, load_config_sync_state(&p));
        assert_eq!(
            (again.namespaces.as_ref(), again.settings_hash.as_ref()),
            (first.namespaces.as_ref(), first.settings_hash.as_ref())
        );
        // Absent settings.json: `{}` and hash "".
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&s);
        let b = ensure_config_sync_baseline(&p, &s);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            r#"{"lastAppliedRev":0,"lastReportTs":0,"namespaces":{},"settingsHash":""}"#
        );
        assert!(b.namespaces.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&h);
    }
}
