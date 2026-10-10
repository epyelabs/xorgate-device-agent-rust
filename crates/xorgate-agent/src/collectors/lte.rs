//! The lte collector — `src/agent/collectors/lte.ts:29-96`
//! (`createLteCollector`): the modem daemon's atomic telemetry JSON
//! (`LTE_TELEMETRY_FILE`, default `/run/sim7600-lte/telemetry.json`) read
//! each tick through WP1's CF16 snapshot reader and mapped by
//! [`xorgate_core::modem::lte_group`] (`mapTelemetry`, `:40-59`).
//!
//! `read()` warns ONCE while the file is missing/unparseable
//! (`[lte] no telemetry yet — is sim7600-lte.service running?`) and re-arms
//! the warning when it comes back. `sample()` re-reads at most once per
//! second and stamps `updatedMono` only when the daemon's
//! `` `${timestamp}|${csq}` `` key changed (mono gate, see
//! [`xorgate_core::collector_cache`]).

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::warn;
use xorgate_core::clock::{Clock, MonoMs};
use xorgate_core::collector_cache::{lte_change_key, poll_due};
use xorgate_core::modem::lte_group;
use xorgate_core::telemetry::MetricGroup;
use xorgate_core::types::ModemTelemetry;
use xorgate_hal::modem_file;

use super::{to_group, Collector};
use crate::ctx::{AgentCtx, BoxFut, GroupSample};

/// The warn-once line (`lte.ts:67`), verbatim.
pub const LTE_NO_TELEMETRY: &str = "[lte] no telemetry yet — is sim7600-lte.service running?";

/// `createLteCollector()`.
pub struct LteCollector {
    ctx: AgentCtx,
    warned: AtomicBool,
    cache: Mutex<Cache>,
}

/// The recorder-side cache (`lte.ts:36-38`).
#[derive(Default)]
struct Cache {
    cached: Option<GroupSample>,
    last_key: Option<String>,
    /// `lastPollAt` (mono); `None` = the TS initial `0`.
    last_poll_at: Option<MonoMs>,
}

/// `mapTelemetry(t)` → the wire map, or `None` (unknown signal).
fn map_telemetry(t: &ModemTelemetry) -> Option<MetricGroup> {
    lte_group(t).map(|g| to_group(&g))
}

impl LteCollector {
    pub fn new(ctx: &AgentCtx) -> Self {
        Self {
            ctx: ctx.clone(),
            warned: AtomicBool::new(false),
            cache: Mutex::new(Cache::default()),
        }
    }
}

impl Collector for LteCollector {
    fn group(&self) -> &'static str {
        "lte"
    }

    /// `read()` (`lte.ts:63-74`).
    fn read(&self) -> BoxFut<'_, MetricGroup> {
        Box::pin(async move {
            let snap =
                modem_file::read(&self.ctx.paths.lte_telemetry, self.ctx.tape.as_ref()).await;
            let Some(t) = snap.typed else {
                if !self.warned.swap(true, Ordering::SeqCst) {
                    warn!("{LTE_NO_TELEMETRY}");
                }
                return MetricGroup::new();
            };
            // recovered; allow a fresh warning if it drops again
            self.warned.store(false, Ordering::SeqCst);
            map_telemetry(&t).unwrap_or_default()
        })
    }

    /// `sample()` (`lte.ts:75-94`).
    fn sample(&self) -> BoxFut<'_, Option<GroupSample>> {
        Box::pin(async move {
            let mut c = self.cache.lock().await;
            let now = self.ctx.clock.mono_ms();
            if poll_due(c.last_poll_at, now) {
                c.last_poll_at = Some(now);
                let snap =
                    modem_file::read(&self.ctx.paths.lte_telemetry, self.ctx.tape.as_ref()).await;
                let fields = snap.typed.as_ref().and_then(map_telemetry);
                match (fields, snap.raw.as_ref()) {
                    (Some(fields), Some(raw)) => {
                        let key = lte_change_key(raw);
                        if c.last_key.as_deref() != Some(key.as_str()) || c.cached.is_none() {
                            c.cached = Some(GroupSample {
                                fields,
                                updated_mono: Instant::now(),
                            });
                            c.last_key = Some(key);
                        } else if let Some(prev) = c.cached.as_mut() {
                            prev.fields = fields;
                        }
                    }
                    _ => c.cached = None,
                }
            }
            c.cached.clone()
        })
    }
}
