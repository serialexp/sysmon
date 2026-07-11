//! All rendering. Layout is: verdict bar / 2x2 axis grid / process table / help.

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Gauge, Paragraph, Row, Sparkline, Table},
    Frame,
};

use crate::app::{App, InputMode};
use crate::bottleneck::{self, Assessment, Axis, CLEAR, SATURATED};
use crate::history::History;
use crate::metrics::{IoHint, Metrics, ProcSample};
use crate::util::{fmt_bits, fmt_bytes, fmt_rate};

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    let Some(m) = &app.metrics else {
        let p = Paragraph::new("Collecting first sample…")
            .alignment(Alignment::Center)
            .block(title_block(" sysmon "));
        f.render_widget(p, area);
        return;
    };

    let assess = bottleneck::assess(m);
    // When we lack the privilege to read other users' I/O, some writers show as
    // `—`. Explain that on its own dedicated line (0 height when not shown) so
    // the message never gets truncated off a block title. `io_hint` is `Full`
    // under root (or when nothing was denied), so this line isn't drawn there.
    let banner_h = if m.io_hint == IoHint::Full { 0 } else { 1 };
    let rows = Layout::vertical([
        Constraint::Length(3),        // verdict
        Constraint::Min(9),           // 2x2 grid
        Constraint::Length(banner_h), // I/O-permission banner (conditional)
        Constraint::Length(12),       // process table
        Constraint::Length(1),        // help
    ])
    .split(area);

    render_verdict(f, rows[0], &assess);
    render_grid(f, rows[1], m, app);
    if banner_h > 0 {
        render_io_banner(f, rows[2], m.io_hint);
    }
    render_processes(f, rows[3], m, &assess, app);
    render_status_bar(f, rows[4], m, app);
}

/// The bottom line is context-sensitive: the search box while searching, the
/// kill confirmation while killing, a transient status message after an action,
/// or the key help otherwise.
fn render_status_bar(f: &mut Frame, area: Rect, m: &Metrics, app: &App) {
    match app.mode {
        InputMode::Search => render_search_line(f, area, m, app),
        InputMode::Kill => render_kill_line(f, area, m, app),
        InputMode::Normal => match &app.status {
            Some((msg, _)) => render_status_line(f, area, msg),
            None => render_help(f, area),
        },
    }
}

fn render_search_line(f: &mut Frame, area: Rect, m: &Metrics, app: &App) {
    let q = app.search_query.to_lowercase();
    let hits = if q.is_empty() {
        0
    } else {
        m.procs
            .iter()
            .filter(|p| p.comm.to_lowercase().contains(&q) || p.pid.to_string().contains(&q))
            .count()
    };
    let count = if app.search_query.is_empty() {
        String::new()
    } else {
        format!("  ({hits} match{})", if hits == 1 { "" } else { "es" })
    };
    let label = Style::default().add_modifier(Modifier::REVERSED);
    let dim = Style::default().fg(Color::DarkGray);
    let line = Line::from(vec![
        Span::styled(" Search ", label),
        Span::raw(" "),
        Span::styled(
            app.search_query.clone(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        // Block cursor so the (possibly empty) input is visibly focused.
        Span::styled("▏", Style::default().fg(Color::Cyan)),
        Span::styled(count, dim),
        Span::styled("   F3 next · Esc done", dim),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_kill_line(f: &mut Frame, area: Rect, m: &Metrics, app: &App) {
    let pid = app.selected_pid.unwrap_or(0);
    let comm = m
        .procs
        .iter()
        .find(|p| p.pid == pid)
        .map(|p| p.comm.as_str())
        .unwrap_or("?");
    let warn = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let key = Style::default().add_modifier(Modifier::REVERSED);
    let dim = Style::default().fg(Color::DarkGray);
    let line = Line::from(vec![
        Span::styled(format!(" Kill {pid} ({comm})? "), warn),
        Span::raw("  "),
        Span::styled(" Enter ", key),
        Span::styled(" SIGTERM   ", dim),
        Span::styled(" k ", key),
        Span::styled(" SIGKILL   ", dim),
        Span::styled(" Esc ", key),
        Span::styled(" cancel", dim),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_status_line(f: &mut Frame, area: Rect, msg: &str) {
    let line = Line::from(Span::styled(
        format!(" {msg}"),
        Style::default().fg(Color::Yellow),
    ));
    f.render_widget(Paragraph::new(line), area);
}

/// A single-line notice that per-process I/O attribution is incomplete, showing
/// the command that will actually fix it on this system. Only rendered when
/// `io_hint` is not `Full`.
fn render_io_banner(f: &mut Frame, area: Rect, hint: IoHint) {
    // Command first, explanation after: on a narrow pane the tail truncates but
    // the actionable command survives. Widths kept ≤ ~40 cells.
    let cmd = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let expl = Style::default().fg(Color::Yellow);
    let (command, note) = match hint {
        // We have no capability yet — offer the cheap one-time grant.
        IoHint::TryGrant => (" sysmon --grant", " — full I/O (one-time)"),
        // We have the cap but this kernel ignores it — only real root works.
        IoHint::CapInert => (" sysmon --sudo", " — full I/O (needs root)"),
        IoHint::Full => return,
    };
    let line = Line::from(vec![
        Span::styled(command, cmd),
        Span::styled(note, expl),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn title_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title.to_string())
}

/// Green below CLEAR, yellow up to SATURATED, red at/above it.
fn sat_color(s: f64) -> Color {
    if s < CLEAR {
        Color::Green
    } else if s < SATURATED {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn render_verdict(f: &mut Frame, area: Rect, a: &Assessment) {
    // Markers are ASCII on purpose: emoji glyphs like ⚠/✓ render as two cells in
    // some terminals (notably browser/xterm.js) while ratatui budgets one, which
    // shifts the rest of the row right and spills the border. ASCII, box-drawing,
    // and block elements are the only reliably single-width glyphs.
    let (icon, msg, color) = if a.worst_sat < CLEAR {
        (
            "[ok]",
            "All clear — no resource is saturated".to_string(),
            Color::Green,
        )
    } else if a.worst_sat < SATURATED {
        (
            "[~]",
            format!(
                "Elevated: {} at {:.0}% — watch it",
                a.worst.label(),
                a.worst_sat * 100.0
            ),
            Color::Yellow,
        )
    } else {
        (
            "[!]",
            format!(
                "BOTTLENECK: {} saturated at {:.0}%",
                a.worst.label(),
                a.worst_sat * 100.0
            ),
            Color::Red,
        )
    };

    let bold = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let line = Line::from(vec![
        Span::styled(format!(" {icon} "), bold),
        Span::styled(msg, bold),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            " sysmon — what's slowing you down ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn render_grid(f: &mut Frame, area: Rect, m: &Metrics, app: &App) {
    let rows = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    let top = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[0]);
    let bot = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[1]);

    // CPU
    axis_pane(
        f,
        top[0],
        "CPU",
        sat_color(m.cpu.usage),
        format!("{:.0}%", m.cpu.usage * 100.0),
        cores_line(&m.cpu.per_core),
        m.cpu.usage,
        format!("{:.0}%", m.cpu.usage * 100.0),
        &app.h_cpu,
    );

    // Memory
    axis_pane(
        f,
        top[1],
        "Memory",
        sat_color(m.mem.used_frac),
        format!("{} / {}", fmt_bytes(m.mem.used), fmt_bytes(m.mem.total)),
        Line::from(format!(
            "swap {} / {}{}",
            fmt_bytes(m.mem.swap_used),
            fmt_bytes(m.mem.swap_total),
            if m.mem.swapping { "   swapping" } else { "" }
        )),
        m.mem.used_frac,
        format!("{:.0}%", m.mem.used_frac * 100.0),
        &app.h_mem,
    );

    // Disk
    axis_pane(
        f,
        bot[0],
        "Disk I/O",
        sat_color(m.disk.util),
        format!(
            "{}  read    {}  write",
            fmt_rate(m.disk.read_bps),
            fmt_rate(m.disk.write_bps)
        ),
        Line::from(format!(
            "util {:.0}%    iowait {:.0}%",
            m.disk.util * 100.0,
            m.disk.iowait * 100.0
        )),
        m.disk.util,
        format!("{:.0}%", m.disk.util * 100.0),
        &app.h_disk,
    );

    // Network
    let (net_ratio, net_label, net_color, net_detail) = match m.net.sat {
        Some(s) => (
            s,
            format!("{:.0}%", s * 100.0),
            sat_color(s),
            Line::from(format!("{:.0}% of link capacity", s * 100.0)),
        ),
        None => (
            0.0,
            "n/a".to_string(),
            Color::Cyan,
            Line::from("link speed unknown (wifi / virtual)"),
        ),
    };
    axis_pane(
        f,
        bot[1],
        "Network",
        net_color,
        format!("Rx {}    Tx {}", fmt_bits(m.net.rx_bps), fmt_bits(m.net.tx_bps)),
        net_detail,
        net_ratio,
        net_label,
        &app.h_net,
    );
}

/// One axis cell: title border, big headline value, saturation gauge, a detail
/// line, and a sparkline of recent saturation.
#[allow(clippy::too_many_arguments)]
fn axis_pane(
    f: &mut Frame,
    area: Rect,
    title: &str,
    color: Color,
    big: String,
    detail: Line,
    gauge_ratio: f64,
    gauge_label: String,
    hist: &History,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let parts = Layout::vertical([
        Constraint::Length(1), // headline
        Constraint::Length(1), // gauge
        Constraint::Length(1), // detail
        Constraint::Min(1),    // sparkline
    ])
    .split(inner);

    f.render_widget(
        Paragraph::new(big).style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
        parts[0],
    );
    f.render_widget(
        Gauge::default()
            .ratio(gauge_ratio.clamp(0.0, 1.0))
            .gauge_style(Style::default().fg(color))
            .label(gauge_label),
        parts[1],
    );
    f.render_widget(Paragraph::new(detail), parts[2]);

    let data: Vec<u64> = hist.iter().map(|&x| (x * 100.0) as u64).collect();
    f.render_widget(
        Sparkline::default()
            .max(100)
            .style(Style::default().fg(color))
            .data(&data),
        parts[3],
    );
}

/// A compact per-core usage strip using block characters, each core coloured by
/// its own load.
fn cores_line(cores: &[f64]) -> Line<'static> {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let spans: Vec<Span> = cores
        .iter()
        .map(|&c| {
            let idx = ((c * 7.0).round() as usize).min(7);
            Span::styled(BLOCKS[idx].to_string(), Style::default().fg(sat_color(c)))
        })
        .collect();
    Line::from(spans)
}

/// The process list in the order the table displays it: sorted descending by
/// the given axis, ties broken by CPU then RSS. Shared by the renderer and the
/// selector (F3/↑↓) so "next hit" walks the same order the user sees.
pub fn ordered_procs(m: &Metrics, sort_axis: Axis) -> Vec<&ProcSample> {
    let mut procs: Vec<&ProcSample> = m.procs.iter().collect();
    procs.sort_by(|x, y| {
        sort_key(y, sort_axis)
            .partial_cmp(&sort_key(x, sort_axis))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    procs
}

fn render_processes(f: &mut Frame, area: Rect, m: &Metrics, a: &Assessment, app: &App) {
    let axis = app.sort_override.unwrap_or(a.worst);
    // Per-process network attribution isn't available from /proc without eBPF,
    // so when the network is the bottleneck we sort by CPU and say so.
    let net_fallback = axis == Axis::Network;
    let sort_axis = if net_fallback { Axis::Cpu } else { axis };

    let procs = ordered_procs(m, sort_axis);

    // Locate the followed process in the current order so we can both highlight
    // it and scroll it into view (it may have drifted far down since selection).
    let sel_idx = app
        .selected_pid
        .and_then(|pid| procs.iter().position(|p| p.pid == pid));

    let visible = area.height.saturating_sub(3) as usize; // borders + header
    // Keep the selected row roughly centred so the eye can follow it as the
    // sort reshuffles; clamp so we never scroll past the ends.
    let max_scroll = procs.len().saturating_sub(visible);
    let scroll = match sel_idx {
        Some(i) if procs.len() > visible => i.saturating_sub(visible / 2).min(max_scroll),
        _ => 0,
    };

    let q = app.search_query.to_lowercase();
    let sel_style = Style::default()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let hit_style = Style::default().fg(Color::Yellow);

    let rows: Vec<Row> = procs
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(i, p)| {
            let row = Row::new(vec![
                p.pid.to_string(),
                truncate(&p.comm, 24),
                format!("{:.1}", p.cpu_frac * 100.0),
                fmt_bytes(p.rss),
                fmt_io(p.io_read_bps),
                fmt_io(p.io_write_bps),
            ]);
            if Some(i) == sel_idx {
                row.style(sel_style)
            } else if !q.is_empty()
                && (p.comm.to_lowercase().contains(&q) || p.pid.to_string().contains(&q))
            {
                row.style(hit_style)
            } else {
                row
            }
        })
        .collect();

    let header = Row::new(vec!["PID", "COMMAND", "CPU%", "RSS", "RD/s", "WR/s"])
        .style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));

    let widths = [
        Constraint::Length(7),
        Constraint::Min(16),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(11),
    ];

    let title = if net_fallback {
        " Top processes — per-process network N/A, sorted by CPU ".to_string()
    } else {
        format!(" Top processes by {} ", axis.label())
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(title_block(&title));
    f.render_widget(table, area);
}

/// Sort descending by the chosen axis, then break ties by CPU, then RSS — so
/// that when the primary metric is zero (or unattributable), we still surface
/// *active* processes rather than idle kernel threads in arbitrary hash order.
fn sort_key(p: &ProcSample, axis: Axis) -> (f64, f64, f64) {
    let io = p.io_read_bps.unwrap_or(0.0) + p.io_write_bps.unwrap_or(0.0);
    let primary = match axis {
        Axis::Cpu => p.cpu_frac,
        Axis::Memory => p.rss as f64,
        Axis::Disk => io,
        Axis::Network => p.cpu_frac,
    };
    (primary, p.cpu_frac, p.rss as f64)
}

/// Format a per-process I/O rate, showing `—` when the counter was unreadable
/// (another user's process) rather than a misleading `0 B/s`.
fn fmt_io(rate: Option<f64>) -> String {
    match rate {
        Some(v) => fmt_rate(v),
        None => "—".to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn render_help(f: &mut Frame, area: Rect) {
    let key = Style::default().add_modifier(Modifier::REVERSED);
    let help = Line::from(vec![
        Span::styled(" q ", key),
        Span::raw(" quit   "),
        Span::styled(" 1-4 ", key),
        Span::raw(" sort   "),
        Span::styled(" 0 ", key),
        Span::raw(" auto   "),
        Span::styled(" / ", key),
        Span::raw(" search   "),
        Span::styled(" F3 ", key),
        Span::raw(" next   "),
        Span::styled(" ↑↓ ", key),
        Span::raw(" select   "),
        Span::styled(" F9 ", key),
        Span::raw(" kill"),
    ]);
    f.render_widget(Paragraph::new(help), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::Duration;

    /// Full render path with real sampled metrics must not panic, at a couple
    /// of terminal sizes (including a cramped one where panes are tiny).
    #[test]
    fn renders_without_panic() {
        let mut app = App::new();
        app.on_tick();
        std::thread::sleep(Duration::from_millis(50));
        app.on_tick();
        assert!(app.metrics.is_some(), "second tick should yield metrics");

        for (w, h) in [(120, 40), (80, 24), (40, 15)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &app)).unwrap();
        }
    }

    /// The interactive overlays (search box, kill confirm) and a followed
    /// selection must render without panic, including when the selected row is
    /// far enough down to force a scroll.
    #[test]
    fn renders_selector_overlays() {
        let mut app = App::new();
        app.on_tick();
        std::thread::sleep(Duration::from_millis(50));
        app.on_tick();
        let m = app.metrics.as_ref().expect("metrics");

        // Follow the last process in sort order — exercises the scroll path.
        let axis = app.sort_axis(m);
        let last = ordered_procs(m, axis).last().map(|p| p.pid);
        app.selected_pid = last;

        for mode in [InputMode::Normal, InputMode::Search, InputMode::Kill] {
            app.mode = mode;
            app.search_query = "sys".to_string();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|f| render(f, &app)).unwrap();
        }

        // A lingering status message path.
        app.mode = InputMode::Normal;
        app.status = Some(("Sent SIGTERM to 1234 (foo)".to_string(), 2));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
    }

    /// The "collecting" first-frame path (metrics still None) must also render.
    #[test]
    fn renders_first_frame_without_metrics() {
        let app = App::new();
        assert!(app.metrics.is_none());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &app)).unwrap();
    }
}
