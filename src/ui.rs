//! All rendering. Layout is: verdict bar / 2x2 axis grid / process table / help.

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Gauge, Paragraph, Row, Sparkline, Table},
    Frame,
};

use crate::app::{App, InputMode};
use crate::bottleneck::{self, Assessment, Axis, CLEAR, SATURATED};
use crate::history::History;
use crate::metrics::{IoHint, Metrics, ProcSample};
use crate::util::{fmt_bits, fmt_bytes, fmt_latency, fmt_rate};

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    let Some(m) = &app.metrics else {
        let p = Paragraph::new("Collecting first sample…").alignment(Alignment::Center);
        f.render_widget(p, area);
        return;
    };

    let assess = bottleneck::assess(m);
    // When we lack the privilege to read other users' I/O, some writers show as
    // `—`. Explain that on its own dedicated line (0 height when not shown) so
    // the message never gets truncated off a block title. `io_hint` is `Full`
    // under root (or when nothing was denied), so this line isn't drawn there.
    let banner_h = if m.io_hint == IoHint::Full { 0 } else { 1 };
    // A 1-column side margin (in place of the old borders) keeps text off the
    // very edge of the terminal without boxing anything in.
    let rows = Layout::vertical([
        Constraint::Length(1),        // verdict
        Constraint::Min(9),           // 2x2 grid
        Constraint::Length(banner_h), // I/O-permission banner (conditional)
        Constraint::Length(12),       // process table
        Constraint::Length(1),        // help
    ])
    .horizontal_margin(1)
    .split(area);

    render_verdict(f, rows[0], &assess, app.paused);
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
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        // Block cursor so the (possibly empty) input is visibly focused.
        Span::styled("▏", Style::default().fg(Color::Cyan)),
        Span::styled(count, dim),
        Span::styled("   F3 next · Esc done", dim),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_kill_line(f: &mut Frame, area: Rect, m: &Metrics, app: &App) {
    let pid = app
        .selected_process
        .map(|identity| identity.pid)
        .unwrap_or(0);
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
        Span::styled(format!(" Kill process {pid} ({comm})? "), warn),
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
    let line = Line::from(vec![Span::styled(command, cmd), Span::styled(note, expl)]);
    f.render_widget(Paragraph::new(line), area);
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

fn render_verdict(f: &mut Frame, area: Rect, a: &Assessment, paused: bool) {
    let mut spans = vec![Span::styled(
        " sysmon ",
        Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
    )];
    // A frozen frame is easy to mistake for a hung one — call it out loudly.
    if paused {
        spans.push(Span::styled(
            " PAUSED ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // One fixed badge per axis instead of a single "worst" headline. Because each
    // axis reports its own state in its own fixed slot, the bar no longer flaps:
    // nothing here changes unless *that* axis actually crosses a threshold, and
    // the status word is padded to a constant width so columns never shift.
    let axes = [
        ("CPU", a.sats[0]),
        ("MEM", a.sats[1]),
        ("DISK", a.sats[2]),
        ("NET", a.sats[3]),
    ];
    for (label, sat) in axes {
        let (word, color) = status_badge(sat);
        spans.push(Span::styled(
            format!("   {label} "),
            Style::default().add_modifier(Modifier::DIM),
        ));
        spans.push(Span::styled(
            format!("{word:<9}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A stable per-axis status word + colour from its saturation. Fixed vocabulary
/// (`ok` / `elevated` / `saturated`) so the verdict bar reads at a glance and,
/// unlike a live percentage, doesn't churn every tick.
fn status_badge(sat: f64) -> (&'static str, Color) {
    if sat < CLEAR {
        ("ok", Color::Green)
    } else if sat < SATURATED {
        ("elevated", Color::Yellow)
    } else {
        ("saturated", Color::Red)
    }
}

fn render_grid(f: &mut Frame, area: Rect, m: &Metrics, app: &App) {
    // The one-cell middle tracks are dividers only: unlike pane borders, they
    // separate neighbours without drawing a box around any section.
    let rows = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .split(area);
    let top = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .split(rows[0]);
    let bot = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .split(rows[2]);
    render_grid_dividers(f, area, top[1].x, rows[1].y);

    // CPU — headline carries load average (run-queue depth incl. D-state), the
    // per-core strip carries the CPU stall % (PSI) when the kernel provides it.
    let cpu_head = if m.load.cores > 0 {
        format!(
            "{:.0}%   load {:.2} / {}",
            m.cpu.usage * 100.0,
            m.load.one,
            m.load.cores
        )
    } else {
        format!("{:.0}%", m.cpu.usage * 100.0)
    };
    let mut cpu_detail = cores_line(&m.cpu.per_core);
    // Label the strip so it reads as a spatial per-core view, not a second
    // history sparkline like the one below it.
    cpu_detail.spans.insert(
        0,
        Span::styled("per-core ", Style::default().fg(Color::DarkGray)),
    );
    if let Some(s) = stall_span(m.psi.cpu.some) {
        cpu_detail.spans.push(s);
    }
    axis_pane(
        f,
        top[0],
        "CPU",
        sat_color(m.cpu.usage),
        cpu_head,
        cpu_detail,
        m.cpu.usage,
        format!("{:.0}%", m.cpu.usage * 100.0),
        &app.h_cpu,
    );

    // Memory
    let mut mem_detail = Line::from(format!(
        "swap {} / {}{}",
        fmt_bytes(m.mem.swap_used),
        fmt_bytes(m.mem.swap_total),
        if m.mem.swapping { "   swapping" } else { "" }
    ));
    if let Some(s) = stall_span(m.psi.mem.some) {
        mem_detail.spans.push(s);
    }
    axis_pane(
        f,
        top[2],
        "Memory",
        sat_color(m.mem.used_frac),
        format!("{} / {}", fmt_bytes(m.mem.used), fmt_bytes(m.mem.total)),
        mem_detail,
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
        {
            // await + aqu-sz make %util legible: they separate slow-and-shallow
            // (high await, low queue — 100% util at a trickle) from fast-and-deep.
            let mut d = Line::from(format!(
                "util {:.0}%   await {}   aqu {:.1}",
                m.disk.util * 100.0,
                fmt_latency(m.disk.await_ms),
                m.disk.aqu_sz,
            ));
            if let Some(s) = stall_span(m.psi.io.some) {
                d.spans.push(s);
            }
            d
        },
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
        bot[2],
        "Network",
        net_color,
        format!(
            "Rx {}    Tx {}",
            fmt_bits(m.net.rx_bps),
            fmt_bits(m.net.tx_bps)
        ),
        net_detail,
        net_ratio,
        net_label,
        &app.h_net,
    );
}

fn render_grid_dividers(f: &mut Frame, area: Rect, vertical_x: u16, horizontal_y: u16) {
    let divider = Style::default().fg(Color::DarkGray);
    let buffer = f.buffer_mut();

    for y in area.y..area.bottom() {
        if let Some(cell) = buffer.cell_mut((vertical_x, y)) {
            cell.set_symbol("│").set_style(divider);
        }
    }
    for x in area.x..area.right() {
        if let Some(cell) = buffer.cell_mut((x, horizontal_y)) {
            cell.set_symbol("─").set_style(divider);
        }
    }
    if let Some(cell) = buffer.cell_mut((vertical_x, horizontal_y)) {
        cell.set_symbol("┼").set_style(divider);
    }
}

/// One axis cell (borderless): a coloured `Title  headline` line, a saturation
/// gauge, a detail line, and a sparkline of recent saturation.
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
    let parts = Layout::vertical([
        Constraint::Length(1), // title + headline
        Constraint::Length(1), // gauge
        Constraint::Length(1), // detail
        Constraint::Min(1),    // sparkline
    ])
    .split(area);

    let strong = Style::default().fg(color).add_modifier(Modifier::BOLD);
    // Title and headline share one line now that there's no border to carry the
    // title; the label stays colour-coded so the four panes read apart.
    let head = Line::from(vec![
        Span::styled(format!("{title}  "), strong),
        Span::styled(big, strong),
    ]);
    f.render_widget(Paragraph::new(head), parts[0]);
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

/// A dim "stall N%" span carrying the PSI `some avg10` for an axis — the honest
/// "tasks were actually delayed this much" figure. `None` when PSI is
/// unavailable on this kernel, so nothing is drawn.
fn stall_span(some: Option<f64>) -> Option<Span<'static>> {
    some.map(|v| {
        // Tint it toward the saturation palette so a painful stall reads at a
        // glance without competing with the pane's headline colour.
        let c = if v < 0.10 {
            Color::DarkGray
        } else {
            sat_color(v)
        };
        Span::styled(
            format!("   stall {:.0}%", v * 100.0),
            Style::default().fg(c),
        )
    })
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

    // Split off a one-line coloured title above the (borderless) table.
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    let title_area = parts[0];
    let table_area = parts[1];

    let title = if net_fallback {
        "Top processes — 3-sample avg, network N/A, sorted by CPU".to_string()
    } else {
        format!(
            "Top processes by {} — 3-sample rolling average",
            axis.label()
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        title_area,
    );

    // Locate the followed process in the current order so we can both highlight
    // it and scroll it into view (it may have drifted far down since selection).
    let sel_idx = app
        .selected_process
        .and_then(|identity| procs.iter().position(|p| p.identity == identity));

    let visible = table_area.height.saturating_sub(1) as usize; // header row
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
                Cell::from(p.pid.to_string()),
                Cell::from(truncate(&p.comm, 25)),
                Cell::from(p.state.to_string()).style(state_style(p.state)),
                Cell::from(format!("{:.1}", p.cpu_frac * 100.0)),
                Cell::from(fmt_bytes(p.rss)),
                Cell::from(fmt_io(p.io_read_bps)),
                Cell::from(fmt_io(p.io_write_bps)),
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

    let header = Row::new(vec!["PID", "COMMAND", "S", "CPU%", "RSS", "RD/s", "WR/s"])
        .style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));

    let widths = [
        Constraint::Length(7),
        Constraint::Min(17),
        Constraint::Length(1),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(11),
    ];

    let table = Table::new(rows, widths).header(header);
    f.render_widget(table, table_area);
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

/// Colour a process state char so the interesting ones pop: `D` (uninterruptible
/// sleep — blocked on I/O, the answer to "who's stuck?") in bold red, `R`
/// (running) green, `Z` (zombie) magenta, everything else dim.
fn state_style(state: char) -> Style {
    match state {
        'D' => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        'R' => Style::default().fg(Color::Green),
        'Z' => Style::default().fg(Color::Magenta),
        _ => Style::default().fg(Color::DarkGray),
    }
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
        Span::styled(" Home/End PgUp/Dn ", key),
        Span::raw(" jump   "),
        Span::styled(" F9 ", key),
        Span::raw(" kill process   "),
        Span::styled(" spc ", key),
        Span::raw(" pause"),
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
        let last = ordered_procs(m, axis).last().map(|p| p.identity);
        app.selected_process = last;

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

    #[test]
    fn grid_dividers_separate_sections_without_surrounding_them() {
        let mut terminal = Terminal::new(TestBackend::new(11, 7)).unwrap();
        terminal
            .draw(|f| render_grid_dividers(f, Rect::new(1, 1, 9, 5), 5, 3))
            .unwrap();
        let buffer = terminal.backend().buffer();

        for y in 1..6 {
            assert_eq!(buffer[(5, y)].symbol(), if y == 3 { "┼" } else { "│" });
        }
        for x in 1..10 {
            assert_eq!(buffer[(x, 3)].symbol(), if x == 5 { "┼" } else { "─" });
        }

        // Divider endpoints stop at the grid's edges; no line turns a corner and
        // continues around a section as a border would.
        for (x, y) in [(1, 1), (9, 1), (1, 5), (9, 5), (5, 0), (0, 3), (10, 3)] {
            assert_eq!(buffer[(x, y)].symbol(), " ");
        }
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
