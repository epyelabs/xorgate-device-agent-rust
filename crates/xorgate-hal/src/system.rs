//! The `system` collector's I/O half — the readers behind
//! `src/hardware/system.ts` / `src/hardware/power.ts` and the per-tick state
//! of `src/agent/collectors/system.ts` (`createSystemCollector`):
//!
//! | field            | source                                               |
//! |------------------|------------------------------------------------------|
//! | cpu_usage        | `/proc/stat` cpuN lines, delta vs the previous tick  |
//! | ram_usage        | `/proc/meminfo` MemTotal/MemAvailable (sysinfo fallback) |
//! | cpu_temp         | `/sys/class/thermal/thermal_zone0/temp`               |
//! | disk_usage       | statvfs("/") shaped like `df -kP /`                   |
//! | throttled        | `vcgencmd get_throttled` (only if vcgencmd is on PATH)|
//! | rail_5v          | `vcgencmd pmic_read_adc` EXT5V (latched off after one failure, or `rails:false`) |
//! | psu_max_current  | `/proc/device-tree/chosen/power/max_current`, read once |
//!
//! Every reader fails open (`None` → field omitted). The CPU delta is kept in
//! the sampler like the TS closure's `prev`; the first `read()` right after
//! construction may report ~0 % for one tick, exactly like the TS.

use std::path::Path;
use std::time::Duration;

use xorgate_core::system::*;
use xorgate_core::telemetry::MetricGroup;

use crate::exec::{command_exists, run};

pub const PROC_STAT: &str = "/proc/stat";
pub const PROC_MEMINFO: &str = "/proc/meminfo";
pub const THERMAL_ZONE0: &str = "/sys/class/thermal/thermal_zone0/temp";
pub const DT_MAX_CURRENT: &str = "/proc/device-tree/chosen/power/max_current";
pub const DISK_MOUNT: &str = "/";
const VCGENCMD_TIMEOUT: Duration = Duration::from_secs(3);

/// Per-process state of the system collector (the TS closure).
pub struct SystemSampler {
    prev: CpuSample,
    has_vcgencmd: bool,
    pmic_unsupported: bool,
    psu_max_current_ma: Option<u32>,
}

impl SystemSampler {
    /// `rails = false` (hardware block `rails:false`, cm4-support) pre-latches
    /// the PMIC reads off and skips the max_current probe.
    pub async fn new(rails: bool) -> Self {
        Self {
            prev: sample_cpu().await,
            has_vcgencmd: command_exists("vcgencmd"),
            pmic_unsupported: !rails,
            psu_max_current_ma: if rails {
                read_max_current_ma(Path::new(DT_MAX_CURRENT)).await
            } else {
                None
            },
        }
    }

    /// One tick: derive the reading and advance the CPU delta window.
    pub async fn read(&mut self) -> MetricGroup {
        let reading = self.compute().await;
        system_group(&reading)
    }

    async fn compute(&mut self) -> SystemReading {
        let curr = sample_cpu().await;
        let cpu_overall = compute_cpu_load(&self.prev, &curr).overall;
        self.prev = curr;

        let meminfo = tokio::fs::read_to_string(PROC_MEMINFO).await.ok();
        let ram = read_memory(meminfo.as_deref(), os_mem_fallback());

        let cpu_temp_c = match tokio::fs::read_to_string(THERMAL_ZONE0).await {
            Ok(raw) => cpu_temp_from_text(&raw),
            Err(_) => None,
        };

        let disk = read_disk(DISK_MOUNT);

        let mut throttled = None;
        let mut rail_5v = None;
        if self.has_vcgencmd {
            throttled = read_throttled().await.map(|t| t.current);
            if !self.pmic_unsupported {
                match read_pmic_adc().await {
                    None => self.pmic_unsupported = true,
                    Some(adc) => rail_5v = rail_5v_from(&adc),
                }
            }
        }

        SystemReading {
            cpu_overall,
            ram_used_pct: ram.map(|u| u.used_pct),
            cpu_temp_c,
            disk_used_pct: disk.map(|u| u.used_pct),
            throttled,
            rail_5v,
            psu_max_current_ma: self.psu_max_current_ma,
        }
    }
}

/// `os.cpus()`: the per-core counters from `/proc/stat`. Empty off-Linux.
pub async fn sample_cpu() -> CpuSample {
    match tokio::fs::read_to_string(PROC_STAT).await {
        Ok(text) => parse_proc_stat(&text),
        Err(_) => Vec::new(),
    }
}

/// `os.totalmem()` / `os.freemem()` via sysinfo(2) — the fallback when
/// `/proc/meminfo` is unavailable.
pub fn os_mem_fallback() -> Option<(f64, f64)> {
    // SAFETY: sysinfo writes into the zeroed struct we hand it.
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sysinfo(&mut info) };
    if rc != 0 {
        return None;
    }
    let unit = info.mem_unit.max(1) as f64;
    Some((info.totalram as f64 * unit, info.freeram as f64 * unit))
}

/// Disk usage of `mount` as `df -kP` would report it. `None` when statvfs fails.
pub fn read_disk(mount: &str) -> Option<UsageInfo> {
    let st = nix::sys::statvfs::statvfs(mount).ok()?;
    disk_usage_from_statvfs(
        st.blocks() as u64,
        st.blocks_free() as u64,
        st.fragment_size() as u64,
    )
}

/// `vcgencmd get_throttled`; `None` on failure/timeout.
pub async fn read_throttled() -> Option<ThrottleState> {
    let res = run("vcgencmd", &["get_throttled"], Some(VCGENCMD_TIMEOUT)).await;
    if res.failed {
        return None;
    }
    parse_throttled(&res.stdout)
}

/// `vcgencmd pmic_read_adc`; `None` on failure/timeout/unsupported.
pub async fn read_pmic_adc() -> Option<PmicAdc> {
    let res = run("vcgencmd", &["pmic_read_adc"], Some(VCGENCMD_TIMEOUT)).await;
    if res.failed {
        return None;
    }
    parse_pmic_adc(&res.stdout)
}

/// PSU max current negotiated at boot (mA); `None` when the node is absent.
pub async fn read_max_current_ma(path: &Path) -> Option<u32> {
    let buf = tokio::fs::read(path).await.ok()?;
    max_current_ma_from_bytes(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sampler_always_has_cpu_and_ram_on_linux() {
        if !Path::new(PROC_STAT).exists() {
            return; // off-Linux dev box
        }
        let mut s = SystemSampler::new(true).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let g = s.read().await;
        let cpu = g.get("cpu_usage").expect("cpu_usage");
        assert_eq!(cpu.unit.as_deref(), Some("%"));
        assert!((0.0..=100.0).contains(&cpu.value.0));
        let ram = g.get("ram_usage").expect("ram_usage");
        assert!((0.0..=100.0).contains(&ram.value.0));
    }

    #[test]
    fn disk_root_is_readable() {
        let u = read_disk("/").expect("statvfs /");
        assert!(u.total_bytes > 0.0);
        assert!((0.0..=100.0).contains(&u.used_pct));
    }
}
