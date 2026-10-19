//! Rendering of the `manage` TUI with ratatui — the layout of
//! `src/monitor/MonitorApp.tsx:173-306` and `MonitorHeader.tsx:39-148`, the
//! ink components `PanelFrame`, `StatusBadge`, `KeyHints`, `LogView`, and
//! `LevelMeter` (in `modules.rs`).
//!
//! ink lays the app out top to bottom at its natural height; ratatui draws a
//! full-screen frame (alternate screen), so when the terminal is shorter
//! than the natural height the BODY (sidebar + detail) is clipped and the
//! header and the key hints stay visible. Colours map ink names to ANSI
//! (`format::ink_color`). Text wraps on words like ink's `wrap`; the
//! `truncate-end` lines (media streams, activity events, log lines) clip
//! with `…`.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use serde_json::Value;
use unicode_width::UnicodeWidthChar;

use super::app::{App, Hint, LIST_WIDTH, LOG_MAX_LINES, NARROW_THRESHOLD, UNIT};
use super::format::{bold, dim_gray, fg, gray, Status, GRAY, WHITE};
use super::modules::{detail_lines, module_status, DetailCtx, MODULES, TRUNCATE};
use crate::cli::status::{js_string, to_number, uptime_label, DaemonStatus};

/// Host facts the header prints (`hostLabel()`, `osInfo()`), read once.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub host_label: String,
    pub is_mock: bool,
    /// `${os.pretty} ${os.arch}`.
    pub os: String,
    /// `GPS_NMEA_PORT`.
    pub gps_port: String,
}

/// Width of a line in terminal cells.
fn line_width(l: &Line) -> usize {
    l.spans.iter().map(|s| s.width()).sum()
}

/// Word-wrap a line to `width` cells (ink `wrap`: break at spaces, hard
/// break a word longer than the width).
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line_width(line) <= width {
        return vec![line.clone()];
    }
    // Tokenise into (char, style) cells, then greedy word wrap.
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    let mut out: Vec<Vec<(char, Style)>> = vec![Vec::new()];
    let mut col = 0usize;
    let mut i = 0;
    while i < cells.len() {
        // next word = run of non-spaces (or one space)
        let start = i;
        if cells[i].0 == ' ' {
            i += 1;
        } else {
            while i < cells.len() && cells[i].0 != ' ' {
                i += 1;
            }
        }
        let word = &cells[start..i];
        let w: usize = word.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
        if word[0].0 == ' ' {
            if col + 1 > width {
                out.push(Vec::new());
                col = 0;
            } else if col > 0 {
                out.last_mut().unwrap().push(word[0]);
                col += 1;
            }
            continue;
        }
        if col + w > width && col > 0 {
            // drop the trailing space of the full line
            if out.last().and_then(|l| l.last()).is_some_and(|(c, _)| *c == ' ') {
                out.last_mut().unwrap().pop();
            }
            out.push(Vec::new());
            col = 0;
        }
        for &(c, st) in word {
            let cw = c.width().unwrap_or(0);
            if col + cw > width {
                out.push(Vec::new());
                col = 0;
            }
            out.last_mut().unwrap().push((c, st));
            col += cw;
        }
    }
    out.into_iter()
        .map(|cells| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, st) in cells {
                match spans.last_mut() {
                    Some(sp) if sp.style == st => sp.content.to_mut().push(c),
                    _ => spans.push(Span::styled(c.to_string(), st)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// `wrap="truncate-end"`: clip to `width` cells ending in `…`.
pub fn truncate_line(line: &Line<'static>, width: usize) -> Line<'static> {
    if line_width(line) <= width {
        return line.clone();
    }
    if width == 0 {
        return Line::default();
    }
    let mut left = width - 1;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last_style = Style::default();
    'outer: for s in &line.spans {
        let mut text = String::new();
        for c in s.content.chars() {
            let cw = c.width().unwrap_or(0);
            if cw > left {
                if !text.is_empty() {
                    spans.push(Span::styled(text.clone(), s.style));
                }
                last_style = s.style;
                break 'outer;
            }
            left -= cw;
            text.push(c);
        }
        last_style = s.style;
        spans.push(Span::styled(text, s.style));
    }
    spans.push(Span::styled("…", last_style));
    Line::from(spans)
}

/// Lines laid out for a content width: wrapped or truncated, marker stripped.
fn layout_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for l in lines {
        if l.style.add_modifier.contains(TRUNCATE) {
            let mut clean = l.clone();
            clean.style.add_modifier.remove(TRUNCATE);
            out.push(truncate_line(&clean, width));
        } else {
            out.extend(wrap_line(l, width));
        }
    }
    out
}

fn rounded(color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(color))
}

/// `area` minus a border and `px`/`py` padding.
fn inner(area: Rect, px: u16, py: u16) -> Rect {
    let x = area.x.saturating_add(1 + px);
    let y = area.y.saturating_add(1 + py);
    let w = area.width.saturating_sub(2 + 2 * px);
    let h = area.height.saturating_sub(2 + 2 * py);
    Rect::new(x, y, w, h)
}

/// `MQTT_COLOR` / `mqttColor` (`MonitorHeader.tsx:24-37`).
fn mqtt_color(st: &DaemonStatus) -> Option<Color> {
    let DaemonStatus::Running {
        state, state_fresh, ..
    } = st
    else {
        return Some(GRAY);
    };
    if !state_fresh {
        return Some(Color::Yellow);
    }
    let conn = match state.as_ref().and_then(|s| s.get("connection")) {
        None | Some(Value::Null) => "connecting".to_string(),
        v => js_string(v),
    };
    match conn.as_str() {
        "connected" => Some(Color::Green),
        "connecting" | "reconnecting" => Some(Color::Yellow),
        "closed" => Some(GRAY),
        "error" => Some(Color::Red),
        _ => None,
    }
}

fn opt_fg(c: Option<Color>) -> Style {
    c.map(fg).unwrap_or_default()
}

/// `ConnectivityRow` (`MonitorHeader.tsx:94-116`).
fn connectivity(app: &App) -> Line<'static> {
    let mut spans = vec![Span::styled("Connectivity ", gray())];
    if app.interfaces.is_empty() {
        spans.push(Span::styled("checking…  ", dim_gray()));
    }
    for i in &app.interfaces {
        let c = match i.state {
            xorgate_hal::network_io::IfaceState::Online => Color::Green,
            xorgate_hal::network_io::IfaceState::Offline => Color::Red,
            xorgate_hal::network_io::IfaceState::Down => GRAY,
        };
        spans.push(Span::styled("●", fg(c)));
        spans.push(Span::styled(format!(" {}  ", i.name), fg(WHITE)));
    }
    spans.push(Span::styled("●", opt_fg(mqtt_color(&app.status))));
    spans.push(Span::styled(" mqtt  ", fg(WHITE)));
    Line::from(spans)
}

/// `DaemonStats` / `NotRunningStat` (`MonitorHeader.tsx:118-148`).
fn daemon_stats(app: &App) -> Line<'static> {
    match &app.status {
        DaemonStatus::Running {
            record,
            state,
            state_fresh,
            ..
        } => {
            let mut spans = Vec::new();
            if record.offline {
                spans.push(Span::styled("offline · ", gray()));
            }
            spans.push(Span::styled("pid ", gray()));
            spans.push(Span::raw(record.pid.to_string()));
            spans.push(Span::styled(
                format!(
                    " · up {} · {} {}Hz",
                    uptime_label(app.now - record.started_at),
                    record.mode,
                    xorgate_core::js::number_to_string(record.rate_hz)
                ),
                gray(),
            ));
            let errs = state.as_ref().and_then(|s| s.get("publishErrors"));
            if state.is_some() && to_number(errs) > 0.0 {
                spans.push(Span::styled(
                    format!(" · err {}", js_string(errs)),
                    fg(Color::Red),
                ));
            }
            if !state_fresh {
                spans.push(Span::styled(" · stale", fg(Color::Yellow)));
            }
            Line::from(spans)
        }
        DaemonStatus::Stale { .. } => Line::from(Span::styled(
            "◌ daemon stopped (stale pidfile)",
            fg(Color::Yellow),
        )),
        DaemonStatus::Stopped => Line::from(Span::styled("○ daemon stopped", gray())),
    }
}

/// The header box (`MonitorHeader`): its height for `width`.
fn header_height(app: &App, width: u16) -> u16 {
    let w = width.saturating_sub(4) as usize;
    if line_width(&connectivity(app)) + line_width(&daemon_stats(app)) > w {
        6
    } else {
        5
    }
}

fn draw_header(f: &mut Frame, app: &App, host: &HostInfo, area: Rect) {
    f.render_widget(rounded(Color::Cyan), area);
    let c = inner(area, 1, 0);
    if c.height == 0 {
        return;
    }
    let row = |y: u16| Rect::new(c.x, c.y + y, c.width, 1);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled("XORGATE Manager", bold(Color::Cyan)))),
        row(0),
    );
    let right = Line::from(vec![
        Span::styled(
            host.host_label.clone(),
            fg(if host.is_mock { Color::Yellow } else { GRAY }),
        ),
        Span::styled(format!("  {}", host.os), gray()),
    ])
    .right_aligned();
    f.render_widget(Paragraph::new(right), row(0));
    if c.height > 1 {
        f.render_widget(
            Paragraph::new(Span::styled("─".repeat(c.width as usize), gray())),
            row(1),
        );
    }
    let conn = connectivity(app);
    let stats = daemon_stats(app);
    let one_line = line_width(&conn) + line_width(&stats) <= c.width as usize;
    if c.height > 2 {
        f.render_widget(Paragraph::new(conn), row(2));
        if one_line {
            f.render_widget(Paragraph::new(stats.right_aligned()), row(2));
        } else if c.height > 3 {
            f.render_widget(Paragraph::new(stats), row(3));
        }
    }
}

/// `PanelFrame` + `StatusBadge` for one sidebar module (4 rows).
fn draw_panel(f: &mut Frame, app: &App, i: usize, area: Rect) {
    let focused = i == app.selected;
    f.render_widget(rounded(if focused { Color::Cyan } else { GRAY }), area);
    let c = inner(area, 1, 0);
    if c.height == 0 {
        return;
    }
    let m = MODULES[i];
    let title = format!("{}{}", if focused { "▶ " } else { "  " }, m.title());
    let tstyle = bold(if focused { Color::Cyan } else { WHITE });
    let running = app.running();
    let (label, status) = module_status(m, app.snapshot());
    let (status, label) = if running {
        (status, label)
    } else {
        (Status::Unknown, "—".to_string())
    };
    let (color, glyph) = status.style();
    let badge = Line::from(vec![Span::styled(format!("{glyph} "), fg(color)), Span::raw(label)]);
    let lines = vec![Line::from(Span::styled(title, tstyle)), badge];
    let lines: Vec<Line> = lines
        .iter()
        .map(|l| truncate_line(l, c.width as usize))
        .collect();
    f.render_widget(Paragraph::new(lines), c);
}

fn detail_content(app: &App, host: &HostInfo, spinner: &str) -> Vec<Line<'static>> {
    let settings = app.settings_view();
    let ctx = DetailCtx {
        snapshot: app.snapshot(),
        imu_detect: app.imu_detect.as_ref(),
        modem: app.modem.as_ref(),
        link: app.link.as_ref(),
        settings: &settings,
        spinner,
        gps_port: &host.gps_port,
    };
    detail_lines(app.module(), &ctx)
}

/// `NotRunning` (`MonitorApp.tsx:288-306`), Q10: the call-to-action names
/// the systemd command instead of the `s` key.
fn not_running_lines(stale: bool) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            if stale {
                "◌ Daemon not running (stale pidfile)"
            } else {
                "○ Daemon not running"
            },
            bold(if stale { Color::Yellow } else { WHITE }),
        )),
        Line::from(vec![
            Span::styled("Run ", gray()),
            Span::styled(format!("sudo systemctl start {UNIT}"), fg(Color::Cyan)),
            Span::styled(
                format!(
                    " to start the telemetry agent{}.",
                    if stale {
                        " (the stale pidfile is cleaned automatically)"
                    } else {
                        ""
                    }
                ),
                gray(),
            ),
        ]),
    ]
}

/// `KeyHints` (`components/KeyHints.tsx`).
fn hints_line(hints: &[Hint]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, h) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", gray()));
        }
        spans.push(Span::styled(h.keys, fg(Color::Cyan)));
        spans.push(Span::styled(format!(" {}", h.label), gray()));
    }
    Line::from(spans)
}

/// Draw the whole app into the frame.
pub fn draw(f: &mut Frame, app: &App, host: &HostInfo, spinner: &str) {
    let area = f.area();
    let w = area.width;
    let two_col = w >= NARROW_THRESHOLD;
    let hh = header_height(app, w).min(area.height);
    // Natural heights of everything below the body.
    let content_w = |box_w: u16, px: u16| box_w.saturating_sub(2 + 2 * px) as usize;
    let hints = wrap_line(&hints_line(&app.hints()), w as usize);
    let msg = app
        .last_msg
        .as_ref()
        .map(|m| wrap_line(&Line::from(Span::styled(m.clone(), dim_gray())), w as usize))
        .unwrap_or_default();
    let log_lines: Vec<Line<'static>> = if app.show_logs {
        let tail = &app.log_lines[app.log_lines.len().saturating_sub(LOG_MAX_LINES)..];
        let mut v = vec![Line::from(Span::styled("Daemon log", gray()))];
        if tail.is_empty() {
            v.push(Line::from(Span::styled("(no output yet)", dim_gray())));
        }
        for l in tail {
            v.push(truncate_line(&Line::raw(l.clone()), content_w(w, 1)));
        }
        v
    } else {
        Vec::new()
    };
    let below = hints.len() as u16
        + 1
        + if msg.is_empty() { 0 } else { msg.len() as u16 + 1 }
        + if app.show_logs {
            log_lines.len() as u16 + 3
        } else {
            0
        };
    let body_y = area.y + hh + 1;
    let body_max = area.height.saturating_sub(hh + 1 + below);

    // Body.
    let body_h: u16;
    if app.running() {
        let list_w = if two_col { LIST_WIDTH } else { w };
        let list_h = 4 * MODULES.len() as u16;
        let det_w = if two_col {
            w.saturating_sub(LIST_WIDTH + 1)
        } else {
            w
        };
        let det = layout_lines(&detail_content(app, host, spinner), content_w(det_w, 1));
        let det_h = det.len() as u16 + 2;
        let (det_x, det_y, det_box_h, natural) = if two_col {
            let h = list_h.max(det_h);
            (area.x + LIST_WIDTH + 1, body_y, h, h)
        } else {
            (area.x, body_y + list_h + 1, det_h, list_h + 1 + det_h)
        };
        body_h = natural.min(body_max);
        let clip = |r: Rect| -> Rect {
            let bottom = body_y + body_h;
            if r.y >= bottom {
                Rect::new(r.x, r.y, r.width, 0)
            } else {
                Rect::new(r.x, r.y, r.width, r.height.min(bottom - r.y))
            }
        };
        for i in 0..MODULES.len() {
            let r = clip(Rect::new(area.x, body_y + 4 * i as u16, list_w, 4));
            if r.height > 0 {
                draw_panel(f, app, i, r);
            }
        }
        let dr = clip(Rect::new(det_x, det_y, det_w, det_box_h));
        if dr.height > 0 {
            f.render_widget(rounded(if app.operating { Color::Cyan } else { GRAY }), dr);
            f.render_widget(Paragraph::new(det), inner(dr, 1, 0));
        }
    } else {
        let stale = matches!(app.status, DaemonStatus::Stale { .. });
        let lines = layout_lines(&not_running_lines(stale), content_w(w, 2));
        body_h = (lines.len() as u16 + 4).min(body_max);
        let r = Rect::new(area.x, body_y, w, body_h);
        f.render_widget(rounded(if stale { Color::Yellow } else { GRAY }), r);
        f.render_widget(Paragraph::new(lines), inner(r, 2, 1));
    }

    // Header (drawn last so nothing overlaps it).
    draw_header(f, app, host, Rect::new(area.x, area.y, w, hh));

    let mut y = body_y + body_h;
    if app.show_logs {
        y += 1;
        let r = Rect::new(area.x, y, w, (log_lines.len() as u16 + 2).min(area.bottom().saturating_sub(y)));
        f.render_widget(rounded(GRAY), r);
        f.render_widget(Paragraph::new(log_lines.clone()), inner(r, 1, 0));
        y += log_lines.len() as u16 + 2;
    }
    if !msg.is_empty() {
        y += 1;
        let r = Rect::new(area.x, y, w, (msg.len() as u16).min(area.bottom().saturating_sub(y)));
        f.render_widget(Paragraph::new(msg.clone()), r);
        y += msg.len() as u16;
    }
    y += 1;
    let r = Rect::new(area.x, y.min(area.bottom()), w, (hints.len() as u16).min(area.bottom().saturating_sub(y)));
    f.render_widget(Paragraph::new(hints), r);
}
