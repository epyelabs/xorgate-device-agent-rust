//! The session I/O of `src/recording/session.ts` (157 lines): `readBootId`
//! (`:49-65`) and `probeTimeSource` (`:83-92`), plus the two OS clocks the
//! recorder stamps with (`process.hrtime.bigint()` — CLOCK_MONOTONIC ns, the
//! mono epoch a `bootId` marks, D6) and `crypto.randomBytes` /
//! `randomUUID()`. The decisions are core's `session::{resolve_boot_id,
//! probe_verdict}`; this file only reads.

use std::path::Path;
use std::sync::OnceLock;

use xorgate_core::session::{
    probe_verdict, resolve_boot_id, ProbedTimeSource, BOOT_ID_ENV, BOOT_ID_FILE,
};

use crate::exec::run;
use crate::io::TapeSink;

/// `/sys/class/rtc/rtc0` (`session.ts:87`).
pub const RTC0_PATH: &str = "/sys/class/rtc/rtc0";

/// `let cachedBootId` (`session.ts:42`): one value per process.
static BOOT_ID: OnceLock<String> = OnceLock::new();

/// `readBootId()` (`session.ts:49-65`): `XORGATE_BOOT_ID` (trimmed,
/// non-blank) > the trimmed `/proc/sys/kernel/random/boot_id` > a random
/// per-process UUID (`randomUUID()`), cached for the process lifetime — a
/// new process is a new "boot" off-Linux. The file is read as bytes then
/// decoded lossily (D4).
pub fn read_boot_id() -> String {
    BOOT_ID
        .get_or_init(|| {
            let forced = std::env::var(BOOT_ID_ENV).ok();
            let file = crate::read_lossy::read_text(Path::new(BOOT_ID_FILE)).ok();
            resolve_boot_id(forced.as_deref(), file.as_deref()).unwrap_or_else(random_uuid_v4)
        })
        .clone()
}

/// 16 bytes from the kernel CSPRNG (`crypto.randomBytes(16)`). Falls back to
/// a time/pid mix only if `/dev/urandom` is unreadable (never on Linux).
pub fn random_bytes16() -> [u8; 16] {
    use std::io::Read;
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut b).is_ok() {
            return b;
        }
    }
    let seed =
        monotonic_ns() ^ ((std::process::id() as u64) << 32) ^ crate::tape::wall_now_ms() as u64;
    for (i, x) in b.iter_mut().enumerate() {
        *x = (seed.rotate_left((i as u32 * 8) % 64) & 0xff) as u8 ^ (i as u8).wrapping_mul(0x9d);
    }
    b
}

/// `crypto.randomUUID()`: an RFC 4122 version-4 UUID, lowercase hex.
pub fn random_uuid_v4() -> String {
    let mut b = random_bytes16();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
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

/// `process.hrtime.bigint()` on Linux: CLOCK_MONOTONIC in nanoseconds — the
/// `mono` of every telemetry sample line (`telemetry-recorder.ts:115`) and
/// the clock a `bootId` scopes (D6). 0 if the call fails (it cannot on Linux).
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec we own; CLOCK_MONOTONIC
    // is always available on Linux and macOS.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// `probeTimeSource()` (`session.ts:83-92`): one uncached probe —
/// `timedatectl show -p NTPSynchronized --value` (no timeout here; the 2 s
/// bound is the manager's `TIME_PROBE_TIMEOUT_MS` race), stdout `yes` →
/// `ntp`; else `/sys/class/rtc/rtc0` exists → `rtc`; else `unsynced`. Tools
/// resolve through `PATH` (the rig shims them).
pub async fn probe_time_source() -> ProbedTimeSource {
    probe_time_source_taped(&crate::io::NullTape).await
}

/// [`probe_time_source`] reporting the `timedatectl` run to the tape (`exec`).
pub async fn probe_time_source_taped(tape: &dyn TapeSink) -> ProbedTimeSource {
    let args = ["show", "-p", "NTPSynchronized", "--value"];
    let res = run("timedatectl", &args, None).await;
    tape.exec(
        &["timedatectl", "show", "-p", "NTPSynchronized", "--value"],
        &res.stdout,
        res.exit_code,
    );
    let ntp = (!res.not_found && !res.failed).then_some(res.stdout.as_str());
    // `existsSync` never throws; a failing stat reads as absent.
    let rtc0 = Path::new(RTC0_PATH).exists();
    probe_verdict(ntp, rtc0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v4_shape() {
        let u = random_uuid_v4();
        assert_eq!(u.len(), 36);
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"));
        assert_ne!(random_uuid_v4(), u);
    }

    #[test]
    fn monotonic_ns_advances() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(a > 0 && b >= a);
    }
}
