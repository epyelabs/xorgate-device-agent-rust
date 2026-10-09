//! Fixture-driven tests for `xorgate_core::network` (`test/network.test.ts`,
//! 10 of 11 cases — `ifaceForIp` reads `os.networkInterfaces()` and is HAL)
//! and `xorgate_core::build_info` (`test/build-info.test.ts`, 1 of 3 —
//! `loadBuildInfo`'s disk read + cache are agent-side).

mod common;

use serde::Deserialize;
use serde_json::Value;
use xorgate_core::build_info::{format_build_info, parse_build_manifest, BuildInfo};
use xorgate_core::network::*;

#[test]
fn network_test_ts() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        op: String,
        input: Value,
        expected: Value,
    }
    let f = common::fixture::<Case>("network/network.json");
    for c in &f.cases {
        let got: Value = match c.op.as_str() {
            "isVirtualIface" => Value::Bool(is_virtual_iface(c.input.as_str().unwrap())),
            "sortInterfaces" => {
                let names: Vec<String> = serde_json::from_value(c.input.clone()).unwrap();
                serde_json::to_value(sort_interfaces(&names)).unwrap()
            }
            "parseWifiDevice" => {
                serde_json::to_value(parse_wifi_device(c.input.as_str().unwrap())).unwrap()
            }
            "parseDeviceEnabled" => Value::Bool(parse_device_enabled(
                c.input[0].as_str().unwrap(),
                c.input[1].as_str().unwrap(),
            )),
            "sshServerIp" => serde_json::to_value(ssh_server_ip(c.input.as_str())).unwrap(),
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 26);
}

#[test]
fn build_info_test_ts() {
    #[derive(Deserialize)]
    struct Case {
        name: String,
        op: String,
        input: Value,
        expected: Value,
    }
    let f = common::fixture::<Case>("build_info/build-info.json");
    for c in &f.cases {
        let got: Value = match c.op.as_str() {
            "formatBuildInfo" => {
                let info: BuildInfo = serde_json::from_value(c.input.clone()).unwrap();
                Value::String(format_build_info(&info))
            }
            "parseBuildManifest" => {
                let text = c.input["text"].as_str().unwrap();
                let pkg = c.input["packageVersion"].as_str().unwrap();
                serde_json::to_value(parse_build_manifest(text, pkg)).unwrap()
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, c.expected, "{}", c.name);
    }
    assert_eq!(f.cases.len(), 5);
}
