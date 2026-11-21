//! The camera-line owner, `system/xorgate-camcfg`, run for REAL against
//! fixture files (support-other-camera-models plan, Phase 1; the precedent
//! is `tests/camera_mount.rs` for `xorgate-camrotate`). The shell is covered
//! rather than trusted:
//!
//! - the three benches' real `config.txt` (read 2026-11-21), byte for byte;
//! - zero, one and two camera lines on a port; lines under `[section]`
//!   filters; CRLF; comments; a missing trailing newline;
//! - hostile arguments: nothing the caller passes is written as given;
//! - the helper and the agent's pure renderer and section rule
//!   (`xorgate_core::boot_config`) agree, line for line.
//!
//! The script runs under `/bin/sh` with the system `awk` (mawk on the build
//! image and on the devices).
#![allow(clippy::disallowed_methods)] // F-P6-ENV opt-out: NOT an agent spawn site (see clippy.toml)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use xorgate_core::boot_config::{
    line_with_rotation, parse_overlay_lines, parse_scoped_lines, render_overlay_line, Board,
    CamPort, LineScope,
};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(repo().join("fixtures/camera_line").join(name)).unwrap()
}

const BENCH1: &str = "config-bench1-imx290.txt";
const BENCH2: &str = "config-bench2-imx335-imx219.txt";
const CM4: &str = "config-cm4-imx708.txt";

const MODEL_CM4: &str = "Raspberry Pi Compute Module 4 Rev 1.1\0";
const MODEL_CM5: &str = "Raspberry Pi Compute Module 5 Rev 1.0\0";

/// One fake boot partition: `config.txt`, `overlays/*.dtbo`, a model file.
struct Boot {
    dir: PathBuf,
}

struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Boot {
    fn new(config: &str, model: Option<&str>) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "xg-camcfg-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("overlays")).unwrap();
        for o in [
            "imx290",
            "imx219",
            "imx708",
            "imx335",
            "imx477",
            "vc4-kms-v3d",
        ] {
            std::fs::write(dir.join("overlays").join(format!("{o}.dtbo")), b"dtbo").unwrap();
        }
        std::fs::write(dir.join("config.txt"), config).unwrap();
        if let Some(m) = model {
            std::fs::write(dir.join("model"), m).unwrap();
        }
        Self { dir }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config.txt")
    }

    fn read(&self) -> String {
        String::from_utf8(std::fs::read(self.config()).unwrap()).unwrap()
    }

    fn backup(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("config.txt.bak-camcfg")).ok()
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg(repo().join("system/xorgate-camcfg"))
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("XORGATE_BOOT_CONFIG", self.config())
            .env(
                "XORGATE_CAMCFG_OVERLAYS",
                repo().join("system/xorgate-camcfg.overlays"),
            )
            .env("XORGATE_CAMCFG_MODEL", self.dir.join("model"))
            .env("XORGATE_CAMCFG_LOCK", self.dir.join("lock"));
        c
    }

    fn run(&self, args: &[&str]) -> Ran {
        ran(self.cmd().args(args))
    }

    /// No temp file, verdict file or diff file is ever left behind.
    fn litter(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| {
                !matches!(
                    n.as_str(),
                    "config.txt" | "config.txt.bak-camcfg" | "overlays" | "model" | "lock"
                )
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for Boot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn ran(c: &mut Command) -> Ran {
    let o = c.output().unwrap();
    Ran {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// The lines of `after` that differ from `before`, as `(line number, before,
/// after)`; the two files must have the same number of lines.
fn changed_lines(before: &str, after: &str) -> Vec<(usize, String, String)> {
    let (b, a): (Vec<&str>, Vec<&str>) =
        (before.split('\n').collect(), after.split('\n').collect());
    assert_eq!(b.len(), a.len(), "line count changed");
    b.iter()
        .zip(&a)
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .map(|(i, (x, y))| (i + 1, x.to_string(), y.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// the three benches
// ---------------------------------------------------------------------------

/// The control bench: two token-less IMX290 lines. Asking for what the file
/// already has writes nothing at all; a real change edits exactly one line.
#[test]
fn bench1_two_imx290_lines() {
    let original = fixture(BENCH1);
    let b = Boot::new(&original, Some(MODEL_CM5));
    // Setting 180 changes exactly the cam0 line.
    let r = b.run(&["rotate", "cam0", "imx290", "180"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        format!(
            "{}: cam0: dtoverlay=imx290,cam0,rotation=180 (applies on next boot)\n",
            b.config().display()
        )
    );
    assert_eq!(
        changed_lines(&original, &b.read()),
        [(
            52,
            "dtoverlay=imx290,cam0".to_string(),
            "dtoverlay=imx290,cam0,rotation=180".to_string()
        )]
    );
    // The backup is the file as it was, taken once.
    assert_eq!(b.backup().as_deref(), Some(original.as_str()));
    // Idempotent: the same call again writes nothing (exit 3).
    let after = b.read();
    let r = b.run(&["rotate", "cam0", "imx290", "180"]);
    assert_eq!((r.code, r.stdout.as_str(), r.stderr.as_str()), (3, "", ""));
    assert_eq!(b.read(), after);
    // Back to 0: an EXPLICIT rotation=0 (bug B2), not a dropped token.
    let r = b.run(&["rotate", "cam0", "imx290", "0"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        changed_lines(&original, &b.read()),
        [(
            52,
            "dtoverlay=imx290,cam0".to_string(),
            "dtoverlay=imx290,cam0,rotation=0".to_string()
        )]
    );
    assert_eq!(
        b.backup().as_deref(),
        Some(original.as_str()),
        "never overwritten"
    );
    // cam1 is the OTHER line, selected by having no cam0 token.
    let r = b.run(&["rotate", "cam1", "imx290", "180"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let lines = changed_lines(&original, &b.read());
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[1],
        (
            53,
            "dtoverlay=imx290,cam1".to_string(),
            "dtoverlay=imx290,cam1,rotation=180".to_string()
        )
    );
    // The commented-out soundcard overlay at the end is still a comment.
    assert!(b.read().ends_with("#dtoverlay=googlevoicehat-soundcard\n"));
    assert_eq!(b.litter(), Vec::<String>::new());
}

/// The CM4 with an IMX708: `dtoverlay=imx708,cam0,rotation=180` under `[all]`.
#[test]
fn cm4_imx708_reaches_both_rotations() {
    let original = fixture(CM4);
    let b = Boot::new(&original, Some(MODEL_CM4));
    // Already 180: nothing written.
    let r = b.run(&["rotate", "cam0", "imx708", "180"]);
    assert_eq!(r.code, 3);
    assert_eq!(b.read(), original);
    assert_eq!(b.backup(), None, "no write, no backup");
    let r = b.run(&["rotate", "cam0", "imx708", "0"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        changed_lines(&original, &b.read()),
        [(
            53,
            "dtoverlay=imx708,cam0,rotation=180".to_string(),
            "dtoverlay=imx708,cam0,rotation=0".to_string()
        )]
    );
    let r = b.run(&["rotate", "cam0", "imx708", "180"]);
    assert_eq!(r.code, 0);
    assert_eq!(b.read(), original, "and back, byte for byte");
    // The port has no other camera, and the line names imx708.
    let r = b.run(&["rotate", "cam1", "imx708", "0"]);
    assert_eq!(r.code, 4);
    assert_eq!(
        r.stderr,
        format!(
            "no camera dtoverlay line for cam1 in {}\n",
            b.config().display()
        )
    );
    let r = b.run(&["rotate", "cam0", "imx290", "0"]);
    assert_eq!(r.code, 5);
    assert_eq!(
        r.stderr,
        format!(
            "the camera line of cam0 names imx708, not imx290 ({} not changed)\n",
            b.config().display()
        )
    );
    assert_eq!(b.read(), original);
}

/// Bench 2: `imx335,cam0` (no token) and `imx219,cam1,rotation=180`.
#[test]
fn bench2_imx335_and_imx219() {
    let original = fixture(BENCH2);
    let b = Boot::new(&original, Some(MODEL_CM5));
    let r = b.run(&["rotate", "cam1", "imx219", "0"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        changed_lines(&original, &b.read()),
        [(
            53,
            "dtoverlay=imx219,cam1,rotation=180".to_string(),
            "dtoverlay=imx219,cam1,rotation=0".to_string()
        )]
    );
    let r = b.run(&["rotate", "cam1", "imx219", "180"]);
    assert_eq!(r.code, 0);
    assert_eq!(b.read(), original);
    // The agent never rotates an imx335 (it cannot flip); the helper would
    // only add the token, and only to that line.
    let r = b.run(&["rotate", "cam0", "imx335", "180"]);
    assert_eq!(r.code, 0);
    assert_eq!(
        changed_lines(&original, &b.read()),
        [(
            52,
            "dtoverlay=imx335,cam0".to_string(),
            "dtoverlay=imx335,cam0,rotation=180".to_string()
        )]
    );
}

// ---------------------------------------------------------------------------
// the shape of the file: CRLF, comments, whitespace, trailing newline
// ---------------------------------------------------------------------------

#[test]
fn every_other_byte_of_the_file_is_kept() {
    // CRLF line endings, a tab-indented line, a commented camera line, a
    // blank line with spaces, non-camera overlays, and no trailing newline.
    let original = "# head\r\n\tdtparam=audio=on\r\ndtoverlay=vc4-kms-v3d\r\n   \r\n#dtoverlay=imx290,cam0,rotation=180\r\n  dtoverlay=imx290 , cam0 ,4lane\r\ndtoverlay=dwc2,dr_mode=host\r\n[all]\r\n# tail";
    let b = Boot::new(original, Some(MODEL_CM5));
    let r = b.run(&["rotate", "cam0", "imx290", "180"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let want = original.replace(
        "  dtoverlay=imx290 , cam0 ,4lane\r\n",
        "dtoverlay=imx290,cam0,4lane,rotation=180\r\n",
    );
    assert_eq!(
        b.read(),
        want,
        "the CR stays, nothing else moves, no newline is added"
    );
    assert!(!b.read().ends_with('\n'));
    // A file that ends in a newline keeps it; the last line may be the
    // camera line itself, with or without one.
    for (text, want) in [
        (
            "dtoverlay=imx708,cam0\n",
            "dtoverlay=imx708,cam0,rotation=0\n",
        ),
        ("dtoverlay=imx708,cam0", "dtoverlay=imx708,cam0,rotation=0"),
        (
            "dtoverlay=imx708,cam0\r\n",
            "dtoverlay=imx708,cam0,rotation=0\r\n",
        ),
        (
            "dtoverlay=imx708,cam0\r",
            "dtoverlay=imx708,cam0,rotation=0\r",
        ),
        (
            "\n\ndtoverlay=imx708,cam0\n\n\n",
            "\n\ndtoverlay=imx708,cam0,rotation=0\n\n\n",
        ),
    ] {
        let b = Boot::new(text, Some(MODEL_CM5));
        let r = b.run(&["rotate", "cam0", "imx708", "0"]);
        assert_eq!(r.code, 0, "{text:?}: {}", r.stderr);
        assert_eq!(b.read(), want, "{text:?}");
    }
    // An empty file has no camera line.
    let b = Boot::new("", Some(MODEL_CM5));
    assert_eq!(b.run(&["rotate", "cam0", "imx708", "0"]).code, 4);
    assert_eq!(b.read(), "");
}

/// "Change only the rotation token: every other token is preserved verbatim
/// and in order", and the helper writes exactly what the agent's pure
/// renderer (`boot_config::line_with_rotation`) predicts.
#[test]
fn rotate_matches_the_agents_renderer_line_for_line() {
    let lines = [
        "dtoverlay=imx290,cam0",
        "dtoverlay=imx290,cam0,rotation=180",
        "dtoverlay=imx290,cam0,rotation=0",
        "dtoverlay=imx290,rotation=180,cam0",
        "dtoverlay=imx290,clock-frequency=74250000,cam0,4lane,media-controller=0,future-thing",
        "dtoverlay=imx290,clock-frequency=74250000,rotation=0,cam0,4lane,mono",
        "dtoverlay=imx708,link-frequency=447000000,cam0,vcm=off",
        "dtoverlay=imx708,cam0,rotation=90",
        "dtoverlay=imx708,cam0,rotation=",
        "dtoverlay=imx708,cam0,rotation=0,rotation=180",
        "dtoverlay=imx708,rotation=180,cam0,rotation=180",
        "  dtoverlay=imx219 ,cam0,, rotation=180 ",
        "dtoverlay=imx477,cam0,always-on,sync-source",
        "dtoverlay=imx335,cam0,4lane,link-frequency=445500000",
    ];
    let mut wrote = 0;
    for text in lines {
        for rot in [0u16, 180] {
            let parsed = parse_overlay_lines(text).remove(0);
            let want = line_with_rotation(&parsed, rot).unwrap();
            let b = Boot::new(&format!("# x\n{text}\n# y\n"), Some(MODEL_CM5));
            let r = b.run(&["rotate", "cam0", &parsed.name, &rot.to_string()]);
            let already = parsed
                .params
                .iter()
                .filter(|p| p.starts_with("rotation="))
                .count()
                == 1
                && parsed.params.contains(&format!("rotation={rot}"));
            if already {
                // The one case with no write: exactly one token, the right one.
                assert_eq!(r.code, 3, "{text} -> {rot}: {}", r.stderr);
                assert_eq!(b.read(), format!("# x\n{text}\n# y\n"));
            } else {
                assert_eq!(r.code, 0, "{text} -> {rot}: {}", r.stderr);
                assert_eq!(b.read(), format!("# x\n{want}\n# y\n"), "{text} -> {rot}");
                wrote += 1;
            }
        }
    }
    assert_eq!(wrote, 23);
}

// ---------------------------------------------------------------------------
// zero, one, two lines; sections
// ---------------------------------------------------------------------------

#[test]
fn rotate_refuses_two_lines_on_one_port_and_a_line_it_cannot_write_back() {
    // A sensor swap left two lines on CAM0.
    let two = "dtoverlay=imx290,cam0\ndtoverlay=imx708,cam0\ndtoverlay=imx219\n";
    let b = Boot::new(two, Some(MODEL_CM5));
    for overlay in ["imx290", "imx708"] {
        let r = b.run(&["rotate", "cam0", overlay, "180"]);
        assert_eq!(r.code, 5);
        assert_eq!(
            r.stderr,
            format!(
                "2 camera lines configure cam0; remove the stale ones first ({} not changed)\n",
                b.config().display()
            )
        );
    }
    // cam1 has exactly one (the token-less imx219) and is not affected.
    assert_eq!(b.run(&["rotate", "cam1", "imx219", "0"]).code, 0);
    assert_eq!(
        b.read(),
        "dtoverlay=imx290,cam0\ndtoverlay=imx708,cam0\ndtoverlay=imx219,rotation=0\n"
    );
    // A token outside the strict charset cannot be written back unchanged.
    for bad in [
        "dtoverlay=imx290,cam0,x=a b",
        "dtoverlay=imx290,cam0,x=$(reboot)",
        "dtoverlay=imx290,cam0,X=1",
        "dtoverlay=imx290,cam0,x=1 # front camera",
        "dtoverlay=imx290,cam0,a=b=c",
    ] {
        let b = Boot::new(&format!("{bad}\n"), Some(MODEL_CM5));
        let r = b.run(&["rotate", "cam0", "imx290", "180"]);
        assert_eq!(r.code, 5, "{bad}");
        assert!(
            r.stderr.starts_with(
                "the camera line of cam0 holds characters this helper does not write back"
            ),
            "{bad}: {}",
            r.stderr
        );
        assert_eq!(b.read(), format!("{bad}\n"));
        assert_eq!(b.backup(), None);
    }
    // A line that would grow past the firmware's 98 characters.
    let long = format!(
        "dtoverlay=imx290,cam0,{}",
        ["abcdefgh=12345678"; 4].join(",")
    );
    assert_eq!(long.len(), 93);
    let b = Boot::new(&format!("{long}\n"), Some(MODEL_CM5));
    let r = b.run(&["rotate", "cam0", "imx290", "180"]);
    assert_eq!(r.code, 5);
    assert!(r
        .stderr
        .starts_with("the line would be 106 characters; the firmware reads at most 98"));
    assert_eq!(b.read(), format!("{long}\n"));
    // Exactly 98 is written.
    let fits = format!(
        "dtoverlay=imx290,cam0,{},abcdefghijk",
        ["abcdefgh=12345678"; 3].join(",")
    );
    assert_eq!(fits.len() + ",rotation=0".len(), 98);
    let b = Boot::new(&format!("{fits}\n"), Some(MODEL_CM5));
    let r = b.run(&["rotate", "cam0", "imx290", "0"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(b.read().trim_end().len(), 98);
}

/// Q6: an overlay outside the allow-list is a port's camera line only with
/// an explicit port token; other overlays are nobody's.
#[test]
fn an_overlay_outside_the_allow_list_needs_an_explicit_port_token() {
    let text = "dtoverlay=vc4-kms-v3d\ndtoverlay=dwc2,dr_mode=host\ndtoverlay=imx477\n";
    let b = Boot::new(text, Some(MODEL_CM5));
    for (port, overlay) in [
        ("cam1", "imx477"),
        ("cam1", "vc4-kms-v3d"),
        ("cam0", "vc4-kms-v3d"),
        ("cam1", "dwc2"),
    ] {
        let r = b.run(&["rotate", port, overlay, "180"]);
        assert_eq!(r.code, 4, "{port} {overlay}: {}", r.stderr);
    }
    assert_eq!(b.read(), text);
    for (line, port, want) in [
        (
            "dtoverlay=imx477,cam0",
            "cam0",
            "dtoverlay=imx477,cam0,rotation=0",
        ),
        (
            "dtoverlay=imx477,cam1",
            "cam1",
            "dtoverlay=imx477,cam1,rotation=0",
        ),
        (
            "dtoverlay=imx477,cam0,rotation=180",
            "cam0",
            "dtoverlay=imx477,cam0,rotation=0",
        ),
    ] {
        let b = Boot::new(&format!("dtoverlay=vc4-kms-v3d\n{line}\n"), Some(MODEL_CM5));
        let r = b.run(&["rotate", port, "imx477", "0"]);
        assert_eq!(r.code, 0, "{line}: {}", r.stderr);
        assert_eq!(b.read(), format!("dtoverlay=vc4-kms-v3d\n{want}\n"));
    }
    // `cam0,cam1` on such a line is CAM0's.
    let b = Boot::new("dtoverlay=imx477,cam0,cam1\n", Some(MODEL_CM5));
    assert_eq!(b.run(&["rotate", "cam1", "imx477", "0"]).code, 4);
    assert_eq!(b.run(&["rotate", "cam0", "imx477", "0"]).code, 0);
}

/// Q4: THE SECTION RULE. The helper and the agent's reader
/// (`boot_config::parse_scoped_lines`) classify every line alike, on a CM4,
/// a CM5 and a board the model file does not name.
#[test]
fn section_filters_agree_with_the_agents_reader() {
    let headers = [
        "",
        "[all]",
        "[cm4]",
        "[cm5]",
        "[pi4]",
        "[pi5]",
        "[none]",
        "[pi3]",
        "[cm4s]",
        "[gpio4=1]",
        "[EDID=VSC-TD2220]",
        "[tryboot]",
        "[0x12345678]",
        "[ALL]",
        "[ all ]",
        "[all] # x",
        "  [cm5]  ",
        "#[cm4]",
    ];
    let boards = [
        (Some(MODEL_CM4), Board::Cm4),
        (Some(MODEL_CM5), Board::Cm5),
        (
            Some("Raspberry Pi Compute Module 4S Rev 1.0\0"),
            Board::Unknown,
        ),
        (Some("Raspberry Pi 5 Model B Rev 1.0\0"), Board::Unknown),
        (None, Board::Unknown),
    ];
    let mut seen = [0usize; 3];
    for (model, board) in boards {
        // One header, and two in a row (a model filter replaces a model
        // filter; any other filter sticks until [all]).
        let mut cases: Vec<String> = headers.iter().map(|h| format!("{h}\n")).collect();
        for a in ["[cm4]", "[cm5]", "[gpio4=1]", "[none]", "[all]"] {
            for b2 in ["[cm4]", "[cm5]", "[pi5]", "[all]", "[tryboot]"] {
                cases.push(format!("{a}\ndtparam=x=1\n{b2}\n"));
            }
        }
        for head in cases {
            let text = format!("{head}dtoverlay=imx290,cam0\n");
            let scope = parse_scoped_lines(&text, board)[0].scope;
            let b = Boot::new(&text, model);
            let r = b.run(&["rotate", "cam0", "imx290", "180"]);
            let (want, i) = match scope {
                LineScope::Applies => (0, 0),
                LineScope::NotApplies => (4, 1),
                LineScope::Unknown => (5, 2),
            };
            seen[i] += 1;
            assert_eq!(
                r.code, want,
                "{board:?} {head:?} (agent: {scope:?}): {}",
                r.stderr
            );
            if want == 0 {
                assert_eq!(
                    b.read(),
                    format!("{head}dtoverlay=imx290,cam0,rotation=180\n")
                );
            } else {
                assert_eq!(b.read(), text, "{board:?} {head:?}");
            }
            if want == 5 {
                assert!(
                    r.stderr
                        .contains("a section this helper cannot evaluate for this board"),
                    "{}",
                    r.stderr
                );
            }
        }
    }
    assert!(seen.iter().all(|n| *n > 10), "{seen:?}");
    // The classic two-section file: each board edits its own line only.
    let text = "[cm4]\ndtoverlay=imx708,cam0\n[cm5]\ndtoverlay=imx290,cam0\n[all]\n";
    let b = Boot::new(text, Some(MODEL_CM5));
    assert_eq!(b.run(&["rotate", "cam0", "imx290", "180"]).code, 0);
    assert_eq!(
        b.read(),
        "[cm4]\ndtoverlay=imx708,cam0\n[cm5]\ndtoverlay=imx290,cam0,rotation=180\n[all]\n"
    );
    let b = Boot::new(text, Some(MODEL_CM4));
    assert_eq!(b.run(&["rotate", "cam0", "imx708", "0"]).code, 0);
    assert_eq!(
        b.read(),
        "[cm4]\ndtoverlay=imx708,cam0,rotation=0\n[cm5]\ndtoverlay=imx290,cam0\n[all]\n"
    );
    // The board is not known: both may apply, so neither is touched.
    let b = Boot::new(text, None);
    assert_eq!(b.run(&["rotate", "cam0", "imx290", "180"]).code, 5);
    assert_eq!(b.read(), text);
}

// ---------------------------------------------------------------------------
// set and remove (Phase 2's verbs)
// ---------------------------------------------------------------------------

#[test]
fn set_replaces_adds_and_collapses_the_ports_line() {
    // One line: replaced in place, with the canonical render.
    let original = fixture(BENCH1);
    let b = Boot::new(&original, Some(MODEL_CM5));
    let r = b.run(&[
        "set",
        "cam0",
        "imx708",
        "link-frequency=447000000",
        "rotation=180",
    ]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let want = render_overlay_line(
        "imx708",
        CamPort::Cam0,
        &["link-frequency=447000000".to_string()],
        180,
    )
    .unwrap();
    assert_eq!(
        want,
        "dtoverlay=imx708,cam0,link-frequency=447000000,rotation=180"
    );
    assert_eq!(
        changed_lines(&original, &b.read()),
        [(52, "dtoverlay=imx290,cam0".to_string(), want.clone())]
    );
    assert_eq!(b.backup().as_deref(), Some(original.as_str()));
    // Idempotent.
    let r = b.run(&[
        "set",
        "cam0",
        "imx708",
        "link-frequency=447000000",
        "rotation=180",
    ]);
    assert_eq!(r.code, 3);
    // CAM1's line, written with the (firmware-skipped) cam1 token.
    let r = b.run(&["set", "cam1", "imx219", "rotation=0"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(b.read().contains("\ndtoverlay=imx219,cam1,rotation=0\n"));
    assert!(!b.read().contains("dtoverlay=imx290"));

    // No line for the port: appended, under [all] when the file does not
    // end in a section that applies.
    for (text, want) in [
        (
            "dtoverlay=vc4-kms-v3d\n[cm5]\ndtoverlay=dwc2\n",
            "dtoverlay=vc4-kms-v3d\n[cm5]\ndtoverlay=dwc2\n[all]\ndtoverlay=imx290,cam0,rotation=0\n",
        ),
        (
            "dtoverlay=vc4-kms-v3d\n[all]\n",
            "dtoverlay=vc4-kms-v3d\n[all]\ndtoverlay=imx290,cam0,rotation=0\n",
        ),
        // No trailing newline: one is added before the new line.
        (
            "dtoverlay=vc4-kms-v3d",
            "dtoverlay=vc4-kms-v3d\ndtoverlay=imx290,cam0,rotation=0\n",
        ),
        ("", "dtoverlay=imx290,cam0,rotation=0\n"),
        // CRLF file: the new lines end the same way.
        (
            "dtoverlay=vc4-kms-v3d\r\n[gpio4=1]\r\n",
            "dtoverlay=vc4-kms-v3d\r\n[gpio4=1]\r\n[all]\r\ndtoverlay=imx290,cam0,rotation=0\r\n",
        ),
    ] {
        let b = Boot::new(text, Some(MODEL_CM4));
        let r = b.run(&["set", "cam0", "imx290", "rotation=0"]);
        assert_eq!(r.code, 0, "{text:?}: {}", r.stderr);
        assert_eq!(b.read(), want, "{text:?}");
    }

    // Two lines on the port (a swap that appended): the LAST is replaced,
    // the other deleted. The other port's line is untouched.
    let two = "dtoverlay=imx290,cam0\n# keep\ndtoverlay=imx219,cam1\ndtoverlay=imx335,cam0,4lane\n";
    let b = Boot::new(two, Some(MODEL_CM5));
    let r = b.run(&["set", "cam0", "imx708", "rotation=180"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        b.read(),
        "# keep\ndtoverlay=imx219,cam1\ndtoverlay=imx708,cam0,rotation=180\n"
    );

    // `camera_auto_detect=1` becomes 0 (and only an uncommented one).
    let b = Boot::new(
        "#camera_auto_detect=1\ncamera_auto_detect=1\ndisplay_auto_detect=1\ndtoverlay=imx290,cam0,rotation=0\n",
        Some(MODEL_CM5),
    );
    let r = b.run(&["set", "cam0", "imx290", "rotation=0"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        b.read(),
        "#camera_auto_detect=1\ncamera_auto_detect=0\ndisplay_auto_detect=1\ndtoverlay=imx290,cam0,rotation=0\n"
    );
    // `rotate` leaves camera_auto_detect alone.
    let b = Boot::new(
        "camera_auto_detect=1\ndtoverlay=imx290,cam0\n",
        Some(MODEL_CM5),
    );
    assert_eq!(b.run(&["rotate", "cam0", "imx290", "180"]).code, 0);
    assert_eq!(
        b.read(),
        "camera_auto_detect=1\ndtoverlay=imx290,cam0,rotation=180\n"
    );

    // A line under a filter it cannot evaluate: refused, whatever the verb.
    let unknown = "[gpio4=1]\ndtoverlay=imx290,cam0\n[all]\n";
    for args in [
        vec!["set", "cam0", "imx708", "rotation=0"],
        vec!["remove", "cam0"],
        vec!["rotate", "cam0", "imx290", "0"],
    ] {
        let b = Boot::new(unknown, Some(MODEL_CM5));
        assert_eq!(b.run(&args).code, 5, "{args:?}");
        assert_eq!(b.read(), unknown);
    }
    // A line under the OTHER board's filter is not this port's: `set` adds
    // one that applies and leaves that one alone.
    let b = Boot::new("[cm4]\ndtoverlay=imx708,cam0\n[all]\n", Some(MODEL_CM5));
    assert_eq!(b.run(&["set", "cam0", "imx290", "rotation=0"]).code, 0);
    assert_eq!(
        b.read(),
        "[cm4]\ndtoverlay=imx708,cam0\n[all]\ndtoverlay=imx290,cam0,rotation=0\n"
    );
}

#[test]
fn set_takes_allow_listed_overlays_that_exist_on_the_device() {
    let original = fixture(BENCH1);
    let b = Boot::new(&original, Some(MODEL_CM5));
    // Not in the allow-list (a real overlay, a real file: still refused).
    for overlay in ["imx477", "vc4-kms-v3d", "dwc2"] {
        let r = b.run(&["set", "cam0", overlay, "rotation=0"]);
        assert_eq!(r.code, 2, "{overlay}");
        assert!(r
            .stderr
            .contains(&format!("the overlay {overlay} is not in the allow-list")));
    }
    // In the allow-list, but this device has no such overlay file.
    std::fs::remove_file(b.dir.join("overlays/imx708.dtbo")).unwrap();
    let r = b.run(&["set", "cam0", "imx708", "rotation=0"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.starts_with("no overlay file "), "{}", r.stderr);
    // No allow-list at all: every verb refuses.
    for args in [
        vec!["set", "cam0", "imx290", "rotation=0"],
        vec!["rotate", "cam0", "imx290", "180"],
        vec!["remove", "cam0"],
    ] {
        let r = ran(b
            .cmd()
            .env("XORGATE_CAMCFG_OVERLAYS", b.dir.join("nope"))
            .args(&args));
        assert_eq!(r.code, 2, "{args:?}");
        assert!(
            r.stderr.contains("no overlay allow-list at"),
            "{}",
            r.stderr
        );
    }
    // A corrupt allow-list is not trusted.
    std::fs::write(b.dir.join("bad-list"), "imx290\n../../etc/passwd\n").unwrap();
    let r = ran(b
        .cmd()
        .env("XORGATE_CAMCFG_OVERLAYS", b.dir.join("bad-list"))
        .args(["rotate", "cam0", "imx290", "180"]));
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("holds a bad entry"), "{}", r.stderr);
    // A line longer than the firmware reads.
    let many: Vec<String> = (0..6).map(|i| format!("p{i:02}=12345678")).collect();
    let mut args = vec!["set", "cam0", "imx290"];
    args.extend(many.iter().map(String::as_str));
    args.push("rotation=0");
    let r = b.run(&args);
    assert_eq!(r.code, 2);
    assert_eq!(
        r.stderr,
        "the line would be 110 characters; the firmware reads at most 98\n"
    );
    assert_eq!(b.read(), original, "none of it changed the file");
    assert_eq!(b.backup(), None);
    assert_eq!(b.litter(), vec!["bad-list".to_string()]);
}

#[test]
fn remove_deletes_only_the_ports_camera_lines() {
    let original = fixture(BENCH2);
    let b = Boot::new(&original, Some(MODEL_CM5));
    let r = b.run(&["remove", "cam0"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(b.read(), original.replace("dtoverlay=imx335,cam0\n", ""));
    assert_eq!(b.run(&["remove", "cam0"]).code, 3, "nothing left to remove");
    let r = b.run(&["remove", "cam1"]);
    assert_eq!(r.code, 0);
    assert_eq!(
        b.read(),
        original
            .replace("dtoverlay=imx335,cam0\n", "")
            .replace("dtoverlay=imx219,cam1,rotation=180\n", "")
    );
    assert!(b.read().contains("dtoverlay=vc4-kms-v3d\n"));
    assert!(b.read().contains("dtoverlay=dwc2,dr_mode=host\n"));
    // Both of two lines go; the file may end up empty.
    let b = Boot::new(
        "dtoverlay=imx290,cam0\ndtoverlay=imx708,cam0",
        Some(MODEL_CM5),
    );
    assert_eq!(b.run(&["remove", "cam0"]).code, 0);
    assert_eq!(b.read(), "");
}

// ---------------------------------------------------------------------------
// hostile arguments
// ---------------------------------------------------------------------------

const USAGE: &str = "usage: xorgate-camcfg rotate <cam0|cam1> <overlay> <0|180> | set <cam0|cam1> <overlay> [param...] rotation=<0|180> | remove <cam0|cam1>\n";

/// Every argument is hostile: nothing below changes a byte of the file, and
/// each is answered with the usage line (exit 2).
#[test]
fn hostile_arguments_are_refused_and_never_written() {
    assert_eq!(xorgate_core::boot_config::CAMCFG_USAGE, USAGE.trim_end());
    let original = fixture(BENCH1);
    let b = Boot::new(&original, Some(MODEL_CM5));
    let nl_line = "imx290\ndtoverlay=evil";
    let nl_param = "x=1\ndtoverlay=evil,cam0";
    let long = "a".repeat(33);
    let long_v = format!("x={}", "1".repeat(33));
    let cases: Vec<Vec<&str>> = vec![
        vec![],
        vec!["rotate"],
        vec!["reboot"],
        vec!["--help"],
        vec!["rotate", "cam0"],
        vec!["rotate", "cam0", "imx290"],
        vec!["rotate", "cam0", "imx290", "180", "extra"],
        vec!["rotate", "cam2", "imx290", "180"],
        vec!["rotate", "cam0;reboot", "imx290", "180"],
        vec!["rotate", "csi0", "imx290", "180"],
        vec!["rotate", "", "imx290", "180"],
        vec!["rotate", "cam0", "", "180"],
        vec!["rotate", "cam0", "IMX290", "180"],
        vec!["rotate", "cam0", "imx 290", "180"],
        vec!["rotate", "cam0", "imx290,cam0", "180"],
        vec!["rotate", "cam0", "../../etc/passwd", "180"],
        vec!["rotate", "cam0", "imx290;reboot", "180"],
        vec!["rotate", "cam0", "$(reboot)", "180"],
        vec!["rotate", "cam0", "`reboot`", "180"],
        vec!["rotate", "cam0", "-n", "180"],
        vec!["rotate", "cam0", "_x", "180"],
        vec!["rotate", "cam0", nl_line, "180"],
        vec!["rotate", "cam0", &long, "180"],
        vec!["rotate", "cam0", "imx290", "90"],
        vec!["rotate", "cam0", "imx290", "180 "],
        vec!["rotate", "cam0", "imx290", "0x0"],
        vec!["rotate", "cam0", "imx290", "-0"],
        vec!["rotate", "cam0", "imx290", ""],
        vec!["rotate", "cam0", "imx290", "180\nrotation=0"],
        vec!["set", "cam0", "imx290"],
        vec!["set", "cam0", "imx290", "rotation=90"],
        vec!["set", "cam0", "imx290", "rotation"],
        vec!["set", "cam0", "imx290", "4lane"],
        vec!["set", "cam0", "imx290", "rotation=0", "4lane"],
        vec!["set", "cam0", "imx290", "rotation=0", "rotation=180"],
        vec!["set", "cam0", "imx290", "cam0", "rotation=0"],
        vec!["set", "cam0", "imx290", "cam1", "rotation=0"],
        vec!["set", "cam0", "imx290", "rotation", "rotation=0"],
        vec!["set", "cam0", "imx290", "a b", "rotation=0"],
        vec!["set", "cam0", "imx290", "x=1,y=2", "rotation=0"],
        vec!["set", "cam0", "imx290", "x=$(reboot)", "rotation=0"],
        vec!["set", "cam0", "imx290", "x=`id`", "rotation=0"],
        vec!["set", "cam0", "imx290", "x=1;reboot", "rotation=0"],
        vec!["set", "cam0", "imx290", "x=", "rotation=0"],
        vec!["set", "cam0", "imx290", "=1", "rotation=0"],
        vec!["set", "cam0", "imx290", "X=1", "rotation=0"],
        vec!["set", "cam0", "imx290", "a=b=c", "rotation=0"],
        vec!["set", "cam0", "imx290", "[all]", "rotation=0"],
        vec!["set", "cam0", "imx290", "#x", "rotation=0"],
        vec!["set", "cam0", "imx290", "x='1'", "rotation=0"],
        vec!["set", "cam0", "imx290", "x=\"1\"", "rotation=0"],
        vec!["set", "cam0", "imx290", "x=\\n", "rotation=0"],
        vec!["set", "cam0", "imx290", nl_param, "rotation=0"],
        vec!["set", "cam0", "imx290", &long_v, "rotation=0"],
        vec!["set", "cam0", "imx290", "", "rotation=0"],
        vec!["set", "cam0", "IMX290", "rotation=0"],
        vec!["set", "cam2", "imx290", "rotation=0"],
        vec!["remove"],
        vec!["remove", "cam2"],
        vec!["remove", "cam0", "cam1"],
        vec!["remove", "all"],
    ];
    assert!(cases.len() >= 60);
    for args in &cases {
        let r = b.run(args);
        assert_eq!(r.code, 2, "{args:?}: {} {}", r.stdout, r.stderr);
        assert_eq!(r.stderr, USAGE, "{args:?}");
        assert_eq!(r.stdout, "", "{args:?}");
    }
    assert_eq!(digest(&b.read()), digest(&original));
    assert_eq!(b.backup(), None);
    assert_eq!(b.litter(), Vec::<String>::new());
    assert!(!b.dir.join("evil").exists());
}

/// The test seams are an environment matter, and sudo resets the
/// environment. Belt and braces: started through sudo (`SUDO_UID` set, as
/// sudo always sets it) the helper ignores them and looks at the real boot
/// config, which a test host does not have.
#[test]
fn the_test_seams_are_ignored_under_sudo() {
    if Path::new("/boot/firmware/config.txt").exists() || Path::new("/boot/config.txt").exists() {
        return; // a real Pi: never point the helper at the real boot config
    }
    let original = fixture(BENCH1);
    let b = Boot::new(&original, Some(MODEL_CM5));
    let r = ran(b
        .cmd()
        .env("SUDO_UID", "1000")
        .args(["rotate", "cam0", "imx290", "180"]));
    assert_eq!(r.code, 2);
    assert_eq!(
        r.stderr,
        "no config.txt found (looked in /boot/firmware and /boot)\n"
    );
    assert_eq!(
        b.read(),
        original,
        "the fixture named by the seam was not touched"
    );
}

/// It never reboots, and it never calls anything that could.
#[test]
fn the_helper_never_contains_a_reboot() {
    let body = std::fs::read_to_string(repo().join("system/xorgate-camcfg")).unwrap();
    for line in body.lines() {
        let code = line.split('#').next().unwrap_or("");
        for word in code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
            assert!(
                !matches!(
                    word,
                    "reboot" | "shutdown" | "poweroff" | "halt" | "systemctl" | "init" | "kexec"
                ),
                "`{word}` in the helper: {line}"
            );
        }
    }
    // The JS agent's helper is still shipped, byte for byte (a device
    // reverted to the JS agent calls it).
    use sha2::{Digest, Sha256};
    let camrotate = std::fs::read(repo().join("system/xorgate-camrotate")).unwrap();
    assert_eq!(
        hex::encode(Sha256::digest(&camrotate)),
        "9ae4f79b1764122a6ac2186d8feeb708f6d70e00830200b674e5a2bd57ccf8ac"
    );
}
