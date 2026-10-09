//! `xorgate-replay inspect` — what is on a tape: header, duration, per-kind
//! counts, connection spans, wall-clock steps (from the `clock` samples and the
//! per-record `w` vs `t`), the inbound topics, and whether the tape was cut.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use xorgate_hal::tape::{ConnState, Tape, TapeEvent};

/// A wall step shows as `Δw − Δt` beyond this between consecutive records.
pub const STEP_THRESHOLD_MS: i64 = 2_000;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Summary {
    pub records: usize,
    pub duration_ms: u64,
    pub counts: BTreeMap<String, usize>,
    /// `(from t, to t or None = open at the end)`.
    pub closed_spans: Vec<(u64, Option<u64>)>,
    /// `(t, step ms)` where the wall clock jumped relative to mono.
    pub wall_steps: Vec<(u64, i64)>,
    pub topics: BTreeMap<String, usize>,
    pub nmea_gaps_over_5s: Vec<(u64, u64)>,
    pub truncated: bool,
    pub out_of_order: usize,
}

pub fn summarize(tape: &Tape) -> Summary {
    let mut s = Summary {
        records: tape.records.len(),
        duration_ms: tape.duration_ms(),
        truncated: tape.truncated,
        ..Default::default()
    };
    let mut closed_from: Option<u64> = None;
    let mut prev: Option<(u64, i64)> = None;
    let mut last_nmea: Option<u64> = None;
    for r in &tape.records {
        *s.counts.entry(r.ev.kind().to_string()).or_default() += 1;
        if let Some((pt, pw)) = prev {
            if r.t < pt {
                s.out_of_order += 1;
            }
            let skew = (r.w - pw) - (r.t as i64 - pt as i64);
            if skew.abs() > STEP_THRESHOLD_MS {
                s.wall_steps.push((r.t, skew));
            }
        }
        prev = Some((r.t, r.w));
        match &r.ev {
            TapeEvent::Conn { state } => match state {
                ConnState::Closed => {
                    closed_from.get_or_insert(r.t);
                }
                ConnState::Connected => {
                    if let Some(f) = closed_from.take() {
                        s.closed_spans.push((f, Some(r.t)));
                    }
                }
            },
            TapeEvent::MqttIn { topic, .. } => {
                let tail = topic.splitn(4, '/').nth(3).unwrap_or(topic).to_string();
                *s.topics.entry(tail).or_default() += 1;
            }
            TapeEvent::Nmea { .. } => {
                if let Some(l) = last_nmea {
                    if r.t - l > 5_000 {
                        s.nmea_gaps_over_5s.push((l, r.t));
                    }
                }
                last_nmea = Some(r.t);
            }
            _ => {}
        }
    }
    if let Some(f) = closed_from {
        s.closed_spans.push((f, None));
    }
    s
}

pub fn render(tape: &Tape, s: &Summary) -> String {
    let m = &tape.meta;
    let mut o = String::new();
    let _ = writeln!(
        o,
        "tape v{} source={} build={}",
        m.v,
        m.source.as_deref().unwrap_or("bench"),
        m.build
    );
    let _ = writeln!(o, "deviceId   {}", m.device_id);
    let _ = writeln!(
        o,
        "startWall  {} ({})",
        m.start_wall,
        crate::rewrite::iso_z(m.start_wall.div_euclid(1000))
    );
    let _ = writeln!(
        o,
        "duration   {:.1} s, {} records{}{}",
        s.duration_ms as f64 / 1000.0,
        s.records,
        if s.truncated {
            ", TRUNCATED (cut write; readable up to the last flush)"
        } else {
            ""
        },
        if s.out_of_order > 0 {
            format!(", {} out-of-order t", s.out_of_order)
        } else {
            String::new()
        }
    );
    if !m.synthetic.is_empty() {
        let _ = writeln!(o, "synthetic  {}", m.synthetic.join(", "));
    }
    let _ = writeln!(
        o,
        "hardware   {}",
        if m.hardware.is_null() {
            "absent".to_string()
        } else {
            m.hardware.to_string()
        }
    );
    let _ = writeln!(o, "kvs        {} channel(s)", m.kvs.channels.len());
    if !m.env.is_empty() {
        let _ = writeln!(o, "env        {}", serde_json::Value::Object(m.env.clone()));
    }
    if let Some(home) = &m.home {
        let names: Vec<_> = home.keys().cloned().collect();
        let _ = writeln!(o, "home       {}", names.join(", "));
    }
    let _ = writeln!(o, "kinds:");
    for (k, n) in &s.counts {
        let _ = writeln!(o, "  {k:<10} {n}");
    }
    if !s.topics.is_empty() {
        let _ = writeln!(o, "mqtt_in topics:");
        for (k, n) in &s.topics {
            let _ = writeln!(o, "  {k:<18} {n}");
        }
    }
    for (f, t) in &s.closed_spans {
        let _ = match t {
            Some(t) => writeln!(
                o,
                "conn closed {:.1}s → {:.1}s ({:.1} s)",
                *f as f64 / 1e3,
                *t as f64 / 1e3,
                (*t - *f) as f64 / 1e3
            ),
            None => writeln!(o, "conn closed {:.1}s → end", *f as f64 / 1e3),
        };
    }
    for (a, b) in &s.nmea_gaps_over_5s {
        let _ = writeln!(
            o,
            "nmea gap   {:.1}s → {:.1}s ({:.1} s)",
            *a as f64 / 1e3,
            *b as f64 / 1e3,
            (*b - *a) as f64 / 1e3
        );
    }
    for (t, d) in &s.wall_steps {
        let _ = writeln!(
            o,
            "WALL STEP  at {:.1}s by {d} ms (O2 mono-vs-wall rows apply, T8)",
            *t as f64 / 1e3
        );
    }
    o
}

/// One line per record, for `inspect --dump`.
pub fn dump(tape: &Tape, kinds: &[String]) -> String {
    let mut o = String::new();
    for r in &tape.records {
        if kinds.is_empty() || kinds.iter().any(|k| k == r.ev.kind()) {
            let _ = writeln!(o, "{}", serde_json::to_string(r).unwrap_or_default());
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use xorgate_hal::tape::{TapeMeta, TapeRecord};

    #[test]
    fn spans_steps_and_counts() {
        let rec = |t: u64, w: i64, ev| TapeRecord { t, w, ev };
        let tape = Tape {
            meta: TapeMeta::new("d", "b", 0),
            records: vec![
                rec(
                    0,
                    0,
                    TapeEvent::Conn {
                        state: ConnState::Connected,
                    },
                ),
                rec(100, 100, TapeEvent::Nmea { line: "a".into() }),
                rec(
                    1_000,
                    1_000,
                    TapeEvent::Conn {
                        state: ConnState::Closed,
                    },
                ),
                rec(7_000, 7_000, TapeEvent::Nmea { line: "b".into() }),
                rec(
                    9_000,
                    9_000,
                    TapeEvent::Conn {
                        state: ConnState::Connected,
                    },
                ),
                rec(
                    10_000,
                    70_000,
                    TapeEvent::Clock {
                        wall: 70_000,
                        mono: 10_000,
                    },
                ),
                rec(
                    11_000,
                    71_000,
                    TapeEvent::Conn {
                        state: ConnState::Closed,
                    },
                ),
            ],
            truncated: true,
        };
        let s = summarize(&tape);
        assert_eq!(s.counts["conn"], 4);
        assert_eq!(s.closed_spans, vec![(1_000, Some(9_000)), (11_000, None)]);
        assert_eq!(s.wall_steps, vec![(10_000, 60_000)]);
        assert_eq!(s.nmea_gaps_over_5s, vec![(100, 7_000)]);
        let text = render(&tape, &s);
        assert!(text.contains("TRUNCATED"));
        assert!(text.contains("WALL STEP"));
    }
}
