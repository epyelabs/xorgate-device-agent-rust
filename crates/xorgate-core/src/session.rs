//! Recording session identity + time provenance. Port of the pure parts of
//! `src/recording/session.ts` (157 lines; plan decisions D5/D6):
//! the UUIDv7 encoder behind `mintSessionId` and the state machine inside
//! `createTimeSourceTracker`.
//!
//! Out of core: `randomBytes` / `Date.now()` defaults (`session.ts:25-26` —
//! injected here), `readBootId` (`:49-65`, `/proc/sys/kernel/random/boot_id`
//! + `XORGATE_BOOT_ID`), `probeTimeSource` (`:83-92`, `timedatectl` +
//! `/sys/class/rtc/rtc0`) and the async refresh plumbing of the tracker
//! (`:128-149`, `probing` flag + the fire-and-forget `void refresh()`).
//!
//! `TimeSource` / `SteppedClockSource` themselves live in WP1's
//! [`crate::types`] (`session.ts:67,76`).

use crate::clock::WallMs;
use crate::types::{SteppedClockSource, TimeSource};

/// `mintSessionId` (`session.ts:25-38`): UUIDv7 — 48-bit big-endian epoch
/// ms, then random, with version/variant bits. Sessions must sort roughly by
/// start time (oldest-first upload/eviction), hence the ms prefix.
///
/// `random` is the 16 bytes `randomBytes(16)` produced (bytes 0-5 are
/// overwritten by the timestamp, byte 6's high nibble by the version and
/// byte 8's top two bits by the variant). `now_ms` is `Math.max(0,
/// Math.floor(now))` taken as a BigInt and masked to 48 bits byte by byte —
/// a negative clock reads as 0, a value ≥ 2^48 silently drops its high bits.
pub fn mint_session_id(now_ms: WallMs, random: [u8; 16]) -> String {
    let mut b = random;
    let ms = now_ms.max(0) as u64;
    b[0] = ((ms >> 40) & 0xff) as u8;
    b[1] = ((ms >> 32) & 0xff) as u8;
    b[2] = ((ms >> 24) & 0xff) as u8;
    b[3] = ((ms >> 16) & 0xff) as u8;
    b[4] = ((ms >> 8) & 0xff) as u8;
    b[5] = (ms & 0xff) as u8;
    b[6] = (b[6] & 0x0f) | 0x70; // version 7
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// The hardware-only view `probeTimeSource` returns (`session.ts:83`): the
/// probe cannot see an offline GPS/NITZ step, which the tracker layers on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbedTimeSource {
    Ntp,
    Rtc,
    Unsynced,
}

impl From<ProbedTimeSource> for TimeSource {
    fn from(p: ProbedTimeSource) -> Self {
        match p {
            ProbedTimeSource::Ntp => TimeSource::Ntp,
            ProbedTimeSource::Rtc => TimeSource::Rtc,
            ProbedTimeSource::Unsynced => TimeSource::Unsynced,
        }
    }
}

/// The state inside `createTimeSourceTracker` (`session.ts:119-157`) without
/// the async probe: the caller runs the probe (HAL) and feeds its result to
/// [`TimeSourceTracker::apply_probe`]; [`TimeSourceTracker::current`] says
/// whether a refresh is due and claims the slot exactly like the TS does.
///
/// Provenance rank unsynced < rtc < nitz < gps < ntp; an offline step
/// (`note_step`) is reported over the weaker probed rtc/unsynced until NTP
/// lands, and "ntp" always wins: once the probe sees NTPSynchronized the
/// stepped state is discarded for good (mirrors the stepper's stand-down).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeSourceTracker {
    refresh_ms: i64,
    value: TimeSource,
    stepped: Option<SteppedClockSource>,
    last_probe_at: WallMs,
}

impl TimeSourceTracker {
    /// `createTimeSourceTracker(refreshMs = 60_000)` (`session.ts:119-126`):
    /// pessimistic initial `unsynced`, nothing stepped, never probed.
    pub fn new(refresh_ms: i64) -> Self {
        Self {
            refresh_ms,
            value: TimeSource::Unsynced,
            stepped: None,
            last_probe_at: 0,
        }
    }

    /// The default `refreshMs` (`session.ts:120`).
    pub const DEFAULT_REFRESH_MS: i64 = 60_000;

    /// The body of `refresh()` once the probe has answered (`session.ts:132-134`):
    /// NTP supersedes any offline step; otherwise the stepped source (if any)
    /// is reported over the probed value. Returns the new value.
    pub fn apply_probe(&mut self, probed: ProbedTimeSource, now: WallMs) -> TimeSource {
        if probed == ProbedTimeSource::Ntp {
            self.stepped = None; // NTP supersedes any offline step
        }
        self.value = match (probed, self.stepped) {
            (ProbedTimeSource::Ntp, _) => TimeSource::Ntp,
            (_, Some(stepped)) => stepped.into(),
            (p, None) => p.into(),
        };
        self.last_probe_at = now;
        self.value
    }

    /// `current()` (`session.ts:142-148`): the cached value, plus whether the
    /// caller should kick a refresh now. When it is due the slot is claimed
    /// (`lastProbeAt = now`) before the async probe lands, so a 20 Hz reader
    /// never piles probes up. `probing` (a probe already in flight) is the
    /// caller's flag — pass it in.
    pub fn current(&mut self, now: WallMs, probing: bool) -> (TimeSource, bool) {
        let due = !probing && now - self.last_probe_at >= self.refresh_ms;
        if due {
            self.last_probe_at = now;
        }
        (self.value, due)
    }

    /// `noteStep` (`session.ts:150-156`): the offline clock stepper just set
    /// the system clock from `source`. Reflected immediately (segments
    /// finalising this instant should carry it); never displaces `ntp`.
    pub fn note_step(&mut self, source: SteppedClockSource) {
        self.stepped = Some(source);
        if self.value != TimeSource::Ntp {
            self.value = source.into();
        }
    }

    /// Last known value without the refresh bookkeeping.
    pub fn value(&self) -> TimeSource {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v7_layout() {
        let id = mint_session_id(1_800_000_000_000, [0xff; 16]);
        // 0x01a3185c5000 = 1_800_000_000_000
        assert!(
            id.starts_with("01a3185c-5000-7fff-bfff-ffffffffffff"),
            "{id}"
        );
        assert_eq!(id.len(), 36);
        let neg = mint_session_id(-5, [0; 16]);
        assert_eq!(neg, "00000000-0000-7000-8000-000000000000");
        // 2^48 + 1 drops the high bit.
        let wrap = mint_session_id((1i64 << 48) + 1, [0; 16]);
        assert!(wrap.starts_with("00000000-0001-7000-8000-"));
    }

    #[test]
    fn tracker_claims_the_refresh_slot() {
        let mut t = TimeSourceTracker::new(60_000);
        assert_eq!(t.current(0, false), (TimeSource::Unsynced, true));
        assert_eq!(t.current(1, false), (TimeSource::Unsynced, false));
        assert_eq!(t.current(60_001, true), (TimeSource::Unsynced, false));
        assert_eq!(t.current(60_001, false), (TimeSource::Unsynced, true));
        assert_eq!(
            t.apply_probe(ProbedTimeSource::Rtc, 60_010),
            TimeSource::Rtc
        );
        t.note_step(SteppedClockSource::Gps);
        assert_eq!(t.value(), TimeSource::Gps);
        assert_eq!(
            t.apply_probe(ProbedTimeSource::Unsynced, 70_000),
            TimeSource::Gps
        );
        assert_eq!(
            t.apply_probe(ProbedTimeSource::Ntp, 80_000),
            TimeSource::Ntp
        );
        t.note_step(SteppedClockSource::Nitz);
        assert_eq!(t.value(), TimeSource::Ntp);
    }
}
