//! Network interface I/O — the reader/prober halves of
//! `src/hardware/network.ts:56-163` (`listInterfaces`, `ipv4Of`,
//! `probeInterface`, `interfaceStatuses`, `detectWifi`, `ifaceForIp`) and the
//! WiFi toggle of `src/daemon/radios.ts:20-33` (`setWifi`). The pure parsers
//! (`isVirtualIface`, `sortInterfaces`, `parseWifiDevice`,
//! `parseDeviceEnabled`, `sshServerIp`) are `xorgate_core::network`.
//!
//! Only the CLI (`set wifi`) and the `manage` TUI use this module; nothing
//! here runs on the agent's telemetry path (`network.ts:11-14`).
//!
//! `os.networkInterfaces()` is libuv's `uv_interface_addresses`: getifaddrs(3)
//! entries of interfaces that are BOTH `IFF_UP` and `IFF_RUNNING`, with an
//! address, excluding `AF_PACKET`; `internal` = `IFF_LOOPBACK`; grouped by
//! name in first-seen order ([`network_interfaces`]). The lookups are pure
//! over that snapshot ([`ipv4_of_in`], [`iface_for_ip_in`]) so a recorded
//! snapshot is a fixture (`fixtures/network/iface_for_ip.json`, CF22).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use xorgate_core::network::{is_virtual_iface, parse_device_enabled, parse_wifi_device};

use crate::exec::{run, RunResult};

/// `/sys/class/net` (`network.ts:28`).
pub const SYS_CLASS_NET: &str = "/sys/class/net";

/// One address of `os.networkInterfaces()[name]` (the fields the TS reads).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAddr {
    /// `"IPv4"` | `"IPv6"` (node ≥ 18).
    pub family: String,
    pub address: String,
    pub internal: bool,
}

/// `os.networkInterfaces()`: `(name, addresses)` in first-seen order.
pub type Interfaces = Vec<(String, Vec<NodeAddr>)>;

/// `os.networkInterfaces()` (libuv `uv_interface_addresses` on Linux; see
/// the module docs for the filter).
pub fn network_interfaces() -> Interfaces {
    use nix::net::if_::InterfaceFlags;
    let mut out: Interfaces = Vec::new();
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else {
        return out;
    };
    for ifa in addrs {
        let up = ifa.flags.contains(InterfaceFlags::IFF_UP)
            && ifa.flags.contains(InterfaceFlags::IFF_RUNNING);
        if !up {
            continue;
        }
        let Some(addr) = ifa.address else { continue };
        let (family, address) = if let Some(v4) = addr.as_sockaddr_in() {
            ("IPv4", v4.ip().to_string())
        } else if let Some(v6) = addr.as_sockaddr_in6() {
            ("IPv6", v6.ip().to_string())
        } else {
            continue; // AF_PACKET and friends
        };
        let entry = NodeAddr {
            family: family.to_string(),
            address,
            internal: ifa.flags.contains(InterfaceFlags::IFF_LOOPBACK),
        };
        match out.iter_mut().find(|(n, _)| *n == ifa.interface_name) {
            Some((_, list)) => list.push(entry),
            None => out.push((ifa.interface_name.clone(), vec![entry])),
        }
    }
    out
}

/// `ipv4Of(name)` (`network.ts:68-75`) over a snapshot: the first
/// non-internal IPv4 of `name`, or None.
pub fn ipv4_of_in(ifs: &Interfaces, name: &str) -> Option<String> {
    ifs.iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, addrs)| addrs.iter().find(|a| a.family == "IPv4" && !a.internal))
        .map(|a| a.address.clone())
}

/// `ifaceForIp(ip)` (`network.ts:154-163`) over a snapshot: the first
/// interface (in `Object.entries` order) owning the IPv4 `ip` — internal
/// ones included — or None; a None/empty `ip` is None.
pub fn iface_for_ip_in(ifs: &Interfaces, ip: Option<&str>) -> Option<String> {
    let ip = ip.filter(|s| !s.is_empty())?;
    ifs.iter()
        .find(|(_, addrs)| addrs.iter().any(|a| a.family == "IPv4" && a.address == ip))
        .map(|(n, _)| n.clone())
}

/// `ipv4Of(name)` against the live interfaces.
pub fn ipv4_of(name: &str) -> Option<String> {
    ipv4_of_in(&network_interfaces(), name)
}

/// `ifaceForIp(ip)` against the live interfaces.
pub fn iface_for_ip(ip: Option<&str>) -> Option<String> {
    iface_for_ip_in(&network_interfaces(), ip)
}

/// `listInterfaces()` (`network.ts:56-65`): real, non-virtual names from
/// `/sys/class/net` (falling back to `os.networkInterfaces()` keys when the
/// directory is unreadable), sorted for display.
pub fn list_interfaces() -> Vec<String> {
    list_interfaces_from(std::path::Path::new(SYS_CLASS_NET))
}

/// [`list_interfaces`] with the sysfs root injectable (tests).
pub fn list_interfaces_from(sys_class_net: &std::path::Path) -> Vec<String> {
    let entries: Vec<String> = match std::fs::read_dir(sys_class_net) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => network_interfaces().into_iter().map(|(n, _)| n).collect(),
    };
    let real: Vec<String> = entries
        .into_iter()
        .filter(|n| !is_virtual_iface(n))
        .collect();
    xorgate_core::network::sort_interfaces(&real)
}

/// `IfaceState` (`network.ts:21`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfaceState {
    Online,
    Offline,
    Down,
}

impl IfaceState {
    pub fn as_str(self) -> &'static str {
        match self {
            IfaceState::Online => "online",
            IfaceState::Offline => "offline",
            IfaceState::Down => "down",
        }
    }
}

/// `IfaceStatus` (`network.ts:23-26`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceStatus {
    pub name: String,
    pub state: IfaceState,
}

/// `PROBE_HOST` (`network.ts:78`): `XORGATE_PROBE_HOST ?? "1.1.1.1"`.
pub fn probe_host() -> String {
    std::env::var("XORGATE_PROBE_HOST").unwrap_or_else(|_| "1.1.1.1".to_string())
}

/// `probeInterface(name)` (`network.ts:86-92`): no IPv4 → down; else one
/// interface-bound `ping -c 1 -W 2 -I <iface> <host>` (3 s timeout).
pub async fn probe_interface(name: &str) -> IfaceState {
    if ipv4_of(name).is_none() {
        return IfaceState::Down;
    }
    let host = probe_host();
    let res = run(
        "ping",
        &["-c", "1", "-W", "2", "-I", name, &host],
        Some(Duration::from_millis(3000)),
    )
    .await;
    if res.failed {
        IfaceState::Offline
    } else {
        IfaceState::Online
    }
}

/// `interfaceStatuses()` (`network.ts:95-98`): discover, probe all in parallel.
pub async fn interface_statuses() -> Vec<IfaceStatus> {
    let names = list_interfaces();
    let probes = names.iter().map(|n| probe_interface(n));
    let states = join_all(probes).await;
    names
        .into_iter()
        .zip(states)
        .map(|(name, state)| IfaceStatus { name, state })
        .collect()
}

/// `Promise.all` over a few futures without a new crate: spawn none, poll
/// them together.
async fn join_all<F: std::future::Future>(futs: impl Iterator<Item = F>) -> Vec<F::Output> {
    let mut futs: Vec<_> = futs.map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in futs.iter_mut().enumerate() {
            if out[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => out[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    out.into_iter().map(|v| v.expect("all ready")).collect()
}

/// `WifiPresence` (`network.ts:102-107`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiPresence {
    pub present: bool,
    pub enabled: bool,
    /// NetworkManager device name (e.g. "wlan0") when present.
    pub device: Option<String>,
}

/// `detectWifi()` (`network.ts:132-142`): the first wifi device of
/// `nmcli -t -f DEVICE,TYPE device status`, enabled when its
/// `DEVICE,STATE` row is connected/connecting; without nmcli (or no wifi
/// device) a `wlan*` netdev gives presence, enabled = it has an IPv4.
pub async fn detect_wifi() -> WifiPresence {
    let dev = run(
        "nmcli",
        &["-t", "-f", "DEVICE,TYPE", "device", "status"],
        None,
    )
    .await;
    let device = if dev.failed {
        None
    } else {
        parse_wifi_device(&dev.stdout).map(str::to_string)
    };
    let Some(device) = device else {
        let fallback = list_interfaces()
            .into_iter()
            .find(|n| n.starts_with("wlan"));
        let enabled = fallback.as_deref().is_some_and(|f| ipv4_of(f).is_some());
        return WifiPresence {
            present: fallback.is_some(),
            enabled,
            device: fallback,
        };
    };
    let st = run(
        "nmcli",
        &["-t", "-f", "DEVICE,STATE", "device", "status"],
        None,
    )
    .await;
    let enabled = !st.failed && parse_device_enabled(&st.stdout, &device);
    WifiPresence {
        present: true,
        enabled,
        device: Some(device),
    }
}

/// `RadioResult` (`radios.ts:14-17`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioResult {
    pub ok: bool,
    pub message: String,
}

/// The ONE argv `setWifi` runs (`radios.ts:27`):
/// `sudo -n nmcli device connect|disconnect <device>`. The TS has no `-n`
/// (the CLI/TUI run interactively as `pi`, evidence wp5 A1); this build
/// passes it (D128): Raspberry Pi OS images since 2026-04-13 (Trixie) give
/// `pi` no passwordless sudo, and no sudoers grant of ours covers `nmcli`,
/// so without `-n` sudo would ask for a password on the terminal, inside
/// the TUI's raw mode, with nothing to stop the wait. With `-n` it fails at
/// once and the reason is printed ("wifi on failed: sudo: a password is
/// required"). As root (`sudo xorgate-rs set wifi on`) nothing changes.
pub fn wifi_toggle_argv(on: bool, device: &str) -> [String; 6] {
    [
        "sudo".to_string(),
        "-n".to_string(),
        "nmcli".to_string(),
        "device".to_string(),
        if on { "connect" } else { "disconnect" }.to_string(),
        device.to_string(),
    ]
}

/// execa's view of a run (`exec.ts:27-50`, probed with tsx @89b5c64): a
/// spawn failure (ENOENT included) under `reject: false` resolves with
/// `stderr: ""`, `exitCode: undefined → null` and `notFound: false` — the
/// catch branch that sets `notFound` is unreachable. Returns
/// `(stderr, notFound, exitCode as JS prints it)`.
pub fn execa_view(res: &RunResult) -> (String, bool, String) {
    if res.spawn_failed {
        return (String::new(), false, "null".to_string());
    }
    let code = res
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "null".to_string());
    (res.stderr.clone(), res.not_found, code)
}

/// The `setWifi` verdict for one `sudo nmcli` result (`radios.ts:28-32`).
pub fn wifi_result(on: bool, device: &str, res: &RunResult) -> RadioResult {
    let word = if on { "on" } else { "off" };
    if res.failed {
        let (stderr, not_found, code) = execa_view(res);
        let trimmed = xorgate_core::js::trim(&stderr);
        let why = if !trimmed.is_empty() {
            trimmed.to_string()
        } else if not_found {
            "sudo/nmcli not found".to_string()
        } else {
            format!("exit {code}")
        };
        return RadioResult {
            ok: false,
            message: format!("wifi {word} failed: {why}"),
        };
    }
    RadioResult {
        ok: true,
        message: format!("wifi {word} ({device})"),
    }
}

/// `setWifi(on)` (`radios.ts:20-33`): re-detects, no-ops when already in
/// the target state, else `sudo -n nmcli device connect|disconnect <dev>`.
pub async fn set_wifi(on: bool) -> RadioResult {
    let wifi = detect_wifi().await;
    if !wifi.present {
        return RadioResult {
            ok: false,
            message: "no WiFi adapter detected".to_string(),
        };
    }
    let device = wifi.device.unwrap_or_else(|| "wlan0".to_string());
    if wifi.enabled == on {
        return RadioResult {
            ok: true,
            message: format!("wifi already {} ({device})", if on { "on" } else { "off" }),
        };
    }
    let argv = wifi_toggle_argv(on, &device);
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let res = run(&argv[0], &args, None).await;
    wifi_result(on, &device, &res)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(family: &str, address: &str, internal: bool) -> NodeAddr {
        NodeAddr {
            family: family.into(),
            address: address.into(),
            internal,
        }
    }

    /// Never a password prompt (D128): `-n` before the command.
    #[test]
    fn the_wifi_toggle_never_waits_for_a_password() {
        assert_eq!(
            wifi_toggle_argv(true, "wlan0"),
            ["sudo", "-n", "nmcli", "device", "connect", "wlan0"]
        );
        assert_eq!(
            wifi_toggle_argv(false, "wlan1"),
            ["sudo", "-n", "nmcli", "device", "disconnect", "wlan1"]
        );
    }

    #[test]
    fn lookups_follow_node_semantics() {
        let ifs: Interfaces = vec![
            (
                "lo".into(),
                vec![a("IPv4", "127.0.0.1", true), a("IPv6", "::1", true)],
            ),
            (
                "wlan0".into(),
                vec![a("IPv6", "fe80::1", false), a("IPv4", "10.0.0.5", false)],
            ),
        ];
        assert_eq!(ipv4_of_in(&ifs, "wlan0").as_deref(), Some("10.0.0.5"));
        assert_eq!(ipv4_of_in(&ifs, "lo"), None); // internal skipped
        assert_eq!(
            iface_for_ip_in(&ifs, Some("127.0.0.1")).as_deref(),
            Some("lo")
        );
        assert_eq!(iface_for_ip_in(&ifs, Some("")), None);
        assert_eq!(iface_for_ip_in(&ifs, Some("fe80::1")), None); // IPv4 only
    }

    #[test]
    fn spawn_failure_reads_as_execa_does() {
        let r = RunResult {
            stderr: "No such file or directory (os error 2)".into(),
            failed: true,
            not_found: true,
            spawn_failed: true,
            ..Default::default()
        };
        assert_eq!(
            wifi_result(false, "wlan0", &r).message,
            "wifi off failed: exit null"
        );
        let r = RunResult {
            stderr: "  Error: Device 'wlan0' not found.\n".into(),
            exit_code: Some(10),
            failed: true,
            ..Default::default()
        };
        assert_eq!(
            wifi_result(true, "wlan0", &r).message,
            "wifi on failed: Error: Device 'wlan0' not found."
        );
        let r = RunResult {
            exit_code: Some(4),
            failed: true,
            ..Default::default()
        };
        assert_eq!(
            wifi_result(true, "wlan0", &r).message,
            "wifi on failed: exit 4"
        );
    }

    /// CF22: `network.test.ts:79-84` + recorded snapshots, against the REAL
    /// TS `ifaceForIp` / `ipv4Of` (`fixtures/network/iface_for_ip.json`,
    /// tsx-dumped by `phase-4/evidence/wp5-tools/dump-iface-for-ip.mts`, for tools/replay/js/ via WP6).
    #[test]
    fn iface_for_ip_fixture() {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Case {
            name: String,
            interfaces: Interfaces,
            iface_for_ip: Vec<(Option<String>, Option<String>)>,
            ipv4_of: Vec<(String, Option<String>)>,
        }
        #[derive(serde::Deserialize)]
        struct Fx {
            cases: Vec<Case>,
        }
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/network/iface_for_ip.json");
        let fx: Fx = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(fx.cases.len(), 6);
        for c in &fx.cases {
            for (ip, want) in &c.iface_for_ip {
                assert_eq!(
                    &iface_for_ip_in(&c.interfaces, ip.as_deref()),
                    want,
                    "{} {ip:?}",
                    c.name
                );
            }
            for (n, want) in &c.ipv4_of {
                assert_eq!(&ipv4_of_in(&c.interfaces, n), want, "{} {n}", c.name);
            }
        }
    }

    #[test]
    fn live_snapshot_has_loopback_on_linux() {
        if cfg!(target_os = "linux") {
            let ifs = network_interfaces();
            assert_eq!(
                iface_for_ip_in(&ifs, Some("127.0.0.1")).as_deref(),
                Some("lo")
            );
        }
    }
}
