//! Network interface helpers — the PURE functions of `src/hardware/network.ts`
//! (`isVirtualIface:38`, `sortInterfaces:51`, `parseWifiDevice:110`,
//! `parseDeviceEnabled:119`, `sshServerIp:147`). The readers/probers
//! (`listInterfaces`, `ipv4Of`, `probeInterface`, `interfaceStatuses`,
//! `detectWifi`, `ifaceForIp`, and the `XORGATE_PROBE_HOST` env read at
//! `:78`) stay in the HAL.

/// `VIRTUAL_IFACE` (`network.ts:34`):
/// `/^(lo|p2p-|veth|docker|br-|bridge|virbr|tun|tap|utun|bond|dummy|wg|ifb|sit|awdl|llw|anpi|gif|stf|ap\d)/`
/// — loopback, software/virtual devices, and the macOS-only pseudo-interfaces.
const VIRTUAL_PREFIXES: &[&str] = &[
    "lo", "p2p-", "veth", "docker", "br-", "bridge", "virbr", "tun", "tap", "utun", "bond",
    "dummy", "wg", "ifb", "sit", "awdl", "llw", "anpi", "gif", "stf",
];

/// `isVirtualIface` (`network.ts:38`): should this interface be hidden from
/// the internet-status row?
pub fn is_virtual_iface(name: &str) -> bool {
    if VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    // `ap\d`
    name.strip_prefix("ap")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
}

/// `ifaceRank` (`network.ts:43`): eth*, usb*, wlan*, then the rest.
fn iface_rank(name: &str) -> u8 {
    if name.starts_with("eth") {
        0
    } else if name.starts_with("usb") {
        1
    } else if name.starts_with("wlan") {
        2
    } else {
        3
    }
}

/// `sortInterfaces` (`network.ts:51`): rank, then `a.localeCompare(b)`.
///
/// `localeCompare` is ICU collation (case-insensitive at the primary level,
/// then case as a tie-break); for the lowercase+digit names interfaces carry
/// it coincides with byte order. We compare case-insensitively first and fall
/// back to byte order, which matches ICU for ASCII names.
pub fn sort_interfaces(names: &[String]) -> Vec<String> {
    let mut v = names.to_vec();
    v.sort_by(|a, b| {
        iface_rank(a)
            .cmp(&iface_rank(b))
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
            .then_with(|| a.cmp(b))
    });
    v
}

/// `parseWifiDevice` (`network.ts:110`): first wifi DEVICE in
/// `nmcli -t -f DEVICE,TYPE device status`, or None.
pub fn parse_wifi_device(nmcli_status: &str) -> Option<&str> {
    for line in nmcli_status.split('\n') {
        let mut parts = line.split(':');
        let device = parts.next().unwrap_or("");
        let ty = parts.next();
        if ty == Some("wifi") && !device.is_empty() {
            return Some(device);
        }
    }
    None
}

/// `parseDeviceEnabled` (`network.ts:119`): from
/// `nmcli -t -f DEVICE,STATE device status`, is `device` up?
pub fn parse_device_enabled(nmcli_status: &str, device: &str) -> bool {
    for line in nmcli_status.split('\n') {
        let mut parts = line.split(':');
        let dev = parts.next().unwrap_or("");
        if dev == device {
            let state = parts.next();
            return state == Some("connected") || state == Some("connecting");
        }
    }
    false
}

/// `sshServerIp` (`network.ts:147`): server-side IP from `$SSH_CONNECTION`
/// ("clientIp clientPort serverIp serverPort"), or None when unset/malformed.
pub fn ssh_server_ip(ssh_connection: Option<&str>) -> Option<&str> {
    let s = ssh_connection?;
    if s.is_empty() {
        return None;
    }
    // `trim().split(/\s+/)`: an all-whitespace string splits to [""] (length
    // 1) in JS and to [] here; both fail the `>= 3` check.
    let parts: Vec<&str> = s.split_whitespace().collect();
    (parts.len() >= 3).then(|| parts[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_prefixes() {
        assert!(is_virtual_iface("ap0"));
        assert!(!is_virtual_iface("apx"));
        assert!(is_virtual_iface("lo"));
        assert!(!is_virtual_iface("eth0"));
    }

    #[test]
    fn ssh_whitespace_only_is_none() {
        assert_eq!(ssh_server_ip(Some("   ")), None);
        assert_eq!(ssh_server_ip(Some(" a b c ")), Some("c"));
    }
}
