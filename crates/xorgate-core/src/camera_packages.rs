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
/// verified `.deb` files) in ONE transaction. The simulation (`simulate`:
/// `-s`, nothing is changed) and the real run get the SAME arguments
/// otherwise.
/// - `--no-remove`: apt aborts rather than remove a package, also when its
///   view changed between the simulation and the real run;
/// - `--allow-downgrades`: the stock package may by then be newer than the
///   pinned build;
/// - `--allow-change-held-packages`: a re-install over packages this
///   command held earlier;
/// - `Dpkg::Options::=--force-confdef` / `--force-confold`: a changed
///   configuration file keeps the local version without asking (there is
///   nobody to ask: stdin is closed), so a prompt can neither hang nor fail
///   the transaction;
/// - `DPkg::Lock::Timeout`: wait for another apt/dpkg to finish.
pub fn apt_install_args(files: &[String], simulate: bool) -> Vec<String> {
    let lock = format!("DPkg::Lock::Timeout={T_LOCK}");
    let mut args: Vec<String> = [
        "-o",
        lock.as_str(),
        "-o",
        "Dpkg::Options::=--force-confdef",
        "-o",
        "Dpkg::Options::=--force-confold",
        "install",
        "-y",
        "--no-install-recommends",
        "--no-remove",
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

/// What an `apt-get -s install` would do, checked against the pin.
/// `pinned` = the packages handed to apt (those not yet at their pinned
/// version). The plan must be EXACTLY them:
/// - every `Inst` line names one of them, in its pinned version;
/// - every one of them has its `Inst` line;
/// - there is no `Remv` / `Purg` line.
///
/// `Err` = a sentence saying what apt wanted instead; nothing has been
/// changed at that point.
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
    let absent: Vec<&str> = pinned
        .iter()
        .map(|p| p.name.as_str())
        .filter(|n| !seen.contains(n))
        .collect();
    if !absent.is_empty() {
        return Err(format!(
            "apt's simulation does not install {}",
            absent.join(", ")
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// time
// ---------------------------------------------------------------------------

// Every bound of the command, in seconds. Two kinds:
// - a KILLABLE phase (a download, the simulation, a query, a lock wait
//   before anything is written): its bound may cut it, and an outer limit
//   may shorten the bound;
// - the TRANSACTION (the real `apt-get install`): nothing but its own
//   bound ends it, and it is started only when that bound, plus what comes
//   after it, fits in the time the caller allows ([`killable_secs`]).

/// `apt-mark showhold`.
pub const T_HELD: u64 = 30;
/// One `dpkg-query` run (the whole database).
pub const T_DPKG: u64 = 30;
/// One file, redirects included.
pub const T_DOWNLOAD: u64 = 180;
/// `apt-get -s install`.
pub const T_SIMULATE: u64 = 90;
/// apt's own wait for the dpkg lock, part of [`T_INSTALL`].
pub const T_LOCK: u64 = 120;
/// The real `apt-get install`: the lock wait and the transaction. Two small
/// packages take seconds; this is the last resort against an apt that
/// hangs, and the only thing that may end a running transaction.
pub const T_INSTALL: u64 = 600;
/// `apt-mark hold`.
pub const T_HOLD: u64 = 30;
/// `systemctl try-restart` of the agent.
pub const T_RESTART: u64 = 90;
/// After the transaction: read the database back, hold, restart.
pub const T_AFTER: u64 = T_DPKG + T_HOLD + T_RESTART;
/// The files of the largest set today (two).
pub const MAX_FILES: u64 = 2;
/// The whole command at its worst: every killable phase at its bound, then
/// the transaction at its bound, then what follows.
pub const STEP_WORST: u64 =
    T_HELD + T_DPKG + MAX_FILES * T_DOWNLOAD + T_SIMULATE + T_DPKG + T_INSTALL + T_AFTER;
/// The time limit a caller gives the command when nothing tighter bounds
/// it (`--time-limit`): the worst case and half a minute.
pub const STEP_LIMIT: u64 = STEP_WORST + 30;
/// How long after the limit a caller may stop a command that did not keep
/// its word.
pub const STEP_GRACE: u64 = 60;

// Checked at compile time: apt's lock wait is inside the install's bound,
// and the worst case is inside the limit a caller gives.
const _: () = assert!(T_LOCK < T_INSTALL);
const _: () = assert!(STEP_WORST <= STEP_LIMIT);

/// The bound of a killable phase whose own bound is `own`, when
/// `left_before_transaction` seconds remain before the transaction would
/// have to start (`None` = the caller set no limit). `None` = it does not
/// fit: the phase is not started.
pub fn killable_secs(own: u64, left_before_transaction: Option<u64>) -> Option<u64> {
    match left_before_transaction {
        None => Some(own),
        Some(left) if left >= 2 => Some(own.min(left)),
        Some(_) => None,
    }
}

// ---------------------------------------------------------------------------
// the dpkg database
// ---------------------------------------------------------------------------

/// One package of `dpkg-query -W -f='${binary:Package}\t${Version}\t${db:Status-Abbrev}\n'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DpkgEntry {
    pub version: String,
    /// `<wanted><status><error>`: `ii `, `hi `, `iU `, `rc `, …
    pub abbrev: String,
}

impl DpkgEntry {
    fn status(&self) -> char {
        self.abbrev.chars().nth(1).unwrap_or(' ')
    }

    /// Installed (status `i`), wanted or on hold: as
    /// `camera_sensors::parse_dpkg_query` reads it.
    pub fn installed(&self) -> bool {
        matches!(self.abbrev.chars().next(), Some('i' | 'h'))
            && self.status() == 'i'
            && !self.version.is_empty()
    }

    /// dpkg stopped in the middle of this package: unpacked, half-installed
    /// or half-configured. (`W`/`t`, a trigger still to run, is not that.)
    pub fn unfinished(&self) -> Option<&'static str> {
        match self.status() {
            'U' => Some("unpacked, not configured"),
            'H' => Some("half-installed"),
            'F' => Some("half-configured"),
            _ => None,
        }
    }
}

/// The dpkg database by package name (an `:arch` qualifier dropped).
pub type DpkgSnapshot = BTreeMap<String, DpkgEntry>;

pub fn parse_dpkg_snapshot(stdout: &str) -> DpkgSnapshot {
    stdout
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (name, version, abbrev) = (f.next()?, f.next()?, f.next()?);
            let name = name.split(':').next().unwrap_or(name);
            (!name.is_empty()).then(|| {
                (
                    name.to_string(),
                    DpkgEntry {
                        version: version.to_string(),
                        abbrev: abbrev.to_string(),
                    },
                )
            })
        })
        .collect()
}

/// The installed versions of a snapshot.
pub fn installed_versions(snapshot: &DpkgSnapshot) -> BTreeMap<String, String> {
    snapshot
        .iter()
        .filter(|(_, e)| e.installed())
        .map(|(n, e)| (n.clone(), e.version.clone()))
        .collect()
}

/// Was dpkg interrupted? The packages it left unfinished (`name (state)`),
/// at most eight named. `journal` = dpkg's own journal of an interrupted
/// run is not empty (`/var/lib/dpkg/updates`, what makes apt say "dpkg was
/// interrupted"). `None` = no.
pub fn interrupted(snapshot: &DpkgSnapshot, journal: bool) -> Option<String> {
    let unfinished: Vec<String> = snapshot
        .iter()
        .filter_map(|(n, e)| e.unfinished().map(|st| format!("{n} is {st}")))
        .collect();
    if unfinished.is_empty() && !journal {
        return None;
    }
    Some(if unfinished.is_empty() {
        "its journal of an unfinished run is not empty".to_string()
    } else {
        let more = unfinished.len().saturating_sub(8);
        let mut text = unfinished[..unfinished.len().min(8)].join(", ");
        if more > 0 {
            text.push_str(&format!(" and {more} more"));
        }
        text
    })
}

/// What to do about an interrupted dpkg.
pub const DPKG_REMEDY: &str = "sudo dpkg --configure -a";

/// What changed between two snapshots OUTSIDE `pinned` (the names of the
/// packages the transaction was to install): `name 1 -> 2`, `name
/// installed 2`, `name removed`, at most eight named. Empty = nothing.
pub fn changed_outside(
    before: &DpkgSnapshot,
    after: &DpkgSnapshot,
    pinned: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let show = |e: &DpkgEntry| format!("{} ({})", e.version, e.abbrev.trim_end());
    for (name, was) in before {
        if pinned.contains(name) {
            continue;
        }
        match after.get(name) {
            None => out.push(format!("{name} removed")),
            Some(now) if now != was => out.push(format!("{name} {} -> {}", show(was), show(now))),
            Some(_) => {}
        }
    }
    for (name, now) in after {
        if !pinned.contains(name) && !before.contains_key(name) {
            out.push(format!("{name} installed {}", show(now)));
        }
    }
    let more = out.len().saturating_sub(8);
    out.truncate(8);
    if more > 0 {
        out.push(format!("and {more} more"));
    }
    out
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

/// What this command did to one set, over all its runs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRecord {
    /// Epoch ms of the last change.
    pub at: i64,
    /// The packages THIS COMMAND installed (a package that was already at
    /// its pinned version when the command first saw it is not here).
    #[serde(default)]
    pub packages: Vec<PackageRecord>,
    /// The packages this command put on hold (one that was already held is
    /// not here: a rollback leaves that hold alone).
    #[serde(default)]
    pub held: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageRecord {
    pub name: String,
    /// The version this command put there.
    pub installed: String,
    /// The version that was there before this command FIRST installed the
    /// package (the stock one); `None` = the package was not installed.
    /// Never the pinned version itself.
    pub replaced: Option<String>,
}

/// One package a run changed: `(name, the version now, the version before)`.
pub type Replaced = (String, String, Option<String>);

/// The record after a run that changed `replaced` and newly held `held`.
/// Written as soon as that is known, also when the run failed half-way: a
/// package apt did upgrade before it failed is recorded then, so the next
/// run, which finds it at the pinned version, does not take that version
/// for what was there before.
/// - A package whose version before IS the version now is not recorded.
/// - A package the earlier record shows as put there by this command (it
///   was at the recorded version) keeps its ORIGINAL `replaced`.
pub fn merge_record(
    earlier: Option<&SetRecord>,
    replaced: &[Replaced],
    held: &[String],
    at: i64,
) -> SetRecord {
    let mut rec = earlier.cloned().unwrap_or_default();
    for (name, now, before) in replaced {
        if before.as_deref() == Some(now.as_str()) {
            continue;
        }
        let ours = rec
            .packages
            .iter()
            .position(|r| &r.name == name && Some(&r.installed) == before.as_ref());
        match ours {
            Some(i) => rec.packages[i].installed = now.clone(),
            None => {
                rec.packages.retain(|r| &r.name != name);
                rec.packages.push(PackageRecord {
                    name: name.clone(),
                    installed: now.clone(),
                    replaced: before.clone(),
                });
            }
        }
    }
    for name in held {
        if !rec.held.contains(name) {
            rec.held.push(name.clone());
        }
    }
    rec.at = at;
    rec
}

/// The commands that undo by hand what this command did (printed by the
/// listing and in the guide), ONLY for what is still in place:
/// - `apt-mark unhold` for the holds it set that still exist (`held` = the
///   packages on hold now; `None` = that could not be read, and no unhold
///   is offered);
/// - `apt-get install -y --allow-downgrades name=version` for the packages
///   that are still at the version it put there (`installed` = the versions
///   installed now), back to what was there before; `apt-get remove -y` for
///   those that were not installed before. `-y`: the lines must work as
///   printed, also where there is no terminal to answer apt's question.
///
/// Nothing still in place = no line (the listing after going back).
pub fn rollback_commands(
    rec: &SetRecord,
    installed: &BTreeMap<String, String>,
    held: Option<&[String]>,
) -> Vec<String> {
    let mut out = Vec::new();
    let unhold: Vec<&str> = rec
        .held
        .iter()
        .filter(|n| held.is_some_and(|h| h.contains(n)))
        .map(String::as_str)
        .collect();
    if !unhold.is_empty() {
        out.push(format!("sudo apt-mark unhold {}", unhold.join(" ")));
    }
    let ours: Vec<&PackageRecord> = rec
        .packages
        .iter()
        .filter(|p| installed.get(&p.name) == Some(&p.installed))
        .collect();
    let back: Vec<String> = ours
        .iter()
        .filter_map(|p| p.replaced.as_ref().map(|v| format!("{}={v}", p.name)))
        .collect();
    if !back.is_empty() {
        out.push(format!(
            "sudo apt-get install -y --allow-downgrades {}",
            back.join(" ")
        ));
    }
    let gone: Vec<&str> = ours
        .iter()
        .filter(|p| p.replaced.is_none())
        .map(|p| p.name.as_str())
        .collect();
    if !gone.is_empty() {
        out.push(format!("sudo apt-get remove -y {}", gone.join(" ")));
    }
    out
}

/// What the listing adds under its "to go back" lines when packages are
/// put back: the agent must be restarted (its running captures keep the
/// removed library loaded, so the port keeps reading `ok` until then).
pub const AFTER_GOING_BACK: &str =
    "then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)";

/// …and that going back does not last by itself.
pub const GOING_BACK_DOES_NOT_LAST: &str =
    "going back does not last while a port is set to a sensor that needs these packages: the next `xorgate-rs update` installs them again. Set the port to another sensor or to `none` first";

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
        let installed: BTreeMap<String, String> = plan
            .status
            .iter()
            .filter_map(|s| Some((s.name.clone(), s.installed.clone()?)))
            .collect();
        let replaced: Vec<String> = rec
            .packages
            .iter()
            .filter(|p| installed.get(&p.name) == Some(&p.installed))
            .map(|p| {
                format!(
                    "{} {}",
                    p.name,
                    p.replaced.as_deref().unwrap_or("(was not installed)")
                )
            })
            .collect();
        if !replaced.is_empty() {
            out.push(format!(
                "  installed by xorgate-rs; it replaced: {}",
                replaced.join(", ")
            ));
        }
        let back = rollback_commands(rec, &installed, held);
        // The agent's restart matters when packages are put back; a
        // released hold alone changes nothing the agent runs.
        let packages = back.iter().any(|c| c.contains("apt-get"));
        for cmd in &back {
            out.push(format!("  to go back: {cmd}"));
        }
        if packages {
            out.push(format!("  to go back: {AFTER_GOING_BACK}"));
        }
        if !back.is_empty() {
            out.push(format!("  ({GOING_BACK_DOES_NOT_LAST})"));
        }
    }
    out
}

/// What the listing says about the package sets NO declared sensor needs
/// (any more) while the record shows this command's work on them still in
/// place: one block per such set. `needed` = the ids of the sets in the
/// plan (listed by [`listing`]); `installed` = the versions installed now
/// (`None` = `dpkg-query` could not be run); `held` = the packages on hold
/// (`None` = `apt-mark` could not be asked).
///
/// "Still in place" is what [`rollback_commands`] would undo: a package
/// that is still at the version this command put there, a hold it set that
/// still exists. A set with nothing in place (it was never installed here,
/// or someone went back) gets no line. Nothing here asks for anything: the
/// packages do no harm, the block only says that they are there, that the
/// hold keeps apt from updating them, and how to go back.
pub fn leftover_listing(
    record: &InstallRecord,
    needed: &[&str],
    catalog: &Catalog,
    installed: Option<&BTreeMap<String, String>>,
    held: Option<&[String]>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (id, rec) in &record.sets {
        if needed.contains(&id.as_str()) || (rec.packages.is_empty() && rec.held.is_empty()) {
            continue;
        }
        // Which set that is, and whose: the catalog's words when it still
        // lists the set.
        let what = match catalog.package_sets.iter().find(|s| &s.id == id) {
            Some(set) => {
                let sensors: Vec<&str> = catalog
                    .sensors
                    .iter()
                    .filter(|s| s.package_set.as_deref() == Some(id.as_str()))
                    .map(|s| s.id.as_str())
                    .collect();
                if sensors.is_empty() {
                    format!("{id} ({})", set.label)
                } else {
                    format!("{id} ({}), the set {} needs", set.label, sensors.join(", "))
                }
            }
            None => format!("{id} (a package set this agent build no longer lists)"),
        };
        let Some(installed) = installed else {
            out.push(format!(
                "{what}: no declared sensor needs it any more; whether what xorgate-rs installed for it is still in place could not be read (dpkg-query)"
            ));
            continue;
        };
        let ours: Vec<&PackageRecord> = rec
            .packages
            .iter()
            .filter(|p| installed.get(&p.name) == Some(&p.installed))
            .collect();
        let still_held: Vec<&String> = rec
            .held
            .iter()
            .filter(|n| held.is_some_and(|h| h.contains(n)))
            .collect();
        if ours.is_empty() && still_held.is_empty() {
            continue;
        }
        out.push(format!(
            "{what}: no declared sensor needs it any more, but what xorgate-rs installed for it is still in place:"
        ));
        // The packages this command installed that are still there, then
        // those of which only its hold is left.
        let mut names: Vec<&str> = ours.iter().map(|p| p.name.as_str()).collect();
        for n in &still_held {
            if !names.contains(&n.as_str()) {
                names.push(n.as_str());
            }
        }
        let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
        for name in &names {
            let hold = match held {
                Some(h) if h.iter().any(|x| x == name) => "held",
                Some(_) => "not held",
                None => "hold not read",
            };
            let was = ours
                .iter()
                .find(|p| p.name == *name)
                .map(|p| match &p.replaced {
                    Some(v) => format!("  (installed by xorgate-rs; it replaced {v})"),
                    None => "  (installed by xorgate-rs; it was not installed before)".to_string(),
                })
                .unwrap_or_default();
            out.push(format!(
                "  {name:width$}  installed {}  {hold}{was}",
                installed.get(*name).map(String::as_str).unwrap_or("-"),
            ));
        }
        if !still_held.is_empty() {
            out.push(format!("  {HELD_MEANS}"));
        }
        let back = rollback_commands(rec, installed, held);
        let packages = back.iter().any(|c| c.contains("apt-get"));
        for cmd in &back {
            out.push(format!("  to go back: {cmd}"));
        }
        if packages {
            out.push(format!("  to go back: {AFTER_GOING_BACK}"));
        }
    }
    out
}

/// What a hold that is left behind means (the block of
/// [`leftover_listing`]).
pub const HELD_MEANS: &str =
    "while they are held, apt does not update these packages: the OS's own updates of them do not reach this device";

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
            interrupted: None,
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
        // Every name of every target: dpkg is asked about all of them.
        assert_eq!(
            package_names(&d, c),
            ["libcamera-ipa", "libcamera0.5", "libcamera0.8"]
        );
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
        // Trixie (catalog 4): its own pair, other package names.
        const V8: &str = "0.8.0+rpt20261106-2";
        const STOCK8: &str = "0.8.0+rpt20261104-1";
        let stock8 = host(
            "trixie",
            "arm64",
            &[
                ("libcamera0.8", STOCK8),
                ("libcamera-ipa", STOCK8),
                // Left over from Bookworm (an upgraded device): not asked for.
                ("libcamera0.5", V),
            ],
        );
        let p = plan(&d, c, &stock8);
        assert_eq!(p[0].state, SetState::Missing);
        assert_eq!(p[0].names(), ["libcamera0.8", "libcamera-ipa"]);
        assert_eq!(
            listing(&p[0], None, None),
            [
                "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), needed by imx335 (cam0):",
                "  libcamera0.8   required 0.8.0+rpt20261106-2  installed 0.8.0+rpt20261104-1  MISSING",
                "  libcamera-ipa  required 0.8.0+rpt20261106-2  installed 0.8.0+rpt20261104-1  MISSING",
            ]
        );
        // Installed by hand at the pinned version, not held (the bench of
        // 2026-11-23): satisfied, only the hold is to do.
        let by_hand = host(
            "trixie",
            "arm64",
            &[("libcamera0.8", V8), ("libcamera-ipa", V8)],
        );
        let p = plan(&d, c, &by_hand);
        assert_eq!(p[0].state, SetState::Satisfied);
        assert!(needs_work(&p[0], Some(&[])));
        assert_eq!(
            listing(&p[0], Some(&[]), None)[3],
            "  apt hold: MISSING on libcamera0.8, libcamera-ipa (an apt upgrade may replace them)"
        );
        // Another OS release or architecture: refused, in a sentence.
        for (os, arch) in [
            ("trixie", "armhf"),
            ("bookworm", "armhf"),
            ("bookworm", "amd64"),
            ("bullseye", "arm64"),
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
            interrupted: None,
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
                &pins[..1]
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
        // EVERY package handed to apt must be in its plan, not just one.
        assert_eq!(
            check_simulation("Reading package lists...\n", &pins),
            Err("apt's simulation does not install libcamera0.5, libcamera-ipa".into())
        );
        let one = "Inst libcamera0.5 [1] (0.5.2+rpt20250909-1 local-deb [arm64])\n";
        assert_eq!(
            check_simulation(one, &pins),
            Err("apt's simulation does not install libcamera-ipa".into())
        );
        // The simulation and the real run: the same arguments but `-s`.
        let files = ["/t/a.deb".to_string(), "/t/b.deb".to_string()];
        let (sim, real) = (
            apt_install_args(&files, true),
            apt_install_args(&files, false),
        );
        assert_eq!(
            real,
            [
                "-o",
                "DPkg::Lock::Timeout=120",
                "-o",
                "Dpkg::Options::=--force-confdef",
                "-o",
                "Dpkg::Options::=--force-confold",
                "install",
                "-y",
                "--no-install-recommends",
                "--no-remove",
                "--allow-downgrades",
                "--allow-change-held-packages",
                "/t/a.deb",
                "/t/b.deb"
            ]
        );
        let without_s: Vec<&String> = sim.iter().filter(|a| *a != "-s").collect();
        assert_eq!(without_s, real.iter().collect::<Vec<_>>());
        assert_eq!(sim.len(), real.len() + 1);
        assert_eq!(
            apt_mark_args("hold", &["a".into(), "b".into()]),
            ["hold", "a", "b"]
        );
        assert_eq!(
            parse_showhold("libcamera-ipa\nlibcamera0.5:arm64\n\n"),
            ["libcamera-ipa", "libcamera0.5"]
        );
    }

    /// The bounds add up: the transaction is only started when it and what
    /// follows fit, and the worst case is inside the limit a caller gives.
    #[test]
    fn the_time_bounds_are_consistent() {
        assert_eq!(T_AFTER, 150);
        assert_eq!(STEP_WORST, 30 + 30 + 2 * 180 + 90 + 30 + 600 + 150);
        assert_eq!(STEP_WORST, 1290);
        assert_eq!(STEP_LIMIT, 1320);
        // The catalog's largest set has no more files than the sum assumes.
        let c = catalog();
        for set in &c.package_sets {
            for t in &set.targets {
                assert!(t.packages.len() as u64 <= MAX_FILES, "{}", set.id);
            }
        }
        // No limit: the phase's own bound. A limit: never past it; with
        // nothing left the phase is not started.
        assert_eq!(killable_secs(180, None), Some(180));
        assert_eq!(killable_secs(180, Some(600)), Some(180));
        assert_eq!(killable_secs(180, Some(45)), Some(45));
        assert_eq!(killable_secs(180, Some(1)), None);
        assert_eq!(killable_secs(180, Some(0)), None);
    }

    #[test]
    fn the_dpkg_database_is_read_whole() {
        let db = "libcamera0.5:arm64\t0.5.2+rpt20250909-1\thi \nlibcamera-ipa:arm64\t0.5.2+rpt20250903-1\tii \nbash\t5.2\tii \nold\t1\trc \nhalf\t2\tiU \nworse\t3\tiHR\nconf\t4\tiF \ntrig\t5\tit \n";
        let snap = parse_dpkg_snapshot(db);
        assert_eq!(snap.len(), 8);
        assert_eq!(
            installed_versions(&snap).into_iter().collect::<Vec<_>>(),
            [
                ("bash".to_string(), "5.2".to_string()),
                (
                    "libcamera-ipa".to_string(),
                    "0.5.2+rpt20250903-1".to_string()
                ),
                (
                    "libcamera0.5".to_string(),
                    "0.5.2+rpt20250909-1".to_string()
                ),
            ]
        );
        // dpkg stopped half-way: named, with the state. A pending trigger
        // is not that.
        assert_eq!(
            interrupted(&snap, false).as_deref(),
            Some("conf is half-configured, half is unpacked, not configured, worse is half-installed")
        );
        let clean = parse_dpkg_snapshot("bash\t5.2\tii \ntrig\t5\tit \nold\t1\trc \n");
        assert_eq!(interrupted(&clean, false), None);
        assert_eq!(
            interrupted(&clean, true).as_deref(),
            Some("its journal of an unfinished run is not empty")
        );
        let many: String = (0..11).map(|i| format!("p{i:02}\t1\tiU \n")).collect();
        let text = interrupted(&parse_dpkg_snapshot(&many), false).unwrap();
        assert!(
            text.ends_with("p07 is unpacked, not configured and 3 more"),
            "{text}"
        );

        // What a transaction changed outside the pinned packages.
        let pinned = ["libcamera0.5".to_string(), "libcamera-ipa".to_string()];
        let before = parse_dpkg_snapshot("libcamera0.5\t1\tii \nlibcamera-ipa\t1\tii \nbash\t5.2\tii \ngone\t1\tii \nheld\t1\thi \n");
        let after = parse_dpkg_snapshot("libcamera0.5\t2\tii \nlibcamera-ipa\t2\tii \nbash\t5.3\tii \nheld\t1\thi \nnew\t7\tii \n");
        assert_eq!(
            changed_outside(&before, &after, &pinned),
            [
                "bash 5.2 (ii) -> 5.3 (ii)",
                "gone removed",
                "new installed 7 (ii)"
            ]
        );
        let same = parse_dpkg_snapshot("libcamera0.5\t2\thi \nlibcamera-ipa\t2\thi \nbash\t5.2\tii \ngone\t1\tii \nheld\t1\thi \n");
        assert!(changed_outside(&before, &same, &pinned).is_empty());
    }

    #[test]
    fn the_record_keeps_what_was_there_first() {
        const STOCK: &str = "0.5.2+rpt20250903-1";
        let r = |name: &str, now: &str, before: Option<&str>| -> Replaced {
            (
                name.to_string(),
                now.to_string(),
                before.map(str::to_string),
            )
        };
        let held = ["libcamera0.5".to_string(), "libcamera-ipa".to_string()];
        let now = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect()
        };
        let both = now(&[("libcamera0.5", V), ("libcamera-ipa", V)]);
        let first = merge_record(
            None,
            &[
                r("libcamera0.5", V, Some(STOCK)),
                r("libcamera-ipa", V, Some(STOCK)),
            ],
            &held,
            5,
        );
        assert_eq!(first.packages[0].replaced.as_deref(), Some(STOCK));
        assert_eq!(first.held, held);
        assert_eq!(
            rollback_commands(&first, &both, Some(&held)),
            [
                "sudo apt-mark unhold libcamera0.5 libcamera-ipa",
                "sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
            ]
        );
        // A second run with nothing to replace: the record is as it was.
        let second = merge_record(Some(&first), &[], &[], 9);
        assert_eq!(
            (second.packages.clone(), second.held.clone()),
            (first.packages.clone(), first.held.clone())
        );
        assert_eq!(second.at, 9);

        // THE TWO-RUN CASE (review N3). Run 1: apt upgrades libcamera0.5 and
        // fails on libcamera-ipa. What was replaced is recorded at once.
        let run1 = merge_record(None, &[r("libcamera0.5", V, Some(STOCK))], &[], 1);
        assert_eq!(run1.packages.len(), 1);
        // Run 2 finds libcamera0.5 at the pinned version and installs only
        // libcamera-ipa. libcamera0.5 keeps the version that was there
        // FIRST; nothing is recorded as replaced by the pinned version.
        let run2 = merge_record(Some(&run1), &[r("libcamera-ipa", V, Some(STOCK))], &held, 2);
        assert_eq!(
            run2.packages,
            [
                PackageRecord {
                    name: "libcamera0.5".into(),
                    installed: V.into(),
                    replaced: Some(STOCK.into())
                },
                PackageRecord {
                    name: "libcamera-ipa".into(),
                    installed: V.into(),
                    replaced: Some(STOCK.into())
                },
            ]
        );
        assert!(run2
            .packages
            .iter()
            .all(|p| p.replaced.as_deref() != Some(V)));
        assert_eq!(
            rollback_commands(&run2, &both, Some(&held))[1],
            "sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1"
        );
        // Without run 1's record (the old behaviour's input): a package
        // found AT the pinned version is simply not recorded, rather than
        // "replaced by" that version.
        let blind = merge_record(
            None,
            &[
                r("libcamera0.5", V, Some(V)),
                r("libcamera-ipa", V, Some(STOCK)),
            ],
            &held,
            2,
        );
        assert_eq!(blind.packages.len(), 1);
        assert_eq!(blind.packages[0].name, "libcamera-ipa");
        assert_eq!(
            rollback_commands(&blind, &both, Some(&held)),
            [
                "sudo apt-mark unhold libcamera0.5 libcamera-ipa",
                "sudo apt-get install -y --allow-downgrades libcamera-ipa=0.5.2+rpt20250903-1",
            ]
        );

        // The pin moves (a newer build of the set): the package this command
        // installed keeps the stock version as what it replaced.
        let moved = merge_record(
            Some(&first),
            &[r("libcamera0.5", "0.6.0-1", Some(V))],
            &[],
            3,
        );
        assert_eq!(moved.packages[0].installed, "0.6.0-1");
        assert_eq!(moved.packages[0].replaced.as_deref(), Some(STOCK));
        // Someone put another version there in between: THAT is what this
        // run replaced.
        let other = merge_record(
            Some(&first),
            &[r("libcamera0.5", V, Some("0.5.3-1"))],
            &[],
            3,
        );
        let entry = other
            .packages
            .iter()
            .find(|p| p.name == "libcamera0.5")
            .unwrap();
        assert_eq!(entry.replaced.as_deref(), Some("0.5.3-1"));
        assert_eq!(other.packages.len(), 2);
        // A package that is no longer at the version this command put there
        // is left out of the way back.
        let changed = now(&[("libcamera0.5", "0.5.3-1"), ("libcamera-ipa", V)]);
        assert_eq!(
            rollback_commands(&first, &changed, Some(&held))[1],
            "sudo apt-get install -y --allow-downgrades libcamera-ipa=0.5.2+rpt20250903-1"
        );
        // A package that was not installed before; a hold this command did
        // not set is not released.
        let none = merge_record(
            None,
            &[r("libcamera0.5", V, None), r("libcamera-ipa", V, None)],
            &held[1..],
            1,
        );
        assert_eq!(
            rollback_commands(&none, &both, Some(&held)),
            [
                "sudo apt-mark unhold libcamera-ipa",
                "sudo apt-get remove -y libcamera0.5 libcamera-ipa"
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
        assert!(text.starts_with(r#"{"v":1,"sets":{"arducam-libcamera":{"at":5,"packages":[{"name":"libcamera0.5","installed":"0.5.2+rpt20250909-1","replaced":"0.5.2+rpt20250903-1"}"#), "{text}");
        assert!(
            text.ends_with(r#""held":["libcamera0.5","libcamera-ipa"]}}}"#),
            "{text}"
        );
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
                "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
                "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
                "  (going back does not last while a port is set to a sensor that needs these packages: the next `xorgate-rs update` installs them again. Set the port to another sensor or to `none` first)",
            ]
        );
        // Only what is still in place: no hold now = no unhold line; the
        // holds unknown = none offered either; nothing left = no line.
        let no_holds: &[String] = &[];
        assert_eq!(rollback_commands(&first, &both, Some(no_holds)).len(), 1);
        assert_eq!(rollback_commands(&first, &both, None).len(), 1);
        assert!(rollback_commands(&first, &BTreeMap::new(), Some(no_holds)).is_empty());
        assert_eq!(
            rollback_commands(&first, &BTreeMap::new(), Some(&held)),
            ["sudo apt-mark unhold libcamera0.5 libcamera-ipa"]
        );

        // THE SENSOR LEFT THE DEVICE: no declared sensor needs the set any
        // more, and what this command installed is still there and held.
        // One block says so, with the same lines to go back.
        let record = InstallRecord {
            v: 1,
            sets: [("arducam-libcamera".to_string(), first.clone())].into(),
        };
        let left = |needed: &[&str],
                    installed: Option<&BTreeMap<String, String>>,
                    held: Option<&[String]>| {
            leftover_listing(&record, needed, c, installed, held)
        };
        assert_eq!(
            left(&[], Some(&both), Some(&held)),
            [
                "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), the set imx335 needs: no declared sensor needs it any more, but what xorgate-rs installed for it is still in place:",
                "  libcamera0.5   installed 0.5.2+rpt20250909-1  held  (installed by xorgate-rs; it replaced 0.5.2+rpt20250903-1)",
                "  libcamera-ipa  installed 0.5.2+rpt20250909-1  held  (installed by xorgate-rs; it replaced 0.5.2+rpt20250903-1)",
                "  while they are held, apt does not update these packages: the OS's own updates of them do not reach this device",
                "  to go back: sudo apt-mark unhold libcamera0.5 libcamera-ipa",
                "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
                "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
            ]
        );
        // A set a declared sensor needs is the plan's to list, not this.
        assert!(left(&["arducam-libcamera"], Some(&both), Some(&held)).is_empty());
        // The hold was released, the packages are still ours: no hold line,
        // no unhold.
        assert_eq!(
            left(&[], Some(&both), Some(no_holds)),
            [
                "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), the set imx335 needs: no declared sensor needs it any more, but what xorgate-rs installed for it is still in place:",
                "  libcamera0.5   installed 0.5.2+rpt20250909-1  not held  (installed by xorgate-rs; it replaced 0.5.2+rpt20250903-1)",
                "  libcamera-ipa  installed 0.5.2+rpt20250909-1  not held  (installed by xorgate-rs; it replaced 0.5.2+rpt20250903-1)",
                "  to go back: sudo apt-get install -y --allow-downgrades libcamera0.5=0.5.2+rpt20250903-1 libcamera-ipa=0.5.2+rpt20250903-1",
                "  to go back: then: sudo systemctl restart xorgate-agent-rs (until the agent restarts, its running captures keep the replaced library loaded and the port still reads ok)",
            ]
        );
        // The packages were put back by hand, the hold was left: the hold
        // is what remains, and the stock versions are not called ours.
        let stock = now(&[("libcamera0.5", STOCK), ("libcamera-ipa", STOCK)]);
        assert_eq!(
            left(&[], Some(&stock), Some(&held)),
            [
                "arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), the set imx335 needs: no declared sensor needs it any more, but what xorgate-rs installed for it is still in place:",
                "  libcamera0.5   installed 0.5.2+rpt20250903-1  held",
                "  libcamera-ipa  installed 0.5.2+rpt20250903-1  held",
                "  while they are held, apt does not update these packages: the OS's own updates of them do not reach this device",
                "  to go back: sudo apt-mark unhold libcamera0.5 libcamera-ipa",
            ]
        );
        // The holds could not be read: the packages are still said, and no
        // unhold is offered.
        let blind = left(&[], Some(&both), None);
        assert_eq!(blind.len(), 5, "{blind:?}");
        assert!(blind[1].ends_with(
            "hold not read  (installed by xorgate-rs; it replaced 0.5.2+rpt20250903-1)"
        ));
        // Everything was put back: nothing is said.
        assert!(left(&[], Some(&stock), Some(no_holds)).is_empty());
        // dpkg could not be asked: that is said, nothing is concluded.
        assert_eq!(
            left(&[], None, Some(&held)),
            ["arducam-libcamera (Arducam's libcamera build (adds the IMX335 camera helper)), the set imx335 needs: no declared sensor needs it any more; whether what xorgate-rs installed for it is still in place could not be read (dpkg-query)"]
        );
        // A record without anything in it, and a set this build's catalog
        // does not list.
        let other = InstallRecord {
            v: 1,
            sets: [
                ("empty".to_string(), SetRecord::default()),
                ("old-set".to_string(), first.clone()),
            ]
            .into(),
        };
        let lines = leftover_listing(&other, &[], c, Some(&both), Some(&held));
        assert_eq!(lines.len(), 7, "{lines:?}");
        assert_eq!(
            lines[0],
            "old-set (a package set this agent build no longer lists): no declared sensor needs it any more, but what xorgate-rs installed for it is still in place:"
        );
    }
}
