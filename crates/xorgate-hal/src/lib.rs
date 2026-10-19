//! `xorgate-hal` — the Linux edges of the agent (plan README §3.1): tokio::fs
//! readers over procfs/sysfs, bounded subprocess runs (`vcgencmd`), statvfs,
//! and the atomic JSON writer every state file goes through.
//!
//! Everything here feeds or is fed by the pure parsers in `xorgate-core`;
//! nothing here decides anything. Every reader fails OPEN: a missing file or
//! tool yields `None` and the caller omits the field (agent-anatomy §10.3).

pub mod atomic_write;
pub mod boot_config_io;
pub mod boot_id;
pub mod boot_sources;
pub mod camera_list;
pub mod child;
pub mod config_sync_file;
pub mod counters_file;
pub mod disk_scan;
pub mod exec;
pub mod fragment_stat;
pub mod fs_watch;
pub mod gzip_finalize;
pub mod hardware_file;
pub mod imu_detect;
pub mod io;
pub mod kvs_manifest_io;
pub mod kvs_runtime;
pub mod led_driver;
pub mod ledger_file;
pub mod modem_file;
pub mod nmea_tty;
pub mod privileged;
pub mod read_lossy;
pub mod segment_io;
pub mod settings_io;
pub mod shm_reaper;
pub mod system;
pub mod tape;
