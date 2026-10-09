//! IMU detection — the I/O half of `src/hardware/imu.ts` `detectImu`
//! (`:123-138`): `commandExists("i2cdetect")` (cached for the life of the
//! detector, like the TS module-level `existsCache`), then `i2cdetect -y 1`
//! with no timeout, parsed by the core grid parser. The mock path (`isMock()`,
//! macOS dev boxes) is not ported: the Rust agent runs on Linux only.
//!
//! Tape: the raw `i2cdetect` stdout (or `notFound`) is reported at the point
//! of reading.

use std::sync::Mutex;

use xorgate_core::imu::{
    build_detect, parse_i2c_detect, unavailable_detect, ImuDetect, I2CDETECT_NOT_FOUND, IMU_I2C_BUS,
};

use crate::exec::{command_exists, run};
use crate::io::TapeSink;

/// `detectImu()` with its `commandExists` cache.
#[derive(Debug, Default)]
pub struct ImuDetector {
    /// `existsCache.get("i2cdetect")`.
    exists: Mutex<Option<bool>>,
}

impl ImuDetector {
    pub fn new() -> Self {
        Self::default()
    }

    fn i2cdetect_exists(&self) -> bool {
        let mut cached = self.exists.lock().unwrap_or_else(|p| p.into_inner());
        *cached.get_or_insert_with(|| command_exists("i2cdetect"))
    }

    /// `detectImu()` (`imu.ts:123-138`).
    pub async fn detect(&self, tape: &dyn TapeSink) -> ImuDetect {
        if !self.i2cdetect_exists() {
            tape.i2cdetect(None);
            return unavailable_detect(I2CDETECT_NOT_FOUND);
        }
        let bus = IMU_I2C_BUS.to_string();
        let res = run("i2cdetect", &["-y", &bus], None).await;
        tape.i2cdetect(Some(&res.stdout));
        if res.failed {
            // execa (reject:false) reports a spawn failure with an EMPTY
            // stderr, so a binary that vanished after the `which` check reads
            // as the generic wording — `exit_code: None` marks that case here.
            let stderr = if res.exit_code.is_none() {
                ""
            } else {
                xorgate_core::js::trim(&res.stderr)
            };
            let reason = if stderr.is_empty() {
                format!("i2cdetect failed on bus {IMU_I2C_BUS}")
            } else {
                stderr.to_string()
            };
            return unavailable_detect(&reason);
        }
        build_detect(parse_i2c_detect(&res.stdout))
    }
}
