//! `hardware.json` I/O — `src/hardware/hardware-config.ts:129-161`
//! (`loadHardwareBlock`, `saveHardwareBlock`). The parse and the equality
//! are `xorgate_core::hardware_block`. The worker loads the block at boot
//! (WP1 `worker::load_hardware_block`, which logs its two `[hardware] …`
//! warnings); the config plane persists a CHANGED cloud-delivered block so
//! the next boot seeds from it (`worker.ts:831-836`).

use std::path::Path;

use xorgate_core::hardware_block::{parse_hardware_block, DeviceHardwareBlock};

use crate::atomic_write::{atomic_write_bytes_traced, AtomicWriteError};
use crate::read_lossy::read_text;

/// `JSON.stringify(block)` — the exact bytes `saveHardwareBlock` writes
/// (numbers printed like JS: `maxFps: 30`, not `30.0`).
pub fn hardware_block_json(block: &DeviceHardwareBlock) -> String {
    xorgate_core::js::to_json_string(
        &serde_json::to_value(block).expect("a hardware block serialises"),
    )
}

/// `saveHardwareBlock(path, block)` (`hardware-config.ts:154-156`):
/// `atomicWriteJson(path, block)`. The error is the caller's to log
/// (`[agent] hardware block persist failed: …`, `worker.ts:835`, with
/// [`AtomicWriteError::node_message`]).
pub fn save_hardware_block(
    path: &Path,
    block: &DeviceHardwareBlock,
) -> Result<(), AtomicWriteError> {
    atomic_write_bytes_traced(path, hardware_block_json(block).as_bytes())
}

/// What `loadHardwareBlock` found, before its caller logs anything.
#[derive(Debug, Clone, PartialEq)]
pub enum HardwareFile {
    /// Absent / unreadable: a legacy device or a v1 model (silent).
    Absent,
    /// Not JSON.
    Invalid,
    /// JSON that fails validation.
    FailedValidation,
    Block(DeviceHardwareBlock),
}

/// The read + parse of `loadHardwareBlock` (`hardware-config.ts:129-151`),
/// bytes decoded lossily (CF21).
pub fn read_hardware_file(path: &Path) -> HardwareFile {
    let Some(text) = read_text(path).ok() else {
        return HardwareFile::Absent;
    };
    let Ok(obj) = serde_json::from_str::<serde_json::Value>(&text) else {
        return HardwareFile::Invalid;
    };
    match parse_hardware_block(&obj) {
        Some(b) => HardwareFile::Block(b),
        None => HardwareFile::FailedValidation,
    }
}
