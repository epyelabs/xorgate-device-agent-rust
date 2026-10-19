//! Phase 4 operations (plan `phase-4-operations.md`): the runtime halves of
//! the TS `src/agent/*` modules that drive the device's operational surface
//! (LED, SMS, clock stepper, camera mount) behind the `PhaseHooks` methods
//! and the worker's Phase 4 call sites.
//!
//! Module list: alphabetical (shared file; the orchestrator keeps every line
//! at merge).

pub mod camera_mount;
pub mod clock_stepper;
pub mod led;
pub mod sms;
