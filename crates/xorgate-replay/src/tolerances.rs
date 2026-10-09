//! `tools/replay/tolerances.toml` — the tolerance table the diff cites.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Table {
    pub version: u32,
    #[serde(rename = "rule")]
    pub rules: Vec<Rule>,
}

/// Fields such as `basis`, `what`, `source` document a row for the reader of
/// the table and of diff.md; the comparator does not branch on them.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Rule {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub basis: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub streams: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub tolerance: BTreeMap<String, f64>,
    #[serde(default)]
    pub startup_ms: Option<u64>,
    #[serde(default)]
    pub window_ms: Option<u64>,
    #[serde(default)]
    pub max_count_delta: Option<usize>,
    #[serde(default)]
    pub quiet_ms: Option<u64>,
    #[serde(default)]
    pub event_prefixes: Vec<String>,
    #[serde(default)]
    pub sudo_argv_contains: Vec<String>,
    #[serde(default)]
    pub normalize_ids: Option<bool>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default, rename = "row")]
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Row {
    pub id: String,
    pub what: String,
    #[serde(default)]
    pub streams: Vec<String>,
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub pinned_by: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub normalize: BTreeMap<String, String>,
    #[serde(default)]
    pub around_closed_ms: Option<u64>,
    #[serde(default)]
    pub event_prefixes: Vec<String>,
    #[serde(default)]
    pub event_exact: Vec<String>,
    #[serde(default)]
    pub own_prefixes: Vec<String>,
}

const KINDS: &[&str] = &[
    "exclude",
    "deltas",
    "numeric",
    "presence_window",
    "report_count",
    "phase_owned",
    "import",
    "decisions",
    "strict_numbers",
];

impl Table {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let t: Table = toml::from_str(text).context("parsing tolerances.toml")?;
        if t.version != 1 {
            bail!("tolerances.toml version {} (want 1)", t.version);
        }
        for want in ["T1", "T2", "T3", "T4", "T5", "T6", "T7", "T8", "T9"] {
            if !t.rules.iter().any(|r| r.id == want) {
                bail!("tolerances.toml lacks rule {want} (phase file T1-T9)");
            }
        }
        for r in &t.rules {
            if !KINDS.contains(&r.kind.as_str()) {
                bail!("rule {}: unknown kind {:?}", r.id, r.kind);
            }
            if r.kind == "import" {
                for row in &r.rows {
                    if row.pinned_by.as_deref().unwrap_or("").is_empty() {
                        bail!("rule {}/{}: an imported row needs pinned_by", r.id, row.id);
                    }
                }
            }
        }
        Ok(t)
    }

    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// Rules of `kind` that apply to `stream`.
    pub fn of_kind<'a>(&'a self, kind: &'a str, stream: &'a str) -> impl Iterator<Item = &'a Rule> {
        self.rules
            .iter()
            .filter(move |r| r.kind == kind && stream_matches(&r.streams, stream))
    }

    /// The first rule/row id excluding `path` on `stream` (T1 exclude, T6
    /// phase-owned, T8 decision rows with paths).
    pub fn excluded(&self, stream: &str, path: &str) -> Option<String> {
        for r in &self.rules {
            if !matches!(r.kind.as_str(), "exclude" | "phase_owned") {
                continue;
            }
            if stream_matches(&r.streams, stream) && r.paths.iter().any(|p| path_matches(p, path)) {
                return Some(r.id.clone());
            }
        }
        for r in self.rules.iter().filter(|r| r.kind == "decisions") {
            for row in &r.rows {
                if stream_matches(&row.streams, stream)
                    && row.paths.iter().any(|p| path_matches(p, path))
                {
                    return Some(format!("{}/{}", r.id, row.id));
                }
            }
        }
        None
    }

    /// Numeric tolerance for `path` on `stream` (T3).
    pub fn numeric(&self, stream: &str, path: &str) -> Option<(String, f64)> {
        self.of_kind("numeric", stream)
            .find_map(|r| r.tolerance.get(path).map(|t| (r.id.clone(), *t)))
    }

    /// Paths compared as deltas (T2).
    pub fn is_delta(&self, stream: &str, path: &str) -> Option<String> {
        self.of_kind("deltas", stream)
            .find(|r| r.paths.iter().any(|p| path_matches(p, path)))
            .map(|r| r.id.clone())
    }

    /// The T8 row (if any) with this id.
    pub fn decision_row(&self, row_id: &str) -> Option<(String, &Row)> {
        self.rules
            .iter()
            .filter(|r| r.kind == "decisions" || r.kind == "import")
            .find_map(|r| {
                r.rows
                    .iter()
                    .find(|w| w.id == row_id)
                    .map(|w| (r.id.clone(), w))
            })
    }

    /// Rows with `matcher == m` applying to `stream`, as `T7/D1`-style ids.
    pub fn matcher_rows(&self, m: &str, stream: &str) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.rules {
            for row in &r.rows {
                if row.matcher.as_deref() == Some(m) && stream_matches(&row.streams, stream) {
                    out.push(format!("{}/{}", r.id, row.id));
                }
            }
        }
        out
    }
}

pub fn stream_matches(streams: &[String], stream: &str) -> bool {
    streams.is_empty() || streams.iter().any(|s| s == stream)
}

/// `*` matches within one segment (`*At`), `**` any number of segments.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = if pattern.is_empty() {
        vec![]
    } else {
        pattern.split('.').collect()
    };
    let s: Vec<&str> = if path.is_empty() {
        vec![]
    } else {
        path.split('.').collect()
    };
    match_segs(&p, &s)
}

fn match_segs(p: &[&str], s: &[&str]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((&"**", rest)) => (0..=s.len()).any(|i| match_segs(rest, &s[i..])),
        Some((first, rest)) => match s.split_first() {
            Some((seg, srest)) => seg_matches(first, seg) && match_segs(rest, srest),
            None => false,
        },
    }
}

fn seg_matches(p: &str, s: &str) -> bool {
    match p.split_once('*') {
        None => p == s,
        Some((pre, post)) => {
            s.len() >= pre.len() + post.len() && s.starts_with(pre) && s.ends_with(post)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        assert!(path_matches("ts", "ts"));
        assert!(!path_matches("ts", "latest.ts"));
        assert!(path_matches("**.ts", "latest.ts"));
        assert!(path_matches("**.ts", "ts"));
        assert!(path_matches("**.*At", "effective.sim.checkedAt"));
        assert!(path_matches("*At", "lastWriteAt"));
        assert!(!path_matches("*At", "a.lastWriteAt"));
        assert!(path_matches("media.**", "media.streams.0.state"));
        assert!(path_matches("media", "media"));
        assert!(path_matches("entries.*.at", "entries.3.at"));
    }

    #[test]
    fn the_repo_table_parses_and_cites() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/replay/tolerances.toml");
        let t = Table::load(&path).unwrap();
        assert_eq!(t.excluded("telemetry", "ts").as_deref(), Some("T1"));
        assert_eq!(
            t.excluded("telemetry", "media.kvs.value").as_deref(),
            Some("T6")
        );
        assert_eq!(
            t.excluded("state", "writtenBy").as_deref(),
            Some("T8/writtenBy")
        );
        assert_eq!(
            t.excluded("report", "effective.agent.build.version")
                .as_deref(),
            Some("T8/build")
        );
        assert_eq!(t.excluded("telemetry", "metrics.gps.lat.value"), None);
        assert_eq!(
            t.numeric("telemetry", "metrics.system.cpu_usage.value"),
            Some(("T3".into(), 10.0))
        );
        assert_eq!(
            t.numeric("telemetry", "metrics.system.throttled.value"),
            None
        );
        assert_eq!(t.is_delta("telemetry", "seq").as_deref(), Some("T2"));
        assert_eq!(
            t.matcher_rows("fffd_vs_lone_surrogate", "sms"),
            vec!["T7/D1"]
        );
        // Every Phase-1 accepted divergence is imported.
        let t7 = t.rule("T7").unwrap();
        for i in 1..=20 {
            assert!(t7.rows.iter().any(|r| r.id == format!("D{i}")), "D{i}");
        }
        assert!(
            Table::parse("version = 1\n[[rule]]\nid='T1'\ntitle='x'\nkind='exclude'\n").is_err()
        );
    }
}
