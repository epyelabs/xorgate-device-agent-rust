//! Phase 6 P3a: the player's HANDOFF mode (plan
//! `phase-6/evidence/P3a-takeover.md` §Design; brief
//! `phase-6/p3a-takeover-brief.md`).
//!
//! At tape time `meta.rig.handoff.atMs` the player stops BOTH agents the way
//! systemd stops a unit, snapshots what each left under `recordings/`, and
//! restarts each side on the SAME home: side A as the `--then-a` kind (the
//! cross-agent takeover), side B as `--then-b` (normally the same kind again:
//! the same-agent control). The diff (`takeover.rs` + the existing streams)
//! then compares the two sides.
//!
//! **Stop = systemd's.** Both units leave `KillMode` at its default
//! `control-group` and set no `TimeoutStopSec` (default 90 s):
//! `systemd/xorgate-agent.service` @ JS 09da38d, `systemd/xorgate-agent-rs.service`;
//! `scripts/install-rs.sh:54-58` relies on exactly that. So the signal goes
//! to EVERY process of the unit at once: here the cgroup-equivalent
//! [`Members`] set — the agent, every descendant (a `/proc` ppid walk) and
//! everything sharing a member's process group (the Rust agent spawns its
//! children in their own groups, `xorgate-hal/src/child.rs:5`, so a bare
//! `killpg` of the agent would miss them). The set is rescanned every
//! [`POLL_MS`] during the stop, so a child born mid-stop still gets the final
//! SIGKILL, as in a cgroup. `term`: SIGTERM to every member, up to
//! [`STOP_TIMEOUT_MS`] for all to die, then SIGKILL the rest. `kill`: SIGKILL
//! to every member (power cut / OOM; the page cache survives, see the plan's
//! §6).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use xorgate_hal::tape::Tape;

use crate::play::AgentKind;

/// systemd `DefaultTimeoutStopSec` (neither unit sets `TimeoutStopSec`).
pub const STOP_TIMEOUT_MS: u64 = 90_000;
/// Stop → start gap: the convert starts the other unit right after the
/// first one's whole stop (`After=` ordering, install-rs.sh:54-58).
pub const RESTART_GAP_MS: u64 = 2_000;
/// Member-set rescan period during a stop.
pub const POLL_MS: u64 = 50;
/// After the timeout's SIGKILL: how long the player waits for the members to
/// be gone before giving up (logged; a D-state process cannot be helped).
pub const KILL_WAIT_MS: u64 = 10_000;

/// The stop the player applies at the handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StopSignal {
    /// `systemctl stop`: SIGTERM to every member, SIGKILL after 90 s.
    Term,
    /// Power cut / OOM: SIGKILL to every member at once.
    Kill,
}

impl StopSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            StopSignal::Term => "term",
            StopSignal::Kill => "kill",
        }
    }

    pub fn first(self) -> nix::sys::signal::Signal {
        match self {
            StopSignal::Term => nix::sys::signal::Signal::SIGTERM,
            StopSignal::Kill => nix::sys::signal::Signal::SIGKILL,
        }
    }
}

/// A resolved handoff: when, how, and what each side becomes.
#[derive(Debug, Clone)]
pub struct Plan {
    pub at_ms: u64,
    pub signal: StopSignal,
    /// The kind each side runs after the handoff (`[a, b]`).
    pub then: [AgentKind; 2],
    pub gap_ms: u64,
    pub stop_timeout_ms: u64,
    /// `meta.rig.handoff` verbatim (the diff reads `expect` / `segMs`).
    pub spec: Value,
}

/// `meta.rig.handoff` of a tape.
pub fn tape_spec(tape: &Tape) -> Option<&Value> {
    tape.meta.rig.as_ref().and_then(|r| r.get("handoff"))
}

impl Plan {
    /// The handoff for this run, or `None` (no `--handoff-signal`: the tape
    /// plays as an ordinary run even when it carries `meta.rig.handoff`).
    /// Refuses the combinations whose tape-time freeze is not implemented:
    /// per-attempt external exits, Pi DT writes, OBD (none of the P3a tapes
    /// has any), and a handoff instant past the tape.
    #[allow(clippy::too_many_arguments)]
    pub fn of(
        tape: &Tape,
        signal: Option<StopSignal>,
        at_override: Option<u64>,
        first: [AgentKind; 2],
        then: [Option<AgentKind>; 2],
        gap_ms: u64,
        stop_timeout_ms: u64,
        has_unsupported: Option<&str>,
    ) -> Result<Option<Self>> {
        let Some(signal) = signal else {
            if then.iter().any(Option::is_some) {
                bail!("--then-a/--then-b need --handoff-signal");
            }
            return Ok(None);
        };
        let spec = tape_spec(tape).cloned().unwrap_or(Value::Null);
        let at_ms = match at_override.or_else(|| spec.get("atMs").and_then(Value::as_u64)) {
            Some(t) => t,
            None => bail!("--handoff-signal needs meta.rig.handoff.atMs on the tape or --handoff-at-ms"),
        };
        if at_ms == 0 || at_ms >= tape.duration_ms() {
            bail!(
                "handoff at {at_ms} ms is not inside the tape (0, {})",
                tape.duration_ms()
            );
        }
        if let Some(why) = has_unsupported {
            bail!("handoff refused on this tape: {why} (its tape-time freeze is not implemented)");
        }
        Ok(Some(Self {
            at_ms,
            signal,
            then: [then[0].unwrap_or(first[0]), then[1].unwrap_or(first[1])],
            gap_ms,
            stop_timeout_ms,
            spec,
        }))
    }
}

/// One `/proc/<pid>/stat` line, the fields the member walk needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: i32,
    pub ppid: i32,
    pub pgrp: i32,
    /// `starttime` (clock ticks since boot): with the pid, a process identity
    /// that a recycled pid cannot fake.
    pub start: u64,
    /// State `Z` (zombie) or `X` (dead): no longer a live member.
    pub dead: bool,
}

/// Parse a `/proc/<pid>/stat` text: `pid (comm) state ppid pgrp … starttime`
/// (field 22). `comm` may hold spaces and parentheses: split at the LAST `)`.
pub fn parse_stat(text: &str) -> Option<ProcEntry> {
    let (head, tail) = text.rsplit_once(')')?;
    let pid: i32 = head.split_whitespace().next()?.parse().ok()?;
    let f: Vec<&str> = tail.split_whitespace().collect();
    // f[0] = state (field 3), f[1] = ppid (4), f[2] = pgrp (5), f[19] = starttime (22).
    let state = *f.first()?;
    Some(ProcEntry {
        pid,
        ppid: f.get(1)?.parse().ok()?,
        pgrp: f.get(2)?.parse().ok()?,
        start: f.get(19)?.parse().ok()?,
        dead: matches!(state, "Z" | "X"),
    })
}

/// Every process of this pid namespace.
pub fn scan_proc() -> Vec<ProcEntry> {
    let Ok(rd) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|e| fs::read_to_string(e.path().join("stat")).ok())
        .filter_map(|t| parse_stat(&t))
        .collect()
}

/// The cgroup-equivalent member set of one agent incarnation (module doc).
#[derive(Debug, Clone, Default)]
pub struct Members {
    /// `(pid, starttime)` of every process ever seen as a member.
    pub known: BTreeSet<(i32, u64)>,
    /// Process groups never adopted (the player's own).
    exclude_pgrp: BTreeSet<i32>,
}

impl Members {
    /// Members rooted at the agent `root`, never adopting the process group
    /// `exclude` (the player's own) through the group rule.
    pub fn new(root: i32, procs: &[ProcEntry], exclude: i32) -> Self {
        let mut m = Self {
            known: BTreeSet::new(),
            exclude_pgrp: BTreeSet::from([exclude]),
        };
        if let Some(p) = procs.iter().find(|p| p.pid == root) {
            m.known.insert((p.pid, p.start));
        }
        m.refresh(procs);
        m
    }

    /// Grow the set to its fixpoint over `procs`: a child of a live member,
    /// or a process in a live member's group, is a member.
    pub fn refresh(&mut self, procs: &[ProcEntry]) {
        loop {
            let live: Vec<&ProcEntry> = procs
                .iter()
                .filter(|p| self.known.contains(&(p.pid, p.start)))
                .collect();
            let pids: BTreeSet<i32> = live.iter().map(|p| p.pid).collect();
            let groups: BTreeSet<i32> = live
                .iter()
                .map(|p| p.pgrp)
                .filter(|g| !self.exclude_pgrp.contains(g))
                .collect();
            let before = self.known.len();
            for p in procs {
                if pids.contains(&p.ppid) || groups.contains(&p.pgrp) {
                    self.known.insert((p.pid, p.start));
                }
            }
            if self.known.len() == before {
                return;
            }
        }
    }

    /// The members still alive in `procs` (not zombies).
    pub fn live(&self, procs: &[ProcEntry]) -> Vec<i32> {
        procs
            .iter()
            .filter(|p| !p.dead && self.known.contains(&(p.pid, p.start)))
            .map(|p| p.pid)
            .collect()
    }
}

/// One file of the inherited state: size, birth (statx btime, when the
/// filesystem keeps one), mtime.
fn file_rec(p: &Path) -> Option<Value> {
    let md = fs::metadata(p).ok()?;
    let ms = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
    };
    Some(json!({"size": md.len(), "birthMs": ms(md.created()), "mtimeMs": ms(md.modified())}))
}

fn walk(root: &Path, dir: &Path, out: &mut Map<String, Value>, copy_to: Option<&Path>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out, copy_to);
            continue;
        }
        let Ok(rel) = p.strip_prefix(root) else {
            continue;
        };
        let rel = rel.display().to_string();
        if let Some(rec) = file_rec(&p) {
            out.insert(rel.clone(), rec);
        }
        let small = rel.ends_with(".json") || rel.ends_with(".jsonl") || rel.ends_with(".jsonl.gz");
        if let (true, Some(dst)) = (small, copy_to) {
            let d = dst.join(&rel);
            if let Some(parent) = d.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::copy(&p, d);
        }
    }
}

/// `<side>/handoff-state.json`: every file under `<home>/recordings` (rel to
/// it, the `recordings.json` keys) + `/dev/shm` names; the small files are
/// copied to `<side>/handoff/recordings/`.
pub fn snapshot(home: &Path, side_dir: &Path, w: i64) -> Result<Value> {
    let root = home.join("recordings");
    let mut files = Map::new();
    walk(
        &root,
        &root,
        &mut files,
        Some(&side_dir.join("handoff/recordings")),
    );
    let mut shm: Vec<String> = fs::read_dir("/dev/shm")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("shmpipe."))
        .collect();
    shm.sort();
    let v = json!({"w": w, "files": files, "shm": shm});
    fs::write(
        side_dir.join("handoff-state.json"),
        serde_json::to_vec_pretty(&v)?,
    )?;
    Ok(v)
}

/// Append ONE line (one `write(2)`, like `media.rs` `append`: the observer
/// appends to the same `procs.ndjson` from another task).
pub fn append_line(path: &Path, v: &Value) {
    use std::io::Write;
    let line = format!("{v}\n");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `"code 0"` / `"signal 15"` / `"unknown"` of an agent's exit.
pub fn exit_text(st: Option<std::process::ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match st {
        Some(s) => match (s.code(), s.signal()) {
            (Some(c), _) => format!("code {c}"),
            (None, Some(g)) => format!("signal {g}"),
            _ => "unknown".into(),
        },
        None => "unknown".into(),
    }
}

/// Per side, the `{fromW, kind}` incarnation list written to `meta.json`.
pub fn incarnations_json(list: &[(i64, AgentKind)]) -> Value {
    Value::Array(
        list.iter()
            .map(|(w, k)| json!({"fromW": w, "kind": k.as_str()}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, pgrp: i32) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            pgrp,
            start: pid as u64 * 10,
            dead: false,
        }
    }

    #[test]
    fn stat_parses_comm_with_spaces_and_parens() {
        let t = "4242 (gst-launch-1.0 (x)) S 4200 4242 4200 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 987654 1000 10";
        let e = parse_stat(t).unwrap();
        assert_eq!((e.pid, e.ppid, e.pgrp, e.start, e.dead), (4242, 4200, 4242, 987654, false));
        let z = "7 (node) Z 1 7 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 55 0 0";
        assert!(parse_stat(z).unwrap().dead);
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn members_follow_children_and_own_process_groups_but_not_the_player() {
        // player 10 (pgrp 10) → agent 100 (own pgrp 100) → gst 200 in its OWN
        // group 200 (Rust ChildSpec) → tee 201 in group 200; kvs 300 in the
        // agent's group but already reparented to init (ppid 1); an unrelated
        // process 400 in the player's group.
        let procs = vec![
            p(1, 0, 1),
            p(10, 1, 10),
            p(100, 10, 100),
            p(200, 100, 200),
            p(201, 1, 200),
            p(300, 1, 100),
            p(400, 10, 10),
        ];
        let m = Members::new(100, &procs, 10);
        let mut live = m.live(&procs);
        live.sort();
        assert_eq!(live, vec![100, 200, 201, 300]);
    }

    #[test]
    fn members_survive_the_agent_dying_and_adopt_late_children() {
        let mut procs = vec![p(10, 1, 10), p(100, 10, 100), p(200, 100, 200)];
        let mut m = Members::new(100, &procs, 10);
        // The agent dies (zombie) and gst is reparented to init, then spawns
        // a helper: still a member (its parent is one).
        procs[1].dead = true;
        procs[2].ppid = 1;
        procs.push(p(250, 200, 250));
        m.refresh(&procs);
        let mut live = m.live(&procs);
        live.sort();
        assert_eq!(live, vec![200, 250]);
        // A recycled pid (same number, other start time) is NOT a member.
        let mut recycled = p(200, 1, 200);
        recycled.start = 1;
        let procs2 = vec![p(10, 1, 10), recycled];
        assert!(m.live(&procs2).is_empty());
    }

    #[test]
    fn plan_needs_a_signal_an_instant_inside_the_tape_and_a_supported_tape() {
        let mut tape = crate::synth::build("p6-takeover-open", None, &crate::synth::test_fixtures()).unwrap();
        let first = [AgentKind::Js, AgentKind::Js];
        // No signal: an ordinary run.
        assert!(Plan::of(&tape, None, None, first, [None, None], 2000, 90_000, None)
            .unwrap()
            .is_none());
        // --then-* without a signal is a usage error.
        assert!(Plan::of(&tape, None, None, first, [Some(AgentKind::Rust), None], 2000, 90_000, None).is_err());
        let p = Plan::of(
            &tape,
            Some(StopSignal::Kill),
            None,
            first,
            [Some(AgentKind::Rust), None],
            2000,
            90_000,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(p.then, [AgentKind::Rust, AgentKind::Js]);
        assert_eq!(p.at_ms, crate::synth::TAKEOVER_OPEN_AT_MS);
        // Unsupported tracks and an instant past the tape are refused.
        assert!(Plan::of(&tape, Some(StopSignal::Term), None, first, [None, None], 2000, 90_000, Some("obd")).is_err());
        assert!(Plan::of(&tape, Some(StopSignal::Term), Some(10_000_000), first, [None, None], 2000, 90_000, None).is_err());
        // A tape without meta.rig.handoff needs the override.
        tape.meta.rig = None;
        assert!(Plan::of(&tape, Some(StopSignal::Term), None, first, [None, None], 2000, 90_000, None).is_err());
        assert!(Plan::of(&tape, Some(StopSignal::Term), Some(5_000), first, [None, None], 2000, 90_000, None)
            .unwrap()
            .is_some());
    }
}
