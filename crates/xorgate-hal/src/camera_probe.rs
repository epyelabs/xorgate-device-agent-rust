//! Which camera sensor nodes does the RUNNING kernel hold on each camera
//! port, and did a driver bind to them? (support-other-camera-models plan,
//! Phase 0; Rust-only, read-only.)
//!
//! `rpicam-hello --list-cameras` answers "does libcamera list a camera".
//! That is not the same question: with an `imx335` overlay and stock
//! libcamera the driver binds and libcamera lists nothing, and with a wrong
//! `imx219` / `imx708` overlay the node exists and the probe fails. This
//! scan reads the kernel's side from sysfs, per port.
//!
//! Layout, read on a CM4 (BCM2711, kernel 6.12) and two CM5 (RP1) on
//! 2026-11-21, identical on both platforms:
//!
//! ```text
//! /sys/bus/i2c/devices/0-001a/of_node -> ../../../../../../../firmware/devicetree/base/soc/i2c0mux/i2c@0/imx708@1a
//! /sys/bus/i2c/devices/0-001a/driver  -> ../../../../../../../bus/i2c/drivers/imx708     (only when bound)
//! /sys/bus/i2c/devices/0-000c/of_node -> …/soc/i2c0mux/i2c@0/dw9817@c                    (the lens: no `port`)
//! /sys/firmware/devicetree/base/soc/i2c0mux/i2c@0/imx708@1a/port/endpoint                 (a sensor has a CSI port)
//! ```
//!
//! - The kernel creates the I2C client device for every device-tree node,
//!   whether or not the probe succeeds; a failed probe only leaves the
//!   `driver` link out.
//! - The port is the I2C bus in the node's device-tree path, the same
//!   needles the camera listing uses (`xorgate_core::cameras`).
//! - A sensor node has a `port` child (its CSI endpoint); a lens / VCM node
//!   on the same bus (`dw9817@c`, `ad5398@c`) has none.
//!
//! Fails open: no sysfs → `None`, and the caller reports "could not look".

use std::path::Path;

use xorgate_core::camera_truth::PortNode;
use xorgate_core::cameras::connector_for_path;
use xorgate_core::hardware_block::Platform;

/// Where the kernel lists I2C client devices.
pub const SYS_I2C_DEVICES: &str = "/sys/bus/i2c/devices";

/// The marker in an `of_node` link after which the device-tree path starts.
const DT_BASE: &str = "devicetree/base";

/// `<bus>-<4 hex address>`: an I2C client device (not an `i2c-N` adapter).
fn is_client_name(name: &str) -> bool {
    let Some((bus, addr)) = name.split_once('-') else {
        return false;
    };
    !bus.is_empty()
        && bus.bytes().all(|b| b.is_ascii_digit())
        && addr.len() == 4
        && addr.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The device-tree path (`/soc/i2c0mux/i2c@0/imx708@1a`) an `of_node` link
/// points at: the part after `devicetree/base`.
fn dt_path_of_link(link: &Path) -> Option<String> {
    let text = link.to_string_lossy();
    let at = text.find(DT_BASE)?;
    let rel = &text[at + DT_BASE.len()..];
    (rel.starts_with('/') && !rel.contains("..")).then(|| rel.to_string())
}

/// The camera sensor nodes on the camera ports of this device, in
/// `(connector, node)` order. `None` = `sys_i2c` cannot be listed (not a
/// Linux device with I2C); `Some(vec![])` = no sensor node on any port.
///
/// `dt_root` is the live device tree (`/proc/device-tree`), consulted only
/// for the `port` child that tells a sensor from a lens.
pub fn scan_port_nodes(
    platform: Platform,
    sys_i2c: &Path,
    dt_root: &Path,
) -> Option<Vec<PortNode>> {
    let entries = std::fs::read_dir(sys_i2c).ok()?;
    let mut out: Vec<PortNode> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_client_name(&name) {
            continue;
        }
        let dev = entry.path();
        let Some(dt_path) = std::fs::read_link(dev.join("of_node"))
            .ok()
            .as_deref()
            .and_then(dt_path_of_link)
        else {
            continue;
        };
        let Some(connector) = connector_for_path(platform, &dt_path) else {
            continue;
        };
        // A sensor has a CSI port; a lens driver on the same bus has none.
        if !dt_root
            .join(dt_path.trim_start_matches('/'))
            .join("port")
            .exists()
        {
            continue;
        }
        let Some(node) = dt_path.rsplit('/').next().filter(|n| !n.is_empty()) else {
            continue;
        };
        let driver = std::fs::read_link(dev.join("driver"))
            .ok()
            .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()));
        out.push(PortNode {
            connector: connector.to_string(),
            node: node.to_string(),
            driver,
        });
    }
    out.sort_by(|a, b| (&a.connector, &a.node).cmp(&(&b.connector, &b.node)));
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xg-camprobe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// One I2C client as sysfs shows it: the `of_node` link text the kernel
    /// writes, the device-tree node (with a `port` for a sensor) and the
    /// `driver` link when bound.
    fn client(root: &Path, dev: &str, dt_path: &str, sensor: bool, driver: Option<&str>) {
        let d = root.join("i2c").join(dev);
        std::fs::create_dir_all(&d).unwrap();
        symlink(
            format!("../../../../../../../firmware/devicetree/base{dt_path}"),
            d.join("of_node"),
        )
        .unwrap();
        let node = root.join("dt").join(dt_path.trim_start_matches('/'));
        std::fs::create_dir_all(&node).unwrap();
        if sensor {
            std::fs::create_dir_all(node.join("port").join("endpoint")).unwrap();
        }
        if let Some(drv) = driver {
            symlink(
                format!("../../../../../../../bus/i2c/drivers/{drv}"),
                d.join("driver"),
            )
            .unwrap();
        }
    }

    fn node(connector: &str, node: &str, driver: Option<&str>) -> PortNode {
        PortNode {
            connector: connector.into(),
            node: node.into(),
            driver: driver.map(str::to_string),
        }
    }

    fn scan(platform: Platform, root: &Path) -> Option<Vec<PortNode>> {
        scan_port_nodes(platform, &root.join("i2c"), &root.join("dt"))
    }

    /// The CM4 bench on 2026-11-21: IMX708 on CAM0 with its lens driver.
    #[test]
    fn cm4_imx708_with_its_lens() {
        let r = scratch("cm4");
        client(
            &r,
            "0-001a",
            "/soc/i2c0mux/i2c@0/imx708@1a",
            true,
            Some("imx708"),
        );
        client(
            &r,
            "0-000c",
            "/soc/i2c0mux/i2c@0/dw9817@c",
            false,
            Some("dw9807"),
        );
        // Adapters and a non-camera bus are not ports.
        std::fs::create_dir_all(r.join("i2c").join("i2c-0")).unwrap();
        client(&r, "1-004a", "/soc/i2c@7e804000/bno085@4a", true, None);
        assert_eq!(
            scan(Platform::RpiCm4, &r),
            Some(vec![node("csi0", "imx708@1a", Some("imx708"))])
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// Bench 2 (CM5) on 2026-11-21: `imx335,cam0` + `imx219,cam1`. On RP1
    /// CAM0 is `i2c@88000` (bus 10) and CAM1 is `i2c@70000` (bus 0).
    #[test]
    fn cm5_two_sensors_bound_and_unbound() {
        let r = scratch("cm5");
        let cam0 = "/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a";
        let cam1 = "/axi/pcie@1000120000/rp1/i2c@70000/imx219@10";
        client(&r, "10-001a", cam0, true, Some("imx335"));
        client(&r, "0-0010", cam1, true, Some("imx219"));
        assert_eq!(
            scan(Platform::RpiCm5, &r),
            Some(vec![
                node("csi0", "imx335@1a", Some("imx335")),
                node("csi1", "imx219@10", Some("imx219")),
            ])
        );
        let _ = std::fs::remove_dir_all(&r);

        // The same boxes with the IMX335 silent (`probe with driver imx335
        // failed`): the client device exists, the driver link does not.
        let r = scratch("cm5-unbound");
        client(&r, "10-001a", cam0, true, None);
        client(&r, "0-0010", cam1, true, Some("imx219"));
        assert_eq!(
            scan(Platform::RpiCm5, &r),
            Some(vec![
                node("csi0", "imx335@1a", None),
                node("csi1", "imx219@10", Some("imx219")),
            ])
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    /// The control bench: two IMX290 (the i2c `name` is `imx290lqr`, the
    /// driver and the node are `imx290`).
    #[test]
    fn cm5_two_imx290() {
        let r = scratch("cm5-290");
        client(
            &r,
            "0-001a",
            "/axi/pcie@1000120000/rp1/i2c@70000/imx290@1a",
            true,
            Some("imx290"),
        );
        client(
            &r,
            "10-001a",
            "/axi/pcie@1000120000/rp1/i2c@88000/imx290@1a",
            true,
            Some("imx290"),
        );
        assert_eq!(
            scan(Platform::RpiCm5, &r),
            Some(vec![
                node("csi0", "imx290@1a", Some("imx290")),
                node("csi1", "imx290@1a", Some("imx290")),
            ])
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn no_sysfs_is_unknown_and_an_empty_one_is_no_node() {
        let r = scratch("none");
        assert_eq!(scan(Platform::RpiCm5, &r), None, "no i2c dir at all");
        std::fs::create_dir_all(r.join("i2c")).unwrap();
        assert_eq!(scan(Platform::RpiCm5, &r), Some(vec![]));
        // A client with no `of_node` (an x86 host) is skipped.
        std::fs::create_dir_all(r.join("i2c").join("3-0050")).unwrap();
        assert_eq!(scan(Platform::RpiCm5, &r), Some(vec![]));
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn name_and_link_parsing() {
        assert!(is_client_name("0-001a"));
        assert!(is_client_name("10-0010"));
        assert!(!is_client_name("i2c-0"));
        assert!(!is_client_name("0-1a"));
        assert!(!is_client_name("-001a"));
        assert!(!is_client_name("x-001a"));
        assert_eq!(
            dt_path_of_link(Path::new(
                "../../../firmware/devicetree/base/soc/i2c0mux/i2c@0/imx708@1a"
            ))
            .as_deref(),
            Some("/soc/i2c0mux/i2c@0/imx708@1a")
        );
        // An absolute (resolved) link reads the same.
        assert_eq!(
            dt_path_of_link(Path::new(
                "/sys/firmware/devicetree/base/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a"
            ))
            .as_deref(),
            Some("/axi/pcie@1000120000/rp1/i2c@88000/imx335@1a")
        );
        assert_eq!(dt_path_of_link(Path::new("../somewhere/else")), None);
        assert_eq!(
            dt_path_of_link(Path::new("x/devicetree/base/../../etc")),
            None
        );
    }
}
