//! The camera sensor catalog (support-other-camera-models plan, Phase 1).
//! RUST-ONLY: the JS agent has no counterpart.
//!
//! One source: the backend's typed module, emitted as `camera-catalog.json`
//! by `npm run contract:generate` and vendored at
//! `crates/xorgate-core/data/camera-catalog.json`. NEVER edit that file by
//! hand: regenerate it (`guide/08-common-tasks.md`), then move the sha256
//! pinned in `tests/camera_catalog.rs`.
//!
//! It is compiled in ([`CATALOG_JSON`]) and parsed once ([`catalog`]).
//! Everything that needs a per-sensor fact reads it here: the overlay name
//! and its default rotation, whether the sensor can flip, the names
//! libcamera may list it under, its device-tree node names and how far its
//! driver proves the chip.
//!
//! THE COMPATIBILITY RULE for every consumer of the file (it is not written
//! inside the JSON; plan PROGRESS.md, "Implementation log"):
//!
//! 1. Unknown keys are ignored. An unknown `schemaVersion` is REFUSED: the
//!    whole file is then unusable ([`CatalogError::SchemaVersion`]) and the
//!    agent behaves as if it knew no sensor at all. It never panics.
//! 2. An unknown enum member (`tier`, `identity`, an OS release, a
//!    `nativeAspect`, a package `arch`) makes THAT entry unusable by this
//!    build, never the file. So does an entry that lacks a field this build
//!    needs or carries one of the wrong type. Such entries are listed in
//!    [`Catalog::skipped`].
//! 3. An option value the entry does not list is rejected
//!    ([`Sensor::option_token`]), and config.txt is left alone.

use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The vendored artifact, byte for byte.
pub const CATALOG_JSON: &str = include_str!("../data/camera-catalog.json");

/// The `schemaVersion` this build reads.
pub const SCHEMA_VERSION: u64 = 1;

/// The OS releases this build knows (rule 2).
pub const KNOWN_OS: &[&str] = &["bookworm", "trixie"];
/// The `nativeAspect` values this build knows (rule 2).
pub const KNOWN_ASPECTS: &[&str] = &["16:9", "4:3"];
/// The package architectures this build knows (rule 2).
pub const KNOWN_ARCH: &[&str] = &["arm64"];

/// The longest `config.txt` line the firmware reads (98 characters; the
/// backend exports the same number as `CONFIG_TXT_LINE_MAX`).
pub const CONFIG_TXT_LINE_MAX: usize = 98;

/// How far "a driver is bound" proves which chip is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Identity {
    /// The driver reads a chip ID at probe and refuses another chip
    /// (`imx219`, `imx708`): bound means it IS that sensor.
    ChipId,
    /// The driver checks one register for a fixed value another chip may
    /// also return (`imx335`): bound is a hint.
    Weak,
    /// The driver does no identity check at all (`imx290`): it binds to
    /// anything, so bound proves nothing.
    None,
}

impl Identity {
    pub fn as_str(self) -> &'static str {
        match self {
            Identity::ChipId => "chip-id",
            Identity::Weak => "weak",
            Identity::None => "none",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "chip-id" => Some(Identity::ChipId),
            "weak" => Some(Identity::Weak),
            "none" => Some(Identity::None),
            _ => None,
        }
    }
}

/// The support tier of a selectable sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Works on the stock OS image.
    Supported,
    /// Works once the entry's package set is installed.
    SupportedWithPackages,
}

impl Tier {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "supported" => Some(Tier::Supported),
            "supported-with-packages" => Some(Tier::SupportedWithPackages),
            _ => None,
        }
    }
}

/// One option a user may set on a sensor's overlay line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorOption {
    /// The setting's key (`clockFrequency`).
    pub key: String,
    /// The overlay parameter it becomes (`clock-frequency`).
    pub param: String,
    /// The values the entry lists; nothing else is accepted (rule 3).
    pub values: Vec<i64>,
    pub default: i64,
}

/// A preset the sensor cannot run, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPreset {
    pub preset: String,
    pub reason: String,
}

/// One selectable sensor of the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sensor {
    pub id: String,
    pub label: String,
    pub tier: Tier,
    /// The `dtoverlay=` name.
    pub overlay: String,
    /// Parameters every line of this sensor carries, verbatim.
    pub fixed_params: Vec<String>,
    /// The device-tree rotation the overlay gives a line with NO `rotation=`
    /// token (0 or 180). The camera mount is added to it.
    pub default_rotation: u16,
    /// Whether the sensor can turn its picture at all.
    pub can_flip: bool,
    pub options: Vec<SensorOption>,
    /// Every name libcamera may list this sensor under (`imx708_wide`…).
    pub libcamera_names: Vec<String>,
    /// The device-tree node names of the sensor (`imx708` in `imx708@1a`).
    pub dt_node_names: Vec<String>,
    pub identity: Identity,
    /// Fixed `libcamerasrc` properties (empty in catalog version 3).
    pub source_props: Vec<String>,
    /// The OS releases the sensor works on.
    pub requires_os: Vec<String>,
    /// The package set the sensor needs, by id.
    pub package_set: Option<String>,
    pub unsupported_presets: Vec<UnsupportedPreset>,
}

/// Why an option cannot be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionError {
    UnknownKey(String),
    /// The value is not one the entry lists (rule 3).
    ValueNotListed {
        key: String,
        value: i64,
    },
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OptionError::UnknownKey(k) => write!(f, "this sensor has no option \"{k}\""),
            OptionError::ValueNotListed { key, value } => {
                write!(f, "{value} is not a listed value of option \"{key}\"")
            }
        }
    }
}

impl Sensor {
    /// The overlay token of one option (`clock-frequency=74250000`). A value
    /// the entry does not list is refused (rule 3).
    pub fn option_token(&self, key: &str, value: i64) -> Result<String, OptionError> {
        let opt = self
            .options
            .iter()
            .find(|o| o.key == key)
            .ok_or_else(|| OptionError::UnknownKey(key.to_string()))?;
        if !opt.values.contains(&value) {
            return Err(OptionError::ValueNotListed {
                key: key.to_string(),
                value,
            });
        }
        Ok(format!("{}={value}", opt.param))
    }

    /// The parameter NAMES a camera line of this sensor may carry besides
    /// the port token and `rotation`: those of its fixed parameters, then
    /// those of its options, each once. The helper's `set` writes no other
    /// name for this overlay (its allow-list is rendered from this).
    pub fn param_names(&self) -> Vec<&str> {
        let fixed = self
            .fixed_params
            .iter()
            .map(|p| p.split('=').next().unwrap_or(p));
        let options = self.options.iter().map(|o| o.param.as_str());
        let mut out: Vec<&str> = Vec::new();
        for name in fixed.chain(options) {
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// The reason this sensor cannot run `preset`, if it cannot.
    pub fn preset_unsupported(&self, preset: &str) -> Option<&str> {
        self.unsupported_presets
            .iter()
            .find(|p| p.preset == preset)
            .map(|p| p.reason.as_str())
    }
}

/// One package of a package set, for one OS release and architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}

/// The packages of a package set for one OS release and architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageTarget {
    pub os: String,
    pub arch: String,
    pub packages: Vec<Package>,
}

/// Extra OS packages some sensors need (the IMX335's libcamera build).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSet {
    pub id: String,
    pub label: String,
    pub apt_hold: bool,
    /// The targets this build can use (a target with an OS release or an
    /// architecture it does not know is left out, rule 2).
    pub targets: Vec<PackageTarget>,
}

/// Why the catalog file as a whole cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// Not JSON, or not the expected top-level shape.
    Malformed(String),
    /// A `schemaVersion` this build does not read (rule 1).
    SchemaVersion(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Malformed(m) => write!(f, "the camera catalog is malformed: {m}"),
            CatalogError::SchemaVersion(v) => write!(
                f,
                "the camera catalog has schemaVersion {v}; this agent build reads {SCHEMA_VERSION}"
            ),
        }
    }
}

/// The parsed catalog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Catalog {
    /// The content version (it moves with every sensor change).
    pub catalog_version: u64,
    /// The sensors this build can use, in file order.
    pub sensors: Vec<Sensor>,
    /// `(id, reason)` of the entries this build cannot use (rule 2).
    pub skipped: Vec<(String, String)>,
    pub package_sets: Vec<PackageSet>,
    /// The ids of the announced, not yet selectable sensors.
    pub planned: Vec<String>,
}

fn str_of<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("\"{key}\" is missing or not a string"))
}

fn strings_of(v: &Value, key: &str) -> Result<Vec<String>, String> {
    let arr = v
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("\"{key}\" is missing or not a list"))?;
    arr.iter()
        .map(|s| {
            s.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("\"{key}\" holds a value that is not a string"))
        })
        .collect()
}

/// The charset of an overlay name and of a parameter name, as the helper
/// checks them (`system/xorgate-camcfg`): `[a-z0-9][a-z0-9_-]*`, at most 32.
pub fn valid_overlay_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// One overlay parameter token as the helper accepts it: a name
/// ([`valid_overlay_name`]), optionally `=` and a value of `[A-Za-z0-9_.+-]`,
/// 1 to 32 characters.
pub fn valid_overlay_param(s: &str) -> bool {
    let (name, value) = match s.split_once('=') {
        Some((n, v)) => (n, Some(v)),
        None => (s, None),
    };
    if !valid_overlay_name(name) {
        return false;
    }
    match value {
        None => true,
        Some(v) => {
            !v.is_empty()
                && v.len() <= 32
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'+' | b'-'))
        }
    }
}

/// A parameter name the camera-line owner writes itself and never takes
/// from a caller or from the catalog: the port tokens, and `rotation` in
/// any form (`rotation`, `rotation-x`…).
pub fn reserved_param_name(name: &str) -> bool {
    name == "cam0" || name == "cam1" || name.starts_with("rotation")
}

fn parse_sensor(v: &Value) -> Result<Sensor, String> {
    let id = str_of(v, "id")?;
    let tier_s = str_of(v, "tier")?;
    let tier = Tier::parse(tier_s).ok_or_else(|| format!("unknown tier \"{tier_s}\""))?;
    let overlay = v
        .get("overlay")
        .filter(|o| o.is_object())
        .ok_or("\"overlay\" is missing or not an object")?;
    let name = str_of(overlay, "name")?;
    if !valid_overlay_name(name) {
        return Err(format!("overlay name \"{name}\" is outside [a-z0-9_-]"));
    }
    let fixed_params = strings_of(overlay, "fixedParams")?;
    if let Some(bad) = fixed_params.iter().find(|p| !valid_overlay_param(p)) {
        return Err(format!("fixed parameter \"{bad}\" is not a valid token"));
    }
    if let Some(bad) = fixed_params
        .iter()
        .find(|p| reserved_param_name(p.split('=').next().unwrap_or(p)))
    {
        return Err(format!(
            "fixed parameter \"{bad}\" is written by the camera line owner itself"
        ));
    }
    let default_rotation = match overlay.get("defaultRotation").and_then(Value::as_u64) {
        Some(0) => 0,
        Some(180) => 180,
        _ => return Err("\"overlay.defaultRotation\" is not 0 or 180".into()),
    };
    let can_flip = overlay
        .get("canFlip")
        .and_then(Value::as_bool)
        .ok_or("\"overlay.canFlip\" is missing or not a boolean")?;
    let mut options = Vec::new();
    for o in v
        .get("options")
        .and_then(Value::as_array)
        .ok_or("\"options\" is missing or not a list")?
    {
        let param = str_of(o, "param")?;
        if !valid_overlay_name(param) {
            return Err(format!(
                "option parameter \"{param}\" is outside [a-z0-9_-]"
            ));
        }
        if reserved_param_name(param) {
            return Err(format!(
                "option parameter \"{param}\" is written by the camera line owner itself"
            ));
        }
        let values: Option<Vec<i64>> = o
            .get("values")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(Value::as_i64).collect())
            .unwrap_or(None);
        let values = values.ok_or("an option's \"values\" is missing or not a list of integers")?;
        let default = o
            .get("default")
            .and_then(Value::as_i64)
            .ok_or("an option's \"default\" is missing or not an integer")?;
        options.push(SensorOption {
            key: str_of(o, "key")?.to_string(),
            param: param.to_string(),
            values,
            default,
        });
    }
    let identity_s = str_of(v, "identity")?;
    let identity =
        Identity::parse(identity_s).ok_or_else(|| format!("unknown identity \"{identity_s}\""))?;
    let requires = v
        .get("requires")
        .filter(|r| r.is_object())
        .ok_or("\"requires\" is missing or not an object")?;
    let requires_os = strings_of(requires, "os")?;
    if let Some(os) = requires_os.iter().find(|o| !KNOWN_OS.contains(&o.as_str())) {
        return Err(format!("unknown OS release \"{os}\""));
    }
    let package_set = match requires.get("packageSet") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("\"requires.packageSet\" is not a string".into()),
    };
    // `info.nativeAspect` is information for the console; the agent reads
    // it only to honour rule 2.
    if let Some(aspect) = v
        .get("info")
        .and_then(|i| i.get("nativeAspect"))
        .filter(|a| !a.is_null())
    {
        match aspect.as_str() {
            Some(a) if KNOWN_ASPECTS.contains(&a) => {}
            Some(a) => return Err(format!("unknown nativeAspect \"{a}\"")),
            None => return Err("\"info.nativeAspect\" is not a string".into()),
        }
    }
    let source_props = match v.get("source") {
        None | Some(Value::Null) => Vec::new(),
        Some(s) => strings_of(s, "props")?,
    };
    let mut unsupported_presets = Vec::new();
    if let Some(list) = v.get("unsupportedPresets").filter(|l| !l.is_null()) {
        for p in list
            .as_array()
            .ok_or("\"unsupportedPresets\" is not a list")?
        {
            unsupported_presets.push(UnsupportedPreset {
                preset: str_of(p, "preset")?.to_string(),
                reason: str_of(p, "reason")?.to_string(),
            });
        }
    }
    let libcamera_names = strings_of(v, "libcameraNames")?;
    let dt_node_names = strings_of(v, "dtNodeNames")?;
    if libcamera_names.is_empty() || dt_node_names.is_empty() {
        return Err("\"libcameraNames\" or \"dtNodeNames\" is empty".into());
    }
    Ok(Sensor {
        id: id.to_string(),
        label: str_of(v, "label")?.to_string(),
        tier,
        overlay: name.to_string(),
        fixed_params,
        default_rotation,
        can_flip,
        options,
        libcamera_names,
        dt_node_names,
        identity,
        source_props,
        requires_os,
        package_set,
        unsupported_presets,
    })
}

fn parse_package_set(v: &Value) -> Result<PackageSet, String> {
    let mut targets = Vec::new();
    for t in v
        .get("targets")
        .and_then(Value::as_array)
        .ok_or("\"targets\" is missing or not a list")?
    {
        let (os, arch) = (str_of(t, "os")?, str_of(t, "arch")?);
        // Rule 2: a target for an OS release or an architecture this build
        // does not know is not usable; the set itself stays.
        if !KNOWN_OS.contains(&os) || !KNOWN_ARCH.contains(&arch) {
            continue;
        }
        let mut packages = Vec::new();
        for p in t
            .get("packages")
            .and_then(Value::as_array)
            .ok_or("a target's \"packages\" is missing or not a list")?
        {
            packages.push(Package {
                name: str_of(p, "name")?.to_string(),
                version: str_of(p, "version")?.to_string(),
                url: str_of(p, "url")?.to_string(),
                sha256: str_of(p, "sha256")?.to_string(),
                bytes: p
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .ok_or("a package's \"bytes\" is missing or not a number")?,
            });
        }
        targets.push(PackageTarget {
            os: os.to_string(),
            arch: arch.to_string(),
            packages,
        });
    }
    Ok(PackageSet {
        id: str_of(v, "id")?.to_string(),
        label: str_of(v, "label")?.to_string(),
        apt_hold: v.get("aptHold").and_then(Value::as_bool).unwrap_or(false),
        targets,
    })
}

/// Parse a catalog file under the compatibility rule (module docs). Never
/// panics, whatever the input.
pub fn parse_catalog(text: &str) -> Result<Catalog, CatalogError> {
    let doc: Value =
        serde_json::from_str(text).map_err(|e| CatalogError::Malformed(e.to_string()))?;
    if !doc.is_object() {
        return Err(CatalogError::Malformed("not a JSON object".into()));
    }
    match doc.get("schemaVersion") {
        Some(v) if v.as_u64() == Some(SCHEMA_VERSION) => {}
        Some(v) => return Err(CatalogError::SchemaVersion(v.to_string())),
        None => return Err(CatalogError::SchemaVersion("(none)".into())),
    }
    let entries = doc
        .get("sensors")
        .and_then(Value::as_array)
        .ok_or_else(|| CatalogError::Malformed("\"sensors\" is missing or not a list".into()))?;
    let mut out = Catalog {
        catalog_version: doc
            .get("catalogVersion")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Catalog::default()
    };
    for (i, entry) in entries.iter().enumerate() {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("#{i}"));
        match parse_sensor(entry) {
            // The first entry of an id, an overlay or a name wins: a later
            // duplicate would make the lookups ambiguous.
            Ok(s)
                if out
                    .sensors
                    .iter()
                    .any(|k| k.id == s.id || k.overlay == s.overlay) =>
            {
                out.skipped
                    .push((id, "duplicate id or overlay name".to_string()));
            }
            Ok(s) => out.sensors.push(s),
            Err(reason) => out.skipped.push((id, reason)),
        }
    }
    if let Some(sets) = doc.get("packageSets").and_then(Value::as_array) {
        for set in sets {
            // A package set this build cannot read is left out; a sensor
            // that names it then has a requirement nothing can install.
            if let Ok(s) = parse_package_set(set) {
                out.package_sets.push(s);
            }
        }
    }
    if let Some(planned) = doc.get("planned").and_then(Value::as_array) {
        out.planned = planned
            .iter()
            .filter_map(|p| p.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
    }
    Ok(out)
}

impl Catalog {
    /// The sensor of a catalog id (`imx708`).
    pub fn by_id(&self, id: &str) -> Option<&Sensor> {
        self.sensors.iter().find(|s| s.id == id)
    }

    /// The sensor whose `dtoverlay=` name this is.
    pub fn by_overlay(&self, overlay: &str) -> Option<&Sensor> {
        self.sensors.iter().find(|s| s.overlay == overlay)
    }

    /// The sensor libcamera lists under this name (`imx708_wide`).
    pub fn by_libcamera_name(&self, listed: &str) -> Option<&Sensor> {
        self.sensors
            .iter()
            .find(|s| s.libcamera_names.iter().any(|n| n == listed))
    }

    /// The sensor of a device-tree node (`imx708@1a`, or the bare `imx708`).
    pub fn by_dt_node(&self, node: &str) -> Option<&Sensor> {
        let name = node.split('@').next().unwrap_or(node);
        self.sensors
            .iter()
            .find(|s| s.dt_node_names.iter().any(|n| n == name))
    }

    pub fn package_set(&self, id: &str) -> Option<&PackageSet> {
        self.package_sets.iter().find(|s| s.id == id)
    }

    /// The body of the helper's allow-list file
    /// (`system/xorgate-camcfg.overlays`): per sensor, in catalog order, one
    /// line holding the overlay name the helper may write into a NEW or
    /// REPLACED camera line, then the parameter NAMES it may write for that
    /// overlay ([`Sensor::param_names`]), separated by spaces. Generated:
    /// `tests/camera_catalog.rs` pins the committed file to this text.
    pub fn helper_allow_list(&self) -> String {
        let mut out = String::from(
            "# Camera overlays xorgate-camcfg may write, and the parameter names it may\n\
             # write for each: <overlay> [<param name>...]. GENERATED from\n\
             # crates/xorgate-core/data/camera-catalog.json (Catalog::helper_allow_list);\n\
             # do not edit by hand.\n",
        );
        out.push_str(&format!("# catalogVersion {}\n", self.catalog_version));
        for s in &self.sensors {
            out.push_str(&s.overlay);
            for name in s.param_names() {
                out.push(' ');
                out.push_str(name);
            }
            out.push('\n');
        }
        out
    }
}

/// The compiled-in catalog and, when it could not be read, why. Parsed
/// once. On an error the catalog is EMPTY: every overlay is then "outside
/// the catalog" and no camera line is ever rendered.
pub fn loaded() -> &'static (Catalog, Option<CatalogError>) {
    static CATALOG: OnceLock<(Catalog, Option<CatalogError>)> = OnceLock::new();
    CATALOG.get_or_init(|| match parse_catalog(CATALOG_JSON) {
        Ok(c) => (c, None),
        Err(e) => (Catalog::default(), Some(e)),
    })
}

/// The compiled-in catalog (empty if the file could not be read).
pub fn catalog() -> &'static Catalog {
    &loaded().0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One edit of the catalog document.
    type Edit = Box<dyn Fn(&mut Value)>;

    fn doc() -> Value {
        serde_json::from_str(CATALOG_JSON).unwrap()
    }

    fn reparse(d: &Value) -> Result<Catalog, CatalogError> {
        parse_catalog(&d.to_string())
    }

    #[test]
    fn the_vendored_catalog_reads_whole() {
        let (c, err) = loaded();
        assert_eq!(*err, None);
        assert_eq!(c.catalog_version, 3);
        assert_eq!(c.skipped, vec![]);
        let ids: Vec<&str> = c.sensors.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["imx290", "imx219", "imx708", "imx335"]);
        assert_eq!(c.planned, ["imx662"]);
        // The facts the rotation rule rests on (bench, 2026-11-21).
        let rot = |id: &str| {
            let s = c.by_id(id).unwrap();
            (s.default_rotation, s.can_flip)
        };
        assert_eq!(rot("imx290"), (0, true));
        assert_eq!(rot("imx219"), (180, true));
        assert_eq!(rot("imx708"), (180, true));
        assert_eq!(rot("imx335"), (180, false));
        let id = |n: &str| c.by_id(n).unwrap().identity;
        assert_eq!(id("imx290"), Identity::None);
        assert_eq!(id("imx219"), Identity::ChipId);
        assert_eq!(id("imx708"), Identity::ChipId);
        assert_eq!(id("imx335"), Identity::Weak);
        assert_eq!(c.by_id("imx335").unwrap().tier, Tier::SupportedWithPackages);
        assert_eq!(
            c.by_id("imx335").unwrap().package_set.as_deref(),
            Some("arducam-libcamera")
        );
        assert_eq!(
            c.package_set("arducam-libcamera").unwrap().targets[0]
                .packages
                .len(),
            2
        );
        assert!(c
            .by_id("imx335")
            .unwrap()
            .preset_unsupported("1080p30-4mbps")
            .is_some());
        assert_eq!(
            c.by_id("imx290")
                .unwrap()
                .preset_unsupported("1080p30-4mbps"),
            None
        );
    }

    #[test]
    fn lookups_by_overlay_libcamera_name_and_node() {
        let c = catalog();
        assert_eq!(c.by_overlay("imx708").unwrap().id, "imx708");
        assert!(c.by_overlay("imx477").is_none());
        for n in ["imx708", "imx708_wide", "imx708_noir", "imx708_wide_noir"] {
            assert_eq!(c.by_libcamera_name(n).unwrap().id, "imx708", "{n}");
        }
        assert!(c.by_libcamera_name("imx219_noir").is_none());
        assert_eq!(c.by_dt_node("imx708@1a").unwrap().id, "imx708");
        assert_eq!(c.by_dt_node("imx290").unwrap().id, "imx290");
        assert!(c.by_dt_node("dw9817@c").is_none());
        assert!(c.by_dt_node("").is_none());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut d = doc();
        d["somethingNew"] = serde_json::json!({"a": 1});
        d["sensors"][0]["somethingNew"] = serde_json::json!([1, 2]);
        d["sensors"][0]["overlay"]["extra"] = serde_json::json!("x");
        d["sensors"][0]["options"][0]["extra"] = serde_json::json!(true);
        d["packageSets"][0]["extra"] = serde_json::json!(null);
        let c = reparse(&d).unwrap();
        assert_eq!(c, *catalog());
    }

    #[test]
    fn an_unknown_schema_version_is_refused_without_a_panic() {
        for v in [
            serde_json::json!(2),
            serde_json::json!(0),
            serde_json::json!("1"),
            serde_json::json!(null),
            serde_json::json!(1.5),
        ] {
            let mut d = doc();
            d["schemaVersion"] = v.clone();
            assert!(
                matches!(reparse(&d), Err(CatalogError::SchemaVersion(_))),
                "{v}"
            );
        }
        let mut d = doc();
        d.as_object_mut().unwrap().remove("schemaVersion");
        assert_eq!(
            reparse(&d),
            Err(CatalogError::SchemaVersion("(none)".into()))
        );
        assert_eq!(
            CatalogError::SchemaVersion("2".into()).to_string(),
            "the camera catalog has schemaVersion 2; this agent build reads 1"
        );
    }

    #[test]
    fn garbage_is_an_error_never_a_panic() {
        for text in [
            "",
            "{",
            "[]",
            "null",
            "42",
            "\"x\"",
            "{\"schemaVersion\":1}",
            "{\"schemaVersion\":1,\"sensors\":{}}",
            "{\"schemaVersion\":1,\"sensors\":7}",
        ] {
            assert!(
                matches!(parse_catalog(text), Err(CatalogError::Malformed(_))),
                "{text:?}"
            );
        }
        // Entries of the wrong shape are skipped one by one.
        let c = parse_catalog(
            "{\"schemaVersion\":1,\"sensors\":[1,\"x\",null,[],{},{\"id\":\"a\"}],\"packageSets\":[3],\"planned\":[4,{}]}",
        )
        .unwrap();
        assert!(c.sensors.is_empty());
        assert_eq!(c.skipped.len(), 6);
        assert_eq!(c.skipped[5].0, "a");
        assert!(c.package_sets.is_empty() && c.planned.is_empty());
    }

    /// Rule 2: an unknown enum member makes that entry unusable, not the file.
    #[test]
    fn an_unknown_enum_member_costs_only_its_entry() {
        let cases: [(&str, Edit, &str); 4] = [
            (
                "tier",
                Box::new(|d: &mut Value| d["sensors"][1]["tier"] = "experimental".into()),
                "unknown tier \"experimental\"",
            ),
            (
                "identity",
                Box::new(|d: &mut Value| d["sensors"][1]["identity"] = "serial-number".into()),
                "unknown identity \"serial-number\"",
            ),
            (
                "os",
                Box::new(|d: &mut Value| {
                    d["sensors"][1]["requires"]["os"] = serde_json::json!(["forky"])
                }),
                "unknown OS release \"forky\"",
            ),
            (
                "aspect",
                Box::new(|d: &mut Value| d["sensors"][1]["info"]["nativeAspect"] = "3:2".into()),
                "unknown nativeAspect \"3:2\"",
            ),
        ];
        for (name, edit, reason) in cases {
            let mut d = doc();
            edit(&mut d);
            let c = reparse(&d).unwrap();
            let ids: Vec<&str> = c.sensors.iter().map(|s| s.id.as_str()).collect();
            assert_eq!(ids, ["imx290", "imx708", "imx335"], "{name}");
            assert_eq!(
                c.skipped,
                vec![("imx219".to_string(), reason.to_string())],
                "{name}"
            );
            assert!(c.by_overlay("imx219").is_none(), "{name}");
        }
        // A package target for an unknown architecture or OS is left out;
        // the set and the sensors stay.
        let mut d = doc();
        d["packageSets"][0]["targets"][0]["arch"] = "riscv64".into();
        let c = reparse(&d).unwrap();
        assert_eq!(c.sensors.len(), 4);
        assert!(c
            .package_set("arducam-libcamera")
            .unwrap()
            .targets
            .is_empty());
    }

    #[test]
    fn a_malformed_entry_costs_only_itself() {
        let edits: [Edit; 10] = [
            Box::new(|d: &mut Value| d["sensors"][2]["overlay"]["defaultRotation"] = 90.into()),
            Box::new(|d: &mut Value| d["sensors"][2]["overlay"]["name"] = "IMX708".into()),
            Box::new(|d: &mut Value| d["sensors"][2]["overlay"]["name"] = "imx708,cam0".into()),
            Box::new(|d: &mut Value| d["sensors"][2]["overlay"]["canFlip"] = "yes".into()),
            Box::new(|d: &mut Value| d["sensors"][2]["libcameraNames"] = serde_json::json!([])),
            Box::new(|d: &mut Value| {
                d["sensors"][2]["overlay"]["fixedParams"] = serde_json::json!(["a b"])
            }),
            Box::new(|d: &mut Value| {
                d["sensors"][2]["options"][0]["values"] = serde_json::json!(["450"])
            }),
            // A parameter the camera line owner writes itself is never a
            // catalog parameter (the helper's allow-list would carry it).
            Box::new(|d: &mut Value| {
                d["sensors"][2]["overlay"]["fixedParams"] = serde_json::json!(["cam0=on"])
            }),
            Box::new(|d: &mut Value| d["sensors"][2]["options"][0]["param"] = "rotation-x".into()),
            Box::new(|d: &mut Value| d["sensors"][2]["options"][0]["param"] = "cam1".into()),
        ];
        for (i, edit) in edits.iter().enumerate() {
            let mut d = doc();
            edit(&mut d);
            let c = reparse(&d).unwrap();
            assert_eq!(c.sensors.len(), 3, "edit {i}");
            assert_eq!(c.skipped.len(), 1, "edit {i}");
            assert_eq!(c.skipped[0].0, "imx708", "edit {i}");
        }
        // A second entry of an id or an overlay is skipped; the first stays.
        let mut d = doc();
        let dup = d["sensors"][0].clone();
        d["sensors"].as_array_mut().unwrap().push(dup);
        let c = reparse(&d).unwrap();
        assert_eq!(c.sensors.len(), 4);
        assert_eq!(c.skipped[0].1, "duplicate id or overlay name");
    }

    /// The parameter names of an entry: fixed parameters first, then the
    /// options, each once. The helper's allow-list is rendered from them.
    #[test]
    fn param_names_and_the_allow_list_text() {
        let mut d = doc();
        d["sensors"][0]["overlay"]["fixedParams"] =
            serde_json::json!(["4lane", "clock-frequency=74250000", "media-controller=0"]);
        let c = reparse(&d).unwrap();
        assert_eq!(
            c.by_id("imx290").unwrap().param_names(),
            ["4lane", "clock-frequency", "media-controller"]
        );
        assert!(c.helper_allow_list().contains(
            "\nimx290 4lane clock-frequency media-controller\nimx219\nimx708 link-frequency\n"
        ));
        assert!(catalog()
            .helper_allow_list()
            .ends_with("# catalogVersion 3\nimx290 clock-frequency\nimx219\nimx708 link-frequency\nimx335 link-frequency\n"));
        assert!(reserved_param_name("cam0") && reserved_param_name("cam1"));
        assert!(reserved_param_name("rotation") && reserved_param_name("rotation-x"));
        assert!(!reserved_param_name("cam0-reg") && !reserved_param_name("orientation"));
    }

    /// Rule 3: a value the entry does not list is rejected.
    #[test]
    fn an_option_value_must_be_listed() {
        let s = catalog().by_id("imx290").unwrap();
        assert_eq!(
            s.option_token("clockFrequency", 74_250_000).unwrap(),
            "clock-frequency=74250000"
        );
        assert_eq!(
            s.option_token("clockFrequency", 24_000_000),
            Err(OptionError::ValueNotListed {
                key: "clockFrequency".into(),
                value: 24_000_000
            })
        );
        assert_eq!(
            s.option_token("linkFrequency", 450_000_000),
            Err(OptionError::UnknownKey("linkFrequency".into()))
        );
        assert_eq!(
            catalog()
                .by_id("imx708")
                .unwrap()
                .option_token("linkFrequency", 447_000_000)
                .unwrap(),
            "link-frequency=447000000"
        );
    }

    #[test]
    fn token_charsets() {
        for ok in ["imx290", "ov5647", "a", "imx708-x_1", "4lane"] {
            assert!(valid_overlay_name(ok), "{ok}");
        }
        for bad in [
            "",
            "IMX290",
            "imx 290",
            "imx290,cam0",
            "../x",
            "-x",
            "_x",
            "imx290\n",
            "é",
            "abcdefghijklmnopqrstuvwxyz0123456",
        ] {
            assert!(!valid_overlay_name(bad), "{bad:?}");
        }
        for ok in [
            "cam0",
            "4lane",
            "rotation=180",
            "clock-frequency=74250000",
            "media-controller=0",
            "vcm=off",
            "x=1.5",
            "x=+1",
        ] {
            assert!(valid_overlay_param(ok), "{ok}");
        }
        for bad in [
            "",
            "=1",
            "rotation=",
            "a=b=c",
            "a=b c",
            "a=b,c",
            "A=1",
            "a=1\n",
            "a=$(x)",
            "a=`x`",
            "a=;",
            "a=#",
            "a=[all]",
        ] {
            assert!(!valid_overlay_param(bad), "{bad:?}");
        }
    }
}
