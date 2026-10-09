//! STUB (Phase 2 WP1) — **WP2 replaces this file wholesale** (and adds
//! `gps.rs`, `imu.rs`, `lte.rs`, `system.rs` beside it).
//!
//! Frozen signature (see `phase-2/evidence/wp1.md` "Interfaces for wave B"):
//!
//! ```ignore
//! pub async fn start(ctx: &AgentCtx) -> Arc<dyn TelemetrySource>
//! ```
//!
//! called once at boot in `real` mode (`createCollector(…)`,
//! `worker.ts:270-284`). The hardware block in force is `ctx.hardware.borrow()`
//! (`sensors.imu = block ? block.imu !== null : true`,
//! `system = block ? { rails: block.rails } : true`); the input knobs are in
//! `ctx.paths` (`gps_nmea_port`, `lte_telemetry`, `python_dir`) and every raw
//! input goes to `ctx.tape`.
//!
//! Until WP2 lands, the stub keeps the Phase 0 behaviour so a bench build
//! still publishes something: the `system` group from the Phase 0 sampler,
//! the idle media block, and inert values everywhere else.

use std::sync::Arc;

use indexmap::IndexMap;
use tokio::sync::Mutex;
use xorgate_core::telemetry::{Metrics, TelemetryMedia, TelemetryReading};
use xorgate_core::types::{GnssView, GpsClockReading, PowerSampleReading};
use xorgate_hal::system::SystemSampler;

use crate::ctx::{AgentCtx, BoxFut, GroupSample, TelemetrySource};

/// The real-mode collector registry (stub: Phase 0's system group only).
pub struct Collectors {
    system: Mutex<SystemSampler>,
}

/// `createCollector(…)` for `MODE=real`.
pub async fn start(ctx: &AgentCtx) -> Arc<dyn TelemetrySource> {
    let rails = ctx.hardware.borrow().as_ref().is_none_or(|b| b.rails);
    Arc::new(Collectors {
        system: Mutex::new(SystemSampler::new(rails).await),
    })
}

impl TelemetrySource for Collectors {
    fn read(&self) -> BoxFut<'_, Result<TelemetryReading, String>> {
        Box::pin(async move {
            let system = self.system.lock().await.read().await;
            let mut metrics = Metrics::new();
            if !system.is_empty() {
                metrics.insert("system".into(), system);
            }
            Ok(TelemetryReading {
                metrics,
                media: TelemetryMedia::idle(),
            })
        })
    }

    fn sample_groups(&self) -> BoxFut<'_, IndexMap<String, GroupSample>> {
        Box::pin(async { IndexMap::new() })
    }

    fn sample_power(&self) -> BoxFut<'_, Option<PowerSampleReading>> {
        Box::pin(async { None })
    }

    fn gps_clock(&self) -> Option<GpsClockReading> {
        None
    }

    fn gnss_view(&self) -> Option<GnssView> {
        None
    }

    fn stop(&self) {}
}
