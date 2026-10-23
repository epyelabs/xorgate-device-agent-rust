//! Merge the LED state into telemetry — port of `src/agent/led-telemetry.ts`
//! @ 09da38d (62 lines; plans/include-led-status-in-telemetry), Phase 5b WP3.
//!
//! `system.led_state` + `system.health_flags` ride the EXISTING `system`
//! group on every live payload and every recorded `system` sample. They are
//! merged in the worker's path, deliberately NOT inside the collector
//! registry: the collector set is byte-checked by the OBD tests, and the
//! LED's inputs (MQTT link, recording verdicts) belong to the worker.
//!
//! Pure helpers so the live payload and the recorder wrapper are testable
//! without a worker. Ownership in, ownership out: the caller hands in its own
//! copy of the metrics, so a collector cache can never be mutated (the TS
//! "never mutates its input — collector caches are shared").

use indexmap::IndexMap;

use crate::led_state::LedTelemetry;
use crate::telemetry::{MetricGroup, MetricLeaf, Metrics};

/// `LED_TELEMETRY_STARTING` (`led-telemetry.ts:18`): before the first LED
/// tick the state is, by definition, "starting".
pub const LED_TELEMETRY_STARTING: LedTelemetry = LedTelemetry {
    led_state: 1,
    health_flags: 0,
};

/// `LED_TELEMETRY_SIM` (`led-telemetry.ts:20`): the simulator is always
/// healthy.
pub const LED_TELEMETRY_SIM: LedTelemetry = LedTelemetry {
    led_state: 0,
    health_flags: 0,
};

/// The group the leaves join.
pub const SYSTEM_GROUP: &str = "system";

/// `leaves(t)` merged into `fields` (`{...fields, ...leaves(t)}`,
/// `led-telemetry.ts:22-26`): `led_state` then `health_flags`, NO unit (same
/// as `system.throttled`). An existing key keeps its position (a JS spread
/// overwrite); a new one is appended.
fn merge_leaves(mut fields: MetricGroup, t: LedTelemetry) -> MetricGroup {
    fields.insert("led_state".into(), MetricLeaf::new(f64::from(t.led_state)));
    fields.insert(
        "health_flags".into(),
        MetricLeaf::new(f64::from(t.health_flags)),
    );
    fields
}

/// `withLedLeaves(metrics, t)` (`led-telemetry.ts:34-41`): the live path.
/// `None` (the TS replay mode; unreachable in Rust, O7) returns `metrics` as
/// is. `system` keeps its position when present (`{...metrics, system}`),
/// else it is appended last.
pub fn with_led_leaves(mut metrics: Metrics, t: Option<LedTelemetry>) -> Metrics {
    let Some(t) = t else { return metrics };
    let system = metrics.get_mut(SYSTEM_GROUP).map(std::mem::take);
    metrics.insert(
        SYSTEM_GROUP.into(),
        merge_leaves(system.unwrap_or_default(), t),
    );
    metrics
}

/// A recorder sample group: its fields (the stamp stays the caller's type —
/// the agent's `GroupSample` keeps a tokio `Instant`).
pub trait LedSampleGroup {
    fn fields_mut(&mut self) -> &mut MetricGroup;
}

/// `withLedSampleGroups(groups, t, nowMono)` (`led-telemetry.ts:50-62`): the
/// recorder path. The leaves reuse the system sample's `updatedMono` (only
/// the fields change), so they land exactly when and as often as the system
/// group does (1 Hz by its native cadence). With no system sample at all a
/// group is synthesised by `fresh(fields)` with a fresh stamp (CF73: the
/// recorder's 1 Hz `system` cadence gate still caps it) and appended last.
/// `None` returns `groups` as is.
pub fn with_led_sample_groups<G: LedSampleGroup>(
    mut groups: IndexMap<String, G>,
    t: Option<LedTelemetry>,
    fresh: impl FnOnce(MetricGroup) -> G,
) -> IndexMap<String, G> {
    let Some(t) = t else { return groups };
    match groups.get_mut(SYSTEM_GROUP) {
        Some(sys) => {
            let fields = std::mem::take(sys.fields_mut());
            *sys.fields_mut() = merge_leaves(fields, t);
        }
        None => {
            groups.insert(
                SYSTEM_GROUP.into(),
                fresh(merge_leaves(MetricGroup::new(), t)),
            );
        }
    }
    groups
}
