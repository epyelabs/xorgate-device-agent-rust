//! MQTT topic builder — the ONE place topic strings come from.
//!
//! Mirrors `src/agent/config.ts:loadConfig()` (`xorgate/devices/${deviceId}/…`)
//! and the tenant-scoped telemetry topic from `src/agent/telemetry-scope.ts`
//! (`xorgate/orgs/${org}/ws/${ws}/devices/${deviceId}/telemetry`). Backend
//! IoT rules are listed in `research/backend-device-contract.md` §2.

/// Every per-device topic the agent publishes to or subscribes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topics {
    /// `xorgate/devices/{id}/telemetry` — QoS 0, unscoped (Phase 0 publishes here only).
    pub telemetry: String,
    /// `xorgate/devices/{id}/status` — QoS 0, not retained; also the LWT topic.
    pub status: String,
    /// `xorgate/devices/{id}/config` — cloud → device, QoS 1, RETAINED.
    pub config: String,
    /// `xorgate/devices/{id}/config/reported` — device → cloud, QoS 1.
    pub config_reported: String,
    /// `xorgate/devices/{id}/command` — cloud → device, QoS 1, not retained.
    pub command: String,
    /// `xorgate/devices/{id}/command/response` — device → cloud, QoS 1.
    pub command_response: String,
    /// `xorgate/devices/{id}/sms` — device → cloud, QoS 1.
    pub sms: String,
}

impl Topics {
    pub fn for_device(device_id: &str) -> Self {
        let base = format!("xorgate/devices/{device_id}");
        Self {
            telemetry: format!("{base}/telemetry"),
            status: format!("{base}/status"),
            config: format!("{base}/config"),
            config_reported: format!("{base}/config/reported"),
            command: format!("{base}/command"),
            command_response: format!("{base}/command/response"),
            sms: format!("{base}/sms"),
        }
    }
}

/// Tenant-scoped telemetry topic (platform-auth Phase 4). Not used in Phase 0
/// (the agent publishes unscoped until the scope plane is ported) but kept here
/// so the string is defined exactly once.
pub fn scoped_telemetry_topic(
    organization_id: &str,
    workspace_id: &str,
    device_id: &str,
) -> String {
    format!("xorgate/orgs/{organization_id}/ws/{workspace_id}/devices/{device_id}/telemetry")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_match_the_ts_config_builder() {
        let t = Topics::for_device("abc");
        assert_eq!(t.telemetry, "xorgate/devices/abc/telemetry");
        assert_eq!(t.status, "xorgate/devices/abc/status");
        assert_eq!(t.config, "xorgate/devices/abc/config");
        assert_eq!(t.config_reported, "xorgate/devices/abc/config/reported");
        assert_eq!(t.command, "xorgate/devices/abc/command");
        assert_eq!(t.command_response, "xorgate/devices/abc/command/response");
        assert_eq!(t.sms, "xorgate/devices/abc/sms");
        assert_eq!(
            scoped_telemetry_topic("o", "w", "abc"),
            "xorgate/orgs/o/ws/w/devices/abc/telemetry"
        );
    }
}
