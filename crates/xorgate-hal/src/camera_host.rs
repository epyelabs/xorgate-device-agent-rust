//! What this device has that a camera sensor may require (the I/O half of
//! `xorgate_core::camera_sensors::missing_requirements`; RUST-ONLY,
//! support-other-camera-models Phase 2): the OS release, the package
//! architecture and the installed versions of the packages asked about.
//!
//! It only LOOKS: `/etc/os-release` is read and `dpkg-query -W` is run. No
//! package is installed, held or removed here.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use xorgate_core::camera_sensors::{os_codename, parse_dpkg_query, HostFacts};

/// The dpkg database: its mtime moves whenever a package is installed,
/// upgraded or removed.
pub const DPKG_STATUS: &str = "/var/lib/dpkg/status";

/// How long `dpkg-query` may take.
pub const DPKG_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The Debian architecture of this build (`dpkg --print-architecture` on a
/// device whose userland matches the agent binary, which is every device
/// the agent runs on).
pub fn package_arch() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "arm" => "armhf",
        "x86_64" => "amd64",
        "x86" => "i386",
        _ => return None,
    };
    Some(arch.to_string())
}

/// `dpkg-query -W -f='${Package}\t${Version}\t${db:Status-Abbrev}\n' <names…>`:
/// the installed versions, or `None` when it could not be run (not found,
/// killed after [`DPKG_QUERY_TIMEOUT`]). A package dpkg does not know makes
/// it exit 1 and still print the others: that is an answer.
pub fn query_packages(names: &[String]) -> Option<BTreeMap<String, String>> {
    if names.is_empty() {
        return Some(BTreeMap::new());
    }
    let mut child = crate::child::std_command("dpkg-query")
        .arg("-W")
        .arg("-f=${Package}\\t${Version}\\t${db:Status-Abbrev}\\n")
        .args(names)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + DPKG_QUERY_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    Some(parse_dpkg_query(&String::from_utf8_lossy(&out.stdout)))
}

/// The last answer, with what it was asked about and the dpkg database's
/// mtime then: `dpkg-query` runs again only when one of them moved.
type Cached = (Vec<String>, Option<SystemTime>, HostFacts);
static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

/// The device's facts for the requirement check of the sensors that need
/// `packages`. Cheap to call every minute: the OS release is one small
/// file, and the package query is repeated only after the dpkg database
/// changed (so a package installed by hand is seen at the next call).
pub fn host_facts(packages: &[String]) -> HostFacts {
    let stamp = std::fs::metadata(DPKG_STATUS)
        .and_then(|m| m.modified())
        .ok();
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((names, at, facts)) = cache.as_ref() {
        if names == packages && *at == stamp && stamp.is_some() {
            return facts.clone();
        }
    }
    let facts = HostFacts {
        os: crate::read_lossy::read_text(Path::new("/etc/os-release"))
            .ok()
            .as_deref()
            .and_then(os_codename),
        arch: package_arch(),
        packages: query_packages(packages),
    };
    *cache = Some((packages.to_vec(), stamp, facts.clone()));
    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_asked_runs_nothing() {
        assert_eq!(query_packages(&[]), Some(BTreeMap::new()));
        assert!(package_arch().is_some());
    }
}
