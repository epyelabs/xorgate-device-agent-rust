//! The media spawn guard (cm4-support Phase 2): the pure rule of the
//! worker's `config.media.streamGuard` closure (`ts:src/worker.ts:205-218`)
//! and the per-stream reconcile verdicts it reads
//! (`StreamHardwareStatus`, `ts:src/media/hardware-reconcile.ts:41-50`).
//!
//! A manifest channel the model does not declare (an orphan channel row), or
//! one whose camera did not enumerate, gets NO master — never the runtime's
//! built-in test pipeline. Replay mode's own guard (`worker.ts:213`) is not
//! ported (P14 / Q6).

/// `StreamHardwareStatus` (`hardware-reconcile.ts:42-50`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamHardwareStatus {
    /// Trio rendered/refreshed this pass (content changed on disk).
    Rendered,
    /// Camera present and artifacts already current — nothing written.
    Ok,
    /// Pipeline exists but is operator-customized — left alone.
    Custom,
    /// The model expects a camera on this connector and none enumerated.
    NoCamera,
}

impl StreamHardwareStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rendered => "rendered",
            Self::Ok => "ok",
            Self::Custom => "custom",
            Self::NoCamera => "no-camera",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "rendered" => Self::Rendered,
            "ok" => Self::Ok,
            "custom" => Self::Custom,
            "no-camera" => Self::NoCamera,
            _ => return None,
        })
    }
}

/// The guard's verdict (`"ok" | "no-camera" | "no-pipeline"`, `config.ts:63`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamGuard {
    Ok,
    NoCamera,
    NoPipeline,
}

impl StreamGuard {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoCamera => "no-camera",
            Self::NoPipeline => "no-pipeline",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => Self::Ok,
            "no-camera" => Self::NoCamera,
            "no-pipeline" => Self::NoPipeline,
            _ => return None,
        })
    }
}

/// `config.media.streamGuard` (`worker.ts:209-219`), real mode:
///
/// ```ts
/// if (!hardwareBlock || !streamGuardArmed) return "ok";
/// const status = streamHardware.get(streamKey);
/// if (status === undefined || status === "no-camera") return "no-camera";
/// return resolveChannelPipeline(dir, streamKey, global) ? "ok" : "no-pipeline";
/// ```
///
/// `has_block` = a hardware block is loaded; `armed` = a reconcile pass
/// could enumerate cameras (`worker.ts:144-148`); `status` = that pass's
/// verdict for the stream (`None` = not in the block's `video` list);
/// `pipeline_resolves` = `resolveChannelPipeline(...)` is truthy — evaluated
/// only when reached, like the TS (it is an `existsSync`).
pub fn stream_guard_verdict(
    has_block: bool,
    armed: bool,
    status: Option<StreamHardwareStatus>,
    pipeline_resolves: impl FnOnce() -> bool,
) -> StreamGuard {
    if !has_block || !armed {
        return StreamGuard::Ok;
    }
    match status {
        None | Some(StreamHardwareStatus::NoCamera) => StreamGuard::NoCamera,
        Some(_) => {
            if pipeline_resolves() {
                StreamGuard::Ok
            } else {
                StreamGuard::NoPipeline
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unarmed_or_blockless_is_open_and_never_probes() {
        let probe = || -> bool { panic!("must not resolve the pipeline") };
        assert_eq!(
            stream_guard_verdict(false, true, None, probe),
            StreamGuard::Ok
        );
        assert_eq!(
            stream_guard_verdict(true, false, Some(StreamHardwareStatus::NoCamera), probe),
            StreamGuard::Ok
        );
    }

    #[test]
    fn strings_round_trip() {
        for g in [
            StreamGuard::Ok,
            StreamGuard::NoCamera,
            StreamGuard::NoPipeline,
        ] {
            assert_eq!(StreamGuard::parse(g.as_str()), Some(g));
        }
        for s in [
            StreamHardwareStatus::Rendered,
            StreamHardwareStatus::Ok,
            StreamHardwareStatus::Custom,
            StreamHardwareStatus::NoCamera,
        ] {
            assert_eq!(StreamHardwareStatus::parse(s.as_str()), Some(s));
        }
    }
}
