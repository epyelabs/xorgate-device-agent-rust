//! The I/O half of `BootDeferPolicy` (`src/recording/boot-policy.ts:304-396`):
//! `readBudgetMa` (`:377-385`) over `/proc/device-tree/chosen/power/max_current`
//! and `readTelemetry` (`:387-395`) over `LTE_TELEMETRY_FILE`, plus the
//! injected GNSS view. The decisions are `xorgate_core::boot_policy`; the
//! clock (wall + mono) is the caller's `&dyn Clock` (Phase 3 P8).
//!
//! Both reads are synchronous like the TS `readFileSync` (the policy is
//! evaluated inside the manager's synchronous `applyVideo`; both files are
//! procfs/tmpfs).

use std::path::PathBuf;
use std::sync::Arc;

use xorgate_core::boot_policy::{parse_budget_ma, BootDeferSources, PolicyTelemetry};
use xorgate_core::types::GnssView;

use crate::io::TapeSink;
use crate::modem_file;

/// `MAX_CURRENT_PATH` (`boot-policy.ts:129`): the negotiated PSU budget, a
/// big-endian u32 in mA (device tree).
pub const MAX_CURRENT_PATH: &str = "/proc/device-tree/chosen/power/max_current";

/// The GNSS view closure (`opts.gnss`, `boot-policy.ts:296`).
pub type GnssFn = Arc<dyn Fn() -> Option<GnssView> + Send + Sync>;

/// [`BootDeferSources`] over the real files.
pub struct FileBootSources {
    /// `budgetPath` (default [`MAX_CURRENT_PATH`]).
    pub budget_path: PathBuf,
    /// `telemetryFile` (default `LTE_TELEMETRY_FILE`).
    pub telemetry_file: PathBuf,
    /// Every telemetry.json read is reported like the modem collector's (`lte_file`).
    pub tape: Arc<dyn TapeSink>,
    /// `None` = no GPS collector (the TS default `() => null`): the gate never arms.
    pub gnss: Option<GnssFn>,
}

impl FileBootSources {
    /// The production paths: the device-tree budget and the agent's
    /// `LTE_TELEMETRY_FILE` (`Paths::lte_telemetry`).
    pub fn new(telemetry_file: PathBuf, tape: Arc<dyn TapeSink>, gnss: Option<GnssFn>) -> Self {
        Self {
            budget_path: PathBuf::from(MAX_CURRENT_PATH),
            telemetry_file,
            tape,
            gnss,
        }
    }
}

impl BootDeferSources for FileBootSources {
    /// `readBudgetMa` (`boot-policy.ts:377-385`): any read error or < 4 bytes → `None`.
    fn read_budget_ma(&mut self) -> Option<u32> {
        std::fs::read(&self.budget_path)
            .ok()
            .and_then(|b| parse_budget_ma(&b))
    }

    /// `readTelemetry` (`boot-policy.ts:387-395`): `JSON.parse(readFileSync(f,
    /// "utf8"))`, `null` unless an object (arrays included, as `typeof [] ===
    /// "object"`). Bytes decoded lossily first (review-wp3 #4 / CF21).
    fn read_telemetry(&mut self) -> Option<PolicyTelemetry> {
        let snap = modem_file::read_blocking(&self.telemetry_file, self.tape.as_ref());
        snap.raw.as_ref().and_then(PolicyTelemetry::from_value)
    }

    /// `this.gnss()` (`boot-policy.ts:349`).
    fn gnss(&mut self) -> Option<GnssView> {
        self.gnss.as_ref().and_then(|g| g())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::null_tape;
    use xorgate_core::types::SkyState;

    #[test]
    fn reads_budget_telemetry_and_gnss() {
        let d = std::env::temp_dir().join(format!("xg-bootsrc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let budget = d.join("max_current");
        let tele = d.join("telemetry.json");
        let mut src = FileBootSources {
            budget_path: budget.clone(),
            telemetry_file: tele.clone(),
            tape: null_tape(),
            gnss: None,
        };
        // Absent files: unreadable budget, missing telemetry, no GNSS collector.
        assert_eq!(src.read_budget_ma(), None);
        assert_eq!(src.read_telemetry(), None);
        assert_eq!(src.gnss(), None);
        std::fs::write(&budget, 3000u32.to_be_bytes()).unwrap();
        assert_eq!(src.read_budget_ma(), Some(3000));
        std::fs::write(&budget, [0u8, 1]).unwrap();
        assert_eq!(src.read_budget_ma(), None, "< 4 bytes");
        // A stray invalid byte inside a string does not make the file "missing".
        let mut bytes = br#"{"timestamp":"2026-10-10T00:00:00Z","status":"conn"#.to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(br#"ecting","reg":0}"#);
        std::fs::write(&tele, bytes).unwrap();
        let t = src.read_telemetry().expect("lossy decode still parses");
        assert_eq!(t.reg, Some(serde_json::json!(0)));
        std::fs::write(&tele, "null").unwrap();
        assert_eq!(src.read_telemetry(), None);
        std::fs::write(&tele, "{torn").unwrap();
        assert_eq!(src.read_telemetry(), None);
        src.gnss = Some(Arc::new(|| {
            Some(GnssView {
                fix: true,
                sky: SkyState::Clear,
            })
        }));
        assert_eq!(src.gnss().map(|g| g.fix), Some(true));
        let _ = std::fs::remove_dir_all(&d);
    }
}
