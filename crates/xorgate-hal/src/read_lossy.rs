//! `readFileSync(path, "utf8")` / `readFile(path, "utf8")` semantics (CF21,
//! common-brief rule 4): read the BYTES, then decode with U+FFFD replacement
//! (`String::from_utf8_lossy`, which is what Node's utf8 decoder does), then
//! let the caller parse. ENOENT is told apart from every other failure, which
//! the TS readers that care (`readSettingsFile`) distinguish by `err.code`.

use std::io;
use std::path::Path;

/// The outcome of a lossy text read.
#[derive(Debug)]
pub enum FileText {
    /// ENOENT.
    Absent,
    /// Any other read error (EACCES, EISDIR, EIO, …).
    Unreadable(io::Error),
    /// The decoded text.
    Text(String),
}

impl FileText {
    /// The text, or `None` for absent/unreadable (`try { … } catch { null }`).
    pub fn ok(self) -> Option<String> {
        match self {
            FileText::Text(t) => Some(t),
            _ => None,
        }
    }
}

fn classify(res: io::Result<Vec<u8>>) -> FileText {
    match res {
        Ok(bytes) => FileText::Text(decode(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => FileText::Absent,
        Err(e) => FileText::Unreadable(e),
    }
}

/// Bytes → `String`, invalid UTF-8 sequences replaced by U+FFFD.
pub fn decode(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// Blocking read (the TS `readFileSync` call sites).
pub fn read_text(path: &Path) -> FileText {
    classify(std::fs::read(path))
}

/// Async read (the TS `await readFile(…)` call sites).
pub async fn read_text_async(path: &Path) -> FileText {
    classify(tokio::fs::read(path).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_becomes_replacement_chars() {
        let dir = std::env::temp_dir().join(format!("xg-lossy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.json");
        std::fs::write(&p, b"{\"a\":\"\xff\xfe\"}").unwrap();
        match read_text(&p) {
            FileText::Text(t) => assert_eq!(t, "{\"a\":\"\u{fffd}\u{fffd}\"}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(read_text(&dir.join("missing")), FileText::Absent));
        assert!(matches!(read_text(&dir), FileText::Unreadable(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
