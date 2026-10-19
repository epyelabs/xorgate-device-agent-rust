//! `xorgate-rs pipeline <list|show|set|clear|bridge|direct>` — port of
//! `src/commands/pipeline.ts:1-234` (per-stream GStreamer pipelines of the
//! KVS WebRTC runtime, plus the bridged/direct switch of the offline
//! recording plan D1).
//!
//! Dir/manifest resolve from `--dir`/`--manifest`, then `KVS_PIPELINE_DIR` /
//! `KVS_MANIFEST`, then `/etc/xorgate/{pipelines,kvs.json}` (`:36-48`). Text,
//! file bytes and exit codes are the TS's: `process.exitCode = 1` → exit 1
//! after printing; a thrown fs error (`main().catch`, `cli.tsx:164-167`)
//! prints Node's `err.message` on stderr and exits 1, keeping whatever was
//! printed before it. Files are written plainly (`writeFileSync`, not
//! atomic), read as bytes then decoded lossily (D4).
//!
//! One deliberate text change (Q9, D-row): [`RESTART_HINT`] names the Rust
//! unit `xorgate-agent-rs` (the TS hint `:39` names `xorgate-agent` and the
//! self-managed `xorgate restart`, which this binary does not have).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use xorgate_core::agent_config::resolve_path;
use xorgate_core::hardware_block::EncoderToken;
use xorgate_core::video_capture::bridged_kvs_pipeline;
use xorgate_hal::hardware_file::{read_hardware_file, HardwareFile};
use xorgate_hal::io::{node_fd_message, node_fs_message};

/// `DEFAULT_DIR` (`pipeline.ts:36`).
pub const DEFAULT_DIR: &str = "/etc/xorgate/pipelines";
/// `DEFAULT_MANIFEST` (`pipeline.ts:37`).
pub const DEFAULT_MANIFEST: &str = "/etc/xorgate/kvs.json";
/// `HARDWARE_MANIFEST ?? "/etc/xorgate/hardware.json"` (`pipeline.ts:193`).
pub const DEFAULT_HARDWARE: &str = "/etc/xorgate/hardware.json";
/// `RESTART_HINT` (`pipeline.ts:39`) — Q9 D-row: the Rust unit; the TS text is
/// `  restart to apply: sudo systemctl restart xorgate-agent  (or: xorgate restart)`.
pub const RESTART_HINT: &str = "  restart to apply: sudo systemctl restart xorgate-agent-rs";

/// The process facts `pipelineCommand` reads.
#[derive(Debug, Clone, Default)]
pub struct PipelineEnv {
    /// `process.cwd()` (for `path.resolve`).
    pub cwd: String,
    pub kvs_pipeline_dir: Option<String>,
    pub kvs_manifest: Option<String>,
    pub hardware_manifest: Option<String>,
    /// `runtimeHome()` (for `shmSocketPathFor`).
    pub home: String,
}

impl PipelineEnv {
    /// From this process (`process.env[k]`: set-but-empty is `""`, which
    /// `??` keeps).
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
        PipelineEnv {
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".to_string()),
            kvs_pipeline_dir: var("KVS_PIPELINE_DIR"),
            kvs_manifest: var("KVS_MANIFEST"),
            hardware_manifest: var("HARDWARE_MANIFEST"),
            home: crate::env::runtime_home().to_string_lossy().into_owned(),
        }
    }
}

/// `console.log` / `console.warn|error` / `process.stdout.write` sinks.
pub trait Console {
    /// `console.log(text)`.
    fn log(&mut self, text: &str);
    /// `console.error(text)` / `console.warn(text)`.
    fn error(&mut self, text: &str);
    /// `process.stdout.write(text)`.
    fn write(&mut self, text: &str);
}

/// The process's stdout/stderr. Write errors (EPIPE under `| head`) are
/// swallowed, as `console.log` does.
pub struct StdConsole;

impl Console for StdConsole {
    fn log(&mut self, text: &str) {
        let _ = writeln!(io::stdout(), "{text}");
    }
    fn error(&mut self, text: &str) {
        let _ = writeln!(io::stderr(), "{text}");
    }
    fn write(&mut self, text: &str) {
        let mut o = io::stdout();
        let _ = o.write_all(text.as_bytes());
        let _ = o.flush();
    }
}

/// A captured console (tests, fixtures): stdout and stderr text.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Captured {
    pub stdout: String,
    pub stderr: String,
}

impl Console for Captured {
    fn log(&mut self, text: &str) {
        self.stdout.push_str(text);
        self.stdout.push('\n');
    }
    fn error(&mut self, text: &str) {
        self.stderr.push_str(text);
        self.stderr.push('\n');
    }
    fn write(&mut self, text: &str) {
        self.stdout.push_str(text);
    }
}

/// `xorgate-rs pipeline …` (raw args after the verb) → exit code.
pub fn main(args: &[String]) -> i32 {
    run(args, &PipelineEnv::from_process(), &mut StdConsole)
}

/// A thrown fs error: Node's `err.message`.
type Thrown = String;

/// `pipelineCommand(rest)` (`pipeline.ts:41-64`) + `main().catch` → exit code.
pub fn run(rest: &[String], env: &PipelineEnv, c: &mut dyn Console) -> i32 {
    match dispatch(rest, env, c) {
        Ok(code) => code,
        Err(msg) => {
            c.error(&msg);
            1
        }
    }
}

fn dispatch(rest: &[String], env: &PipelineEnv, c: &mut dyn Console) -> Result<i32, Thrown> {
    let (pos, flags) = parse(rest);
    let flag = |k: &str| {
        flags
            .iter()
            .rev()
            .find(|(f, _)| f == k)
            .map(|(_, v)| v.clone())
    };
    let dir_in = flag("--dir")
        .or_else(|| env.kvs_pipeline_dir.clone())
        .unwrap_or_else(|| DEFAULT_DIR.to_string());
    let dir = resolve_path(&env.cwd, &dir_in);
    let sub = pos.first().map(String::as_str).unwrap_or("list");
    let key = pos.get(1).map(String::as_str);
    match sub {
        "list" => {
            let manifest = flag("--manifest")
                .or_else(|| env.kvs_manifest.clone())
                .unwrap_or_else(|| DEFAULT_MANIFEST.to_string());
            list(&dir, &manifest, &env.cwd, c)
        }
        "show" => show(&dir, key, c),
        "set" => set(&dir, key, pos.get(2).map(String::as_str), env, c),
        "clear" => clear(&dir, key, c),
        "bridge" => bridge(&dir, key, env, c),
        "direct" => direct(&dir, key, c),
        other => {
            c.error(&format!("unknown subcommand '{other}'"));
            usage(c);
            Ok(1)
        }
    }
}

/// `parse(rest)` (`pipeline.ts:66-79`): `--dir`/`--manifest` take the next
/// token (`?? ""`), any other `-…` token is skipped, the rest are positionals.
/// A repeated flag: the last one wins (object assignment).
pub fn parse(rest: &[String]) -> (Vec<String>, Vec<(String, String)>) {
    let mut pos = Vec::new();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if a == "--dir" || a == "--manifest" {
            i += 1;
            flags.push((a.clone(), rest.get(i).cloned().unwrap_or_default()));
            i += 1;
            continue;
        }
        if !a.starts_with('-') {
            pos.push(a.clone());
        }
        i += 1;
    }
    (pos, flags)
}

/// `validKey` (`pipeline.ts:82-89`): `/^[A-Za-z0-9_-]+$/` — stream keys map
/// to filenames, so no path bits.
pub fn valid_key(key: Option<&str>) -> Option<&str> {
    key.filter(|k| {
        !k.is_empty()
            && k.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

const BAD_KEY: &str = "a valid streamKey is required (letters, digits, '_' or '-'), e.g. cam0";

/// Node's `path.normalize` (POSIX, `lib/path.js` `posix.normalize`).
pub fn node_normalize(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let abs = p.starts_with('/');
    let trailing = p.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !abs {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut out = parts.join("/");
    if out.is_empty() {
        return match (abs, trailing) {
            (true, _) => "/".to_string(),
            (false, true) => "./".to_string(),
            (false, false) => ".".to_string(),
        };
    }
    if trailing {
        out.push('/');
    }
    if abs {
        format!("/{out}")
    } else {
        out
    }
}

/// Node's `path.join(a, b)`.
pub fn node_join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => ".".to_string(),
        (true, false) => node_normalize(b),
        (false, true) => node_normalize(a),
        (false, false) => node_normalize(&format!("{a}/{b}")),
    }
}

/// `pipelineFileFor` (`media/pipeline.ts:14-16`).
pub fn pipeline_file_for(dir: &str, key: &str) -> String {
    node_join(dir, &format!("{key}.gst"))
}
/// `cameraFileFor` (`media/pipeline.ts:24-26`).
pub fn camera_file_for(dir: &str, key: &str) -> String {
    node_join(dir, &format!("{key}.camera"))
}
/// `directBackupFor` (`media/pipeline.ts:28-30`).
pub fn direct_backup_for(dir: &str, key: &str) -> String {
    node_join(dir, &format!("{key}.gst.direct"))
}

/// `existsSync(p)`.
fn exists(p: &str) -> bool {
    Path::new(p).exists()
}

/// `readFileSync(p, "utf8")`, throwing Node's message
/// (`EACCES: permission denied, open '<p>'`, `EISDIR: …, read`).
fn read_utf8(p: &str) -> Result<String, Thrown> {
    let path = Path::new(p);
    let mut f = std::fs::File::open(path).map_err(|e| node_fs_message(&e, "open", path))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)
        .map_err(|e| node_fd_message(&e, "read"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `try { readFileSync(p, "utf8") } catch { null }`.
fn read_opt(p: &str) -> Option<String> {
    read_utf8(p).ok()
}

/// `writeFileSync(p, text)` (plain, mode 0o666 & ~umask).
fn write_text(p: &str, text: &str) -> Result<(), Thrown> {
    let path = Path::new(p);
    let mut f = std::fs::File::create(path).map_err(|e| node_fs_message(&e, "open", path))?;
    f.write_all(text.as_bytes())
        .map_err(|e| node_fd_message(&e, "write"))
}

/// `mkdirSync(dir, { recursive: true })`: Node names the requested path.
fn mkdir_p(dir: &str) -> Result<(), Thrown> {
    let path = Path::new(dir);
    std::fs::create_dir_all(path).map_err(|e| node_fs_message(&e, "mkdir", path))
}

/// `rmSync(p)` (node 20, probed): a directory throws `ERR_FS_EISDIR`.
fn rm(p: &str) -> Result<(), Thrown> {
    let path = Path::new(p);
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(format!(
            "Path is a directory: rm returned EISDIR (is a directory) {p}"
        ));
    }
    std::fs::remove_file(path).map_err(|e| node_fs_message(&e, "unlink", path))
}

/// `readdirSync(dir)`: libuv scandir sorts with `strcmp` (byte order).
fn readdir(dir: &str) -> Result<Vec<String>, Thrown> {
    let path = Path::new(dir);
    let rd = std::fs::read_dir(path).map_err(|e| node_fs_message(&e, "scandir", path))?;
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

/// `String.prototype.padEnd(n)` (UTF-16 length).
fn pad_end(s: &str, n: usize) -> String {
    let len = s.encode_utf16().count();
    format!("{s}{}", " ".repeat(n.saturating_sub(len)))
}

/// `list(dir, manifestPath)` (`pipeline.ts:91-111`).
fn list(dir: &str, manifest_path: &str, cwd: &str, c: &mut dyn Console) -> Result<i32, Thrown> {
    c.log(&format!("pipeline dir: {dir}"));
    // `readFileSync(manifestPath)` is relative to the process cwd: resolve it
    // against `env.cwd` (the same directory in the real process) so the
    // fixture runs hermetically; the printed path stays the given one.
    let manifest =
        xorgate_hal::kvs_manifest_io::load_manifest(Path::new(&resolve_path(cwd, manifest_path)));
    match manifest.filter(|m| !m.channels.is_empty()) {
        Some(m) => {
            c.log("channels:");
            for ch in &m.channels {
                let f = pipeline_file_for(dir, &ch.stream_key);
                let mode = if is_bridged(dir, &ch.stream_key) {
                    " [bridged]"
                } else {
                    ""
                };
                let tag = if exists(&f) {
                    format!("{f}{mode}")
                } else {
                    "(runtime default)".to_string()
                };
                c.log(&format!(
                    "  {} {}  ←  {tag}",
                    pad_end(&ch.stream_key, 8),
                    ch.channel_name
                ));
            }
        }
        None => c.log(&format!(
            "(no manifest at {manifest_path}; showing pipeline files only)"
        )),
    }
    let files: Vec<String> = if exists(dir) {
        readdir(dir)?
            .into_iter()
            .filter(|f| f.ends_with(".gst"))
            .collect()
    } else {
        Vec::new()
    };
    if files.is_empty() {
        c.log("no per-stream pipeline files set (all channels use the runtime default)");
    } else {
        c.log(&format!("pipeline files: {}", files.join(", ")));
    }
    Ok(0)
}

/// `isBridgedPipeline(dir, key)` (`media/pipeline.ts:36-42`).
fn is_bridged(dir: &str, key: &str) -> bool {
    xorgate_core::video_capture::is_bridged_pipeline(
        read_opt(&pipeline_file_for(dir, key)).as_deref(),
    )
}

/// `resolveCameraName(dir, key)` (`media/pipeline.ts:55-71`) over the three
/// files (core `resolve_camera_name`; an unreadable file is skipped).
fn resolve_camera_name(dir: &str, key: &str) -> Option<String> {
    xorgate_core::video_capture::resolve_camera_name(
        read_opt(&camera_file_for(dir, key)).as_deref(),
        read_opt(&direct_backup_for(dir, key)).as_deref(),
        read_opt(&pipeline_file_for(dir, key)).as_deref(),
    )
}

/// `show(dir, key)` (`pipeline.ts:113-122`).
fn show(dir: &str, key: Option<&str>, c: &mut dyn Console) -> Result<i32, Thrown> {
    let Some(key) = valid_key(key) else {
        c.error(BAD_KEY);
        return Ok(1);
    };
    let f = pipeline_file_for(dir, key);
    if !exists(&f) {
        c.log(&format!(
            "{key}: no pipeline set (uses the runtime default)"
        ));
        return Ok(0);
    }
    c.log(&format!("# {f}"));
    let text = read_utf8(&f)?;
    c.write(&text);
    Ok(0)
}

/// `set(dir, key, file)` (`pipeline.ts:124-148`).
fn set(
    dir: &str,
    key: Option<&str>,
    file: Option<&str>,
    env: &PipelineEnv,
    c: &mut dyn Console,
) -> Result<i32, Thrown> {
    let Some(key) = valid_key(key) else {
        c.error(BAD_KEY);
        return Ok(1);
    };
    let Some(file) = file.filter(|f| !f.is_empty()) else {
        c.error("usage: xorgate pipeline set <streamKey> <file>");
        return Ok(1);
    };
    let src = resolve_path(&env.cwd, file);
    if !exists(&src) {
        c.error(&format!("pipeline file not found: {src}"));
        return Ok(1);
    }
    let content = read_utf8(&src)?;
    if !content.contains("appsink") {
        c.error(
            "WARN: pipeline has no 'appsink'; it should end with 'appsink ... name=appsink-video'",
        );
    }
    mkdir_p(dir)?;
    let dest = pipeline_file_for(dir, key);
    write_text(&dest, &content)?;
    c.log(&format!("✓ set {key} pipeline → {dest}"));
    c.log(RESTART_HINT);
    Ok(0)
}

/// `clear(dir, key)` (`pipeline.ts:150-160`).
fn clear(dir: &str, key: Option<&str>, c: &mut dyn Console) -> Result<i32, Thrown> {
    let Some(key) = valid_key(key) else {
        c.error(BAD_KEY);
        return Ok(1);
    };
    let f = pipeline_file_for(dir, key);
    if !exists(&f) {
        c.log(&format!("{key}: no pipeline set; nothing to clear"));
        return Ok(0);
    }
    rm(&f)?;
    c.log(&format!(
        "✓ cleared {key} pipeline ({f}); reverts to the runtime default"
    ));
    c.log(RESTART_HINT);
    Ok(0)
}

/// `loadHardwareBlock(HARDWARE_MANIFEST ?? …, () => {})?.encoder` — silent.
fn hardware_encoder(env: &PipelineEnv) -> Option<EncoderToken> {
    let path = env
        .hardware_manifest
        .clone()
        .unwrap_or_else(|| DEFAULT_HARDWARE.to_string());
    match read_hardware_file(&PathBuf::from(resolve_path(&env.cwd, &path))) {
        HardwareFile::Block(b) => Some(b.encoder),
        _ => None,
    }
}

/// `bridge(dir, key)` (`pipeline.ts:169-201`): back up the direct `.gst` to
/// `.gst.direct`, write `<key>.camera`, install the shmsrc consumer
/// (`bridgedKvsPipeline(shmSocketPathFor(key), encoder)` + `\n`).
fn bridge(
    dir: &str,
    key: Option<&str>,
    env: &PipelineEnv,
    c: &mut dyn Console,
) -> Result<i32, Thrown> {
    let Some(key) = valid_key(key) else {
        c.error(BAD_KEY);
        return Ok(1);
    };
    let gst = pipeline_file_for(dir, key);
    if is_bridged(dir, key) {
        c.log(&format!("{key}: already bridged ({gst})"));
        return Ok(0);
    }
    let Some(camera_name) = resolve_camera_name(dir, key) else {
        c.error(&format!(
            "{key}: cannot determine the libcamera camera-name (no {} and no camera-name= in the .gst). Write the camera name to that file and retry.",
            camera_file_for(dir, key)
        ));
        return Ok(1);
    };
    mkdir_p(dir)?;
    if exists(&gst) {
        let direct = read_utf8(&gst)?;
        write_text(&direct_backup_for(dir, key), &direct)?;
    }
    write_text(&camera_file_for(dir, key), &format!("{camera_name}\n"))?;
    let encoder = hardware_encoder(env);
    // `shmSocketPathFor(key)` = join(runtimeHome(), "shm", `${key}.sock`).
    let sock = node_join(&node_join(&env.home, "shm"), &format!("{key}.sock"));
    write_text(&gst, &format!("{}\n", bridged_kvs_pipeline(&sock, encoder)))?;
    c.log(&format!(
        "✓ {key} bridged: capture daemon will own {camera_name}"
    ));
    c.log(&format!(
        "  .gst → shmsrc consumer (direct pipeline saved to {})",
        direct_backup_for(dir, key)
    ));
    c.log("  NOTE: live streaming on this camera now requires the agent's capture");
    c.log(&format!(
        "  daemon (media on). Revert anytime with: xorgate pipeline direct {key}"
    ));
    c.log(RESTART_HINT);
    Ok(0)
}

/// `direct(dir, key)` (`pipeline.ts:204-218`).
fn direct(dir: &str, key: Option<&str>, c: &mut dyn Console) -> Result<i32, Thrown> {
    let Some(key) = valid_key(key) else {
        c.error(BAD_KEY);
        return Ok(1);
    };
    let backup = direct_backup_for(dir, key);
    if !exists(&backup) {
        c.error(&format!(
            "{key}: no {backup} to restore; use 'xorgate pipeline set' instead"
        ));
        return Ok(1);
    }
    let text = read_utf8(&backup)?;
    write_text(&pipeline_file_for(dir, key), &text)?;
    rm(&backup)?;
    c.log(&format!(
        "✓ {key} restored to direct mode (KVS master owns the camera again)"
    ));
    c.log("  if recording.video.enabled is on, disable it or the capture daemon will");
    c.log("  contend for the camera: xorgate set recording.video.enabled off");
    c.log(RESTART_HINT);
    Ok(0)
}

/// `usage()` (`pipeline.ts:220-234`), verbatim.
fn usage(c: &mut dyn Console) {
    c.log(&format!(
        "usage: xorgate pipeline <list|show|set|clear|bridge|direct> [streamKey] [file]

  list                      channels (from kvs.json) + which have a pipeline
  show  <streamKey>         print a stream's pipeline
  set   <streamKey> <file>  install a .gst pipeline for a stream (e.g. cam0)
  clear <streamKey>         remove a stream's pipeline (revert to default)
  bridge <streamKey>        capture-daemon mode: agent owns the camera, KVS
                            reads shm (required for recording.video; backs up
                            the direct .gst)
  direct <streamKey>        restore the backed-up direct pipeline

  --dir <path>       pipeline dir (default: $KVS_PIPELINE_DIR or {DEFAULT_DIR})
  --manifest <path>  kvs.json for 'list' (default: $KVS_MANIFEST or {DEFAULT_MANIFEST})"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_path_semantics() {
        assert_eq!(node_join("/a/b", "c.gst"), "/a/b/c.gst");
        assert_eq!(node_join("/a/b/", "../c.gst"), "/a/c.gst");
        assert_eq!(node_join("/", "x"), "/x");
        assert_eq!(node_join("rel//h", "shm"), "rel/h/shm");
        assert_eq!(node_join("../h", "../../x"), "../../x");
        assert_eq!(node_normalize("a/"), "a/");
        assert_eq!(node_normalize("/"), "/");
    }

    #[test]
    fn keys_and_flags() {
        assert_eq!(valid_key(Some("cam0")), Some("cam0"));
        assert_eq!(valid_key(Some("../evil")), None);
        assert_eq!(valid_key(Some("")), None);
        assert_eq!(valid_key(Some("a.b")), None);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (pos, flags) = parse(&s(&["set", "--dir", "/x", "-q", "cam0", "--dir"]));
        assert_eq!(pos, s(&["set", "cam0"]));
        assert_eq!(flags.last().unwrap(), &("--dir".to_string(), String::new()));
    }

    #[test]
    fn pad_end_is_utf16() {
        assert_eq!(pad_end("cam0", 8), "cam0    ");
        assert_eq!(pad_end("verylongkey", 8), "verylongkey");
    }
}
