//! RGB status LED — the PURE half of `src/hardware/led.ts` (433 lines): the
//! colour/pattern vocabulary, the pattern → timeline table, the pin-map
//! validation and the `gpioinfo` busy-line parser.
//!
//! Left OUT (Phase 4 HAL): `pinctrlWriter` (`led.ts:219-225`, spawns
//! `pinctrl`), `LedDriver` (`led.ts:249-417`, timers + writes) and
//! `probeHost` (`led.ts:420-433`, host probing). The Rust agent will drive
//! the lines through gpiocdev instead of `pinctrl`; see the evidence file for
//! the `[used]` consequence on the busy check.

use serde::{Deserialize, Serialize};

use crate::hardware_block::StatusLedPins;
use crate::js_fmt::number_to_string;
use crate::telemetry::js_num;

/// Argus CM5 Rev1 carrier pins (`led.ts:26`). HARDCODED deliberately; the
/// device-model map (`hardware.statusLed`) overrides it via
/// [`LedPinMap::from_hardware`].
pub const STATUS_LED_PINS: LedPins = LedPins {
    r: 12.0,
    g: 21.0,
    b: 16.0,
};
/// `led.ts:27` — `1` = LED on (board guide Rev B).
pub const ACTIVE_HIGH: bool = true;
/// Highest BCM line on the RP1 (`led.ts:30`): gpiochip0 reports 54 lines, 0–53.
pub const MAX_GPIO: u32 = 53;

/// `LedPinMap.pins` (`led.ts:39`). `f64` like the JS numbers they are parsed
/// from (`hardware-config.ts:85-90` / WP1 `StatusLedPins`), so
/// [`validate_pin_map`] can still reject a non-integer pin the way
/// `Number.isInteger` does.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LedPins {
    #[serde(with = "js_num")]
    pub r: f64,
    #[serde(with = "js_num")]
    pub g: f64,
    #[serde(with = "js_num")]
    pub b: f64,
}

impl From<StatusLedPins> for LedPins {
    fn from(p: StatusLedPins) -> Self {
        LedPins {
            r: p.r,
            g: p.g,
            b: p.b,
        }
    }
}

/// `LedPinMap` (`led.ts:36-42`): the one seam the device-model work replaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedPinMap {
    /// Wiring topology. Only "rgb-gpio" (three plain GPIOs) is implemented.
    pub kind: String,
    pub pins: LedPins,
    /// `true` = driving the pin high lights the channel.
    pub active_high: bool,
}

impl LedPinMap {
    /// `STATUS_LED_MAP` (`led.ts:44-48`).
    pub fn status_led_map() -> Self {
        LedPinMap {
            kind: "rgb-gpio".to_string(),
            pins: STATUS_LED_PINS,
            active_high: ACTIVE_HIGH,
        }
    }

    /// `worker.ts:608`: `{ kind: "rgb-gpio", pins: hardwareBlock.statusLed, activeHigh: true }`.
    pub fn from_hardware(pins: StatusLedPins) -> Self {
        LedPinMap {
            kind: "rgb-gpio".to_string(),
            pins: pins.into(),
            active_high: true,
        }
    }
}

/// `STATUS_LED_MAP` (`led.ts:44`).
pub fn status_led_map() -> LedPinMap {
    LedPinMap::status_led_map()
}

/// `LedColour` (`led.ts:57`): the six colours measured as distinguishable on
/// real hardware, plus `off`. `yellow` (r+g) is deliberately absent — the red
/// die is weak on this package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LedColour {
    Red,
    Green,
    Blue,
    Purple,
    Cyan,
    White,
    Off,
}

/// `LedChannels` (`led.ts:59`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedChannels {
    pub r: bool,
    pub g: bool,
    pub b: bool,
}

const fn ch(r: bool, g: bool, b: bool) -> LedChannels {
    LedChannels { r, g, b }
}

/// `LED_COLOURS` (`led.ts:66-74`) in its literal (= `Object.keys`) order:
/// red 100 · green 010 · blue 001 · purple 101 · cyan 011 · white 111.
pub const LED_COLOURS: [(LedColour, LedChannels); 7] = [
    (LedColour::Off, ch(false, false, false)),
    (LedColour::Red, ch(true, false, false)),
    (LedColour::Green, ch(false, true, false)),
    (LedColour::Blue, ch(false, false, true)),
    (LedColour::Purple, ch(true, false, true)),
    (LedColour::Cyan, ch(false, true, true)),
    (LedColour::White, ch(true, true, true)),
];

/// `LED_COLOUR_NAMES` (`led.ts:76`): `Object.keys(LED_COLOURS)`.
pub const LED_COLOUR_NAMES: [LedColour; 7] = [
    LedColour::Off,
    LedColour::Red,
    LedColour::Green,
    LedColour::Blue,
    LedColour::Purple,
    LedColour::Cyan,
    LedColour::White,
];

impl LedColour {
    pub fn as_str(self) -> &'static str {
        match self {
            LedColour::Red => "red",
            LedColour::Green => "green",
            LedColour::Blue => "blue",
            LedColour::Purple => "purple",
            LedColour::Cyan => "cyan",
            LedColour::White => "white",
            LedColour::Off => "off",
        }
    }

    pub fn parse(v: &str) -> Option<Self> {
        LED_COLOUR_NAMES.into_iter().find(|c| c.as_str() == v)
    }

    /// `LED_COLOURS[colour]` (`led.ts:66`).
    pub fn channels(self) -> LedChannels {
        LED_COLOURS
            .iter()
            .find(|(c, _)| *c == self)
            .map(|(_, ch)| *ch)
            .expect("every LedColour has a channel triple")
    }
}

/// `isLedColour` (`led.ts:78`).
pub fn is_led_colour(v: &str) -> bool {
    LedColour::parse(v).is_some()
}

/// `LedPattern` (`led.ts:82-87`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LedPattern {
    Solid,
    /// ~4 Hz, 50% duty.
    BlinkFast,
    /// ~1 Hz, 50% duty.
    BlinkSlow,
    /// Double-pulse, then dark for the rest of a 5.5 s cycle.
    Heartbeat,
    /// 2 quick flashes then done (the 2 s overlay).
    Flash2,
}

/// `LED_PATTERN_NAMES` (`led.ts:89-95`).
pub const LED_PATTERN_NAMES: [LedPattern; 5] = [
    LedPattern::Solid,
    LedPattern::BlinkFast,
    LedPattern::BlinkSlow,
    LedPattern::Heartbeat,
    LedPattern::Flash2,
];

impl LedPattern {
    pub fn as_str(self) -> &'static str {
        match self {
            LedPattern::Solid => "solid",
            LedPattern::BlinkFast => "blink-fast",
            LedPattern::BlinkSlow => "blink-slow",
            LedPattern::Heartbeat => "heartbeat",
            LedPattern::Flash2 => "flash2",
        }
    }

    pub fn parse(v: &str) -> Option<Self> {
        LED_PATTERN_NAMES.into_iter().find(|p| p.as_str() == v)
    }
}

/// `isLedPattern` (`led.ts:97`).
pub fn is_led_pattern(v: &str) -> bool {
    LedPattern::parse(v).is_some()
}

/// One leg of a pattern (`led.ts:102`). `ms: 0` means "hold here until
/// stopped" (terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedStep {
    pub on: bool,
    pub ms: u64,
}

/// `LedTimeline` (`led.ts:107`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedTimeline {
    pub steps: Vec<LedStep>,
    /// Loop the steps forever. A non-repeating timeline blanks when it ends.
    pub repeat: bool,
}

const fn step(on: bool, ms: u64) -> LedStep {
    LedStep { on, ms }
}

/// `ledTimeline` (`led.ts:117-157`): pattern → the on/off timeline it should
/// produce. Timings are named here so the ladder stays reviewable in one place.
pub fn led_timeline(pattern: LedPattern) -> LedTimeline {
    match pattern {
        LedPattern::Solid => LedTimeline {
            steps: vec![step(true, 0)],
            repeat: false,
        },
        LedPattern::BlinkFast => LedTimeline {
            steps: vec![step(true, 125), step(false, 125)],
            repeat: true,
        },
        LedPattern::BlinkSlow => LedTimeline {
            steps: vec![step(true, 500), step(false, 500)],
            repeat: true,
        },
        LedPattern::Heartbeat => LedTimeline {
            steps: vec![
                step(true, 120),
                step(false, 180),
                step(true, 120),
                step(false, 5080),
            ],
            repeat: true,
        },
        LedPattern::Flash2 => LedTimeline {
            steps: vec![step(true, 150), step(false, 200), step(true, 150)],
            repeat: false,
        },
    }
}

/// `PinMapCheck` (`led.ts:159`). `reason` is absent on success (`{ ok: true }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinMapCheck {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PinMapCheck {
    fn fail(reason: String) -> Self {
        PinMapCheck {
            ok: false,
            reason: Some(reason),
        }
    }
}

/// `validatePinMap` (`led.ts:169-189`): validate the map even though it is
/// hardcoded. `busy_pins` comes from [`parse_busy_pins`] — refusing a line
/// that already has a gpiod consumer is exactly the check that would have
/// caught the GPIO16 `sdmode` case in Phase 0 by itself.
pub fn validate_pin_map(map: &LedPinMap, busy_pins: &[u32]) -> PinMapCheck {
    if map.kind != "rgb-gpio" {
        return PinMapCheck::fail(format!("unimplemented LED kind '{}'", map.kind));
    }
    let entries = [("r", map.pins.r), ("g", map.pins.g), ("b", map.pins.b)];
    for (ch, pin) in entries {
        // `!Number.isInteger(pin) || pin < 0 || pin > MAX_GPIO` (`led.ts:175`);
        // NaN/±Infinity fail the integer test on both sides.
        if pin.fract() != 0.0 || pin < 0.0 || pin > MAX_GPIO as f64 {
            return PinMapCheck::fail(format!(
                "{ch} pin {} is outside the RP1 range 0–{MAX_GPIO}",
                number_to_string(pin)
            ));
        }
    }
    let pins: Vec<f64> = entries.iter().map(|(_, p)| *p).collect();
    let distinct = pins
        .iter()
        .enumerate()
        .all(|(i, p)| pins[..i].iter().all(|q| q != p));
    if !distinct {
        let list: Vec<String> = pins.iter().map(|p| number_to_string(*p)).collect();
        return PinMapCheck::fail(format!("pins {} are not distinct", list.join(",")));
    }
    let busy: Vec<String> = entries
        .iter()
        .filter(|(_, pin)| busy_pins.iter().any(|b| *b as f64 == *pin))
        .map(|(ch, pin)| format!("{ch}=GPIO{}", number_to_string(*pin)))
        .collect();
    if !busy.is_empty() {
        return PinMapCheck::fail(format!(
            "{} already held by another driver (gpioinfo: [used])",
            busy.join(", ")
        ));
    }
    PinMapCheck {
        ok: true,
        reason: None,
    }
}

/// `parseBusyPins` (`led.ts:196-204`): BCM lines that `gpioinfo` reports as
/// `[used]`, i.e. claimed by a kernel driver or another process. Lines look
/// like `line  16:  "GPIO16"  "sdmode"  output  active-high  [used]`; the TS
/// matches `/^\s*line\s+(\d+):/` on every line containing `[used]`.
///
/// Phase 4 note: once the Rust agent holds the three lines through gpiocdev
/// they show `[used]` to a later `gpioinfo` too (`pinctrl` never claimed
/// them), so a re-probe while running must exclude its own consumer. Ported
/// as-is; the adjustment belongs to the driver.
pub fn parse_busy_pins(gpioinfo_output: &str) -> Vec<u32> {
    let mut busy = Vec::new();
    for line in gpioinfo_output.split('\n') {
        if !line.contains("[used]") {
            continue;
        }
        if let Some(n) = busy_line_number(line) {
            busy.push(n);
        }
    }
    busy
}

/// `^\s*line\s+(\d+):` by hand (no regex crate in core).
fn busy_line_number(line: &str) -> Option<u32> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix("line")?;
    let after_ws = rest.trim_start();
    if after_ws.len() == rest.len() {
        return None; // `\s+` needs at least one whitespace
    }
    let digits_len = after_ws.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_len == 0 || !after_ws[digits_len..].starts_with(':') {
        return None;
    }
    after_ws[..digits_len].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_names_and_channels() {
        assert_eq!(
            LED_COLOUR_NAMES.map(|c| c.as_str()),
            ["off", "red", "green", "blue", "purple", "cyan", "white"]
        );
        assert!(is_led_colour("purple"));
        assert!(!is_led_colour("yellow"));
        assert!(!is_led_colour("amber"));
        assert_eq!(LedColour::Purple.channels(), ch(true, false, true));
        assert!(is_led_pattern("flash2"));
        assert!(!is_led_pattern("strobe"));
        assert_eq!(
            serde_json::to_string(&LedPattern::BlinkFast).unwrap(),
            r#""blink-fast""#
        );
        assert_eq!(
            serde_json::to_string(&LedPattern::Flash2).unwrap(),
            r#""flash2""#
        );
    }

    /// The derived assertions of `led.test.ts:94-122` (duty, rate, cycle
    /// length); the exact tables are replayed by `tests/led_fixtures.rs`.
    #[test]
    fn timeline_shape_assertions_from_the_vitest() {
        let fast = led_timeline(LedPattern::BlinkFast);
        let slow = led_timeline(LedPattern::BlinkSlow);
        for tl in [&fast, &slow] {
            assert!(tl.repeat);
            assert_eq!(
                tl.steps.iter().map(|s| s.on).collect::<Vec<_>>(),
                [true, false]
            );
            assert_eq!(tl.steps[0].ms, tl.steps[1].ms);
        }
        let hz = |tl: &LedTimeline| 1000.0 / (tl.steps[0].ms + tl.steps[1].ms) as f64;
        assert!((hz(&fast) - 4.0).abs() < 0.05);
        assert!((hz(&slow) - 1.0).abs() < 0.05);
        let hb = led_timeline(LedPattern::Heartbeat);
        assert!(hb.repeat);
        assert_eq!(
            hb.steps.iter().map(|s| s.on).collect::<Vec<_>>(),
            [true, false, true, false]
        );
        let cycle: u64 = hb.steps.iter().map(|s| s.ms).sum();
        assert!((5000..=6000).contains(&cycle));
        assert!(hb.steps[3].ms as f64 / cycle as f64 > 0.9);
        let f2 = led_timeline(LedPattern::Flash2);
        assert!(!f2.repeat);
        assert_eq!(
            f2.steps.iter().map(|s| s.on).collect::<Vec<_>>(),
            [true, false, true]
        );
        assert!(f2.steps.iter().map(|s| s.ms).sum::<u64>() < 2000);
    }

    #[test]
    fn busy_line_regex_equivalence() {
        assert_eq!(busy_line_number("line 16: x"), Some(16));
        assert_eq!(busy_line_number("\t  line\t7:"), Some(7));
        assert_eq!(busy_line_number("line16:"), None);
        assert_eq!(busy_line_number("line 16 :"), None);
        assert_eq!(busy_line_number("lines 16:"), None);
        assert_eq!(busy_line_number("xline 16:"), None);
        assert_eq!(
            parse_busy_pins("line 1: a [used]\r\nline 2: b\nline 3: c [used]"),
            [1, 3]
        );
    }

    #[test]
    fn pin_map_from_hardware_block() {
        let m = LedPinMap::from_hardware(StatusLedPins {
            r: 5.0,
            g: 6.0,
            b: 7.0,
        });
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"kind":"rgb-gpio","pins":{"r":5,"g":6,"b":7},"activeHigh":true}"#
        );
        assert!(validate_pin_map(&m, &[]).ok);
        let nan = LedPinMap {
            pins: LedPins {
                r: f64::NAN,
                g: 1.0,
                b: 2.0,
            },
            ..status_led_map()
        };
        assert!(!validate_pin_map(&nan, &[]).ok);
    }
}
