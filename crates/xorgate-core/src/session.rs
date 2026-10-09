//! Recording session identity + time provenance. Port of the pure parts of
//! `src/recording/session.ts` (157 lines; plan decisions D5/D6):
//! the UUIDv7 encoder behind `mintSessionId`, the boot-id and probe rules,
//! and the state machine inside `createTimeSourceTracker`.
//!
//! Out of core: `randomBytes` / `Date.now()` defaults (`session.ts:25-26` —
//! injected here), the reads behind `readBootId` (`:49-65`,
//! `/proc/sys/kernel/random/boot_id` + `XORGATE_BOOT_ID`; the rule is
//! [`resolve_boot_id`]), the execs behind `probeTimeSource` (`:83-92`,
//! `timedatectl` + `/sys/class/rtc/rtc0`; the rule is [`probe_verdict`]) and
//! the async probe call of the tracker (the state machine around it is
//! [`TimeSourceTracker`]).
//!
//! `TimeSource` / `SteppedClockSource` themselves live in WP1's
//! [`crate::types`] (`session.ts:67,76`).

use serde::{Deserialize, Serialize};

use crate::clock::WallMs;
use crate::js::trim as js_trim;
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
/// `now_ms` is an integer `WallMs` (`Date.now()` always is); a fractional
/// `now` is the caller's `floor` to apply.
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

/// `BOOT_ID_FILE` (`session.ts:40`): the kernel's boot UUID.
pub const BOOT_ID_FILE: &str = "/proc/sys/kernel/random/boot_id";
/// The device-simulator override `readBootId` honours first (`session.ts:54`).
pub const BOOT_ID_ENV: &str = "XORGATE_BOOT_ID";

/// The decision of `readBootId` (`session.ts:49-65`) over what the HAL read:
/// a non-blank `XORGATE_BOOT_ID` (trimmed) wins — a container shares the
/// host's boot_id, so the simulator mints one per start; else the trimmed
/// contents of [`BOOT_ID_FILE`]; `None` (file unreadable, off-Linux) means
/// "mint a random per-process UUID" (`randomUUID()`), which keeps the
/// contract: new process = new "boot". The per-process cache is the caller's.
pub fn resolve_boot_id(forced_env: Option<&str>, boot_id_file: Option<&str>) -> Option<String> {
    if let Some(forced) = forced_env.map(js_trim).filter(|f| !f.is_empty()) {
        return Some(forced.to_string());
    }
    boot_id_file.map(|t| js_trim(t).to_string())
}

/// The hardware-only view `probeTimeSource` returns (`session.ts:83`): the
/// probe cannot see an offline GPS/NITZ step, which the tracker layers on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// The decision of `probeTimeSource` (`session.ts:83-92`). `ntp_synchronized`
/// is the stdout of `timedatectl show -p NTPSynchronized --value` when the
/// tool ran and succeeded (`!notFound && !failed`), else `None`;
/// `rtc0_exists` is `existsSync("/sys/class/rtc/rtc0")` (a throw = false).
pub fn probe_verdict(ntp_synchronized: Option<&str>, rtc0_exists: bool) -> ProbedTimeSource {
    if ntp_synchronized.is_some_and(|out| js_trim(out) == "yes") {
        ProbedTimeSource::Ntp
    } else if rtc0_exists {
        ProbedTimeSource::Rtc
    } else {
        ProbedTimeSource::Unsynced
    }
}

/// The state captured by `createTimeSourceTracker`'s closure
/// (`session.ts:119-157`: `value`, `stepped`, `lastProbeAt`, `probing`)
/// with the async probe lifted out. The TS `refresh()` is split at its
/// `await`: [`TimeSourceTracker::begin_refresh`] is the synchronous prefix
/// (`probing = true`), [`TimeSourceTracker::apply_probe`] the continuation
/// once the probe answered, [`TimeSourceTracker::probe_failed`] the
/// `finally` of a probe that threw. `Date.now()` is the explicit `now`.
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
    probing: bool,
}

impl TimeSourceTracker {
    /// The default `refreshMs` (`session.ts:120`).
    pub const DEFAULT_REFRESH_MS: i64 = 60_000;

    /// `createTimeSourceTracker(refreshMs)` (`session.ts:119-126`):
    /// pessimistic initial `unsynced`, nothing stepped, never probed.
    pub fn new(refresh_ms: i64) -> Self {
        Self {
            refresh_ms,
            value: TimeSource::Unsynced,
            stepped: None,
            last_probe_at: 0,
            probing: false,
        }
    }

    /// `refresh()` up to its `await` (`session.ts:129`): a probe is in flight.
    /// The caller runs the probe, then calls [`Self::apply_probe`] or
    /// [`Self::probe_failed`].
    pub fn begin_refresh(&mut self) {
        self.probing = true;
    }

    /// `refresh()` after the probe answered (`session.ts:131-138`): NTP
    /// supersedes any offline step; otherwise the stepped source (if any) is
    /// reported over the probed value. Returns the new value (what the
    /// awaited `refresh()` resolves to).
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
        self.probing = false;
        self.value
    }

    /// The `finally` of a `refresh()` whose probe threw (`session.ts:135-137`):
    /// nothing but `probing` changes (`value`/`lastProbeAt` keep their old
    /// values; `current()`'s `void refresh().catch(() => {})` swallows it).
    pub fn probe_failed(&mut self) {
        self.probing = false;
    }

    /// `current()` (`session.ts:142-148`): the cached value — never blocks —
    /// plus whether the caller must kick a background refresh now. When due,
    /// the slot is claimed (`lastProbeAt = now`) and the refresh begun
    /// ([`Self::begin_refresh`]) before returning, exactly as the TS does
    /// synchronously, so a 20 Hz reader never piles probes up.
    pub fn current(&mut self, now: WallMs) -> (TimeSource, bool) {
        let due = !self.probing && now - self.last_probe_at >= self.refresh_ms;
        if due {
            self.last_probe_at = now; // claim the slot before the async probe lands
            self.begin_refresh();
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

    /// Whether a probe is in flight.
    pub fn probing(&self) -> bool {
        self.probing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v7_layout() {
        let id = mint_session_id(1_800_000_000_000, [0xff; 16]);
        // 0x01a3185c5000 = 1_800_000_000_000
        assert_eq!(id, "01a3185c-5000-7fff-bfff-ffffffffffff");
        let neg = mint_session_id(-5, [0; 16]);
        assert_eq!(neg, "00000000-0000-7000-8000-000000000000");
        // 2^48 + 1 drops the high bit.
        let wrap = mint_session_id((1i64 << 48) + 1, [0; 16]);
        assert!(wrap.starts_with("00000000-0001-7000-8000-"));
    }

    #[test]
    fn tracker_claims_the_refresh_slot() {
        let t0: WallMs = 1_750_000_000_000;
        let mut t = TimeSourceTracker::new(60_000);
        // Never probed (lastProbeAt 0): the first read is due and claims the slot.
        assert_eq!(t.current(t0), (TimeSource::Unsynced, true));
        assert!(t.probing());
        // In flight: no second probe, however stale.
        assert_eq!(t.current(t0 + 120_000), (TimeSource::Unsynced, false));
        assert_eq!(
            t.apply_probe(ProbedTimeSource::Rtc, t0 + 10),
            TimeSource::Rtc
        );
        assert_eq!(t.current(t0 + 60_009), (TimeSource::Rtc, false));
        assert_eq!(t.current(t0 + 60_010), (TimeSource::Rtc, true));
        // A failed probe changes nothing but the in-flight flag.
        t.probe_failed();
        assert!(!t.probing());
        assert_eq!(t.current(t0 + 60_011), (TimeSource::Rtc, false));
        t.note_step(SteppedClockSource::Gps);
        assert_eq!(t.value(), TimeSource::Gps);
        assert_eq!(
            t.apply_probe(ProbedTimeSource::Unsynced, t0 + 70_000),
            TimeSource::Gps
        );
        assert_eq!(
            t.apply_probe(ProbedTimeSource::Ntp, t0 + 80_000),
            TimeSource::Ntp
        );
        t.note_step(SteppedClockSource::Nitz);
        assert_eq!(t.value(), TimeSource::Ntp);
    }

    #[test]
    fn probe_and_boot_id_decisions() {
        assert_eq!(probe_verdict(Some("yes\n"), false), ProbedTimeSource::Ntp);
        assert_eq!(probe_verdict(Some("no\n"), true), ProbedTimeSource::Rtc);
        assert_eq!(probe_verdict(None, true), ProbedTimeSource::Rtc);
        assert_eq!(
            probe_verdict(Some("yes please"), false),
            ProbedTimeSource::Unsynced
        );
        assert_eq!(
            resolve_boot_id(Some("  sim-boot \n"), Some("k")).as_deref(),
            Some("sim-boot")
        );
        assert_eq!(
            resolve_boot_id(Some("   "), Some("abc\n")).as_deref(),
            Some("abc")
        );
        assert_eq!(resolve_boot_id(None, None), None);
    }
}
