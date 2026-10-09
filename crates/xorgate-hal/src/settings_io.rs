//! settings.json I/O — `src/daemon/settings.ts:554-642` (`settingsPath`,
//! `readSettingsFile`, `loadSettings`, `saveSettings`) and
//! `src/agent/config-sync.ts:readSettingsHash` (`:271-279`). The parsing
//! (allowlist, clamps, the plain-object gate) is `xorgate_core::settings`.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use xorgate_core::settings::{
    load_settings_from, settings_from_read, AgentSettings, SettingsRead, SettingsReadResult,
};

use crate::read_lossy::{read_text, FileText};

/// `settingsPath()` (`settings.ts:554`): `<XORGATE_HOME>/settings.json`.
pub fn settings_path(home: &Path) -> PathBuf {
    home.join("settings.json")
}

fn with_read<T>(path: &Path, f: impl FnOnce(SettingsRead<'_>) -> T) -> T {
    match read_text(path) {
        FileText::Absent => f(SettingsRead::Absent),
        FileText::Unreadable(_) => f(SettingsRead::Unreadable),
        FileText::Text(t) => f(SettingsRead::Text(&t)),
    }
}

/// `readSettingsFile` (`settings.ts:589-607`): absent / unreadable / invalid
/// told apart, never throws. The config-sync watcher and `applyCloudConfig`
/// refuse to act on `Unreadable` / `Invalid` (fail closed, invariant 3).
pub fn read_settings_file(path: &Path) -> SettingsReadResult {
    with_read(path, settings_from_read)
}

/// `loadSettings` (`settings.ts:610-613`): the allowlisted document, or
/// `{}` (default) on any failure.
pub fn load_settings(path: &Path) -> AgentSettings {
    with_read(path, load_settings_from)
}

/// The raw file text (`readFileSync(settingsPath(), "utf8")` inside
/// `migrateMediaEnabled`, `settings-migrate.ts:49-54`): `None` when it cannot
/// be read at all.
pub fn read_settings_text(path: &Path) -> Option<String> {
    read_text(path).ok()
}

/// `saveSettings` (`settings.ts:640-642`): `atomicWriteJson` — temp + fsync +
/// rename + dir fsync, and the chown-back to the existing owner when running
/// as root (`sudo xorgate-rs set …`).
pub fn save_settings(path: &Path, settings: &AgentSettings) -> io::Result<()> {
    crate::atomic_write::atomic_write_json(path, settings)
}

/// `readSettingsHash` (`config-sync.ts:271-279`): lowercase hex sha256 of the
/// file's raw bytes, or `""` when missing/unreadable.
pub fn sha256_file(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => hex::encode(Sha256::digest(&bytes)),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_core::settings::SettingsReadFailure;
    use xorgate_core::telemetry::RunMode;

    fn home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// settings.test.ts:13 "round-trips the run mode" (I/O half: the
    /// allowlist itself is pinned by fixtures/settings/settings.json).
    #[test]
    fn round_trips_the_run_mode() {
        let h = home("mode");
        let p = settings_path(&h);
        for m in [RunMode::Real, RunMode::Sim] {
            let s = AgentSettings {
                mode: Some(m),
                ..Default::default()
            };
            save_settings(&p, &s).unwrap();
            assert_eq!(load_settings(&p), s);
            assert_eq!(read_settings_file(&p), Ok(s));
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), r#"{"mode":"sim"}"#);
        let _ = std::fs::remove_dir_all(&h);
    }

    /// settings.test.ts:83 "returns {} when the file is missing" + :87
    /// "returns {} on corrupt or invalid content" + readSettingsFile's
    /// absent / unreadable / invalid split.
    #[test]
    fn absent_unreadable_invalid() {
        let h = home("fail");
        let p = settings_path(&h);
        assert_eq!(load_settings(&p), AgentSettings::default());
        assert_eq!(read_settings_file(&p), Err(SettingsReadFailure::Absent));
        assert_eq!(sha256_file(&p), "");

        std::fs::write(&p, "not json").unwrap();
        assert_eq!(load_settings(&p), AgentSettings::default());
        assert_eq!(read_settings_file(&p), Err(SettingsReadFailure::Invalid));
        std::fs::write(&p, r#"{"mode":"bogus"}"#).unwrap();
        assert_eq!(load_settings(&p), AgentSettings::default());
        assert_eq!(read_settings_file(&p), Ok(AgentSettings::default()));

        // A directory where the file should be: EISDIR → unreadable, not absent.
        std::fs::remove_file(&p).unwrap();
        std::fs::create_dir(&p).unwrap();
        assert_eq!(read_settings_file(&p), Err(SettingsReadFailure::Unreadable));
        assert_eq!(load_settings(&p), AgentSettings::default());
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn sha256_is_of_the_raw_bytes() {
        let h = home("hash");
        let p = settings_path(&h);
        std::fs::write(&p, "{}").unwrap();
        // printf '{}' | sha256sum
        assert_eq!(
            sha256_file(&p),
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn lossy_decode_before_parse() {
        let h = home("lossy");
        let p = settings_path(&h);
        // An invalid byte inside a string value: JSON.parse of the U+FFFD text
        // still succeeds, so the rest of the document survives.
        std::fs::write(&p, b"{\"mode\":\"real\",\"x\":\"\xff\"}").unwrap();
        assert_eq!(load_settings(&p).mode, Some(RunMode::Real));
        let _ = std::fs::remove_dir_all(&h);
    }
}
