//! The extra OS packages a camera sensor needs: the PLAN (pure; RUST-ONLY,
//! support-other-camera-models Phase 4). The I/O half (download, verify,
//! install, hold) is `xorgate-agent`'s `cli::camera_packages`, a ROOT
//! command; the unprivileged agent only reads
//! (`camera_sensors::missing_requirements`).
//!
//! What may be installed is decided here and nowhere else:
//! - ONLY packages the compiled-in sensor catalog pins (`packageSets`): a
//!   name, a version, an HTTPS URL, a sha256 and a size, per OS release and
//!   architecture. Nothing is read from the network, the cloud or a setting
//!   to decide WHAT to install; the settings only say WHICH sensors the
//!   device declares.
//! - Every pinned entry is checked again before use ([`check_package`]):
//!   the host must be on [`PACKAGE_HOSTS`] (the backend's
//!   `CAMERA_PACKAGE_HOSTS`), the path may hold only plain characters, the
//!   file must be a `.deb` named after its package. No catalog string ever
//!   reaches a shell: every tool is run with an argument vector.
//! - A redirect is followed only to [`REDIRECT_HOSTS`]. GitHub serves a
//!   release asset by answering `302` from `github.com` to its asset store
//!   (`release-assets.githubusercontent.com`; `objects.githubusercontent.com`
//!   is the store's earlier name, still answered for some assets). Whatever
//!   host serves the bytes, they are accepted only when their size and
//!   sha256 are the pinned ones.
//! - `apt-get` is first run in SIMULATION and its plan is read
//!   ([`check_simulation`]): it may install the pinned packages in the
//!   pinned versions and nothing else, and remove nothing. A plan that
//!   wants anything more is refused before the system is touched.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::camera_catalog::{Catalog, Package, PackageSet};
use crate::camera_sensors::HostFacts;

/// How every line of `xorgate-rs camera requirements --install` begins.
/// The updater and the installer pass on the lines that carry it and
/// nothing else of the command's output.
pub const LINE_PREFIX: &str = "camera packages: ";

/// The hosts a package URL may name (the backend's `CAMERA_PACKAGE_HOSTS`).
pub const PACKAGE_HOSTS: &[&str] = &["github.com"];

/// The hosts a download may be redirected to (module docs).
pub const REDIRECT_HOSTS: &[&str] = &[
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// How many redirects one download may take.
pub const MAX_REDIRECTS: usize = 4;

/// The largest package the catalog may pin (the backend's bound, 256 MiB).
pub const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;

fn plain(s: &str, extra: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// `https://<host>/<plain segments>/<file>` with `host` one of `hosts`: no
/// userinfo, no port, no query, no fragment, no `.`/`..` segment. Returns
/// the last path segment.
pub fn check_url(url: &str, hosts: &[&str]) -> Result<String, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("its URL is not an https:// URL")?;
    let (host, path) = rest.split_once('/').ok_or("its URL has no path")?;
    if !hosts.contains(&host) {
        return Err(format!(
            "its URL names a host that is not allowed (allowed: {})",
            hosts.join(", ")
        ));
    }
    let mut last = "";
    for seg in path.split('/') {
        if !plain(seg, "._+~-") || seg == "." || seg == ".." {
            return Err("its URL path holds a character or segment that is not allowed".into());
        }
        last = seg;
    }
    Ok(last.to_string())
}

/// May a download be redirected to `url`? `https` and a host of
/// [`REDIRECT_HOSTS`]; the path and query are the server's (the asset
/// store's URLs are signed), the bytes are checked against the pin.
pub fn redirect_allowed(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "a redirect to a URL that is not https://".to_string())?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if REDIRECT_HOSTS.contains(&authority) {
        Ok(())
    } else {
        // Clipped: a server chose this text.
        let shown: String = authority.chars().take(80).collect();
        Err(format!(
            "a redirect to a host that is not allowed ({shown:?}; allowed: {})",
            REDIRECT_HOSTS.join(", ")
        ))
    }
}

/// One pinned package, checked: `Ok(the .deb's file name)`, or why the
/// entry is not usable (a sentence fragment after "<name>: ").
pub fn check_package(p: &Package) -> Result<String, String> {
    // Debian policy 5.6.1 / 5.6.12, a little stricter.
    let name_ok = plain(&p.name, "+.-")
        && p.name.len() >= 2
        && p.name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && !p.name.chars().any(|c| c.is_ascii_uppercase());
    if !name_ok {
        return Err("its package name is not a Debian package name".into());
    }
    if !(plain(&p.version, ".+~:-") && p.version.starts_with(|c: char| c.is_ascii_digit())) {
        return Err("its version is not a Debian version".into());
    }
    if !(p.sha256.len() == 64
        && p.sha256
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)))
    {
        return Err("its sha256 is not 64 lowercase hex digits".into());
    }
    if p.bytes == 0 || p.bytes > MAX_PACKAGE_BYTES {
        return Err(format!(
            "its size is not between 1 and {MAX_PACKAGE_BYTES} bytes"
        ));
    }
    let file = check_url(&p.url, PACKAGE_HOSTS)?;
    if !(file.ends_with(".deb") && file.starts_with(&format!("{}_", p.name))) {
        return Err("its URL does not end in a .deb file named after the package".into());
    }
    Ok(file)
}

/// One package of a set on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageStatus {
    pub name: String,
    pub required: String,
    /// `None` = not installed.
    pub installed: Option<String>,
}

impl PackageStatus {
    pub fn ok(&self) -> bool {
        self.installed.as_deref() == Some(self.required.as_str())
    }
}

/// Where a package set stands on this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetState {
    /// Every package is installed in the required version.
    Satisfied,
    /// Some package is not: an install is needed.
    Missing,
    /// The catalog lists no packages of this set for the device's OS
    /// release and architecture: nothing can be installed.
    NoTarget { os: String, arch: String },
    /// The device could not be asked (no `os-release`, an unknown
    /// architecture, no `dpkg-query`): nothing is concluded or installed.
    Unknown(String),
    /// A pinned entry fails [`check_package`]: nothing is installed.
    Invalid(String),
}

/// One package set some declared sensor needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetPlan {
    pub id: String,
    pub label: String,
    pub apt_hold: bool,
    /// Who needs it: `imx335 (cam0)`.
    pub needed_by: Vec<String>,
    pub state: SetState,
    /// The packages of the device's target (empty without one).
    pub packages: Vec<Package>,
    pub status: Vec<PackageStatus>,
}

impl SetPlan {
    pub fn names(&self) -> Vec<String> {
        self.packages.iter().map(|p| p.name.clone()).collect()
    }
}

/// The package sets the declared sensors need: `(set, who needs it)`, in
/// catalog order. `declared` = `(stream key, sensor id)`; an id the catalog
/// does not know, and `none`, need nothing.
pub fn needed_sets<'c>(
    declared: &[(String, String)],
    catalog: &'c Catalog,
) -> Vec<(&'c PackageSet, Vec<String>)> {
    catalog
        .package_sets
        .iter()
        .filter_map(|set| {
            let by: Vec<String> = declared
                .iter()
                .filter(|(_, id)| {
                    catalog
                        .by_id(id)
                        .is_some_and(|s| s.package_set.as_deref() == Some(set.id.as_str()))
                })
                .map(|(key, id)| format!("{id} ({key})"))
                .collect();
            (!by.is_empty()).then_some((set, by))
        })
        .collect()
}

/// Every package name of the needed sets, on any target: what to ask
/// `dpkg-query` about.
pub fn package_names(declared: &[(String, String)], catalog: &Catalog) -> Vec<String> {
    let mut out: Vec<String> = needed_sets(declared, catalog)
        .iter()
        .flat_map(|(set, _)| set.targets.iter())
        .flat_map(|t| t.packages.iter().map(|p| p.name.clone()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The plan for the declared sensors on this device.
pub fn plan(declared: &[(String, String)], catalog: &Catalog, host: &HostFacts) -> Vec<SetPlan> {
    needed_sets(declared, catalog)
        .into_iter()
        .map(|(set, needed_by)| {
            let mut out = SetPlan {
                id: set.id.clone(),
                label: set.label.clone(),
                apt_hold: set.apt_hold,
                needed_by,
                state: SetState::Satisfied,
                packages: Vec::new(),
                status: Vec::new(),
            };
            let (Some(os), Some(arch)) = (host.os.as_deref(), host.arch.as_deref()) else {
                out.state = SetState::Unknown(
                    "the OS release or the package architecture of this device could not be read"
                        .into(),
                );
                return out;
            };
            let Some(target) = set.targets.iter().find(|t| t.os == os && t.arch == arch) else {
                out.state = SetState::NoTarget {
                    os: os.to_string(),
                    arch: arch.to_string(),
                };
                return out;
            };
            out.packages = target.packages.clone();
            if let Some(why) = target
                .packages
                .iter()
                .find_map(|p| check_package(p).err().map(|e| format!("{}: {e}", p.name)))
            {
                out.state = SetState::Invalid(why);
                return out;
            }
            let Some(installed) = host.packages.as_ref() else {
                out.state = SetState::Unknown(
                    "dpkg-query could not be run, so the installed versions are not known".into(),
                );
                return out;
            };
            out.status = target
                .packages
                .iter()
                .map(|p| PackageStatus {
                    name: p.name.clone(),
                    required: p.version.clone(),
                    installed: installed.get(&p.name).cloned(),
                })
                .collect();
            if out.status.iter().any(|s| !s.ok()) {
                out.state = SetState::Missing;
            }
            out
        })
        .collect()
}

// ---------------------------------------------------------------------------
// apt
// ---------------------------------------------------------------------------

/// `apt-get` arguments that install exactly `files` (absolute paths of the
/// verified `.deb` files) in ONE transaction. `--allow-downgrades`: the
/// stock package may by then be newer than the pinned build.
/// `--allow-change-held-packages`: a re-install over packages this command
/// held earlier. `simulate`: `-s`, nothing is changed.
pub fn apt_install_args(files: &[String], simulate: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "-o",
        "DPkg::Lock::Timeout=120",
        "install",
        "-y",
        "--no-install-recommends",
        "--allow-downgrades",
        "--allow-change-held-packages",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if simulate {
        args.push("-s".into());
    }
    args.extend(files.iter().cloned());
    args
}

/// `apt-mark hold|unhold <names…>`.
pub fn apt_mark_args(verb: &str, names: &[String]) -> Vec<String> {
    std::iter::once(verb.to_string())
        .chain(names.iter().cloned())
        .collect()
}

/// `apt-mark showhold` output → the held package names (an `:arch` suffix
/// dropped).
pub fn parse_showhold(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| l.trim().split(':').next().unwrap_or("").to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// What an `apt-get -s install` would do, checked against the pin: every
/// `Inst` line must name a pinned package in its pinned version, every
/// pinned package that is not yet at its version must be there, and there
/// must be no `Remv` line. `Err` = a sentence saying what apt wanted
/// instead; nothing has been changed at that point.
pub fn check_simulation(stdout: &str, pinned: &[Package]) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for line in stdout.lines() {
        let mut words = line.split_whitespace();
        let (Some(verb), Some(name)) = (words.next(), words.next()) else {
            continue;
        };
        let name = name.split(':').next().unwrap_or(name);
        let shown: String = name.chars().take(80).collect();
        match verb {
            "Remv" | "Purg" => {
                return Err(format!(
                    "apt would remove {shown}, which the catalog does not ask for"
                ));
            }
            "Inst" => {
                let Some(pin) = pinned.iter().find(|p| p.name == name) else {
                    return Err(format!(
                        "apt would also install {shown}, which the catalog does not pin"
                    ));
                };
                // `Inst name [old] (new origin [arch])`: the first token in
                // parentheses is the version to be installed.
                let version = line
                    .split_once('(')
                    .and_then(|(_, r)| r.split_whitespace().next())
                    .unwrap_or("");
                if version != pin.version {
                    let v: String = version.chars().take(80).collect();
                    return Err(format!(
                        "apt would install {shown} {v}, not the pinned {}",
                        pin.version
                    ));
                }
                seen.push(pin.name.as_str());
            }
            _ => {}
        }
    }
    // Not every pinned package must change (one may already be at its
    // version); but a simulation that installs NONE of them did not
    // understand the request.
    if seen.is_empty() {
        return Err("apt's simulation installs none of the pinned packages".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the record
// ---------------------------------------------------------------------------

/// What an install replaced, kept so it can be undone by hand
/// ([`rollback_commands`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InstallRecord {
    pub v: u32,
    #[serde(default)]
    pub sets: BTreeMap<String, SetRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRecord {
    /// Epoch ms of the (last) install.
    pub at: i64,
    /// The packages were put on hold by the install.
    pub hold: bool,
    pub packages: Vec<PackageRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageRecord {
    pub name: String,
    /// The version the install put there.
    pub installed: String,
    /// The version that was there before the FIRST install of this set
    /// (the stock one); `None` = the package was not installed.
    pub replaced: Option<String>,
    /// It was already on hold before the install (so a rollback leaves the
    /// hold alone).
    pub was_held: bool,
}

/// The record of an install of `packages` over a device that had `before`
/// installed and `held_before` on hold. A package the earlier record shows
/// as put there by this command keeps its ORIGINAL `replaced` / `wasHeld`:
/// a second install must not record the first one's version as "stock".
pub fn set_record(
    earlier: Option<&SetRecord>,
    packages: &[Package],
    before: &BTreeMap<String, String>,
    held_before: &[String],
    hold: bool,
    at: i64,
) -> SetRecord {
    SetRecord {
        at,
        hold,
        packages: packages
            .iter()
            .map(|p| {
                let was = before.get(&p.name);
                let ours = earlier
                    .and_then(|e| e.packages.iter().find(|r| r.name == p.name))
                    .filter(|r| was == Some(&r.installed));
                PackageRecord {
                    name: p.name.clone(),
                    installed: p.version.clone(),
                    replaced: match ours {
                        Some(r) => r.replaced.clone(),
                        None => was.cloned(),
                    },
                    was_held: match ours {
                        Some(r) => r.was_held,
                        None => held_before.contains(&p.name),
                    },
                }
            })
            .collect(),
    }
}

/// The commands that undo an install by hand (printed by the listing and
/// in the guide): release the hold this command set, then put the replaced
/// versions back from apt.
pub fn rollback_commands(rec: &SetRecord) -> Vec<String> {
    let mut out = Vec::new();
    let unhold: Vec<&str> = rec
        .packages
        .iter()
        .filter(|p| rec.hold && !p.was_held)
        .map(|p| p.name.as_str())
        .collect();
    if !unhold.is_empty() {
        out.push(format!("sudo apt-mark unhold {}", unhold.join(" ")));
    }
    let back: Vec<String> = rec
        .packages
        .iter()
        .filter_map(|p| p.replaced.as_ref().map(|v| format!("{}={v}", p.name)))
        .collect();
    if !back.is_empty() {
        out.push(format!(
            "sudo apt-get install --allow-downgrades {}",
            back.join(" ")
        ));
    }
    let gone: Vec<&str> = rec
        .packages
        .iter()
        .filter(|p| p.replaced.is_none())
        .map(|p| p.name.as_str())
        .collect();
    if !gone.is_empty() {
        out.push(format!("sudo apt-get remove {}", gone.join(" ")));
    }
    out
}

// ---------------------------------------------------------------------------
// what the command prints
// ---------------------------------------------------------------------------

/// The listing of one set (`xorgate-rs camera requirements`). `held` = the
/// packages on hold (`None` = `apt-mark` could not be asked).
pub fn listing(plan: &SetPlan, held: Option<&[String]>, record: Option<&SetRecord>) -> Vec<String> {
    let mut out = vec![format!(
        "{} ({}), needed by {}:",
        plan.id,
        plan.label,
        plan.needed_by.join(", ")
    )];
    let width = plan.status.iter().map(|s| s.name.len()).max().unwrap_or(0);
    for s in &plan.status {
        out.push(format!(
            "  {:width$}  required {}  installed {}  {}",
            s.name,
            s.required,
            s.installed.as_deref().unwrap_or("-"),
            if s.ok() { "ok" } else { "MISSING" },
        ));
    }
    match &plan.state {
        SetState::Satisfied => {
            if plan.apt_hold {
                match held {
                    Some(held) => {
                        let not: Vec<String> = plan
                            .names()
                            .into_iter()
                            .filter(|n| !held.contains(n))
                            .collect();
                        if not.is_empty() {
                            out.push("  apt hold: set on every package".into());
                        } else {
                            out.push(format!(
                                "  apt hold: MISSING on {} (an apt upgrade may replace them)",
                                not.join(", ")
                            ));
                        }
                    }
                    None => out.push("  apt hold: could not be read (apt-mark)".into()),
                }
            }
        }
        SetState::Missing => {}
        SetState::NoTarget { os, arch } => out.push(format!(
            "  REFUSED: the catalog lists no packages of this set for {os}/{arch}; nothing can be installed on this device"
        )),
        SetState::Unknown(why) => out.push(format!("  UNKNOWN: {why}")),
        SetState::Invalid(why) => out.push(format!(
            "  REFUSED: the catalog entry is not usable ({why}); nothing is installed"
        )),
    }
    if let Some(rec) = record {
        let replaced: Vec<String> = rec
            .packages
            .iter()
            .map(|p| {
                format!(
                    "{} {}",
                    p.name,
                    p.replaced.as_deref().unwrap_or("(was not installed)")
                )
            })
            .collect();
        out.push(format!(
            "  installed by xorgate-rs; it replaced: {}",
            replaced.join(", ")
        ));
        for cmd in rollback_commands(rec) {
            out.push(format!("  to go back: {cmd}"));
        }
    }
    out
}

/// What is still to do for `plan`, given the holds: `true` = `--install`
/// has work (a package to install, or a hold to set).
pub fn needs_work(plan: &SetPlan, held: Option<&[String]>) -> bool {
    match plan.state {
        SetState::Missing => true,
        SetState::Satisfied => {
            plan.apt_hold && held.is_some_and(|held| plan.names().iter().any(|n| !held.contains(n)))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_catalog::catalog;

    fn pkg(name: &str, version: &str, url: &str) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            url: url.into(),
            sha256: "a".repeat(64),
            bytes: 10,
        }
    }

    fn host(os: &str, arch: &str, installed: &[(&str, &str)]) -> HostFacts {
        HostFacts {
            os: Some(os.into()),
            arch: Some(arch.into()),
            packages: Some(
                installed
                    .iter()
                    .map(|(n, v)| (n.to_string(), v.to_string()))
                    .collect(),
            ),
        }
    }

    const V: &str = "0.5.2+rpt20250909-1";

    #[test]
    fn the_compiled_in_pins_are_usable() {
        let c = catalog();
        let set = c.package_set("arducam-libcamera").unwrap();
        for t in &set.targets {
            for p in &t.packages {
                assert_eq!(
                    check_package(p),
                    Ok(format!("{}_{}_{}.deb", p.name, p.version, t.arch)),
                    "{}",
                    p.name
                );
            }
        }
        // Every host a package URL names is one a redirect may also stay on.
        for h in PACKAGE_HOSTS {
            assert!(REDIRECT_HOSTS.contains(h));
        }
    }

    #[test]
    fn a_pin_that_is_not_plain_is_refused() {
        let good = "https://github.com/a/b/releases/download/v1/x_1.0_arm64.deb";
        assert_eq!(
            check_package(&pkg("x", "1.0", good)).map(|_| ()),
            Err("its package name is not a Debian package name".into()),
            "one letter"
        );
        assert!(check_package(&pkg("xy", "1.0", &good.replace("x_1", "xy_1"))).is_ok());
        let bad_urls = [
            ("http://github.com/a/xy_1.0.deb", "not an https"),
            (
                "https://github.com.evil.io/a/xy_1.0.deb",
                "host that is not allowed",
            ),
            (
                "https://user@github.com/a/xy_1.0.deb",
                "host that is not allowed",
            ),
            (
                "https://github.com:8443/a/xy_1.0.deb",
                "host that is not allowed",
            ),
            ("https://github.com/a/../xy_1.0.deb", "not allowed"),
            ("https://github.com/a/xy_1.0.deb?x=1", "not allowed"),
            ("https://github.com/a/xy_1.0.deb#f", "not allowed"),
            ("https://github.com/a b/xy_1.0.deb", "not allowed"),
            ("https://github.com/a/$(id)/xy_1.0.deb", "not allowed"),
            ("https://github.com/a//xy_1.0.deb", "not allowed"),
            ("https://github.com/a/xy_1.0.tar", "does not end in a .deb"),
            (
                "https://github.com/a/other_1.0.deb",
                "named after the package",
            ),
            ("https://github.com", "no path"),
        ];
        for (url, why) in bad_urls {
            let e = check_package(&pkg("xy", "1.0", url)).unwrap_err();
            assert!(e.contains(why), "{url}: {e}");
        }
        let url = "https://github.com/a/xy_1.0.deb";
        for name in ["Xy", "-xy", "x y", "xy;id", ""] {
            assert!(check_package(&pkg(name, "1.0", url)).is_err(), "{name:?}");
        }
        for version in ["", "-1", "1.0 ", "1.0;id", "$(id)", "v1"] {
            assert!(
                check_package(&pkg("xy", version, url)).is_err(),
                "{version:?}"
            );
        }
        let mut p = pkg("xy", "1.0", url);
        p.sha256 = "A".repeat(64);
        assert!(check_package(&p).is_err());
        p.sha256 = "a".repeat(63);
        assert!(check_package(&p).is_err());
        p.sha256 = "a".repeat(64);
        p.bytes = 0;
        assert!(check_package(&p).is_err());
        p.bytes = MAX_PACKAGE_BYTES + 1;
        assert!(check_package(&p).is_err());
        p.bytes = MAX_PACKAGE_BYTES;
        assert!(check_package(&p).is_ok());
    }

    #[test]
    fn a_redirect_stays_on_the_pinned_hosts() {
        for ok in [
            "https://github.com/x",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r&sig=abc",
            "https://objects.githubusercontent.com/x?y",
        ] {
            assert_eq!(redirect_allowed(ok), Ok(()), "{ok}");
        }
        for bad in [
            "http://github.com/x",
            "https://evil.example/x",
            "https://github.com.evil.example/x",
            "https://github.com@evil.example/x",
            "https://github.com:444/x",
            "https://raw.githubusercontent.com/x",
            "ftp://github.com/x",
            "/relative",
            "",
        ] {
            assert!(redirect_allowed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_plan_follows_the_catalog_and_the_host() {
        let c = catalog();
        let declared = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, s)| (k.to_string(), s.to_string()))
                .collect()
        };
        // Sensors that need nothing: no set, nothing to ask dpkg about.
        let plain = declared(&[
            ("cam0", "imx290"),
            ("cam1", "imx708"),
            ("cam2", "none"),
            ("cam3", "imx999"),
        ]);
        assert!(plan(&plain, c, &host("bookworm", "arm64", &[])).is_empty());
        assert!(package_names(&plain, c).is_empty());

        let d = declared(&[("cam0", "imx335"), ("cam1", "imx219")]);
        assert_eq!(package_names(&d, c), ["libcamera-ipa", "libcamera0.5"]);
        // Stock packages: an install is needed.
        let stock = host(
            "bookworm",
            "arm64",
            &[
                ("libcamera0.5", "0.5.2+rpt20250903-1"),
                ("libcamera-ipa", "0.5.2+rpt20250903-1"),
            ],
        );
        let p = plan(&d, c, &stock);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].id, "arducam-libcamera");
        assert_eq!(p[0].needed_by, ["imx335 (cam0)"]);
        assert_eq!(p[0].state, SetState::Missing);
        assert!(p[0].apt_hold);
        assert_eq!(p[0].names(), ["libcamera0.5", "libcamera-ipa"]);
        assert!(needs_work(&p[0], None));
        assert_eq!(
            listing(&p[0], None, None),
            [
                "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), needed by imx335 (cam0):",
                "  libcamera0.5   required 0.5.2+rpt20250909-1  installed 0.5.2+rpt20250903-1  MISSING",
                "  libcamera-ipa  required 0.5.2+rpt20250909-1  installed 0.5.2+rpt20250903-1  MISSING",
            ]
        );
        // One of the two installed: still missing.
        let half = host("bookworm", "arm64", &[("libcamera0.5", V)]);
        let p = plan(&d, c, &half);
        assert_eq!(p[0].state, SetState::Missing);
        assert_eq!(
            p[0].status.iter().map(|s| s.ok()).collect::<Vec<_>>(),
            [true, false]
        );
        // Both at the pinned version: satisfied; the hold is the only thing
        // that can still be to do.
        let done = host(
            "bookworm",
            "arm64",
            &[("libcamera0.5", V), ("libcamera-ipa", V)],
        );
        let p = plan(&d, c, &done);
        assert_eq!(p[0].state, SetState::Satisfied);
        let held = ["libcamera0.5".to_string(), "libcamera-ipa".to_string()];
        assert!(!needs_work(&p[0], Some(&held)));
        assert!(needs_work(&p[0], Some(&held[..1])));
        assert!(
            !needs_work(&p[0], None),
            "holds unknown: nothing is done blind"
        );
        assert_eq!(
            listing(&p[0], Some(&held[..1]), None)[3],
            "  apt hold: MISSING on libcamera-ipa (an apt upgrade may replace them)"
        );
        assert_eq!(
            listing(&p[0], Some(&held), None)[3],
            "  apt hold: set on every package"
        );
        // Another OS release or architecture: refused, in a sentence.
        for (os, arch) in [
            ("trixie", "arm64"),
            ("bookworm", "armhf"),
            ("bookworm", "amd64"),
        ] {
            let p = plan(&d, c, &host(os, arch, &[]));
            assert_eq!(
                p[0].state,
                SetState::NoTarget {
                    os: os.into(),
                    arch: arch.into()
                }
            );
            assert!(!needs_work(&p[0], None));
            assert_eq!(
                listing(&p[0], None, None)[1],
                format!("  REFUSED: the catalog lists no packages of this set for {os}/{arch}; nothing can be installed on this device")
            );
        }
        // A device that could not be asked: nothing is concluded.
        let blind = HostFacts {
            os: Some("bookworm".into()),
            arch: Some("arm64".into()),
            packages: None,
        };
        assert!(matches!(plan(&d, c, &blind)[0].state, SetState::Unknown(_)));
        assert!(matches!(
            plan(&d, c, &HostFacts::default())[0].state,
            SetState::Unknown(_)
        ));
    }

    #[test]
    fn apt_may_install_the_pinned_packages_and_nothing_else() {
        let pins = [
            pkg("libcamera0.5", V, "https://github.com/a/libcamera0.5_x.deb"),
            pkg(
                "libcamera-ipa",
                V,
                "https://github.com/a/libcamera-ipa_x.deb",
            ),
        ];
        let good = "NOTE: This is only a simulation!\nReading package lists...\nInst libcamera0.5 [0.5.2+rpt20250903-1] (0.5.2+rpt20250909-1 local-deb [arm64])\nInst libcamera-ipa [0.5.2+rpt20250903-1] (0.5.2+rpt20250909-1 local-deb [arm64])\nConf libcamera0.5 (0.5.2+rpt20250909-1 local-deb [arm64])\nConf libcamera-ipa (0.5.2+rpt20250909-1 local-deb [arm64])\n";
        assert_eq!(check_simulation(good, &pins), Ok(()));
        // Multi-arch spelling.
        assert_eq!(
            check_simulation(
                "Inst libcamera0.5:arm64 [1] (0.5.2+rpt20250909-1 local-deb [arm64])\n",
                &pins
            ),
            Ok(())
        );
        let extra = format!("{good}Inst libfoo7 (1.2 Debian:12/stable [arm64])\n");
        assert_eq!(
            check_simulation(&extra, &pins),
            Err("apt would also install libfoo7, which the catalog does not pin".into())
        );
        let removes = format!("Remv gstreamer1.0-libcamera [0.5.2]\n{good}");
        assert_eq!(
            check_simulation(&removes, &pins),
            Err(
                "apt would remove gstreamer1.0-libcamera, which the catalog does not ask for"
                    .into()
            )
        );
        let other_version =
            "Inst libcamera0.5 [1] (0.5.3-1 Raspberry Pi Foundation:stable [arm64])\n";
        assert_eq!(
            check_simulation(other_version, &pins),
            Err(
                "apt would install libcamera0.5 0.5.3-1, not the pinned 0.5.2+rpt20250909-1".into()
            )
        );
        assert_eq!(
            check_simulation("Reading package lists...\n", &pins),
            Err("apt's simulation installs none of the pinned packages".into())
        );
        assert_eq!(
            apt_install_args(&["/t/a.deb".into(), "/t/b.deb".into()], true),
            [
                "-o",
                "DPkg::Lock::Timeout=120",
                "install",
                "-y",
                "--no-install-recommends",
                "--allow-downgrades",
                "--allow-change-held-packages",
                "-s",
                "/t/a.deb",
                "/t/b.deb"
            ]
        );
        assert_eq!(apt_install_args(&["/t/a.deb".into()], false).len(), 8);
        assert_eq!(
            apt_mark_args("hold", &["a".into(), "b".into()]),
            ["hold", "a", "b"]
        );
        assert_eq!(
            parse_showhold("libcamera-ipa\nlibcamera0.5:arm64\n\n"),
            ["libcamera-ipa", "libcamera0.5"]
        );
    }

    #[test]
    fn the_record_keeps_what_was_there_first() {
        let pins = [
            pkg("libcamera0.5", V, "https://github.com/a/libcamera0.5_x.deb"),
            pkg(
                "libcamera-ipa",
                V,
                "https://github.com/a/libcamera-ipa_x.deb",
            ),
        ];
        let stock: BTreeMap<String, String> = [
            ("libcamera0.5", "0.5.2+rpt20250903-1"),
            ("libcamera-ipa", "0.5.2+rpt20250903-1"),
        ]
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
        let first = set_record(None, &pins, &stock, &[], true, 5);
        assert_eq!(
            first.packages[0].replaced.as_deref(),
            Some("0.5.2+rpt20250903-1")
        );
        assert!(!first.packages[0].was_held);
        assert_eq!(
            rollback_commands(&first),
            [
                "sudo apt-mark unhold libcamera0.5 libcamera-ipa",
                "sudo apt-get install --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
            ]
        );
        // A second install over our own (a hold that had to be set again):
        // the stock version is still the one recorded.
        let ours: BTreeMap<String, String> = pins
            .iter()
            .map(|p| (p.name.clone(), p.version.clone()))
            .collect();
        let held = ["libcamera0.5".to_string(), "libcamera-ipa".to_string()];
        let second = set_record(Some(&first), &pins, &ours, &held, true, 9);
        assert_eq!(second.packages, first.packages);
        assert_eq!(second.at, 9);
        // Someone put another version there in between: that is what this
        // install replaced.
        let mut other = ours.clone();
        other.insert("libcamera0.5".into(), "0.5.3-1".into());
        let third = set_record(Some(&first), &pins, &other, &held, true, 9);
        assert_eq!(third.packages[0].replaced.as_deref(), Some("0.5.3-1"));
        assert!(third.packages[0].was_held);
        assert_eq!(third.packages[1], first.packages[1]);
        // A package that was not installed, and one that was already held.
        let none = set_record(None, &pins, &BTreeMap::new(), &held[..1], true, 1);
        assert_eq!(
            rollback_commands(&none),
            [
                "sudo apt-mark unhold libcamera-ipa",
                "sudo apt-get remove libcamera0.5 libcamera-ipa"
            ]
        );
        // The file round-trips.
        let rec = InstallRecord {
            v: 1,
            sets: [("arducam-libcamera".to_string(), first.clone())]
                .into_iter()
                .collect(),
        };
        let text = serde_json::to_string(&rec).unwrap();
        assert!(text.starts_with(r#"{"v":1,"sets":{"arducam-libcamera":{"at":5,"hold":true,"packages":[{"name":"libcamera0.5","installed":"0.5.2+rpt20250909-1","replaced":"0.5.2+rpt20250903-1","wasHeld":false}"#), "{text}");
        assert_eq!(serde_json::from_str::<InstallRecord>(&text).unwrap(), rec);
        let c = catalog();
        let d = vec![("cam0".to_string(), "imx335".to_string())];
        let p = plan(
            &d,
            c,
            &host(
                "bookworm",
                "arm64",
                &[("libcamera0.5", V), ("libcamera-ipa", V)],
            ),
        );
        let lines = listing(&p[0], Some(&held), Some(&first));
        assert_eq!(
            lines[4..],
            [
                "  installed by xorgate-rs; it replaced: libcamera0.5 0.5.2+rpt20250903-1, libcamera-ipa 0.5.2+rpt20250903-1",
                "  to go back: sudo apt-mark unhold libcamera0.5 libcamera-ipa",
                "  to go back: sudo apt-get install --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
            ]
        );
    }
}
