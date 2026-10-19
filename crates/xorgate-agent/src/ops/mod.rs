//! Phase 4 operations: the runtimes behind the `PhaseHooks` methods and the
//! worker's Phase 4 call sites (LED, SMS, clock stepper, camera mount).
//!
//! Module list: append-only, alphabetical (shared file; the orchestrator
//! keeps every line at merge).

pub mod camera_mount;
pub mod clock_stepper;
