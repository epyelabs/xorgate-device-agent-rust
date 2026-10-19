//! Phase 4 operations (plan `phase-4-operations.md`): the runtime halves of
//! the TS `src/agent/*` modules that drive the device's operational surface.
//! One module per work package (append-only list: WP1 `led`, WP2 `sms`, WP3
//! `clock_stepper` / `camera_mount`, WP6 `hooks`).

pub mod led;
