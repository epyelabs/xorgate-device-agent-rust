//! `xorgate-core` — the PURE half of the Rust device agent (plan README §3.1).
//!
//! Rules for this crate: no I/O, no tokio, no wall clock (timestamps are
//! injected by callers), no process spawning. Everything here is a 1:1 port
//! of a pure TypeScript module in `device/xorgate-device-agent` and is tested
//! against fixtures extracted from that repo's vitest suites (`fixtures/`).
//!
//! Phase 0 content: the telemetry payload shapes + serde (`telemetry`), the
//! `system` collector's parsers and field assembly (`system`), the JS-exact
//! rounding helper (`round`) and the MQTT topic builder (`topics`).

pub mod round;
pub mod system;
pub mod telemetry;
pub mod topics;
