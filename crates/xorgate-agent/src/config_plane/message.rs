//! `handleConfigMessage` (`src/worker.ts:781-900`) and the I/O of
//! `applyCloudConfig` (`src/agent/cloud-config.ts:293-411`).
//!
//! ORDER (the reviewer checks it against the TS line by line):
//! 1. lossy-decode + `parseCloudMessage`; malformed → log + event, return;
//! 2. `uploadBucket` → kvs.json (skipped offline; `applied` → log + event +
//!    the upload re-arm hook, Phase 3);
//! 3. `scope` → `telemetryScope.applyScope` (+ `noteScopeChange`);
//! 4. `hardware` — `null` → warn + event; changed → hardware.json + log +
//!    event + the re-render hook (Phase 3);
//! 5. the rev guard → `applyCloudConfig` → `cloud config applied: …` (log +
//!    event + the LED hook, Phase 4) — or `rev … already applied`;
//! 6. the reapply hooks on BOTH paths (`recording.reapply`,
//!    `reconcileCameraMountNow`);
//! 7. a thrown apply → `cloud config apply failed: …`;
//! 8. ALWAYS `noteCloudConfig(rev, raw, parsed, applied)` + `request("cloud")`.
//!
//! Steps 2–4 sit OUTSIDE the rev guard on purpose: envelope facts, not
//! settings, and after a restart the guarded path never re-delivers them.

use std::path::Path;
use std::sync::Arc;

use tracing::{error, info, warn};
use xorgate_core::cloud_config::{merge, parse_cloud_message, CloudConfig};
use xorgate_core::config_report::{adoption_subset, unheld_adopt_log, ReportOrigin};
use xorgate_core::hardware_block::{
    hardware_blocks_equal, hardware_warn_line, parse_hardware_block_with,
};
use xorgate_core::telemetry::ActivityLevel;
use xorgate_hal::atomic_write::atomic_write_bytes_traced;
use xorgate_hal::hardware_file::save_hardware_block;
use xorgate_hal::kvs_manifest_io::{persist_upload_bucket, PersistOutcome};
use xorgate_hal::settings_io::read_settings_file;

use super::sync::ConfigSync;
use crate::ctx::{AgentCtx, ObdControl};
use crate::scope::ScopeHandle;

/// `ignoring malformed cloud config message` (`worker.ts:786`).
pub const MALFORMED_EVENT: &str = "ignoring malformed cloud config message";
/// `invalid hardware block on config message — keeping current` (`worker.ts:829`).
pub const INVALID_HARDWARE_EVENT: &str =
    "invalid hardware block on config message — keeping current";

/// `[agent] hardware block changed peripherals.obd — restart the agent to
/// apply` (`worker.ts:924-925` @ 09da38d): the OBD collector is built once
/// from the BOOT-time block; a live change only says so. Event = the text
/// without `[agent] `.
pub const OBD_HARDWARE_CHANGED_EVENT: &str =
    "hardware block changed peripherals.obd — restart the agent to apply";

/// `JSON.stringify(parsed)` — the `cloud config applied: …` summary
/// (`worker.ts:863`), numbers printed like JS.
pub fn applied_summary(parsed: &CloudConfig) -> String {
    xorgate_core::js::to_json_string(
        &serde_json::to_value(parsed).expect("a parsed cloud config serialises"),
    )
}

/// `applyCloudConfig(config, path)` (`cloud-config.ts:293-411`): re-read
/// settings.json IMMEDIATELY before the write; refuse (the TS `throw`) when
/// it exists but cannot be read; merge (`xorgate_core::cloud_config::merge`);
/// write only on change (`atomicWriteJson` of the merged document, bytes =
/// [`MergeOutcome::document_json`](xorgate_core::cloud_config::MergeOutcome::document_json)).
/// `Ok(changed)`; `Err(message)` where the TS throws.
pub fn apply_cloud_config(config: &CloudConfig, path: &Path) -> Result<bool, String> {
    let before = read_settings_file(path);
    let out = merge(&before, config, &path.display().to_string());
    if let Some(reason) = out.refuse_reason {
        return Err(reason);
    }
    let Some(doc) = out.document_json() else {
        return Ok(false);
    };
    // Node's `err.message`: the TEMP path for `open`, both paths for `rename`.
    atomic_write_bytes_traced(path, doc.as_bytes()).map_err(|e| e.node_message())?;
    Ok(true)
}

/// Everything `handleConfigMessage` touches.
pub struct MessageHandler {
    pub ctx: AgentCtx,
    pub sync: ConfigSync,
    pub scope: ScopeHandle,
    /// The OBD collector (Phase 5b I4): `source.obdReapply?.()` after every
    /// message and the `obdRunning` half of the hardware-change warning.
    pub obd: Option<Arc<dyn ObdControl>>,
}

impl MessageHandler {
    /// `handleConfigMessage(payload)` (`worker.ts:781-900`), in the TS order.
    pub fn handle(&self, payload: &[u8]) {
        let ctx = &self.ctx;
        let now = ctx.now_ms();
        // `parseCloudMessage(payload)` = `JSON.parse(payload.toString())`: lossy UTF-8 (CF12).
        let Some(message) = parse_cloud_message(&String::from_utf8_lossy(payload)) else {
            error!("[agent] ignoring malformed config message");
            ctx.state.note(ActivityLevel::Warn, MALFORMED_EVENT, now);
            return;
        };
        let raw = message.raw;
        let parsed = message.config;
        // `parseHardwareBlock(hardware)` inside `parseCloudMessage`
        // (`cloud-config.ts:284-287` @ 09da38d) warns through its DEFAULT
        // sink, `console.warn("[hardware] …")`: the `peripherals` warnings
        // (journal only, no event). Core's parse is silent, so replay them here.
        if let Some(hw) = raw.get("hardware").filter(|v| v.is_object()) {
            let _ = parse_hardware_block_with(hw, &mut |m| warn!("{}", hardware_warn_line(&m)));
        }

        // uploadBucket backfill (`worker.ts:796-812`), outside the rev guard.
        if let Some(bucket) = parsed.upload_bucket.as_deref().filter(|b| !b.is_empty()) {
            if !ctx.env.config.offline {
                let path = &ctx.paths.kvs_manifest;
                match persist_upload_bucket(path, bucket) {
                    Ok(PersistOutcome::Applied) => {
                        info!("[agent] uploadBucket persisted into kvs.json: {bucket}");
                        ctx.state.note(
                            ActivityLevel::Info,
                            format!("uploadBucket backfilled: {bucket}"),
                            now,
                        );
                        // `recording.setUpload(buildUploadOptions())` (Phase 3).
                        ctx.hooks.rearm_upload();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let msg = e.node_message();
                        error!("[agent] uploadBucket persist failed {msg}");
                        ctx.state.note(
                            ActivityLevel::Warn,
                            format!("uploadBucket persist failed: {msg}"),
                            now,
                        );
                    }
                }
            }
        }

        // Tenant scope (`worker.ts:815-818`), outside the rev guard.
        if let Some(scope) = parsed.scope.clone() {
            self.scope.apply_scope(scope, now);
        }

        // Hardware block (`worker.ts:826-853`), outside the rev guard.
        match &parsed.hardware {
            Some(None) => {
                warn!("[agent] config message carried an invalid hardware block — keeping current");
                ctx.state
                    .note(ActivityLevel::Warn, INVALID_HARDWARE_EVENT, now);
            }
            Some(Some(block))
                if !hardware_blocks_equal(Some(block), ctx.hardware.borrow().as_ref()) =>
            {
                ctx.hardware.send_replace(Some(block.clone()));
                if let Err(e) = save_hardware_block(&ctx.paths.hardware, block) {
                    warn!(
                        "[agent] hardware block persist failed: {}",
                        e.node_message()
                    );
                }
                let (platform, encoder) = (block.platform.as_str(), block.encoder.as_str());
                info!(
                    "[agent] hardware block updated from cloud ({platform}, {encoder}) — re-rendering pipelines"
                );
                ctx.state.note(
                    ActivityLevel::Info,
                    format!("hardware block updated: {platform}/{encoder}"),
                    now,
                );
                // `obdDeclared !== obdRunning` (`worker.ts:917-927` @ 09da38d).
                let obd_declared = block.peripherals.as_ref().is_some_and(|p| p.obd.is_some());
                if obd_declared != self.obd.is_some() {
                    warn!("[agent] {OBD_HARDWARE_CHANGED_EVENT}");
                    ctx.state
                        .note(ActivityLevel::Warn, OBD_HARDWARE_CHANGED_EVENT, now);
                }
                // `reconcileHardwareNow()` → master bounce → `recording.reapply()` (Phase 3).
                ctx.hooks.hardware_changed();
            }
            _ => {}
        }

        // Rev guard (`worker.ts:938-1003` @ 09da38d).
        let should_apply = self.sync.should_apply(parsed.rev);
        // Upgrade exception (`:944-947`): an already-applied document can
        // carry a namespace the PREVIOUS build never persisted; adopt exactly
        // those keys, everything else stays rev-guarded.
        let adopt = if should_apply {
            Vec::new()
        } else {
            self.sync.unheld_namespaces(&parsed)
        };
        let mut applied = false;
        let outcome: Result<(), String> = (|| {
            if should_apply || !adopt.is_empty() {
                let subset = if should_apply {
                    parsed.clone()
                } else {
                    adoption_subset(&parsed, &adopt)
                };
                if !should_apply {
                    info!("{}", unheld_adopt_log(parsed.rev, &adopt));
                }
                let changed = apply_cloud_config(&subset, &ctx.paths.settings)?;
                applied = true;
                if changed {
                    let summary = applied_summary(&subset);
                    info!("[agent] cloud config applied: {summary}");
                    ctx.state.note(
                        ActivityLevel::Info,
                        format!("cloud config applied: {summary}"),
                        now,
                    );
                    ctx.hooks.led_config_applied();
                }
            } else {
                info!(
                    "[agent] cloud config rev {} already applied — not re-applying",
                    parsed.rev.unwrap_or(0)
                );
            }
            // `recording.reapply()` + `replay?.reapply()` (Phase 3) and
            // `void reconcileCameraMountNow()` (Phase 4) — on BOTH paths. A
            // throwing reapply skips the reconcile and lands in the catch.
            ctx.hooks.reapply_recording()?;
            // `source.obdReapply?.()` (`:984-987` @ 09da38d).
            if let Some(obd) = &self.obd {
                obd.reapply();
            }
            ctx.hooks.reconcile_camera_mount();
            Ok(())
        })();
        if let Err(msg) = outcome {
            error!("[agent] cloud config apply failed {msg}");
            ctx.state.note(
                ActivityLevel::Warn,
                format!("cloud config apply failed: {msg}"),
                now,
            );
        }
        // Echo the verdict even when the apply FAILED; `applied` gates only the rev.
        self.sync
            .note_cloud_config(parsed.rev, raw, parsed, applied);
        self.sync.request(ReportOrigin::Cloud);
    }
}
