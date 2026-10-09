//! `xorgate-core` — the PURE half of the Rust device agent (plan README §3.1).
//!
//! Rules for this crate: no I/O, no tokio, no wall clock (timestamps are
//! injected by callers — see [`clock`]), no process spawning, no `unsafe`.
//! Everything here is a 1:1 port of a pure TypeScript module in
//! `device/xorgate-device-agent` and is tested against fixtures extracted
//! from that repo's vitest suites (`fixtures/`). `tests/purity.rs` enforces
//! the dependency rules.
//!
//! Phase 0: the telemetry payload shapes + serde (`telemetry`), the `system`
//! collector's parsers and field assembly (`system`), the JS-exact rounding
//! helper (`round`) and the MQTT topic builder (`topics`).
//!
//! Phase 1 WP1 (foundation): injected clocks (`clock`), the shared types the
//! later work packages hang off (`types`), the complete `telemetry` type set,
//! IMU `orientation` math, the settings.json allowlist + clamp parsers
//! (`settings`), the resolved recording config and preset table
//! (`recording_config`, `video_presets`), the cloud namespace registry
//! (`config_namespaces`), the device `hardware_block` parser, the pure
//! `network` parsers, `build_info` and the `settings_migrate` decision.

#![forbid(unsafe_code)]

pub mod boot_config;
pub mod boot_policy;
pub mod build_info;
pub mod cameras;
pub mod clock;
pub mod clock_step;
pub mod cloud_config;
pub mod commands;
pub mod config_namespaces;
pub mod config_report;
pub mod hardware_block;
pub mod imu;
pub mod js;
pub mod led;
pub mod led_state;
pub mod modem;
pub mod network;
pub mod nmea;
pub mod orientation;
pub mod pipeline_builder;
pub mod power;
pub mod recording_config;
pub mod round;
pub mod segments;
pub mod session;
pub mod settings;
pub mod settings_migrate;
pub mod sidecar;
pub mod sky_state;
pub mod sms;
pub mod sms_pdu;
pub mod system;
pub mod telemetry;
pub mod telemetry_scope;
pub mod topics;
pub mod types;
pub mod video_capture;
pub mod video_presets;
