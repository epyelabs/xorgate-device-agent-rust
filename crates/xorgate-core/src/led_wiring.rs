//! Which LED driver the worker builds — `worker.ts:592-614` @ 89b5c64 (WP6,
//! CF61). Pure: the hardware block in force AT BOOT decides once (the TS
//! builds the `LedManager` once; a later cloud hardware block does not
//! rebuild it — parity).
//!
//! ```ts
//! ...(replay ? { driver: new LedDriver({ probe: async () => ({ ok: false, reason: "replay mode (container)" }) }) }
//!   : hardwareBlock
//!   ? { driver: hardwareBlock.statusLed
//!         ? new LedDriver({ map: { kind: "rgb-gpio", pins: hardwareBlock.statusLed, activeHigh: true } })
//!         : new LedDriver({ probe: async () => ({ ok: false, reason: "model declares no status LED" }) }) }
//!   : {}),
//! ```
//!
//! The replay branch is out (the Rust agent refuses `MODE=replay`, P14).

use crate::hardware_block::DeviceHardwareBlock;
use crate::led::LedPinMap;

/// The inert probe's reason when the hardware block has no `statusLed`
/// (`worker.ts:611`, verbatim).
pub const NO_STATUS_LED_REASON: &str = "model declares no status LED";

/// What the worker hands `LedDriver::new`.
#[derive(Debug, Clone, PartialEq)]
pub enum LedDriverChoice {
    /// The default host probe over this pin map (`new LedDriver({ map })`, or
    /// `new LedDriver()` = the Argus CM5 `STATUS_LED_MAP` when there is no block).
    Probe(LedPinMap),
    /// An inert driver whose probe answers `{ ok: false, reason }`.
    Inert(&'static str),
}

/// `worker.ts:600-614` (replay branch out): no block → the default map; a
/// block with `statusLed` → `rgb-gpio`, active-high, its pins; a block
/// without one → inert, [`NO_STATUS_LED_REASON`].
pub fn led_driver_choice(block: Option<&DeviceHardwareBlock>) -> LedDriverChoice {
    match block {
        None => LedDriverChoice::Probe(LedPinMap::status_led_map()),
        Some(b) => match b.status_led {
            Some(pins) => LedDriverChoice::Probe(LedPinMap::from_hardware(pins)),
            None => LedDriverChoice::Inert(NO_STATUS_LED_REASON),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware_block::parse_hardware_block;
    use crate::led::STATUS_LED_PINS;

    fn block(json: &str) -> DeviceHardwareBlock {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        parse_hardware_block(&v).expect("valid block")
    }

    #[test]
    fn no_block_is_the_default_cm5_map() {
        match led_driver_choice(None) {
            LedDriverChoice::Probe(m) => {
                assert_eq!(m.kind, "rgb-gpio");
                assert_eq!(m.pins, STATUS_LED_PINS);
                assert!(m.active_high);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn declared_status_led_replaces_the_default_map() {
        let b = block(
            r#"{"v":2,"platform":"rpi-cm4","rails":false,"statusLed":{"r":5,"g":6,"b":13},"imu":null,"video":[]}"#,
        );
        match led_driver_choice(Some(&b)) {
            LedDriverChoice::Probe(m) => {
                assert_eq!(m.kind, "rgb-gpio");
                assert_eq!((m.pins.r, m.pins.g, m.pins.b), (5.0, 6.0, 13.0));
                assert!(m.active_high);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn block_without_status_led_is_inert_with_the_ts_reason() {
        let b = block(
            r#"{"v":2,"platform":"rpi-cm4","rails":false,"statusLed":null,"imu":null,"video":[]}"#,
        );
        assert_eq!(
            led_driver_choice(Some(&b)),
            LedDriverChoice::Inert("model declares no status LED")
        );
    }
}
