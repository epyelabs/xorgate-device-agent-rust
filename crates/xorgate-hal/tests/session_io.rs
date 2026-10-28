//! WP2 / CF43: the two I/O it()s of `test/recording-session.test.ts`
//! ("returns a stable per-process boot id", "probes to one of the three
//! provenance values without throwing") as HAL tests, plus the probe's
//! branches with a PATH-shimmed `timedatectl` (the rig's route). One test
//! function: it mutates PATH / XORGATE_BOOT_ID for the whole process.

use std::path::Path;

use xorgate_core::session::ProbedTimeSource;
use xorgate_hal::boot_id::{probe_time_source, read_boot_id, RTC0_PATH};

fn shim(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join("timedatectl");
    xorgate_hal::test_support::write_executable(&p, &format!("#!/bin/sh\n{body}\n")).unwrap();
}

#[tokio::test]
async fn boot_id_and_time_source_probe() {
    // Stable per process; XORGATE_BOOT_ID wins when set before the first read.
    // Single test in this binary: nothing else reads the environment.
    std::env::set_var("XORGATE_BOOT_ID", "  sim-boot-7 \n");
    let a = read_boot_id();
    assert_eq!(a, "sim-boot-7");
    std::env::remove_var("XORGATE_BOOT_ID");
    assert_eq!(read_boot_id(), a, "cached for the process lifetime");

    // Whatever the host, the probe lands on one of the three values.
    let v = probe_time_source().await;
    assert!(matches!(
        v,
        ProbedTimeSource::Ntp | ProbedTimeSource::Rtc | ProbedTimeSource::Unsynced
    ));

    let base = std::env::temp_dir().join(format!("xg-wp2-shim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let no_ntp = if Path::new(RTC0_PATH).exists() {
        ProbedTimeSource::Rtc
    } else {
        ProbedTimeSource::Unsynced
    };
    let cases: [(&str, Option<&str>, ProbedTimeSource); 5] = [
        ("yes", Some("echo yes"), ProbedTimeSource::Ntp),
        (
            "yes-trimmed",
            Some("printf '  yes\\n\\n'"),
            ProbedTimeSource::Ntp,
        ),
        ("no", Some("echo no"), no_ntp),
        ("failed", Some("echo yes; exit 1"), no_ntp),
        ("absent", None, no_ntp),
    ];
    for (tag, body, want) in cases {
        let dir = base.join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(b) = body {
            shim(&dir, b);
        }
        std::env::set_var("PATH", &dir);
        assert_eq!(probe_time_source().await, want, "{tag}");
    }
    let _ = std::fs::remove_dir_all(&base);
}
