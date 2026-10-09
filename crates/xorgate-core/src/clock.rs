//! Injected clocks (plan Phase 1 rule 3). Nothing in `xorgate-core` reads
//! `SystemTime` or `Instant`; every decider takes `&dyn Clock` or explicit
//! `now`/`mono` arguments. The TS agent has the same split: `Date.now()` for
//! naming/stamping and `monotonicMs()` (`src/recording/boot-policy.ts:60`,
//! `process.hrtime.bigint() / 1_000_000n`) for every interval that may span a
//! clock step (agent-anatomy invariant 2).

/// Wall-clock epoch milliseconds (`Date.now()`).
pub type WallMs = i64;
/// Monotonic milliseconds since an arbitrary origin (`monotonicMs()`).
pub type MonoMs = u64;

pub trait Clock {
    /// `Date.now()` — names things and stamps samples; may jump.
    fn wall_ms(&self) -> WallMs;
    /// `monotonicMs()` — never jumps; the only clock intervals are measured on.
    fn mono_ms(&self) -> MonoMs;
}

/// A clock that returns fixed values until told otherwise. Test helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FixedClock {
    pub wall: WallMs,
    pub mono: MonoMs,
}

impl FixedClock {
    pub fn new(wall: WallMs, mono: MonoMs) -> Self {
        Self { wall, mono }
    }
}

impl Clock for FixedClock {
    fn wall_ms(&self) -> WallMs {
        self.wall
    }
    fn mono_ms(&self) -> MonoMs {
        self.mono
    }
}

/// A clock a test script advances by hand; wall and mono move together
/// unless [`StepClock::step_wall`] / [`StepClock::step_mono`] are used to
/// simulate a wall-clock correction (the thing the monotonic split exists for).
///
/// Interior mutability so it can be shared as `&dyn Clock` while the test
/// drives it.
#[derive(Debug, Default)]
pub struct StepClock {
    wall: std::cell::Cell<WallMs>,
    mono: std::cell::Cell<MonoMs>,
}

impl StepClock {
    pub fn new(wall: WallMs, mono: MonoMs) -> Self {
        Self {
            wall: std::cell::Cell::new(wall),
            mono: std::cell::Cell::new(mono),
        }
    }

    /// Advance both clocks by `ms` (time passing normally).
    pub fn advance(&self, ms: u64) {
        self.wall.set(self.wall.get() + ms as WallMs);
        self.mono.set(self.mono.get() + ms);
    }

    /// Move the wall clock alone (a clock step / NTP correction).
    pub fn step_wall(&self, delta_ms: i64) {
        self.wall.set(self.wall.get() + delta_ms);
    }

    /// Move the monotonic clock alone.
    pub fn step_mono(&self, ms: u64) {
        self.mono.set(self.mono.get() + ms);
    }

    pub fn set(&self, wall: WallMs, mono: MonoMs) {
        self.wall.set(wall);
        self.mono.set(mono);
    }
}

impl Clock for StepClock {
    fn wall_ms(&self) -> WallMs {
        self.wall.get()
    }
    fn mono_ms(&self) -> MonoMs {
        self.mono.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_clock_moves_as_told() {
        let c = StepClock::new(1_000, 50);
        let d: &dyn Clock = &c;
        assert_eq!((d.wall_ms(), d.mono_ms()), (1_000, 50));
        c.advance(10);
        assert_eq!((d.wall_ms(), d.mono_ms()), (1_010, 60));
        c.step_wall(-500);
        assert_eq!((d.wall_ms(), d.mono_ms()), (510, 60));
        c.step_mono(5);
        assert_eq!((d.wall_ms(), d.mono_ms()), (510, 65));
        let f = FixedClock::new(7, 8);
        assert_eq!((f.wall_ms(), f.mono_ms()), (7, 8));
    }
}
