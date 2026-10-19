//! `xorgate-rs pipeline …` / `recording reset-counters` against the REAL TS
//! commands @89b5c64 (`src/commands/{pipeline,recording}.ts`):
//! `fixtures/cli_pipeline/cli-pipeline.json` (tsx-dumped by
//! `phase-4/evidence/wp5-tools/dump-cli-pipeline.mts`, for tools/replay/js/ via WP6) — per step stdout, stderr and exit
//! code, then the exact bytes of every file left in the case's root. Covers
//! the 6 CLI it()s of `test/pipeline.test.ts` (:51 :62 :69 :103 :124 :133)
//! plus list/show/env/usage/throw/UTF-8 cases and the `recording` golden.
//!
//! The one expected text difference is Q9's restart hint (D-row): the TS
//! names `xorgate-agent` + `xorgate restart`, the Rust `xorgate-agent-rs`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use xorgate_agent::cli::pipeline::{self, Captured, PipelineEnv, RESTART_HINT};
use xorgate_agent::cli::recording;

const TS_RESTART_HINT: &str =
    "  restart to apply: sudo systemctl restart xorgate-agent  (or: xorgate restart)";

#[derive(Deserialize)]
struct Fixture {
    #[serde(rename = "jsCommit")]
    js_commit: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    seed_hex: BTreeMap<String, String>,
    dirs: Vec<String>,
    env: BTreeMap<String, Option<String>>,
    steps: Vec<Step>,
    tree_after_hex: BTreeMap<String, Option<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Step {
    argv: Vec<String>,
    stdout: String,
    stderr: String,
    exit_code: i32,
}

fn tree(root: &Path, rel: &str, root_s: &str, out: &mut BTreeMap<String, Option<String>>) {
    let mut names: Vec<String> = std::fs::read_dir(root.join(rel))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for n in names {
        let r = if rel.is_empty() {
            n
        } else {
            format!("{rel}/{n}")
        };
        let p = root.join(&r);
        if p.is_dir() {
            out.insert(r.clone(), None);
            tree(root, &r, root_s, out);
        } else {
            // ROOT → "<ROOT>" at the byte level (the driver's latin1 round trip).
            let bytes = std::fs::read(&p).unwrap();
            let latin1: String = bytes.iter().map(|&b| b as char).collect();
            let replaced = latin1.replace(root_s, "<ROOT>");
            let back: Vec<u8> = replaced.chars().map(|c| c as u8).collect();
            out.insert(r, Some(hex::encode(back)));
        }
    }
}

#[test]
fn cli_pipeline_fixture() {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cli_pipeline/cli-pipeline.json"),
    )
    .unwrap();
    let fx: Fixture = serde_json::from_str(&text).unwrap();
    assert_eq!(fx.js_commit, "89b5c64");
    assert!(fx.cases.len() >= 19);
    let mut failures = Vec::new();
    for (i, c) in fx.cases.iter().enumerate() {
        let base = std::env::temp_dir().join(format!("xg-wp5-pipe-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let root = base.canonicalize().unwrap();
        let root_s = root.to_string_lossy().into_owned();
        for d in &c.dirs {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for (p, h) in &c.seed_hex {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, hex::decode(h).unwrap()).unwrap();
        }
        let env_of = |k: &str| c.env.get(k).cloned().flatten();
        let env = PipelineEnv {
            cwd: root_s.clone(),
            kvs_pipeline_dir: env_of("KVS_PIPELINE_DIR"),
            kvs_manifest: env_of("KVS_MANIFEST"),
            hardware_manifest: Some(format!("{root_s}/hardware.json")),
            home: format!("{root_s}/home"),
        };
        for s in &c.steps {
            let mut out = Captured::default();
            let args = &s.argv[1..];
            let code = if s.argv[0] == "pipeline" {
                pipeline::run(args, &env, &mut out)
            } else {
                recording::run(args, &root.join("home"), &mut out)
            };
            let norm = |t: &str| t.replace(&root_s, "<ROOT>");
            let want_out = s.stdout.replace(TS_RESTART_HINT, RESTART_HINT);
            let got = (norm(&out.stdout), norm(&out.stderr), code);
            let want = (want_out, s.stderr.clone(), s.exit_code);
            if got != want {
                failures.push(format!(
                    "{} / {:?}:\n got {got:?}\nwant {want:?}",
                    c.name, s.argv
                ));
            }
        }
        let mut got_tree = BTreeMap::new();
        tree(&root, "", &root_s, &mut got_tree);
        if got_tree != c.tree_after_hex {
            failures.push(format!(
                "{} tree:\n got {got_tree:?}\nwant {:?}",
                c.name, c.tree_after_hex
            ));
        }
        let _ = std::fs::remove_dir_all(&base);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
