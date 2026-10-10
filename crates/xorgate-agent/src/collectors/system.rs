//! The system collector — `src/agent/collectors/system.ts:32-115`
//! (`createSystemCollector`) over the Phase 0 sampler
//! ([`xorgate_hal::system::SystemSampler`]: /proc/stat, /proc/meminfo, the
//! thermal zone, statvfs, `vcgencmd get_throttled` / `pmic_read_adc`, the
//! device-tree PSU limit — every raw input reported to the tape).
//!
//! Two CPU-delta chains, like the TS: `read()` (the live tick) keeps the
//! sampler's own `prev`; `sample()` (the recorder / power-shed cache,
//! `:89-114`) keeps a SEPARATE previous sample and recomputes at most once
//! per second (mono gate). The PMIC latch and the vcgencmd gate are shared.

use tokio::sync::Mutex;
use tokio::time::Instant;
use xorgate_core::clock::{Clock, MonoMs};
use xorgate_core::collector_cache::poll_due;
use xorgate_core::system::CpuSample;
use xorgate_core::telemetry::MetricGroup;
use xorgate_hal::system::{sample_cpu, SystemSampler};

use super::Collector;
use crate::ctx::{AgentCtx, BoxFut, GroupSample};

/// `createSystemCollector({ rails })`.
pub struct SystemCollector {
    ctx: AgentCtx,
    sampler: Mutex<SystemSampler>,
    rec: Mutex<RecCache>,
}

/// `recPrev` / `recCache` / `recLastAt` (`system.ts:93-95`).
#[derive(Default)]
struct RecCache {
    prev: Option<CpuSample>,
    cache: Option<GroupSample>,
    last_at: Option<MonoMs>,
}

impl SystemCollector {
    /// `rails = false` pre-latches the PMIC reads off and skips the
    /// device-tree probe (`system.ts:45-48`).
    pub async fn new(ctx: &AgentCtx, rails: bool) -> Self {
        Self {
            ctx: ctx.clone(),
            sampler: Mutex::new(SystemSampler::with_tape(rails, ctx.tape.clone()).await),
            rec: Mutex::new(RecCache::default()),
        }
    }
}

impl Collector for SystemCollector {
    fn group(&self) -> &'static str {
        "system"
    }

    /// `read()` (`system.ts:99-103`).
    fn read(&self) -> BoxFut<'_, MetricGroup> {
        Box::pin(async move { self.sampler.lock().await.read().await })
    }

    /// `sample()` (`system.ts:104-113`).
    fn sample(&self) -> BoxFut<'_, Option<GroupSample>> {
        Box::pin(async move {
            let mut rec = self.rec.lock().await;
            let now = self.ctx.clock.mono_ms();
            if poll_due(rec.last_at, now) {
                rec.last_at = Some(now);
                let prev = match rec.prev.take() {
                    Some(p) => p,
                    None => sample_cpu().await,
                };
                let (fields, curr) = self.sampler.lock().await.compute_from(&prev).await;
                rec.prev = Some(curr);
                rec.cache = Some(GroupSample {
                    fields,
                    updated_mono: Instant::now(),
                });
            }
            rec.cache.clone()
        })
    }
}
