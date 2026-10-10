//! The real-mode collector registry — `createCollector(…)`
//! (`src/agent/collectors/index.ts:84-192`) as `worker.ts:270-292` builds it
//! (Phase 2 WP2; replaces WP1's stub, frozen signature [`start`]).
//!
//! | group | collector | TS |
//! |---|---|---|
//! | `imu` | [`imu::ImuCollector`] — i2cdetect + the supervised Python helper | `collectors/imu.ts`, `hardware/imu.ts` |
//! | `gps` | [`gps::GpsCollector`] — the NMEA tty, TTFF, sky tracker + `gnss-sky.json` | `collectors/gps.ts`, `hardware/gps.ts` |
//! | `lte` | [`lte::LteCollector`] — the modem daemon's telemetry.json | `collectors/lte.ts` |
//! | `system` | [`system::SystemCollector`] — /proc, /sys, statvfs, vcgencmd, device tree | `collectors/system.ts` |
//!
//! Read order is imu → gps → lte → system (`index.ts:125-129`); a group whose
//! read is empty is omitted. `media` is the idle block: the KVS media
//! supervisor (`index.ts:134-135`) is Phase 3 (the worker overlays it through
//! `PhaseHooks::overlay_media`).
//!
//! Each collector self-gates (absent hardware → `{}` + at most one warning),
//! so real mode enables all of them; the hardware block only gates the IMU
//! (`imu: null` → no collector, no i2cdetect probe) and the PMIC rails
//! (`rails: false`), exactly as `worker.ts:281-287`.

pub mod gps;
pub mod imu;
pub mod lte;
pub mod system;

use std::sync::Arc;

use indexmap::IndexMap;
use serde::Serialize;
use xorgate_core::power::power_reading_from_fields;
use xorgate_core::telemetry::{MetricGroup, Metrics, TelemetryMedia, TelemetryReading};
use xorgate_core::types::{GnssView, GpsClockReading, PowerSampleReading};
use xorgate_hal::nmea_tty::{NmeaOpener, TtyOpener};

use crate::ctx::{AgentCtx, BoxFut, GroupSample, TelemetrySource};

/// One telemetry group's source (`Collector`, `index.ts:19-30`).
pub trait Collector: Send + Sync + 'static {
    /// The `metrics` key.
    fn group(&self) -> &'static str;
    /// One tick's fields; empty = the worker omits the group.
    fn read(&self) -> BoxFut<'_, MetricGroup>;
    /// The recorder's cached view + its freshness stamp (`sample()`); `None`
    /// when the group is inert/stale (the same condition as `read() → {}`).
    fn sample(&self) -> BoxFut<'_, Option<GroupSample>>;
}

/// The `io_capabilities` the worker derives (`IoCapabilities`, `index.ts:66-71`
/// as `worker.ts:281-287` fills it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoCapabilities {
    /// `sensors.imu`: `block ? block.imu !== null : true`.
    pub imu: bool,
    /// `comm.gps` (always true in real mode).
    pub gps: bool,
    /// `comm.signal` (always true in real mode).
    pub signal: bool,
    /// `system`: `block ? { rails: block.rails } : true` → `Some(rails)`;
    /// `None` = no system collector (never in real mode).
    pub system_rails: Option<bool>,
}

impl IoCapabilities {
    /// `worker.ts:281-287`.
    pub fn from_hardware(
        block: Option<&xorgate_core::hardware_block::DeviceHardwareBlock>,
    ) -> Self {
        Self {
            imu: block.is_none_or(|b| b.imu.is_some()),
            gps: true,
            signal: true,
            system_rails: Some(block.is_none_or(|b| b.rails)),
        }
    }
}

/// The edges the collectors open, injectable for tests.
#[derive(Clone)]
pub struct Backends {
    pub nmea: Arc<dyn NmeaOpener>,
    pub imu: Arc<dyn imu::ImuIo>,
}

impl Default for Backends {
    fn default() -> Self {
        Self {
            nmea: Arc::new(TtyOpener),
            imu: Arc::new(imu::RealImuIo::default()),
        }
    }
}

/// The real-mode registry (`createCollector`'s return value).
pub struct Collectors {
    /// Read order: imu → gps → lte → system.
    order: Vec<Arc<dyn Collector>>,
    imu: Option<Arc<imu::ImuCollector>>,
    gps: Option<Arc<gps::GpsCollector>>,
    system: Option<Arc<system::SystemCollector>>,
}

/// `createCollector(…)` for `MODE=real` (`worker.ts:279-292`). Frozen
/// signature (WP1 "Interfaces for wave B").
pub async fn start(ctx: &AgentCtx) -> Arc<dyn TelemetrySource> {
    let caps = IoCapabilities::from_hardware(ctx.hardware.borrow().as_ref());
    Arc::new(Collectors::new(ctx, caps, Backends::default()).await)
}

impl Collectors {
    /// `createCollector(caps)` (`index.ts:119-129`): the system collector is
    /// built first (its constructor seeds the CPU sample, probes vcgencmd and
    /// reads the device tree), then gps, then the read order is assembled.
    pub async fn new(ctx: &AgentCtx, caps: IoCapabilities, backends: Backends) -> Self {
        let system = match caps.system_rails {
            Some(rails) => Some(Arc::new(system::SystemCollector::new(ctx, rails).await)),
            None => None,
        };
        let gps = caps
            .gps
            .then(|| Arc::new(gps::GpsCollector::new(ctx, backends.nmea.clone())));
        let imu = caps
            .imu
            .then(|| Arc::new(imu::ImuCollector::new(ctx, backends.imu.clone())));
        let lte = caps.signal.then(|| Arc::new(lte::LteCollector::new(ctx)));

        let mut order: Vec<Arc<dyn Collector>> = Vec::new();
        if let Some(c) = &imu {
            order.push(c.clone());
        }
        if let Some(c) = &gps {
            order.push(c.clone());
        }
        if let Some(c) = &lte {
            order.push(c.clone());
        }
        if let Some(c) = &system {
            order.push(c.clone());
        }
        Self {
            order,
            imu,
            gps,
            system,
        }
    }

    /// The gps collector (Phase 3/4 consumers reach `clock()`/`sky()` here).
    pub fn gps(&self) -> Option<&Arc<gps::GpsCollector>> {
        self.gps.as_ref()
    }
}

impl TelemetrySource for Collectors {
    /// `read()` (`index.ts:138-150`): every collector in order, empty groups
    /// omitted; `media` is the idle block (no supervisor before Phase 3).
    fn read(&self) -> BoxFut<'_, Result<TelemetryReading, String>> {
        Box::pin(async move {
            let mut metrics = Metrics::new();
            for c in &self.order {
                let fields = c.read().await;
                if !fields.is_empty() {
                    metrics.insert(c.group().to_string(), fields);
                }
            }
            Ok(TelemetryReading {
                metrics,
                media: TelemetryMedia::idle(),
            })
        })
    }

    /// `sampleGroups()` (`index.ts:151-163`): non-empty samples only (a
    /// collector hiccup cannot fail the recorder tick — the Rust collectors
    /// have no error path).
    fn sample_groups(&self) -> BoxFut<'_, IndexMap<String, GroupSample>> {
        Box::pin(async move {
            let mut out = IndexMap::new();
            for c in &self.order {
                if let Some(s) = c.sample().await {
                    if !s.fields.is_empty() {
                        out.insert(c.group().to_string(), s);
                    }
                }
            }
            out
        })
    }

    /// `samplePower()` (`index.ts:176-183`): from the system collector's
    /// 1 Hz sample cache; `None` without a system group or vcgencmd.
    fn sample_power(&self) -> BoxFut<'_, Option<PowerSampleReading>> {
        Box::pin(async move {
            let system = self.system.as_ref()?;
            let sample = system.sample().await;
            power_reading_from_fields(sample.as_ref().map(|s| &s.fields))
        })
    }

    /// `gpsClock()` (`index.ts:184-186`).
    fn gps_clock(&self) -> Option<GpsClockReading> {
        self.gps.as_ref()?.clock()
    }

    /// `gnssView()` (`index.ts:187-190`): `{ fix: hasFix(), sky: sky().state }`.
    fn gnss_view(&self) -> Option<GnssView> {
        let gps = self.gps.as_ref()?;
        let fix = gps.has_fix();
        let sky = gps.sky().state;
        Some(GnssView { fix, sky })
    }

    /// `stop()` (`index.ts:164-166`) stops only the media supervisor (Phase
    /// 3). The TS process exit then takes the IMU helper down (execa's
    /// `cleanup`) and closes the tty; here the same happens explicitly: the
    /// helper gets SIGTERM and the NMEA reader thread closes the port.
    fn stop(&self) {
        if let Some(imu) = &self.imu {
            imu.stop();
        }
        if let Some(gps) = &self.gps {
            gps.stop();
        }
    }
}

/// Lower a typed group (`GpsGroup`, `ImuGroup`, `LteGroup`) to the wire map,
/// keeping the struct's field order and its omission rules.
pub fn to_group<T: Serialize>(typed: &T) -> MetricGroup {
    serde_json::to_value(typed)
        .and_then(serde_json::from_value)
        .unwrap_or_default()
}
