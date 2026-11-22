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

use xorgate_core::camera_packages::{
    installed_versions, interrupted, parse_dpkg_snapshot, DpkgSnapshot,
};
use xorgate_core::camera_sensors::{os_codename, HostFacts};

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

/// dpkg's journal of a run that did not finish. apt refuses to work while
/// it holds an entry ("dpkg was interrupted, you must manually run 'dpkg
/// --configure -a'").
pub const DPKG_UPDATES: &str = "/var/lib/dpkg/updates";

/// Does dpkg's journal hold an entry (a file whose name is all digits, the
/// ones apt looks for)? An unreadable or absent directory is "no".
pub fn dpkg_journal_pending(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                !name.is_empty() && name.chars().all(|c| c.is_ascii_digit())
            })
        })
        .unwrap_or(false)
}

/// `dpkg-query -W -f='${binary:Package}\t${Version}\t${db:Status-Abbrev}\n' [names…]`:
/// the database's word on `names`, or on EVERY package when `names` is
/// `None`. `None` = it could not be run (not found, or killed after
/// `timeout`). A package dpkg does not know makes it exit 1 and still
/// print the others: that is an answer.
///
/// `program` and `env` are named by the caller: a ROOT caller (`xorgate-rs
/// camera requirements --install`) runs `/usr/bin/dpkg-query`, not whatever
/// `PATH` finds, with `env` as its WHOLE environment (`None` = inherited,
/// as the unprivileged agent does).
pub fn dpkg_snapshot(
    program: &Path,
    env: Option<&[(String, String)]>,
    timeout: Duration,
    names: Option<&[String]>,
) -> Option<DpkgSnapshot> {
    if names.is_some_and(<[String]>::is_empty) {
        return Some(DpkgSnapshot::new());
    }
    let mut cmd = crate::child::std_command(program);
    if let Some(env) = env {
        cmd.env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    let mut child = cmd
        .arg("-W")
        .arg("-f=${binary:Package}\\t${Version}\\t${db:Status-Abbrev}\\n")
        .args(names.unwrap_or(&[]))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Read while it runs: the whole database is more than a pipe holds.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?;
    // Asked about EVERYTHING, dpkg-query has no reason to fail: a failure
    // is "could not be read". (Asked about names, exit 1 means only that
    // it does not know one of them.)
    if names.is_none() && !status.success() {
        return None;
    }
    Some(parse_dpkg_snapshot(&String::from_utf8_lossy(&out)))
}

/// The installed versions of `names`, as the unprivileged agent asks
/// (`dpkg-query` from `PATH`, [`DPKG_QUERY_TIMEOUT`]).
pub fn query_packages(names: &[String]) -> Option<BTreeMap<String, String>> {
    dpkg_snapshot(
        Path::new("dpkg-query"),
        None,
        DPKG_QUERY_TIMEOUT,
        Some(names),
    )
    .map(|s| installed_versions(&s))
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
    let snapshot = dpkg_snapshot(
        Path::new("dpkg-query"),
        None,
        DPKG_QUERY_TIMEOUT,
        Some(packages),
    );
    let facts = HostFacts {
        os: crate::read_lossy::read_text(Path::new("/etc/os-release"))
            .ok()
            .as_deref()
            .and_then(os_codename),
        arch: package_arch(),
        packages: snapshot.as_ref().map(installed_versions),
        // A package asked about that dpkg left half-installed, or dpkg's
        // own journal of an unfinished run.
        interrupted: snapshot
            .as_ref()
            .and_then(|s| interrupted(s, dpkg_journal_pending(Path::new(DPKG_UPDATES)))),
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
