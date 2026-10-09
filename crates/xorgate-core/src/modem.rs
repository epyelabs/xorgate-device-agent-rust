//! Modem presence — the pure `findModemLine` of `src/hardware/modem.ts:215-229`
//! — and the lte telemetry group, the pure `mapTelemetry` of
//! `src/agent/collectors/lte.ts:43-57`. The telemetry types (`modem.ts:21-196`)
//! are WP1's [`crate::types`] (parse with [`ModemTelemetry::from_json`]);
//! `readModemTelemetry:203` (file read), `listTtyUsb:231`,
//! `detectModem:248` (`lsusb` spawn) and the lte collector's read/sample
//! cadence (`lte.ts:62-98`) are HAL / agent.

use serde::{Deserialize, Serialize};

use crate::telemetry::{LteGroup, MetricLeaf};
use crate::types::{GnssHealth, ModemTelemetry};

/// `MODEM_USB_VENDOR` (`modem.ts:17`): SimCom SIM7600 as seen by lsusb.
pub const MODEM_USB_VENDOR: &str = "1e0e";
/// `MODEM_USB_PRODUCTS` (`modem.ts:18`).
pub const MODEM_USB_PRODUCTS: [&str; 2] = ["9011", "9001"];

/// `findModemLine`'s match (`modem.ts:219`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModemUsbMatch {
    /// `vid:pid` exactly as lsusb printed them.
    pub usb_id: String,
    /// The rest of the line, trimmed.
    pub description: String,
}

/// `findModemLine(stdout, vendorId, productIds)` (`modem.ts:215-229`): the
/// first line matching `/ID\s+([0-9a-f]{4}):([0-9a-f]{4})\s*(.*)/i` whose
/// vendor equals `vendor_id` (case-insensitively) and whose product id
/// (lower-cased) is listed in `product_ids`.
pub fn find_modem_line(
    stdout: &str,
    vendor_id: &str,
    product_ids: &[&str],
) -> Option<ModemUsbMatch> {
    for line in stdout.split('\n') {
        let Some((vid, pid, desc)) = match_id_line(line) else {
            continue;
        };
        if vid.to_lowercase() == vendor_id.to_lowercase()
            && product_ids.contains(&pid.to_lowercase().as_str())
        {
            return Some(ModemUsbMatch {
                usb_id: format!("{vid}:{pid}"),
                description: crate::js::trim(desc).to_string(),
            });
        }
    }
    None
}

/// `findModemLine(stdout)` with the SIM7600 defaults.
pub fn find_modem_line_default(stdout: &str) -> Option<ModemUsbMatch> {
    find_modem_line(stdout, MODEM_USB_VENDOR, &MODEM_USB_PRODUCTS)
}

/// Leftmost match of `/ID\s+([0-9a-f]{4}):([0-9a-f]{4})\s*(.*)/i` in one line.
fn match_id_line(line: &str) -> Option<(&str, &str, &str)> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    for (ci, &(_, c)) in chars.iter().enumerate() {
        if !c.eq_ignore_ascii_case(&'I') {
            continue;
        }
        let Some(&(_, d)) = chars.get(ci + 1) else {
            break;
        };
        if !d.eq_ignore_ascii_case(&'D') {
            continue;
        }
        let mut k = ci + 2;
        let ws_start = k;
        while k < chars.len() && chars[k].1.is_whitespace() {
            k += 1;
        }
        if k == ws_start {
            continue; // `\s+` needs at least one
        }
        let hex4 = |at: usize| -> Option<(usize, usize)> {
            if at + 4 > chars.len() {
                return None;
            }
            if chars[at..at + 4].iter().all(|(_, c)| c.is_ascii_hexdigit()) {
                Some((chars[at].0, chars[at + 3].0 + 1))
            } else {
                None
            }
        };
        let Some(vid) = hex4(k) else { continue };
        if chars.get(k + 4).map(|(_, c)| *c) != Some(':') {
            continue;
        }
        let Some(pid) = hex4(k + 5) else { continue };
        let mut r = k + 9;
        while r < chars.len() && chars[r].1.is_whitespace() {
            r += 1;
        }
        // `.*` runs to the end of input or the first line terminator.
        let desc_start = chars.get(r).map(|(i, _)| *i).unwrap_or(line.len());
        let desc_end = chars[r..]
            .iter()
            .find(|(_, c)| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
            .map(|(i, _)| *i)
            .unwrap_or(line.len());
        return Some((
            &line[vid.0..vid.1],
            &line[pid.0..pid.1],
            &line[desc_start..desc_end],
        ));
    }
    None
}

/// `mapTelemetry` (`collectors/lte.ts:43-57`): the `lte` telemetry group, or
/// `None` (the collector returns `{}`, the worker omits the group) when `csq`
/// is not a number or is 99 (unknown signal). Every other leaf is gated on
/// its own `typeof` check, so one bad field drops only its own leaf.
///
/// Deviation (unreachable from the daemon, which writes only the four labels
/// via `gnss_health_state`): the TS tests `health in GNSS_HEALTH_CODES`, which
/// is also true for inherited keys such as `"toString"` and would emit a
/// value-less `gnss_health: {}`; here only the four labels map.
pub fn lte_group(t: &ModemTelemetry) -> Option<LteGroup> {
    let csq = t.csq.filter(|&c| c != 99.0)?;
    Some(LteGroup {
        quality: MetricLeaf::with_unit(csq, "asu"),
        rssi: t.rssi_dbm.map(|v| MetricLeaf::with_unit(v, "dBm")),
        modem_temp: t
            .modem_temp
            .filter(|v| v.is_finite())
            .map(|v| MetricLeaf::with_unit(v, "°C")),
        gnss_health: t
            .gnss
            .as_ref()
            .and_then(|g| g.health.as_deref())
            .and_then(GnssHealth::parse)
            .map(|h| MetricLeaf::new(f64::from(h.code()))),
        resets_24h: t
            .recovery
            .as_ref()
            .and_then(|r| r.resets_24h)
            .filter(|v| v.is_finite())
            .map(MetricLeaf::new),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_shape() {
        let m = find_modem_line_default(
            "Bus 003 Device 002: ID 1e0e:9011 Qualcomm / Option SimTech, Incorporated",
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"usbId":"1e0e:9011","description":"Qualcomm / Option SimTech, Incorporated"}"#
        );
        assert_eq!(
            find_modem_line_default("ID 1e0e:9011\r"),
            Some(ModemUsbMatch {
                usb_id: "1e0e:9011".into(),
                description: String::new()
            })
        );
        assert_eq!(find_modem_line_default("ID1e0e:9011"), None);
    }
}
