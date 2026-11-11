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

pub mod agent_config;
pub mod boot_config;
pub mod boot_policy;
pub mod build_info;
pub mod cameras;
pub mod capture_ladder;
pub mod capture_timing;
pub mod cli_set;
pub mod clock;
pub mod clock_step;
pub mod clock_trust;
pub mod cloud_config;
pub mod collector_cache;
pub mod command_flow;
pub mod commands;
pub mod config_namespaces;
pub mod config_report;
pub mod config_sync_rules;
pub mod disk_plan;
pub mod gps_group;
pub mod hardware_block;
pub mod imu;
pub mod imu_group;
pub mod js;
pub mod kvs_manifest;
pub mod led;
pub mod led_state;
pub mod led_telemetry;
pub mod led_wiring;
pub mod media_ladder;
pub mod modem;
pub mod network;
pub mod nmea;
pub mod obd;
pub mod obd_supervisor;
pub mod orientation;
pub mod pipeline_builder;
pub mod power;
pub mod recorder_gate;
pub mod recording_config;
pub mod recording_flow;
pub mod round;
pub mod segments;
pub mod session;
pub mod settings;
pub mod settings_migrate;
pub mod sidecar;
pub mod sky_state;
pub mod sms;
pub mod sms_cli;
pub mod sms_pdu;
pub mod stream_guard;
pub mod system;
pub mod telemetry;
pub mod telemetry_scope;
pub mod topics;
pub mod types;
pub mod update_plan;
pub mod upload_keys;
pub mod upload_step;
pub mod video_capture;
pub mod video_presets;
// Phase 7c WP1 (the Rust BNO085 reader's pure codec + its frozen I/O traits).
pub mod imu_io;
pub mod py_fmt;
pub mod sh2;
pub mod shtp;
// Phase 7c WP2: the BNO085 client state machine and its lockstep runner.
pub mod bno08x_client;
pub mod imu_lockstep;
