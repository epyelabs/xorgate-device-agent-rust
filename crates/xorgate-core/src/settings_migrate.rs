//! One-shot settings.json upgrades (settings-management plan, Phase 4). The
//! pure decision of `src/daemon/settings-migrate.ts:48-89
//! migrateMediaEnabled`; the HAL wrapper reads the raw file, calls
//! [`migrate_media_enabled`], and `saveSettings` the returned document when
//! there is one.
//!
//! Retire the `mediaEnabled` setting (Q4, resolved 2026-08-06): removing the
//! key flips the default (media used to be OFF unless enabled, now ON unless
//! the cameras are individually disabled), so an explicit `false` is
//! converted into every manifest stream key written as `false` under
//! `recording.video.cameras` — a behaviour-preserving conversion, not an
//! approximation. Reads the RAW document because the allowlist has already
//! dropped the key by the time `loadSettings` has run.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::js::object_key_order;
use crate::settings::{parse_settings_object, AgentSettings, RecordingSettings, VideoSettings};

/// `MediaEnabledMigration` (`settings-migrate.ts:12`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum MediaEnabledMigration {
    /// No legacy key present — nothing to do (the steady state).
    None,
    /// The key was `true`; dropped, no behaviour change.
    Dropped,
    /// The key was explicitly `false`; converted to per-camera disables.
    CamerasDisabled { cameras: Vec<String> },
}

/// What the HAL must do after the decision.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaEnabledOutcome {
    pub result: MediaEnabledMigration,
    /// The document to `saveSettings` (None = leave the file untouched).
    pub save: Option<AgentSettings>,
}

/// `migrateMediaEnabled` (`settings-migrate.ts:48`) over the raw file text
/// (`None` = the file could not be read; a corrupt file = `Some("{not json")`).
pub fn migrate_media_enabled(
    raw_text: Option<&str>,
    stream_keys: &[String],
) -> MediaEnabledOutcome {
    let none = MediaEnabledOutcome {
        result: MediaEnabledMigration::None,
        save: None,
    };
    let Some(text) = raw_text else {
        return none; // missing — loadSettings ignores it too
    };
    let Ok(Value::Object(raw)) = serde_json::from_str::<Value>(text) else {
        return none; // corrupt, or not a plain object
    };
    let Some(Value::Bool(legacy)) = raw.get("mediaEnabled") else {
        return none;
    };

    // `loadSettings` has already dropped the key; saving is what makes it stick.
    let mut settings = parse_settings_object(&raw);

    if *legacy {
        return MediaEnabledOutcome {
            result: MediaEnabledMigration::Dropped,
            save: Some(settings),
        };
    }

    let cameras: Vec<String> = stream_keys.to_vec();
    if cameras.is_empty() {
        return MediaEnabledOutcome {
            result: MediaEnabledMigration::CamerasDisabled { cameras },
            save: Some(settings),
        };
    }

    // Existing explicit choices are preserved as keys but forced off: the
    // operator's master switch is the stronger statement of intent.
    // (`{ ...video.cameras, ...Object.fromEntries(cameras.map((k) => [k, false])) }`)
    let recording = settings.recording.take().unwrap_or_default();
    let video = recording.video.clone().unwrap_or_default();
    let mut merged: IndexMap<String, bool> = video.cameras.clone().unwrap_or_default();
    for k in &cameras {
        merged.insert(k.clone(), false);
    }
    settings.recording = Some(RecordingSettings {
        video: Some(VideoSettings {
            cameras: Some(object_key_order(merged)),
            ..video
        }),
        ..recording
    });
    MediaEnabledOutcome {
        result: MediaEnabledMigration::CamerasDisabled { cameras },
        save: Some(settings),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_serde_matches_the_ts_union() {
        assert_eq!(
            serde_json::to_string(&MediaEnabledMigration::None).unwrap(),
            r#"{"result":"none"}"#
        );
        assert_eq!(
            serde_json::to_string(&MediaEnabledMigration::CamerasDisabled {
                cameras: vec!["cam0".into()]
            })
            .unwrap(),
            r#"{"result":"cameras-disabled","cameras":["cam0"]}"#
        );
    }
}
