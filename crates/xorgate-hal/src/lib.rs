//! `xorgate-hal` — the Linux edges of the agent (plan README §3.1): tokio::fs
//! readers over procfs/sysfs, bounded subprocess runs (`vcgencmd`), statvfs,
//! and the atomic JSON writer every state file goes through.
//!
//! Everything here feeds or is fed by the pure parsers in `xorgate-core`;
//! nothing here decides anything. Every reader fails OPEN: a missing file or
//! tool yields `None` and the caller omits the field (agent-anatomy §10.3).

pub mod atomic_write;
pub mod child;
pub mod config_sync_file;
pub mod exec;
pub mod hardware_file;
pub mod imu_detect;
pub mod io;
pub mod kvs_manifest_io;
pub mod ledger_file;
pub mod modem_file;
pub mod nmea_tty;
pub mod privileged;
pub mod read_lossy;
pub mod settings_io;
pub mod system;
