//! The cloud config namespace registry as the agent knows it — port of
//! `CONFIG_APPLY_CLASSES` / `CONFIG_NAMESPACE_KEYS` (`src/agent/config-sync.ts:133-160` @ 09da38d).
//! Pinned against the cloud's `config-contract.json` by `tests/config_contract.rs`.
//! WP4 (config_report) extends this module with the rest of the reported plane.

use serde::{Deserialize, Serialize};

/// `ApplyClass` (`config-sync.ts:84-131`): HOW a namespace reaches the hardware,
/// so the cloud can say whether "saved" already means "in force".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyClass {
    /// The worker applies it on its own settings poll / cloud message.
    AgentPoll,
    /// The sim7600-lte daemon applies it on its ~30 s poll of settings.json.
    ModemPoll,
    /// No namespace uses it today (Q4); kept so a future restart-applied key
    /// has somewhere to declare itself.
    AgentRestart,
}

impl ApplyClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ApplyClass::AgentPoll => "agent-poll",
            ApplyClass::ModemPoll => "modem-poll",
            ApplyClass::AgentRestart => "agent-restart",
        }
    }
}

/// `CONFIG_APPLY_CLASSES` (`config-sync.ts:133-151`), in the TS object's
/// (= wire) order. Must stay identical to the cloud registry.
pub const CONFIG_APPLY_CLASSES: &[(&str, ApplyClass)] = &[
    ("gnssAntBias", ApplyClass::ModemPoll),
    ("imuMount", ApplyClass::AgentPoll),
    ("recording", ApplyClass::AgentPoll),
    ("lteRecovery", ApplyClass::ModemPoll),
    ("timeSync", ApplyClass::AgentPoll),
    ("cellular", ApplyClass::ModemPoll),
    // The boot-config reconciler picks it up on every cloud message like the
    // other agent-applied namespaces; the EDIT is immediate, the effect waits
    // for the next boot (the agent never reboots by itself).
    ("cameraMount", ApplyClass::AgentPoll),
    // Device-simulator scenario: only simulated devices are ever sent it; on
    // real hardware it is inert.
    ("simulation", ApplyClass::AgentPoll),
    // Vehicle OBD-II adapter (`config-sync.ts:153-156` @ 09da38d): the OBD
    // collector re-reads settings.json each tick and after every cloud
    // message, and restarts its helper when the adapter/PIN/rate/fields/
    // power source move.
    ("obd", ApplyClass::AgentPoll),
    // Camera sensor per port (RUST-ONLY: the JS agent has no such key and
    // the cloud never sends it to one). The camera reconciler picks it up
    // like `cameraMount`: the config.txt edit is immediate, the effect
    // waits for the next boot.
    ("cameraSensors", ApplyClass::AgentPoll),
];

/// `CONFIG_NAMESPACE_KEYS` (`config-sync.ts:153`): `Object.keys(CONFIG_APPLY_CLASSES)`.
pub const CONFIG_NAMESPACE_KEYS: &[&str] = &[
    "gnssAntBias",
    "imuMount",
    "recording",
    "lteRecovery",
    "timeSync",
    "cellular",
    "cameraMount",
    "simulation",
    "obd",
    "cameraSensors",
];

/// Apply class of a namespace key, or None for an unknown key.
pub fn apply_class_of(namespace: &str) -> Option<ApplyClass> {
    CONFIG_APPLY_CLASSES
        .iter()
        .find(|(k, _)| *k == namespace)
        .map(|(_, c)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_mirror_the_class_table() {
        let keys: Vec<&str> = CONFIG_APPLY_CLASSES.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, CONFIG_NAMESPACE_KEYS);
        assert_eq!(apply_class_of("cellular"), Some(ApplyClass::ModemPoll));
        assert_eq!(apply_class_of("agent"), None);
        assert_eq!(
            serde_json::to_string(&ApplyClass::AgentPoll).unwrap(),
            r#""agent-poll""#
        );
        // The full TS union (config-sync.ts:84), wire string == as_str().
        for c in [
            ApplyClass::ModemPoll,
            ApplyClass::AgentPoll,
            ApplyClass::AgentRestart,
        ] {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
            assert_eq!(serde_json::from_str::<ApplyClass>(&json).unwrap(), c);
        }
    }
}
