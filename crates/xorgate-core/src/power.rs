//! Power health — the pure half of `src/hardware/power.ts` (97 lines) plus
//! `powerReadingFromFields` from `src/agent/collectors/index.ts:44-64`
//! (moved here because that file imports every collector; inventory §8.4).
//!
//! The `vcgencmd get_throttled` / `pmic_read_adc` / device-tree `max_current`
//! parsers were already ported by Phase 0 into [`crate::system`] (the system
//! collector consumes them); they are re-exported here under the TS module's
//! names and replayed against the full `test/power.test.ts` fixture so this
//! module is the one-stop port of `power.ts`. The readers themselves
//! (`readThrottled:42`, `readPmicAdc:71`, `readMaxCurrentMa:87`) are HAL.

use crate::js;
pub use crate::system::{
    max_current_ma_from_bytes, parse_pmic_adc, parse_throttled, rail_5v_from, PmicAdc, PmicRail,
    ThrottleState,
};
use crate::telemetry::MetricGroup;
use crate::types::PowerSampleReading;

/// One entry of `THROTTLE_BITS` (`power.ts:19-24`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleBit {
    pub bit: u32,
    pub label: &'static str,
}

/// `THROTTLE_BITS` (`power.ts:19-24`): bits 0–3 of the mask (same legend for
/// current and sticky).
pub const THROTTLE_BITS: [ThrottleBit; 4] = [
    ThrottleBit {
        bit: 0x1,
        label: "undervoltage",
    },
    ThrottleBit {
        bit: 0x2,
        label: "freq capped",
    },
    ThrottleBit {
        bit: 0x4,
        label: "throttled",
    },
    ThrottleBit {
        bit: 0x8,
        label: "soft temp limit",
    },
];

/// `powerReadingFromFields` (`collectors/index.ts:44-64`): decode one `system`
/// group sample into a load-shed power reading.
///
/// Returns a reading when EITHER the throttle mask OR the SoC temperature is
/// readable; the absent half stays at `uv: false` / `railV: null` /
/// `tempC: null`, so the arms that depend on it remain inert (fail open,
/// invariant 3). With neither readable (a dev box) → `None`, feature off.
/// Only the CAUSE bits are read: 0x1 undervoltage and 0x8 soft temperature
/// limit; 0x2/0x4 are consequences of EITHER cause and deliberately ignored.
pub fn power_reading_from_fields(fields: Option<&MetricGroup>) -> Option<PowerSampleReading> {
    let leaf = |k: &str| fields.and_then(|f| f.get(k)).map(|l| l.value.0);
    let throttled = leaf("throttled");
    let temp = leaf("cpu_temp");
    if throttled.is_none() && temp.is_none() {
        return None;
    }
    let bits = throttled.map(js::to_int32);
    let rail = leaf("rail_5v");
    Some(PowerSampleReading {
        uv: bits.is_some_and(|b| b & 0x1 != 0),
        rail_v: rail,
        temp_c: temp,
        soft_temp: Some(bits.is_some_and(|b| b & 0x8 != 0)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::MetricLeaf;

    #[test]
    fn throttle_bits_table() {
        assert_eq!(THROTTLE_BITS[0].label, "undervoltage");
        assert_eq!(THROTTLE_BITS[3].bit, 0x8);
    }

    #[test]
    fn reading_from_sticky_only_mask() {
        let mut g = MetricGroup::new();
        g.insert("throttled".into(), MetricLeaf::new(0xe0000 as f64));
        let r = power_reading_from_fields(Some(&g)).unwrap();
        assert_eq!(
            r,
            PowerSampleReading {
                uv: false,
                rail_v: None,
                temp_c: None,
                soft_temp: Some(false)
            }
        );
        assert_eq!(power_reading_from_fields(None), None);
    }
}
