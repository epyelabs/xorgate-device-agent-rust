//! The `cameraSensors` namespace against the sensor catalog and config.txt:
//! everything PURE about "the sensor is a setting of the device, per port"
//! (support-other-camera-models plan, Phase 2; RUST-ONLY, no TS
//! counterpart).
//!
//! - [`wanted`]: what one entry of the namespace asks for, checked against
//!   the compiled-in catalog. A sensor id or an option this build's catalog
//!   lacks, and an entry of the wrong type, are a [`Refusal`]: that port is
//!   reported as refused and config.txt is left alone.
//! - [`line_op`]: the ONE helper call that makes a port's camera line the
//!   declared one, or `None` when the line already says exactly that. Used
//!   by the agent's reconciler and by `xorgate-rs camera apply` (the
//!   installer), so there is one implementation of "which line".
//! - [`BootLines`]: each port's camera line as the agent first saw it in
//!   this boot, to tell "written, not booted yet" from "in force".
//! - [`missing_requirements`]: the catalog's OS and package requirements
//!   against what the device has.
//! - [`apply_plan`]: the installer's choice per port, read from a claim
//!   response.
//!
//! WHICH LINE, AND WHEN IT IS WRITTEN ([`line_op`]). The rule is: NEVER
//! LOSE A HAND-SET PARAMETER SILENTLY.
//!
//! The port's line names the SAME overlay as the declared sensor:
//! - an option the entry does NOT name is "as found": whatever the line
//!   has for it stays (`{"sensor": "imx290"}` on
//!   `dtoverlay=imx290,cam1,clock-frequency=74250000` writes nothing);
//! - an option the entry names is enforced, its default included (a line
//!   without the token already gives the default, so that is no write);
//! - the rotation is enforced only when `cameraMount` manages the port and
//!   the sensor can flip; otherwise it is as found;
//! - nothing to enforce differs, one line, in a section that applies: NO
//!   call;
//! - only the rotation differs: `rotate`, in place (it keeps every token);
//! - a named option differs (or the port has several lines): one `set`
//!   with the named options plus the line's own un-named catalog options;
//! - but when that `set` would drop a token the catalog does not list for
//!   the overlay (`4lane`, `mono`, `media-controller=…`), nothing is
//!   written: the port is refused with a sentence that names the token.
//!
//! The port's line names ANOTHER overlay (a sensor change), or the port has
//! no line: one `set` with the catalog's fixed parameters and the named
//! options; un-named options are left out (the overlay's defaults). The
//! old line's tokens belong to the old sensor and go with it.
//!
//! `none`: `remove` when the port has a line, else nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::boot_config::{
    camera_lines_for_port, line_rotation, parse_scoped_lines, rotation_for_mount, Board, CamCfgOp,
    CamPort, LineScope, OverlayLine, ScopedLine,
};
use crate::camera_catalog::{reserved_param_name, Catalog, OptionError, Sensor};
use crate::js;
use crate::settings::{camera_sensor_choice, clip, CameraSensorChoice, CAMERA_SENSOR_NONE};

// ---------------------------------------------------------------------------
// what an entry asks for
// ---------------------------------------------------------------------------

/// What a port's entry asks config.txt to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wanted<'c> {
    /// `"sensor": "none"`: the port has no camera line.
    Empty,
    /// The declared sensor's line.
    Line {
        sensor: &'c Sensor,
        /// The options the entry NAMES, in the catalog's order, as
        /// `(parameter name, token)`: `("clock-frequency",
        /// "clock-frequency=74250000")`. An option the entry does not name
        /// is not here: on a line of the same sensor it is left as found.
        named: Vec<(String, String)>,
    },
}

impl Wanted<'_> {
    /// The sensor id as the setting names it (`none` included).
    pub fn id(&self) -> &str {
        match self {
            Wanted::Empty => CAMERA_SENSOR_NONE,
            Wanted::Line { sensor, .. } => &sensor.id,
        }
    }
}

/// Why an entry cannot be applied. `Display` completes "<key>: …".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The entry is not `{ "sensor": "<id>", "options"?: {…} }`.
    Malformed(String),
    /// The compiled-in catalog has no such sensor (a newer backend).
    UnknownSensor { id: String, known: Vec<String> },
    /// The sensor has no such option in this build, or not that value.
    Option(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Malformed(why) => {
                write!(f, "the camera sensor setting of this port {why}; config.txt is left as it is")
            }
            Refusal::UnknownSensor { id, known } => write!(
                f,
                "this agent build does not know the camera sensor \"{id}\" (it knows: {}); update the agent or choose another sensor. config.txt is left as it is",
                known.join(", ")
            ),
            Refusal::Option(why) => write!(f, "{why}; config.txt is left as it is"),
        }
    }
}

/// The sensor id an entry names, when it names one (whatever else is wrong
/// with it): what the per-port report shows as declared.
pub fn declared_id(entry: &Value) -> Option<String> {
    entry
        .get("sensor")
        .and_then(Value::as_str)
        .map(clip)
        .filter(|s| !s.is_empty())
}

/// Read one `cameraSensors.cameras.<key>` entry against the catalog.
pub fn wanted<'c>(entry: &Value, catalog: &'c Catalog) -> Result<Wanted<'c>, Refusal> {
    let (id, options) = match camera_sensor_choice(entry).map_err(Refusal::Malformed)? {
        CameraSensorChoice::Empty => return Ok(Wanted::Empty),
        CameraSensorChoice::Sensor { id, options } => (id, options),
    };
    let Some(sensor) = catalog.by_id(&id) else {
        return Err(Refusal::UnknownSensor {
            id: clip(&id),
            known: catalog.sensors.iter().map(|s| s.id.clone()).collect(),
        });
    };
    let mut named: Vec<(String, String)> = Vec::new();
    // The catalog's order, so the same choice always renders the same line.
    for opt in &sensor.options {
        let Some((_, value)) = options.iter().find(|(k, _)| *k == opt.key) else {
            continue;
        };
        let listed = if value.fract() == 0.0 && value.abs() < 9e15 {
            sensor.option_token(&opt.key, *value as i64)
        } else {
            Err(OptionError::ValueNotListed {
                key: opt.key.clone(),
                value: *value as i64,
            })
        };
        match listed {
            Ok(token) => named.push((opt.param.clone(), token)),
            Err(_) => {
                return Err(Refusal::Option(format!(
                    "option \"{}\" of the {} sensor must be one of {} (got {})",
                    opt.key,
                    sensor.id,
                    opt.values
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    js::number_to_string(*value)
                )))
            }
        }
    }
    if let Some((key, _)) = options
        .iter()
        .find(|(k, _)| !sensor.options.iter().any(|o| o.key == *k))
    {
        let has = if sensor.options.is_empty() {
            "it has no options".to_string()
        } else {
            format!(
                "its options: {}",
                sensor
                    .options
                    .iter()
                    .map(|o| o.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(Refusal::Option(format!(
            "the {} sensor has no option \"{}\" in this agent build ({has})",
            sensor.id,
            clip(key)
        )));
    }
    Ok(Wanted::Line { sensor, named })
}

// ---------------------------------------------------------------------------
// the form of a line
// ---------------------------------------------------------------------------

/// What one camera line says, in a form two spellings of the same line
/// share: the overlay, its parameters as a sorted set (port tokens and
/// `rotation` left out; for a catalog overlay an option at its default
/// value left out too), and the rotation it gives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineForm {
    pub overlay: String,
    pub params: Vec<String>,
    /// The device-tree rotation the line gives (`"0"`, `"180"`) when the
    /// agent can tell: its token, else the catalog overlay's default.
    /// Otherwise the raw `rotation=` tokens (`""` when it has none).
    pub rotation: String,
}

/// Is `token` an option of `sensor` at its default value?
fn is_default_option(sensor: &Sensor, token: &str) -> bool {
    sensor
        .options
        .iter()
        .any(|o| token == format!("{}={}", o.param, o.default))
}

fn params_form(params: &[String], sensor: Option<&Sensor>) -> Vec<String> {
    let mut out: Vec<String> = params
        .iter()
        .filter(|p| !reserved_param_name(p.split('=').next().unwrap_or(p)))
        .filter(|p| !sensor.is_some_and(|s| is_default_option(s, p)))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The [`LineForm`] of a line.
pub fn line_form(line: &OverlayLine, catalog: &Catalog) -> LineForm {
    let sensor = catalog.by_overlay(&line.name);
    let rotation = match sensor.and_then(|s| line_rotation(line, s.default_rotation)) {
        Some(r) => r.to_string(),
        None => line
            .params
            .iter()
            .filter(|p| p.starts_with("rotation"))
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
    };
    LineForm {
        overlay: line.name.clone(),
        params: params_form(&line.params, sensor),
        rotation,
    }
}

// ---------------------------------------------------------------------------
// the one helper call
// ---------------------------------------------------------------------------

/// The port's camera lines that may take effect on `board`, in file order.
pub fn port_lines<'a>(
    scoped: &'a [ScopedLine],
    port: CamPort,
    catalog: &Catalog,
) -> Vec<&'a ScopedLine> {
    camera_lines_for_port(scoped, port, catalog)
}

/// The name of an overlay parameter token (`clock-frequency=74250000` →
/// `clock-frequency`).
fn token_name(token: &str) -> &str {
    token.split('=').next().unwrap_or(token)
}

/// The tokens of a line that are not the port token or a rotation.
fn own_tokens(line: &OverlayLine) -> Vec<&String> {
    line.params
        .iter()
        .filter(|p| !reserved_param_name(token_name(p)))
        .collect()
}

/// The tokens of `line` whose NAME the catalog does not list for `sensor`:
/// what a `set` would drop.
pub fn unlisted_tokens<'a>(line: &'a OverlayLine, sensor: &Sensor) -> Vec<&'a String> {
    let listed = sensor.param_names();
    own_tokens(line)
        .into_iter()
        .filter(|p| !listed.contains(&token_name(p)))
        .collect()
}

/// A config.txt token as it may appear in a sentence: at most 48
/// characters of `[A-Za-z0-9_.+=-]` (anything else becomes `?`).
fn clip_token(s: &str) -> String {
    s.chars()
        .take(48)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '=' | '+') {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// The one helper call that makes `port`'s camera line the declared one.
///
/// - `Ok(None)`: config.txt already says it; nothing is written.
/// - `Ok(Some(op))`: one call (`rotate`, `set` or `remove`; module docs).
/// - `Err(sentence)`: a write is needed and would drop a hand-set token the
///   catalog does not list. Nothing is written; the sentence names it.
///
/// `mount` is the `cameraMount` rotation of the port when that namespace
/// manages it. With `None` the rotation is not the agent's to choose: the
/// line's own rotation stays, and a new line gets the overlay's default.
/// A sensor that cannot flip is never rewritten for its rotation.
pub fn line_op(
    text: &str,
    board: Board,
    port: CamPort,
    want: &Wanted<'_>,
    mount: Option<u16>,
    catalog: &Catalog,
) -> Result<Option<CamCfgOp>, String> {
    let scoped = parse_scoped_lines(text, board);
    let lines = port_lines(&scoped, port, catalog);
    let (sensor, named) = match want {
        Wanted::Empty => {
            return Ok((!lines.is_empty()).then_some(CamCfgOp::Remove { port }));
        }
        Wanted::Line { sensor, named } => (*sensor, named),
    };
    // The rotation to enforce; `None` = as found.
    let enforced = mount
        .filter(|_| sensor.can_flip)
        .map(|m| rotation_for_mount(sensor.default_rotation, m));
    let named_tokens = || named.iter().map(|(_, t)| t.clone());
    let Some(last) = lines.last().filter(|l| l.line.name == sensor.overlay) else {
        // No line, or a line of another overlay (a sensor change): the
        // declared line replaces whatever is there.
        return Ok(Some(CamCfgOp::Set {
            port,
            overlay: sensor.overlay.clone(),
            params: sensor
                .fixed_params
                .iter()
                .cloned()
                .chain(named_tokens())
                .collect(),
            rotation: enforced.unwrap_or(sensor.default_rotation),
        }));
    };
    let line = &last.line;
    let own = own_tokens(line);
    // What the line gives for one catalog parameter: its token, else the
    // option's default.
    let gives = |param: &str| -> Option<String> {
        own.iter()
            .find(|t| token_name(t) == param)
            .map(|t| (*t).clone())
            .or_else(|| {
                sensor
                    .options
                    .iter()
                    .find(|o| o.param == param)
                    .map(|o| format!("{}={}", o.param, o.default))
            })
    };
    let named_ok = named
        .iter()
        .all(|(param, token)| gives(param).as_deref() == Some(token.as_str()));
    let fixed_ok = sensor.fixed_params.iter().all(|f| own.contains(&f));
    let rotation_now = line_rotation(line, sensor.default_rotation);
    let rotation_ok = enforced.is_none_or(|r| rotation_now == Some(r));
    let one_applying = lines.len() == 1 && last.scope == LineScope::Applies;
    if one_applying && named_ok && fixed_ok {
        return Ok(match enforced {
            Some(rotation) if !rotation_ok => Some(CamCfgOp::Rotate {
                port,
                overlay: sensor.overlay.clone(),
                rotation,
            }),
            _ => None,
        });
    }
    // A `set` is needed. It writes only what the catalog lists, so a token
    // it does not list would be lost: refuse, and say which.
    let unlisted = unlisted_tokens(line, sensor);
    if !unlisted.is_empty() {
        let tokens = unlisted
            .iter()
            .map(|t| format!("\"{}\"", clip_token(t)))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "the camera line for {} holds {tokens}, which the sensor catalog does not list for {}: rewriting the line would drop it, so the line is left as it is. Edit the line by hand, or remove {tokens} from it",
            port.token(),
            sensor.overlay
        ));
    }
    // The named options, plus the line's own un-named catalog options.
    let mut params: Vec<String> = sensor.fixed_params.clone();
    for opt in &sensor.options {
        match named.iter().find(|(param, _)| *param == opt.param) {
            Some((_, token)) => params.push(token.clone()),
            None => params.extend(
                own.iter()
                    .find(|t| token_name(t) == opt.param)
                    .map(|t| (*t).clone()),
            ),
        }
    }
    Ok(Some(CamCfgOp::Set {
        port,
        overlay: sensor.overlay.clone(),
        params,
        rotation: enforced.or(rotation_now).unwrap_or(sensor.default_rotation),
    }))
}

// ---------------------------------------------------------------------------
// the lines as booted
// ---------------------------------------------------------------------------

/// One port's camera line as first seen in a boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootLine {
    pub form: LineForm,
    /// The line as written (trimmed, at most 200 characters), for sentences.
    pub text: String,
}

/// Each camera port's line (`cam0`, `cam1`; `None` = no line) as the agent
/// first saw it in the boot `boot_id`. With several lines on a port, the
/// LAST (the one the kernel takes).
///
/// The device tree a kernel runs with is fixed at boot, so a port whose
/// line differs now has a change that waits for a reboot, whoever made it
/// and whatever it changed (the overlay, an option, the rotation). What
/// this cannot see: an edit made in this boot BEFORE the agent first ran in
/// it. An overlay swapped that way is still found by the live device tree
/// (`camera_truth`); an option or rotation changed that way is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootLines {
    pub boot_id: String,
    pub ports: BTreeMap<String, Option<BootLine>>,
}

/// The port's line in force (the last that may apply), as a [`BootLine`].
pub fn port_boot_line(scoped: &[ScopedLine], port: CamPort, catalog: &Catalog) -> Option<BootLine> {
    port_lines(scoped, port, catalog).last().map(|l| BootLine {
        form: line_form(&l.line, catalog),
        text: js::trim(&l.line.text).chars().take(200).collect(),
    })
}

/// Take the snapshot of a config.txt.
pub fn snapshot_boot_lines(
    boot_id: &str,
    text: &str,
    board: Board,
    catalog: &Catalog,
) -> BootLines {
    let scoped = parse_scoped_lines(text, board);
    BootLines {
        boot_id: boot_id.to_string(),
        ports: [CamPort::Cam0, CamPort::Cam1]
            .into_iter()
            .map(|p| (p.token().to_string(), port_boot_line(&scoped, p, catalog)))
            .collect(),
    }
}

/// A port's line changed since the boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineChange {
    /// The line the device booted with (`None` = it had none).
    pub booted: Option<String>,
    /// The line now (`None` = removed).
    pub now: Option<String>,
    /// The overlay itself changed (or the line appeared or went).
    pub overlay: bool,
}

/// Has `port`'s line changed since the snapshot was taken? `None` = no, or
/// the snapshot has no word on that port.
pub fn changed_since_boot(
    booted: &BootLines,
    text: &str,
    board: Board,
    port: CamPort,
    catalog: &Catalog,
) -> Option<LineChange> {
    let was = booted.ports.get(port.token())?;
    let scoped = parse_scoped_lines(text, board);
    let now = port_boot_line(&scoped, port, catalog);
    if was.as_ref().map(|l| &l.form) == now.as_ref().map(|l| &l.form) {
        return None;
    }
    Some(LineChange {
        overlay: was.as_ref().map(|l| &l.form.overlay) != now.as_ref().map(|l| &l.form.overlay),
        booted: was.as_ref().map(|l| l.text.clone()),
        now: now.map(|l| l.text),
    })
}

// ---------------------------------------------------------------------------
// requirements
// ---------------------------------------------------------------------------

/// What the device has, as far as the agent could find out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostFacts {
    /// `VERSION_CODENAME` of `/etc/os-release` (`bookworm`).
    pub os: Option<String>,
    /// `dpkg --print-architecture` (`arm64`).
    pub arch: Option<String>,
    /// The installed version of each package asked about (`dpkg-query`,
    /// status `ii` only). `None` = `dpkg-query` could not be run.
    pub packages: Option<BTreeMap<String, String>>,
}

/// `dpkg-query -W -f='${Package}\t${Version}\t${db:Status-Abbrev}\n'`
/// output → the packages that are installed (`ii`), with their versions.
pub fn parse_dpkg_query(stdout: &str) -> BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (name, version, status) = (f.next()?, f.next()?, f.next()?);
            (status.trim_start().starts_with("ii") && !version.is_empty())
                .then(|| (name.to_string(), version.to_string()))
        })
        .collect()
}

/// `VERSION_CODENAME=bookworm` out of an `/etc/os-release`.
pub fn os_codename(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|l| {
        let v = l.trim().strip_prefix("VERSION_CODENAME=")?;
        let v = v.trim_matches(|c| c == '"' || c == '\'');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// The package names a sensor's requirements ask about on any target (what
/// to hand to `dpkg-query`).
pub fn required_package_names(sensor: &Sensor, catalog: &Catalog) -> Vec<String> {
    let mut out: Vec<String> = sensor
        .package_set
        .as_deref()
        .and_then(|id| catalog.package_set(id))
        .into_iter()
        .flat_map(|set| set.targets.iter())
        .flat_map(|t| t.packages.iter().map(|p| p.name.clone()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What `sensor` needs that the device does not have, as the tail of a
/// sentence ("<port> is set to <sensor>, which ….", without the final full
/// stop), or `None` when nothing
/// is known to be missing. A fact the agent could not find out (no
/// `os-release`, no `dpkg-query`) is never taken for a missing requirement.
pub fn missing_requirements(
    sensor: &Sensor,
    catalog: &Catalog,
    host: &HostFacts,
) -> Option<String> {
    if let Some(os) = host.os.as_deref() {
        if !sensor.requires_os.is_empty() && !sensor.requires_os.iter().any(|o| o == os) {
            return Some(format!(
                "needs an OS release this device does not run (needs {}; this device runs {os}). The camera cannot work on this OS release",
                sensor.requires_os.join(" or ")
            ));
        }
    }
    let set = catalog.package_set(sensor.package_set.as_deref()?)?;
    let installed = host.packages.as_ref()?;
    let (os, arch) = (host.os.as_deref()?, host.arch.as_deref()?);
    let Some(target) = set.targets.iter().find(|t| t.os == os && t.arch == arch) else {
        return Some(format!(
            "needs extra packages ({}) that the catalog does not list for {os}/{arch}. The camera cannot work on this device",
            set.label
        ));
    };
    let missing: Vec<String> = target
        .packages
        .iter()
        .filter_map(|p| match installed.get(&p.name) {
            Some(v) if *v == p.version => None,
            Some(v) => Some(format!("{} {} (installed: {v})", p.name, p.version)),
            None => Some(format!("{} {} (not installed)", p.name, p.version)),
        })
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "needs extra packages that are not installed in the required versions: {}. The camera will not appear until they are installed",
            missing.join(", ")
        )
    })
}

// ---------------------------------------------------------------------------
// the installer's plan (`xorgate-rs camera apply --from-claim`)
// ---------------------------------------------------------------------------

/// Where a port's choice came from in the claim response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceSource {
    /// `cameraSensors.cameras.<key>`: the choice made at registration (or
    /// the model default the backend seeded).
    Setting,
    /// `hardware.video[].sensor`: the model's sensor (a backend that does
    /// not seed the setting).
    Model,
    /// Neither: the two `imx290` lines every installer wrote before.
    Default,
}

impl ChoiceSource {
    pub fn words(self) -> &'static str {
        match self {
            ChoiceSource::Setting => "the camera sensor setting",
            ChoiceSource::Model => "the device model",
            ChoiceSource::Default => "the default",
        }
    }
}

/// One port of the installer's plan.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyPort {
    pub stream_key: String,
    pub source: ChoiceSource,
    /// The port, or why the key has none on this device.
    pub port: Result<CamPort, String>,
    /// The entry to apply (`{"sensor": …, "options"?: …}`).
    pub entry: Value,
}

/// `camN` for a connector `csiN`; a `camN` stream key stands for itself
/// when the claim carries no hardware block.
fn port_of(key: &str, connector: Option<&str>) -> Result<CamPort, String> {
    let token = match connector {
        Some(c) => crate::boot_config::overlay_cam_token(c),
        None => Some(key.to_string()),
    };
    token
        .as_deref()
        .and_then(CamPort::from_token)
        .ok_or_else(|| match connector {
            Some(c) => format!(
                "its connector \"{}\" is not a camera port a camera line can name (csi0, csi1)",
                clip(c)
            ),
            None => {
                "the claim carries no hardware block, and the key is not cam0 or cam1".to_string()
            }
        })
}

/// The installer's plan out of a claim response, in this order of
/// preference:
///
/// 1. `cameraSensors.cameras` when the response carries it: its entries,
///    each on the port its stream key has in `hardware.video[]`. A port of
///    the hardware block the setting leaves out is unmanaged: not in the
///    plan, its line is left as found.
/// 2. else `hardware.video[]`: each camera's `sensor`, as the model names it.
/// 3. else the two `imx290` lines on `cam0` and `cam1`.
pub fn apply_plan(claim: &Value) -> Vec<ApplyPort> {
    // `(stream key, connector, sensor)`. An entry without a `key` is named
    // by its port (`cam0` for `csi0`), as every installer named it before.
    let video: Vec<(String, &str, Option<&str>)> = claim
        .get("hardware")
        .and_then(|h| h.get("video"))
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(|e| {
                    let connector = e.get("connector")?.as_str()?;
                    let key = match e.get("key").and_then(Value::as_str) {
                        Some(k) => k.to_string(),
                        None => crate::boot_config::overlay_cam_token(connector)?,
                    };
                    Some((key, connector, e.get("sensor").and_then(Value::as_str)))
                })
                .collect()
        })
        .unwrap_or_default();
    let has_hardware = !video.is_empty();
    if let Some(cameras) = claim
        .get("cameraSensors")
        .and_then(|c| c.get("cameras"))
        .and_then(Value::as_object)
    {
        return cameras
            .iter()
            .map(|(key, entry)| {
                let connector = video.iter().find(|(k, _, _)| k == key).map(|(_, c, _)| *c);
                let port = if has_hardware && connector.is_none() {
                    Err("the device model declares no camera by that stream key".to_string())
                } else {
                    port_of(key, connector)
                };
                ApplyPort {
                    stream_key: key.clone(),
                    source: ChoiceSource::Setting,
                    port,
                    entry: entry.clone(),
                }
            })
            .collect();
    }
    let from_model: Vec<ApplyPort> = video
        .iter()
        .filter_map(|(key, connector, sensor)| {
            let sensor = sensor.filter(|s| !s.is_empty())?;
            Some(ApplyPort {
                stream_key: key.clone(),
                source: ChoiceSource::Model,
                port: port_of(key, Some(connector)),
                entry: serde_json::json!({ "sensor": sensor }),
            })
        })
        .collect();
    if !from_model.is_empty() {
        return from_model;
    }
    ["cam0", "cam1"]
        .into_iter()
        .map(|key| ApplyPort {
            stream_key: key.to_string(),
            source: ChoiceSource::Default,
            port: port_of(key, None),
            entry: serde_json::json!({ "sensor": "imx290" }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_catalog::catalog;
    use serde_json::json;

    const BENCH1: &str =
        "[all]\ncamera_auto_detect=0\ndtoverlay=imx290,cam0\ndtoverlay=imx290,cam1\n";

    fn try_op(
        text: &str,
        port: CamPort,
        entry: Value,
        mount: Option<u16>,
    ) -> Result<Option<Vec<String>>, String> {
        let c = catalog();
        let w = wanted(&entry, c).expect("a usable entry");
        line_op(text, Board::Cm5, port, &w, mount, c).map(|o| o.map(|o| o.args()))
    }

    fn op(text: &str, port: CamPort, entry: Value, mount: Option<u16>) -> Option<Vec<String>> {
        try_op(text, port, entry, mount).expect("not refused")
    }

    fn args(a: &[&str]) -> Option<Vec<String>> {
        Some(a.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn an_entry_against_the_catalog() {
        let c = catalog();
        assert_eq!(wanted(&json!({"sensor": "none"}), c), Ok(Wanted::Empty));
        let w = wanted(
            &json!({"sensor": "imx290", "options": {"clockFrequency": 74250000}, "future": 1}),
            c,
        )
        .unwrap();
        let Wanted::Line { sensor, named } = &w else {
            panic!()
        };
        assert_eq!(sensor.id, "imx290");
        assert_eq!(
            named,
            &[(
                "clock-frequency".to_string(),
                "clock-frequency=74250000".to_string()
            )]
        );
        assert_eq!(w.id(), "imx290");
        // Refusals, as the status prints them.
        let say = |e: Value| wanted(&e, c).unwrap_err().to_string();
        assert_eq!(
            say(json!({"sensor": "imx999"})),
            "this agent build does not know the camera sensor \"imx999\" (it knows: imx290, imx219, imx708, imx335); update the agent or choose another sensor. config.txt is left as it is"
        );
        assert_eq!(
            say(json!({"sensor": "imx290", "options": {"clockFrequency": 24000000}})),
            "option \"clockFrequency\" of the imx290 sensor must be one of 37125000, 74250000 (got 24000000); config.txt is left as it is"
        );
        assert_eq!(
            say(json!({"sensor": "imx290", "options": {"clockFrequency": 37125000.5}})),
            "option \"clockFrequency\" of the imx290 sensor must be one of 37125000, 74250000 (got 37125000.5); config.txt is left as it is"
        );
        assert_eq!(
            say(json!({"sensor": "imx219", "options": {"lanes": 4}})),
            "the imx219 sensor has no option \"lanes\" in this agent build (it has no options); config.txt is left as it is"
        );
        assert_eq!(
            say(json!({"sensor": "imx708", "options": {"lanes": 4}})),
            "the imx708 sensor has no option \"lanes\" in this agent build (its options: linkFrequency); config.txt is left as it is"
        );
        assert_eq!(
            say(json!("imx290")),
            "the camera sensor setting of this port is not an object; config.txt is left as it is"
        );
        assert_eq!(
            say(json!({"sensor": 5})),
            "the camera sensor setting of this port has no \"sensor\" name; config.txt is left as it is"
        );
        assert_eq!(
            say(json!({"sensor": "none", "options": {"a": 1}})),
            "the camera sensor setting of this port declares the port empty (\"none\") and still carries options; config.txt is left as it is"
        );
        assert_eq!(
            say(json!({"sensor": "imx290", "options": []})),
            "the camera sensor setting of this port has \"options\" that is not an object; config.txt is left as it is"
        );
        assert_eq!(
            say(json!({"sensor": "imx290", "options": {"clockFrequency": "x"}})),
            "the camera sensor setting of this port has an option \"clockFrequency\" that is not a number; config.txt is left as it is"
        );
        // What the report shows as declared, however broken the entry.
        assert_eq!(
            declared_id(&json!({"sensor": "imx999"})).as_deref(),
            Some("imx999")
        );
        assert_eq!(declared_id(&json!({"sensor": 5})), None);
        assert_eq!(declared_id(&json!(7)), None);
        assert_eq!(
            declared_id(&json!({"sensor": "a b\n\"c"})).as_deref(),
            Some("a?b??c")
        );
    }

    #[test]
    fn a_line_that_already_says_it_is_not_written() {
        // Every bench: the line as an installer wrote it, the sensor's own entry.
        for port in [CamPort::Cam0, CamPort::Cam1] {
            assert_eq!(op(BENCH1, port, json!({"sensor": "imx290"}), None), None);
            assert_eq!(op(BENCH1, port, json!({"sensor": "imx290"}), Some(0)), None);
            // A NAMED option at its default: the line without the token
            // already gives it.
            assert_eq!(
                op(
                    BENCH1,
                    port,
                    json!({"sensor": "imx290", "options": {"clockFrequency": 37125000}}),
                    None
                ),
                None
            );
        }
        let cm4 = "camera_auto_detect=0\n[all]\ndtoverlay=imx708,cam0,rotation=180\n";
        assert_eq!(
            op(cm4, CamPort::Cam0, json!({"sensor": "imx708"}), None),
            None
        );
        assert_eq!(
            op(cm4, CamPort::Cam0, json!({"sensor": "imx708"}), Some(0)),
            None
        );
        let bench2 =
            "camera_auto_detect=0\ndtoverlay=imx335,cam0\ndtoverlay=imx219,cam1,rotation=180\n";
        assert_eq!(
            op(bench2, CamPort::Cam0, json!({"sensor": "imx335"}), None),
            None
        );
        // A sensor that cannot flip is not rewritten for a mount it cannot take.
        assert_eq!(
            op(
                bench2,
                CamPort::Cam0,
                json!({"sensor": "imx335"}),
                Some(180)
            ),
            None
        );
        assert_eq!(
            op(bench2, CamPort::Cam1, json!({"sensor": "imx219"}), Some(0)),
            None
        );
        // A port declared empty that has no line.
        assert_eq!(
            op(cm4, CamPort::Cam1, json!({"sensor": "none"}), None),
            None
        );
        // Token order and an explicit default rotation do not matter.
        let spelled = "dtoverlay=imx290,clock-frequency=74250000,cam0,rotation=0\n";
        assert_eq!(
            op(
                spelled,
                CamPort::Cam0,
                json!({"sensor": "imx290", "options": {"clockFrequency": 74250000}}),
                Some(0)
            ),
            None
        );
    }

    /// NEVER LOSE A HAND-SET PARAMETER SILENTLY: on a line of the same
    /// sensor, what the entry does not name is as found.
    #[test]
    fn what_the_entry_does_not_name_is_as_found() {
        // An option the entry does not name: kept, no write (a 74.25 MHz
        // board keeps its camera when the port becomes managed).
        let hand = "dtoverlay=imx290,cam1,clock-frequency=74250000\n";
        assert_eq!(
            op(hand, CamPort::Cam1, json!({"sensor": "imx290"}), None),
            None
        );
        assert_eq!(
            op(hand, CamPort::Cam1, json!({"sensor": "imx290"}), Some(0)),
            None
        );
        // ... and the same option NAMED is enforced, its default included.
        assert_eq!(
            op(
                hand,
                CamPort::Cam1,
                json!({"sensor": "imx290", "options": {"clockFrequency": 37125000}}),
                None
            ),
            args(&[
                "set",
                "cam1",
                "imx290",
                "clock-frequency=37125000",
                "rotation=0"
            ])
        );
        // A token the catalog does not list, and nothing to write: applied.
        let lanes = "dtoverlay=imx290,cam0,4lane\n";
        assert_eq!(
            op(lanes, CamPort::Cam0, json!({"sensor": "imx290"}), None),
            None
        );
        assert_eq!(
            op(lanes, CamPort::Cam0, json!({"sensor": "imx290"}), Some(0)),
            None
        );
        // Only the rotation differs: `rotate`, in place, which keeps every
        // token (the unlisted one and the un-named option included).
        assert_eq!(
            op(lanes, CamPort::Cam0, json!({"sensor": "imx290"}), Some(180)),
            args(&["rotate", "cam0", "imx290", "180"])
        );
        assert_eq!(
            op(hand, CamPort::Cam1, json!({"sensor": "imx290"}), Some(180)),
            args(&["rotate", "cam1", "imx290", "180"])
        );
        // A named option differs and the line holds only catalog tokens:
        // one `set` with the named option; the rotation the line gives stays.
        let turned = "dtoverlay=imx708,cam0,rotation=0\n";
        assert_eq!(
            op(
                turned,
                CamPort::Cam0,
                json!({"sensor": "imx708", "options": {"linkFrequency": 447000000}}),
                None
            ),
            args(&[
                "set",
                "cam0",
                "imx708",
                "link-frequency=447000000",
                "rotation=0"
            ])
        );
        // A named option differs and the line holds a token the catalog
        // does not list: NOT written, and the sentence names the token.
        assert_eq!(
            try_op(
                lanes,
                CamPort::Cam0,
                json!({"sensor": "imx290", "options": {"clockFrequency": 74250000}}),
                None
            ),
            Err("the camera line for cam0 holds \"4lane\", which the sensor catalog does not list for imx290: rewriting the line would drop it, so the line is left as it is. Edit the line by hand, or remove \"4lane\" from it".to_string())
        );
        // Several of them, and a value form.
        assert_eq!(
            try_op(
                "dtoverlay=imx290,cam0,mono,media-controller=0,clock-frequency=37125000\n",
                CamPort::Cam0,
                json!({"sensor": "imx290", "options": {"clockFrequency": 74250000}}),
                Some(0)
            ),
            Err("the camera line for cam0 holds \"mono\", \"media-controller=0\", which the sensor catalog does not list for imx290: rewriting the line would drop it, so the line is left as it is. Edit the line by hand, or remove \"mono\", \"media-controller=0\" from it".to_string())
        );
        // Two lines on the port, the one in force with an unlisted token:
        // collapsing them would rewrite it, so it is refused too.
        assert!(try_op(
            "dtoverlay=imx219,cam0\ndtoverlay=imx290,cam0,4lane\n",
            CamPort::Cam0,
            json!({"sensor": "imx290"}),
            None
        )
        .is_err());
    }

    #[test]
    fn one_call_makes_the_line() {
        // A sensor change: the declared line replaces the old one (its
        // tokens were the old sensor's), with the new overlay's default
        // rotation and no un-named option.
        assert_eq!(
            op(BENCH1, CamPort::Cam0, json!({"sensor": "imx708"}), None),
            args(&["set", "cam0", "imx708", "rotation=180"])
        );
        assert_eq!(
            op(
                "dtoverlay=imx290,cam0,4lane,clock-frequency=74250000,rotation=180\n",
                CamPort::Cam0,
                json!({"sensor": "imx708"}),
                None
            ),
            args(&["set", "cam0", "imx708", "rotation=180"])
        );
        // ... plus the mount, when cameraMount manages the port.
        assert_eq!(
            op(
                BENCH1,
                CamPort::Cam0,
                json!({"sensor": "imx708"}),
                Some(180)
            ),
            args(&["set", "cam0", "imx708", "rotation=0"])
        );
        // A sensor change with a named option.
        assert_eq!(
            op(
                BENCH1,
                CamPort::Cam1,
                json!({"sensor": "imx335", "options": {"linkFrequency": 445500000}}),
                None
            ),
            args(&[
                "set",
                "cam1",
                "imx335",
                "link-frequency=445500000",
                "rotation=180"
            ])
        );
        // Rotation only, on a clean line: `rotate`.
        assert_eq!(
            op(
                BENCH1,
                CamPort::Cam1,
                json!({"sensor": "imx290"}),
                Some(180)
            ),
            args(&["rotate", "cam1", "imx290", "180"])
        );
        // A named option and the rotation together: one `set`, which keeps
        // the line's own un-named catalog options.
        assert_eq!(
            op(
                "dtoverlay=imx708,cam0,link-frequency=453000000\n",
                CamPort::Cam0,
                json!({"sensor": "imx708"}),
                Some(180)
            ),
            args(&["rotate", "cam0", "imx708", "0"])
        );
        assert_eq!(
            op(
                "dtoverlay=imx708,cam0,link-frequency=453000000,rotation=0\n",
                CamPort::Cam0,
                json!({"sensor": "imx708", "options": {"linkFrequency": 447000000}}),
                Some(0)
            ),
            args(&[
                "set",
                "cam0",
                "imx708",
                "link-frequency=447000000",
                "rotation=180"
            ])
        );
        // A sensor that cannot flip is never written for its rotation; a new
        // line of it gets the overlay's default whatever is asked.
        assert_eq!(
            op(
                BENCH1,
                CamPort::Cam0,
                json!({"sensor": "imx335"}),
                Some(180)
            ),
            args(&["set", "cam0", "imx335", "rotation=180"])
        );
        // No line yet; a port declared empty.
        assert_eq!(
            op(
                "dtparam=audio=on\n",
                CamPort::Cam1,
                json!({"sensor": "imx219"}),
                None
            ),
            args(&["set", "cam1", "imx219", "rotation=180"])
        );
        assert_eq!(
            op(BENCH1, CamPort::Cam1, json!({"sensor": "none"}), None),
            args(&["remove", "cam1"])
        );
        // Two lines on the port: one `set` collapses them into the declared
        // one, keeping the un-named option of the line in force.
        assert_eq!(
            op(
                "dtoverlay=imx290,cam0\ndtoverlay=imx290,cam0,clock-frequency=74250000\n",
                CamPort::Cam0,
                json!({"sensor": "imx290"}),
                None
            ),
            args(&[
                "set",
                "cam0",
                "imx290",
                "clock-frequency=74250000",
                "rotation=0"
            ])
        );
        // A line under a section the agent cannot evaluate is not "settled":
        // the helper is asked, and refuses.
        assert_eq!(
            op(
                "[pi3]\ndtoverlay=imx290,cam0\n",
                CamPort::Cam0,
                json!({"sensor": "imx290"}),
                None
            ),
            args(&["set", "cam0", "imx290", "rotation=0"])
        );
        // A line of the other board family is not this board's line.
        assert_eq!(
            op(
                "[cm4]\ndtoverlay=imx290,cam0\n[all]\n",
                CamPort::Cam0,
                json!({"sensor": "imx708"}),
                None
            ),
            args(&["set", "cam0", "imx708", "rotation=180"])
        );
        // `camera_auto_detect` is not the agent's reason to write: a line
        // that matches beside `camera_auto_detect=1` is left (the installer
        // edits that key itself, and every `set` fixes it on the way).
        assert_eq!(
            op(
                "camera_auto_detect=1\ndtoverlay=imx290,cam0\n",
                CamPort::Cam0,
                json!({"sensor": "imx290"}),
                None
            ),
            None
        );
    }

    #[test]
    fn a_line_changed_since_boot() {
        let c = catalog();
        let boot = snapshot_boot_lines("b1", BENCH1, Board::Cm5, c);
        assert_eq!(boot.ports.len(), 2);
        assert_eq!(
            boot.ports["cam0"].as_ref().unwrap().text,
            "dtoverlay=imx290,cam0"
        );
        let same = |text: &str, port| changed_since_boot(&boot, text, Board::Cm5, port, c);
        assert_eq!(same(BENCH1, CamPort::Cam0), None);
        // Another spelling of the same line is not a change.
        assert_eq!(
            same(
                "dtoverlay=imx290,cam0,rotation=0,clock-frequency=37125000\ndtoverlay=imx290,cam1\n",
                CamPort::Cam0
            ),
            None
        );
        // The rotation, an option, the overlay, the line itself.
        let ch = same("dtoverlay=imx290,cam0,rotation=180\n", CamPort::Cam0).unwrap();
        assert!(!ch.overlay);
        assert_eq!(ch.booted.as_deref(), Some("dtoverlay=imx290,cam0"));
        assert_eq!(
            ch.now.as_deref(),
            Some("dtoverlay=imx290,cam0,rotation=180")
        );
        assert!(
            !same(
                "dtoverlay=imx290,cam0,clock-frequency=74250000\n",
                CamPort::Cam0
            )
            .unwrap()
            .overlay
        );
        assert!(
            same("dtoverlay=imx708,cam0\n", CamPort::Cam0)
                .unwrap()
                .overlay
        );
        let gone = same("dtoverlay=imx290,cam0\n", CamPort::Cam1).unwrap();
        assert!(gone.overlay);
        assert_eq!(gone.now, None);
        // The snapshot round-trips as the file the agent keeps.
        let text = serde_json::to_string(&boot).unwrap();
        assert!(text.starts_with(r#"{"bootId":"b1","ports":{"cam0":{"form":{"overlay":"imx290","params":[],"rotation":"0"},"text":"dtoverlay=imx290,cam0"}"#), "{text}");
        assert_eq!(serde_json::from_str::<BootLines>(&text).unwrap(), boot);
    }

    #[test]
    fn requirements_against_the_host() {
        let c = catalog();
        let imx335 = c.by_id("imx335").unwrap();
        let imx290 = c.by_id("imx290").unwrap();
        assert_eq!(
            required_package_names(imx335, c),
            ["libcamera-ipa", "libcamera0.5"]
        );
        assert!(required_package_names(imx290, c).is_empty());
        let host = |pkgs: &[(&str, &str)]| HostFacts {
            os: Some("bookworm".into()),
            arch: Some("arm64".into()),
            packages: Some(
                pkgs.iter()
                    .map(|(n, v)| (n.to_string(), v.to_string()))
                    .collect(),
            ),
        };
        let ok = host(&[
            ("libcamera0.5", "0.5.2+rpt20250909-1"),
            ("libcamera-ipa", "0.5.2+rpt20250909-1"),
        ]);
        assert_eq!(missing_requirements(imx335, c, &ok), None);
        assert_eq!(missing_requirements(imx290, c, &host(&[])), None);
        assert_eq!(
            missing_requirements(imx335, c, &host(&[("libcamera0.5", "0.5.0+rpt20250429-1")])).as_deref(),
            Some("needs extra packages that are not installed in the required versions: libcamera0.5 0.5.2+rpt20250909-1 (installed: 0.5.0+rpt20250429-1), libcamera-ipa 0.5.2+rpt20250909-1 (not installed). The camera will not appear until they are installed")
        );
        // What could not be found out is not a missing requirement.
        assert_eq!(missing_requirements(imx335, c, &HostFacts::default()), None);
        assert_eq!(
            missing_requirements(
                imx335,
                c,
                &HostFacts {
                    packages: None,
                    ..ok.clone()
                }
            ),
            None
        );
        // Another OS release; another architecture.
        assert_eq!(
            missing_requirements(
                imx290,
                c,
                &HostFacts {
                    os: Some("trixie".into()),
                    ..ok.clone()
                }
            )
            .as_deref(),
            Some("needs an OS release this device does not run (needs bookworm; this device runs trixie). The camera cannot work on this OS release")
        );
        assert_eq!(
            missing_requirements(
                imx335,
                c,
                &HostFacts {
                    arch: Some("armhf".into()),
                    ..ok
                }
            )
            .as_deref(),
            Some("needs extra packages (Arducam's libcamera build (adds the IMX335 camera helper)) that the catalog does not list for bookworm/armhf. The camera cannot work on this device")
        );
        // The two readers.
        assert_eq!(
            parse_dpkg_query(
                "libcamera0.5\t0.5.2+rpt20250909-1\tii \nlibcamera-ipa\t0.5.2\trc \nfoo\t\tun \n"
            ),
            [(
                "libcamera0.5".to_string(),
                "0.5.2+rpt20250909-1".to_string()
            )]
            .into()
        );
        assert_eq!(
            os_codename("PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nVERSION_CODENAME=bookworm\nID=debian\n").as_deref(),
            Some("bookworm")
        );
        assert_eq!(os_codename("ID=debian\n"), None);
    }

    #[test]
    fn the_installers_plan() {
        let hw = json!({"video": [
            {"key": "cam0", "connector": "csi0", "sensor": "imx290"},
            {"key": "cam1", "connector": "csi1", "sensor": "imx290"}
        ]});
        // 1. The setting wins, entry by entry; a port it leaves out is not planned.
        let plan = apply_plan(&json!({
            "hardware": hw,
            "cameraSensors": {"cameras": {"cam1": {"sensor": "none"}, "cam0": {"sensor": "imx708"}, "cam9": {"sensor": "imx219"}}}
        }));
        assert_eq!(
            plan.iter()
                .map(|p| (
                    p.stream_key.as_str(),
                    p.source,
                    p.port.clone().ok(),
                    p.entry["sensor"].as_str().unwrap()
                ))
                .collect::<Vec<_>>(),
            [
                ("cam1", ChoiceSource::Setting, Some(CamPort::Cam1), "none"),
                ("cam0", ChoiceSource::Setting, Some(CamPort::Cam0), "imx708"),
                ("cam9", ChoiceSource::Setting, None, "imx219"),
            ]
        );
        assert_eq!(
            plan[2].port.clone().unwrap_err(),
            "the device model declares no camera by that stream key"
        );
        // 2. No setting: the model's sensors.
        let plan = apply_plan(&json!({"hardware": hw}));
        assert_eq!(plan.len(), 2);
        assert!(plan.iter().all(|p| p.source == ChoiceSource::Model));
        assert_eq!(plan[1].port, Ok(CamPort::Cam1));
        // 3. Neither: the two imx290 lines.
        for claim in [
            json!({}),
            json!({"hardware": {"video": []}}),
            json!({"hardware": null}),
        ] {
            let plan = apply_plan(&claim);
            assert_eq!(
                plan.iter()
                    .map(|p| (p.stream_key.as_str(), p.source, p.port.clone().ok()))
                    .collect::<Vec<_>>(),
                [
                    ("cam0", ChoiceSource::Default, Some(CamPort::Cam0)),
                    ("cam1", ChoiceSource::Default, Some(CamPort::Cam1)),
                ],
                "{claim}"
            );
        }
        // Whatever the source, a line of the same sensor keeps what the
        // entry does not name (a re-provision resets nothing by itself).
        let c = catalog();
        let kept = "dtoverlay=imx290,cam0,clock-frequency=74250000,4lane,rotation=180\n";
        for claim in [
            json!({"hardware": hw}),
            json!({"hardware": hw, "cameraSensors": {"cameras": {"cam0": {"sensor": "imx290"}}}}),
        ] {
            let choice = &apply_plan(&claim)[0];
            let w = wanted(&choice.entry, c).unwrap();
            assert_eq!(
                line_op(kept, Board::Cm5, CamPort::Cam0, &w, None, c),
                Ok(None),
                "{claim}"
            );
        }
    }
}
