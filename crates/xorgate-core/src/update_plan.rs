//! The pure half of `xorgate-rs update` (Phase 4 WP4, I8): the port of the
//! decisions and texts of `ts:src/commands/update.ts` @ 89b5c64 — the
//! `agent.env` parser, the `--from`/`--tag` resolution order, the artifact and
//! checksum-sidecar names, the required members, the CLI wrapper body, the
//! sudoers drop-in bodies, the shared-asset tables and the coexistence policy.
//! The I/O (curl, tar, rename, copy, visudo, systemctl) lives in
//! `xorgate-agent::cli::update`.
//!
//! Deliberate differences from the TS rail (`phase-4-operations.md` §5, §13),
//! each a D-row in `phase-4/evidence/wp4.md`:
//! - **Q14** the tarball's `<tarball>.sha256` sidecar is downloaded and
//!   verified; missing or mismatching is fatal ([`parse_sha256_sidecar`]).
//! - **Q13** the staged binary is smoke-executed (`bin/xorgate-rs -v`) in place
//!   of `npm ci` ([`SMOKE_PREFIX`]).
//! - **Q1** while the JS agent is installed (`/opt/xorgate` exists) the shared
//!   modem/system assets and sudoers 021/023 are NOT refreshed
//!   ([`asset_policy`], CF62).
//! - **Q5** the agent unit is restarted only if it was active before the swap.
//! - **O3/Q11** the Rust updater unit + its own `024` grant replace `022`.
//! - **Q9** operator hints name `xorgate-rs` / `xorgate-agent-rs`.

use indexmap::IndexMap;

use crate::js::{is_js_whitespace, trim, trim_start};

/// `CONF_DIR` / `AGENT_ENV` (`update.ts:29-30`) — the JS agent's env file,
/// shared unchanged under coexistence (README §6); read, never written.
pub const AGENT_ENV: &str = "/etc/xorgate/agent.env";
/// `INSTALL_PREFIX` (`update.ts:31`) for the Rust build (README §6).
pub const INSTALL_PREFIX: &str = "/opt/xorgate-rs";
/// `STAGING` (`update.ts:32`): on the `/opt` filesystem so the swap is a
/// `rename(2)`.
pub const STAGING: &str = "/opt/xorgate-rs.new";
/// `BACKUP` (`update.ts:33`).
pub const BACKUP: &str = "/opt/xorgate-rs.old";
/// The JS agent's install prefix: its presence means "coexistence" (Q1).
pub const JS_INSTALL_PREFIX: &str = "/opt/xorgate";
/// `UNIT_DEST` (`update.ts:34`) for the Rust unit.
pub const UNIT_DEST: &str = "/etc/systemd/system/xorgate-agent-rs.service";
/// The JS unit path whose `.d/10-user.conf` is the service-user fallback (§5
/// step 6: provisioning wrote only the JS drop-in on a coexisting device).
pub const JS_UNIT_DEST: &str = "/etc/systemd/system/xorgate-agent.service";
/// `WRAPPER` (`update.ts:35`).
pub const WRAPPER: &str = "/usr/local/bin/xorgate-rs";
/// `SERVICE` (`update.ts:36`).
pub const SERVICE: &str = "xorgate-agent-rs";
/// The staged unit file (`update.ts:273,301` `systemd/xorgate-agent.service`).
pub const UNIT_SRC: &str = "systemd/xorgate-agent-rs.service";
/// The staged binary.
pub const BIN_SRC: &str = "bin/xorgate-rs";
/// `UPDATER_UNIT_SRC` (`update.ts:78`) for the Rust updater (O3).
pub const UPDATER_UNIT_SRC: &str = "systemd/xorgate-updater-rs.service";
/// `UPDATER_UNIT_DEST` (`update.ts:79`) for the Rust updater (O3).
pub const UPDATER_UNIT_DEST: &str = "/etc/systemd/system/xorgate-updater-rs.service";
/// The modem daemon script whose presence gates the modem assets
/// (`update.ts:309`).
pub const MODEM_DAEMON: &str = "/usr/local/bin/sim7600-lte.sh";
/// The camera-rotation helper whose presence gates sudoers 023
/// (`update.ts:337`).
pub const CAMROTATE_BIN: &str = "/usr/local/bin/xorgate-camrotate";

/// The members a staged build must carry before the swap (§5 step 4; the TS
/// checked `dist/worker.js` + the unit, `update.ts:271-277`).
pub const REQUIRED_MEMBERS: &[&str] = &[BIN_SRC, UNIT_SRC, "build-info.json"];

/// `WRAPPER_BODY` (`update.ts:38-40`) with `node dist/cli.js` → the Rust
/// binary (§5 step 6). The `XORGATE_HOME` default is the shipped one
/// (`/var/lib/xorgate`, the unit's `StateDirectory`), as the TS.
pub const WRAPPER_BODY: &str = "#!/usr/bin/env bash\n\
exec env XORGATE_HOME=\"${XORGATE_HOME:-/var/lib/xorgate}\" /opt/xorgate-rs/bin/xorgate-rs \"$@\"\n";

/// `MODEM_ASSETS` (`update.ts:47-54`), verbatim: `(src, dest, mode)`.
pub const MODEM_ASSETS: &[(&str, &str, u32)] = &[
    (
        "modem/sim7600-lte.sh",
        "/usr/local/bin/sim7600-lte.sh",
        0o755,
    ),
    (
        "modem/sim7600-lte.service",
        "/etc/systemd/system/sim7600-lte.service",
        0o644,
    ),
    (
        "modem/99-sim7600-lte.rules",
        "/etc/udev/rules.d/99-sim7600-lte.rules",
        0o644,
    ),
    (
        "modem/99-sim7600-unmanaged.conf",
        "/etc/NetworkManager/conf.d/99-sim7600-unmanaged.conf",
        0o644,
    ),
];

/// `SYSTEM_ASSETS` (`update.ts:60-67`), verbatim: `(src, dest, mode)`.
pub const SYSTEM_ASSETS: &[(&str, &str, u32)] = &[
    (
        "system/xorgate-setclock",
        "/usr/local/bin/xorgate-setclock",
        0o755,
    ),
    (
        "system/xorgate-fake-hwclock.cron",
        "/etc/cron.d/xorgate-fake-hwclock",
        0o644,
    ),
    (
        "system/xorgate-camrotate",
        "/usr/local/bin/xorgate-camrotate",
        0o755,
    ),
];

/// `CLOCK_SUDOERS_DEST` (`update.ts:69`).
pub const CLOCK_SUDOERS_DEST: &str = "/etc/sudoers.d/021_xorgate-clock";
/// `CAMROTATE_SUDOERS_DEST` (`update.ts:71`).
pub const CAMROTATE_SUDOERS_DEST: &str = "/etc/sudoers.d/023_xorgate-camrotate";
/// The Rust updater's exact-args grant (O3/Q11; replaces `022`'s role,
/// `update.ts:70`).
pub const UPDATE_RS_SUDOERS_DEST: &str = "/etc/sudoers.d/024_xorgate-update-rs";

/// The argv after `sudo` that the agent runs for a cloud `update` (O3) — the
/// ONE spelling shared by `xorgate_hal::privileged` and the `024` sudoers
/// line ([`updater_sudoers_line`]). `sudo` resolves `systemctl` through its
/// `secure_path`, which is why the sudoers line names [`SYSTEMCTL_PATH`].
pub const UPDATE_RS_ARGV: [&str; 5] = [
    "-n",
    "systemctl",
    "start",
    "--no-block",
    "xorgate-updater-rs.service",
];
/// The absolute `systemctl` the sudoers grant names (`update.ts:156`).
pub const SYSTEMCTL_PATH: &str = "/usr/bin/systemctl";

/// What the staged binary's `-v` must print first (Q13 smoke-exec; the
/// `version::PROGRAM` banner).
pub const SMOKE_PREFIX: &str = "xorgate-rs ";

/// The log prefix (`update.ts:81` `[xorgate] `; Q9: this journal belongs to
/// `xorgate-updater-rs`, so the lines name the Rust binary — §5).
pub const LOG_PREFIX: &str = "[xorgate-rs] ";
/// The failure prefix (`update.ts:83`), printed to stderr before exit 1.
pub const FAIL_PREFIX: &str = "[xorgate-rs] update failed: ";
/// The non-root failure (`update.ts:226`; Q9: the hint names `xorgate-rs`).
pub const NOT_ROOT_MESSAGE: &str =
    "run with sudo (writes /opt, /usr/local/bin, /etc/systemd): sudo xorgate-rs update";

/// `curl -fsSL --retry 3 --retry-delay 2 <url> -o <dest>` (`update.ts:255`).
pub fn curl_args(url: &str, dest: &str) -> Vec<String> {
    [
        "-fsSL",
        "--retry",
        "3",
        "--retry-delay",
        "2",
        url,
        "-o",
        dest,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `download failed (…); check the version/tag and bucket` (`update.ts:258`).
pub fn download_failed_message(stderr: &str, exit_code: Option<i32>) -> String {
    format!(
        "download failed ({}); check the version/tag and bucket",
        failure_detail(stderr, exit_code)
    )
}

/// Q14: the sidecar could not be fetched or has no entry for the tarball.
pub fn no_checksum_message(tarball: &str, detail: &str) -> String {
    format!("no checksum published for {tarball} ({detail}); refusing an unverified build")
}

/// Q14: the downloaded bytes do not hash to the published digest.
pub fn checksum_mismatch_message(tarball: &str, published: &[u8; 32], got: &[u8; 32]) -> String {
    format!(
        "checksum mismatch for {tarball} (published {}, downloaded {})",
        hex32(published),
        hex32(got)
    )
}

/// Q14: the success line E11 greps (`download → checksum ok → swap`).
pub fn checksum_ok_log(digest: &[u8; 32]) -> String {
    format!("checksum ok (sha256 {})", hex32(digest))
}

/// Q13: logged where the TS logged `installing node modules` (`update.ts:281`).
pub const SMOKE_LOG: &str = "checking the staged build runs on this device";

/// Q13: the smoke-exec failure (`npm ci failed: …`'s slot, `update.ts:289`).
pub fn smoke_failed_message(detail: &str) -> String {
    format!("staged build does not run on this device ({detail}); not installed")
}

/// Where the downloaded build comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSource {
    /// No trailing `/` (one is stripped, `update.ts:235`).
    pub bucket_url: String,
    pub version: String,
}

/// `flagValue` (`update.ts:87-90`): the token after the FIRST `name`, if any.
pub fn flag_value<'a>(argv: &'a [String], name: &str) -> Option<&'a str> {
    let i = argv.iter().position(|a| a == name)?;
    argv.get(i + 1).map(String::as_str)
}

/// `parseEnvFile` (`update.ts:207-222`) over already-read text (the caller
/// reads bytes + `from_utf8_lossy`, D4; an unreadable file is `""` → `{}`).
///
/// Per `\n`-split line, the regex `^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)$`
/// (no flags): JS `\s` is [`is_js_whitespace`], and JS `.` excludes the line
/// terminators CR/LS/PS — so a CRLF line whose value is non-empty does NOT
/// match (the CR is neither `.` nor followed by `$`), exactly as the TS.
/// The value is `trim()`med, then ONE leading and ONE trailing `"`/`'` are
/// stripped independently (`replace(/^["']|["']$/g, "")`). Later keys
/// overwrite earlier ones in place (JS object key order; keys are never
/// array-index-like).
pub fn parse_env_file(text: &str) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for line in text.split('\n') {
        let Some((key, raw)) = match_env_line(line) else {
            continue;
        };
        out.insert(key.to_string(), strip_quotes(trim(raw)).to_string());
    }
    out
}

/// The anchored env-line regex (see [`parse_env_file`]); `Some((key, value))`.
fn match_env_line(line: &str) -> Option<(&str, &str)> {
    let s = trim_start(line);
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let key_end = s
        .char_indices()
        .skip(1)
        .find(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '_'))
        .map_or(s.len(), |(i, _)| i);
    let key = &s[..key_end];
    let rest = trim_start(&s[key_end..]);
    let rest = rest.strip_prefix('=')?;
    // `\s*(.*)$`: greedy whitespace, then the remainder must hold no line
    // terminator (giving whitespace back to `.*` can never remove a later one).
    let value = trim_start(rest);
    if value.contains(['\r', '\u{2028}', '\u{2029}', '\n']) {
        return None;
    }
    Some((key, value))
}

/// `.replace(/^["']|["']$/g, "")`: one leading and one trailing quote of
/// either kind, independently; a lone quote leaves `""`.
fn strip_quotes(v: &str) -> &str {
    let is_q = |b: u8| b == b'"' || b == b'\'';
    let b = v.as_bytes();
    let start = usize::from(!b.is_empty() && is_q(b[0]));
    let end = if b.len() > start && is_q(b[b.len() - 1]) {
        b.len() - 1
    } else {
        b.len()
    };
    &v[start..end]
}

/// The missing-bucket failure (`update.ts:236-240`; `AGENT_ENV` substituted).
pub fn no_bucket_message() -> String {
    format!("no dist bucket URL — set DEVICE_DIST_BUCKET_URL in {AGENT_ENV} or pass --from <url>")
}

/// Bucket + version resolution (`update.ts:229-245`): `--from` → env
/// `DEVICE_DIST_BUCKET_URL` → `agent.env` → `""` (fail), ONE trailing `/`
/// stripped; `--tag` → env `DEVICE_DIST_VERSION` → `agent.env` → `latest`.
/// `??` semantics: an env var set to `""` is NOT nullish and wins (then
/// fails the bucket check), as in the TS.
pub fn resolve_source(
    from_flag: Option<&str>,
    tag_flag: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    file: &IndexMap<String, String>,
) -> Result<UpdateSource, String> {
    let bucket = from_flag
        .map(str::to_string)
        .or_else(|| env("DEVICE_DIST_BUCKET_URL"))
        .or_else(|| file.get("DEVICE_DIST_BUCKET_URL").cloned())
        .unwrap_or_default();
    let bucket_url = bucket.strip_suffix('/').unwrap_or(&bucket).to_string();
    if bucket_url.is_empty() {
        return Err(no_bucket_message());
    }
    let version = tag_flag
        .map(str::to_string)
        .or_else(|| env("DEVICE_DIST_VERSION"))
        .or_else(|| file.get("DEVICE_DIST_VERSION").cloned())
        .unwrap_or_else(|| "latest".to_string());
    Ok(UpdateSource {
        bucket_url,
        version,
    })
}

/// `xorgate-agent-rs-<v>.tar.gz` (`update.ts:247` with the `-rs` artifact
/// name, §5 step 2; CI publishes only `-rs` keys).
pub fn tarball_name(version: &str) -> String {
    format!("xorgate-agent-rs-{version}.tar.gz")
}

/// `<tarball>.sha256` (`scripts/pack.sh`, `device-rs-deploy-*.yml`).
pub fn sidecar_name(tarball: &str) -> String {
    format!("{tarball}.sha256")
}

/// `${bucketUrl}/${file}` (`update.ts:248`).
pub fn artifact_url(src: &UpdateSource, file: &str) -> String {
    format!("{}/{file}", src.bucket_url)
}

/// Q14: the digest a `sha256sum -c` sidecar publishes for `basename`
/// (`<64 hex>  <basename>`, text or `*` binary marker; `scripts/pack.sh`,
/// `device-rs-deploy-dev.yml:189`). Hex is case-insensitive; leading/trailing
/// whitespace and a CR are tolerated; the name must equal `basename` exactly
/// (no path, no bare-hex line). `None` = no usable entry → fail closed.
pub fn parse_sha256_sidecar(text: &str, basename: &str) -> Option<[u8; 32]> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let (hex, rest) = line.split_once([' ', '\t'])?;
        let name = rest.trim_start_matches([' ', '\t']);
        let name = name.strip_prefix('*').unwrap_or(name);
        if name != basename {
            return None;
        }
        decode_hex32(hex)
    })
}

/// 64 hex digits → 32 bytes.
pub fn decode_hex32(hex: &str) -> Option<[u8; 32]> {
    let b = hex.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
    }
    Some(out)
}

/// Lower-case hex of a digest (log lines).
pub fn hex32(d: &[u8; 32]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in d {
        s.push(H[usize::from(b >> 4)] as char);
        s.push(H[usize::from(b & 0xf)] as char);
    }
    s
}

/// `serviceUser`'s match (`update.ts:97-105`): `/^\s*User\s*=\s*(\S+)/m`
/// over the drop-in text — the first line start (input start or after
/// LF/CR/LS/PS, the JS `m` flag) where it matches; `\s` may span lines.
pub fn parse_service_user(text: &str) -> Option<String> {
    let starts = std::iter::once(0).chain(
        text.char_indices()
            .filter(|&(_, c)| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
            .map(|(i, c)| i + c.len_utf8()),
    );
    for p in starts {
        let s = trim_start(&text[p..]);
        let Some(s) = s.strip_prefix("User") else {
            continue;
        };
        let Some(s) = trim_start(s).strip_prefix('=') else {
            continue;
        };
        let s = trim_start(s);
        let end = s.find(is_js_whitespace).unwrap_or(s.len());
        if end > 0 {
            return Some(s[..end].to_string());
        }
    }
    None
}

/// `if (!user || user === "root") return;` (`update.ts:115,151,186`): the
/// user a sudoers grant is written for, if any.
pub fn grant_user(user: Option<&str>) -> Option<&str> {
    user.filter(|u| !u.is_empty() && *u != "root")
}

/// `installClockSudoers` body (`update.ts:116-119`), verbatim — the SHARED
/// `021` grant, written only on a Rust-only device (Q1).
pub fn clock_sudoers_body(user: &str) -> String {
    format!(
        "# Installed by xorgate. Allows the agent (running as {user}) to apply an\n\
# offline GPS/NITZ clock step. See device-agent src/agent/clock-step.ts.\n\
{user} ALL=(root) NOPASSWD: /usr/local/bin/xorgate-setclock\n"
    )
}

/// `installCamRotateSudoers` body (`update.ts:187-191`), verbatim — the
/// SHARED `023` grant, written only on a Rust-only device (Q1).
pub fn camrotate_sudoers_body(user: &str) -> String {
    format!(
        "# Installed by xorgate. Allows the agent (running as {user}) to set the\n\
# camera overlay's rotation parameter in the boot config, and nothing else.\n\
# See device-agent src/hardware/boot-config.ts + system/xorgate-camrotate.\n\
{user} ALL=(root) NOPASSWD: /usr/local/bin/xorgate-camrotate\n"
    )
}

/// The command half of the `024` grant, generated from [`UPDATE_RS_ARGV`]:
/// `/usr/bin/systemctl start --no-block xorgate-updater-rs.service`.
pub fn update_rs_sudoers_command() -> String {
    let [_n, systemctl, rest @ ..] = UPDATE_RS_ARGV;
    debug_assert_eq!(systemctl, "systemctl");
    let mut s = SYSTEMCTL_PATH.to_string();
    for w in rest {
        s.push(' ');
        s.push_str(w);
    }
    s
}

/// `<user> ALL=(root) NOPASSWD: <update_rs_sudoers_command>`.
pub fn updater_sudoers_line(user: &str) -> String {
    format!(
        "{user} ALL=(root) NOPASSWD: {}",
        update_rs_sudoers_command()
    )
}

/// The `024_xorgate-update-rs` body: the TS `022` body (`update.ts:152-156`)
/// with the Rust unit and sources named (O3/Q11).
pub fn updater_sudoers_body(user: &str) -> String {
    format!(
        "# Installed by xorgate-rs. Allows the agent (running as {user}) to start the\n\
# one-shot self-update unit on a cloud-issued 'update' command, and nothing\n\
# else. See device-agent-rust crates/xorgate-hal/src/privileged.rs + systemd/xorgate-updater-rs.service.\n\
{}\n",
        updater_sudoers_line(user)
    )
}

/// One sudoers drop-in to (maybe) install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SudoersGrant {
    /// `021_xorgate-clock` (`installClockSudoers`).
    Clock,
    /// `023_xorgate-camrotate` (`installCamRotateSudoers`).
    CamRotate,
    /// `024_xorgate-update-rs` (the Rust `installUpdateSudoers`).
    UpdateRs,
}

impl SudoersGrant {
    /// The drop-in's basename (also the mkdtemp file name, `update.ts:120`).
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Clock => "021_xorgate-clock",
            Self::CamRotate => "023_xorgate-camrotate",
            Self::UpdateRs => "024_xorgate-update-rs",
        }
    }

    pub fn dest(self) -> &'static str {
        match self {
            Self::Clock => CLOCK_SUDOERS_DEST,
            Self::CamRotate => CAMROTATE_SUDOERS_DEST,
            Self::UpdateRs => UPDATE_RS_SUDOERS_DEST,
        }
    }

    pub fn body(self, user: &str) -> String {
        match self {
            Self::Clock => clock_sudoers_body(user),
            Self::CamRotate => camrotate_sudoers_body(user),
            Self::UpdateRs => updater_sudoers_body(user),
        }
    }

    /// The visudo-refusal warning (`update.ts:125,162,197`).
    pub fn refused_warning(self) -> &'static str {
        match self {
            Self::Clock => "warning: clock sudoers drop-in failed visudo validation; offline clock steps will not work",
            Self::CamRotate => "warning: camrotate sudoers drop-in failed visudo validation; camera rotation will not apply",
            Self::UpdateRs => "warning: update sudoers drop-in failed visudo validation; remote updates will not work",
        }
    }
}

/// Which shared assets this update installs (Q1, CF62).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetPolicy {
    /// The JS agent is installed: it owns modem/, system/ and sudoers
    /// 020–023 (README §6 "shared, unchanged") — log [`SHARED_SKIP_LOG`].
    pub skip_shared: bool,
    /// Refresh [`MODEM_ASSETS`] (`update.ts:309`: only with the daemon).
    pub modem: bool,
    /// Install [`SYSTEM_ASSETS`] + `021` (+ `023` when the helper exists).
    pub system: bool,
}

/// The coexistence skip line (§5 step 7).
pub const SHARED_SKIP_LOG: &str =
    "shared assets owned by the JS agent (coexistence) — not refreshed";

/// The modem refresh line (`update.ts:319-321`), verbatim (names the shared
/// `sim7600-lte` unit, not an agent).
pub const MODEM_REFRESHED_LOG: &str =
    "modem assets refreshed — applies on next boot, or now with: sudo systemctl restart sim7600-lte";

/// Q1: with `/opt/xorgate` present nothing shared is touched; without it the
/// TS rules apply verbatim (modem only when the daemon is installed; system
/// assets unconditional). The Rust-only `024` grant + updater unit are
/// installed either way (they are not shared).
pub fn asset_policy(js_installed: bool, modem_daemon_present: bool) -> AssetPolicy {
    AssetPolicy {
        skip_shared: js_installed,
        modem: !js_installed && modem_daemon_present,
        system: !js_installed,
    }
}

/// The final line (`update.ts:367`, Q9 hint).
pub fn updated_log(version: &str) -> String {
    format!("updated to '{version}'. check it with: xorgate-rs status")
}

/// Q5: the agent was not running before the swap — no restart.
pub fn not_started_log() -> String {
    format!("installed; not started ({SERVICE} is inactive)")
}

/// The restart-failure warning pair (`update.ts:361-363`).
pub fn restart_failed_logs(stderr: &str) -> [String; 2] {
    [
        format!(
            "warning: 'systemctl restart {SERVICE}' failed: {}",
            trim(stderr)
        ),
        format!("the new build is installed; restart manually: sudo systemctl restart {SERVICE}"),
    ]
}

/// The `download failed (…)` detail (`update.ts:258`):
/// `stderr.trim() || exitCode` (a null exit code prints `null`).
pub fn failure_detail(stderr: &str, exit_code: Option<i32>) -> String {
    let t = trim(stderr);
    if !t.is_empty() {
        return t.to_string();
    }
    exit_code.map_or_else(|| "null".to_string(), |c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_line_regex_edges() {
        let m = parse_env_file("A=1\n  B = \"two\" \nexport C=3\nD='x\"\n1E=no\nF=a\r\nG=\r\n");
        let got: Vec<(&str, &str)> = m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(got, [("A", "1"), ("B", "two"), ("D", "x"), ("G", "")]);
        assert_eq!(strip_quotes("\""), "");
        assert_eq!(strip_quotes("\"\""), "");
        assert_eq!(strip_quotes("'a'b'"), "a'b");
    }

    #[test]
    fn sidecar_parser() {
        let h = "AbCdEf0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let lo = h.to_ascii_lowercase();
        let name = "xorgate-agent-rs-latest.tar.gz";
        let want = decode_hex32(&lo).unwrap();
        assert_eq!(hex32(&want), lo);
        assert_eq!(
            parse_sha256_sidecar(&format!("{lo}  {name}\n"), name),
            Some(want)
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("{h}  {name}"), name),
            Some(want)
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("  {lo}  *{name}  \r\n"), name),
            Some(want)
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("{lo} {name}\n"), name),
            Some(want)
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("x  y\n{lo}  {name}\n"), name),
            Some(want)
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("{lo}  other.tar.gz\n"), name),
            None
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("{lo}  ./{name}\n"), name),
            None
        );
        assert_eq!(parse_sha256_sidecar(&format!("{lo}\n"), name), None);
        assert_eq!(parse_sha256_sidecar("", name), None);
        assert_eq!(
            parse_sha256_sidecar(&format!("{}  {name}", &lo[1..]), name),
            None
        );
        assert_eq!(
            parse_sha256_sidecar(&format!("{}g  {name}", &lo[1..]), name),
            None
        );
    }

    #[test]
    fn update_rs_line_is_generated_from_the_argv_const() {
        assert_eq!(
            updater_sudoers_line("pi"),
            "pi ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block xorgate-updater-rs.service"
        );
        assert!(updater_sudoers_body("pi").ends_with(&format!("{}\n", updater_sudoers_line("pi"))));
        assert!(updater_sudoers_body("pi").starts_with("# Installed by xorgate-rs. "));
    }

    #[test]
    fn policy_and_grant_user() {
        assert_eq!(
            asset_policy(true, true),
            AssetPolicy {
                skip_shared: true,
                modem: false,
                system: false
            }
        );
        assert_eq!(
            asset_policy(false, true),
            AssetPolicy {
                skip_shared: false,
                modem: true,
                system: true
            }
        );
        assert_eq!(
            asset_policy(false, false),
            AssetPolicy {
                skip_shared: false,
                modem: false,
                system: true
            }
        );
        assert_eq!(grant_user(Some("pi")), Some("pi"));
        assert_eq!(grant_user(Some("root")), None);
        assert_eq!(grant_user(None), None);
    }

    #[test]
    fn misc_lines() {
        assert_eq!(failure_detail("  \n", Some(22)), "22");
        assert_eq!(failure_detail("", None), "null");
        assert_eq!(failure_detail(" curl: (22) x\n", Some(22)), "curl: (22) x");
        assert_eq!(tarball_name("latest"), "xorgate-agent-rs-latest.tar.gz");
        let a: Vec<String> = ["--tag", "v1", "--from"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(flag_value(&a, "--tag"), Some("v1"));
        assert_eq!(flag_value(&a, "--from"), None);
        assert_eq!(flag_value(&a, "--x"), None);
    }
}
