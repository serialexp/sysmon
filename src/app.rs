//! Application state: owns the sampler, the latest metrics, and the sparkline
//! histories. Also owns the interactive process selector (search / follow /
//! kill), whose keystrokes are dispatched here via [`App::on_key`].

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::bottleneck::{self, Axis};
use crate::history::History;
use crate::metrics::{Metrics, ProcSample, ProcessIdentity, Sampler};
use crate::ui;

const HISTORY_LEN: usize = 240;
/// Number of table rows moved by Page Up/Down. This stays useful regardless of
/// terminal height without coupling input handling to the renderer's layout.
const PAGE_STEP: isize = 10;
/// How many ticks (≈ seconds) a transient status message stays on screen.
const STATUS_TICKS: u8 = 4;

/// Which interactive overlay, if any, is currently capturing input. The search
/// query itself lives outside this enum (in [`App::search_query`]) so it — and
/// the followed selection — survive after the search box is closed, letting F3
/// keep cycling hits from normal mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    /// The incremental search box is open; typed characters edit the query.
    Search,
    /// The kill confirmation is open for [`App::selected_process`].
    Kill,
}

pub struct App {
    sampler: Sampler,
    pub metrics: Option<Metrics>,
    pub h_cpu: History,
    pub h_mem: History,
    pub h_disk: History,
    pub h_net: History,
    /// When set, the process table is sorted by this axis instead of the
    /// current bottleneck.
    pub sort_override: Option<Axis>,
    /// The process lifetime the selector is locked onto. Start time prevents a
    /// reused numeric PID from inheriting a highlight or destructive action.
    pub selected_process: Option<ProcessIdentity>,
    /// The current incremental-search query (matched against command and PID).
    pub search_query: String,
    /// Which input overlay is active.
    pub mode: InputMode,
    /// A transient one-line status (e.g. the result of a kill), with a countdown
    /// of ticks remaining before it clears.
    pub status: Option<(String, u8)>,
    /// When true, sampling is suspended so the once-a-second frame holds still
    /// long enough to actually read. Toggled with space.
    pub paused: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            sampler: Sampler::new(),
            metrics: None,
            h_cpu: History::new(HISTORY_LEN),
            h_mem: History::new(HISTORY_LEN),
            h_disk: History::new(HISTORY_LEN),
            h_net: History::new(HISTORY_LEN),
            sort_override: None,
            selected_process: None,
            search_query: String::new(),
            mode: InputMode::Normal,
            status: None,
            paused: false,
            should_quit: false,
        }
    }

    pub fn on_tick(&mut self) {
        // While frozen we skip sampling entirely so the displayed frame — and
        // its histories — hold still. The next unpause computes rates across the
        // whole paused interval, yielding an interval average rather than losing
        // that activity or fabricating a one-second spike.
        if self.paused {
            self.age_status();
            return;
        }
        if let Some(m) = self.sampler.sample() {
            self.h_cpu.push(m.cpu.usage);
            self.h_mem.push(m.mem.used_frac);
            self.h_disk.push(m.disk.util);
            self.h_net.push(m.net.sat.unwrap_or(0.0));
            self.metrics = Some(m);
        }

        // Drop a selection whose process has exited. This both stops the
        // highlight lingering on a dead row and — importantly — prevents a kill
        // from later landing on an unrelated process that reused the PID.
        if let (Some(identity), Some(m)) = (self.selected_process, &self.metrics) {
            if !m.procs.iter().any(|p| p.identity == identity) {
                self.selected_process = None;
            }
        }

        self.age_status();
    }

    /// Count down and clear a transient status message. Runs every tick,
    /// including while paused, so a kill result still fades on schedule.
    fn age_status(&mut self) {
        if let Some((_, n)) = &mut self.status {
            if *n == 0 {
                self.status = None;
            } else {
                *n -= 1;
            }
        }
    }

    /// Resolve the axis the process table is currently sorted by. Mirrors the
    /// logic in [`ui::render_processes`]: an explicit override wins, else the
    /// bottleneck; Network falls back to CPU (no per-process net attribution).
    pub fn sort_axis(&self, m: &Metrics) -> Axis {
        let axis = self.sort_override.unwrap_or(bottleneck::assess(m).worst);
        if axis == Axis::Network {
            Axis::Cpu
        } else {
            axis
        }
    }

    /// Dispatch a keypress. All interactive behaviour (sort, search, follow,
    /// kill, quit) funnels through here so the input model lives in one place.
    pub fn on_key(&mut self, key: KeyEvent) {
        match self.mode {
            InputMode::Kill => self.on_key_kill(key),
            InputMode::Search => self.on_key_search(key),
            InputMode::Normal => self.on_key_normal(key),
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            // Esc backs out of a selection/search first; only quits when there's
            // nothing to clear (preserving the old "Esc quits" reflex).
            KeyCode::Esc => {
                if self.selected_process.is_some() || !self.search_query.is_empty() {
                    self.selected_process = None;
                    self.search_query.clear();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('1') => self.sort_override = Some(Axis::Cpu),
            KeyCode::Char('2') => self.sort_override = Some(Axis::Memory),
            KeyCode::Char('3') => self.sort_override = Some(Axis::Disk),
            KeyCode::Char('4') => self.sort_override = Some(Axis::Network),
            KeyCode::Char('0') => self.sort_override = None,
            // Freeze/unfreeze the display so a 1 Hz frame can be read.
            KeyCode::Char(' ') => self.paused = !self.paused,
            // Open the incremental search box with a fresh query.
            KeyCode::Char('/') => {
                self.search_query.clear();
                self.mode = InputMode::Search;
            }
            // F3 opens search when there's no query yet, otherwise cycles hits
            // (Shift+F3 goes backwards).
            KeyCode::F(3) => {
                if self.search_query.is_empty() {
                    self.mode = InputMode::Search;
                } else {
                    self.cycle_hit(!shift);
                }
            }
            // n / N walk hits like F3 / Shift+F3, for terminals that swallow the
            // function keys. Only active with a live query, so `n` is free to
            // mean something else if we ever need it when not searching.
            KeyCode::Char('n') if !self.search_query.is_empty() => self.cycle_hit(true),
            KeyCode::Char('N') if !self.search_query.is_empty() => self.cycle_hit(false),
            KeyCode::Home => self.select_edge(false),
            KeyCode::End => self.select_edge(true),
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-PAGE_STEP),
            KeyCode::PageDown => self.move_sel(PAGE_STEP),
            // F9 (htop's kill key) or `k` opens the kill confirmation.
            KeyCode::F(9) | KeyCode::Char('k') => self.open_kill(),
            _ => {}
        }
    }

    fn on_key_search(&mut self, key: KeyEvent) {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            // Close the box but keep the query + selection so F3 still cycles.
            KeyCode::Enter | KeyCode::Esc => self.mode = InputMode::Normal,
            KeyCode::Backspace => {
                self.search_query.pop();
                self.reselect_first();
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.reselect_first();
            }
            KeyCode::F(3) => self.cycle_hit(!shift),
            KeyCode::Home => self.select_edge(false),
            KeyCode::End => self.select_edge(true),
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-PAGE_STEP),
            KeyCode::PageDown => self.move_sel(PAGE_STEP),
            _ => {}
        }
    }

    fn on_key_kill(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.kill_selected(libc::SIGTERM, "SIGTERM");
                self.mode = InputMode::Normal;
            }
            KeyCode::Char('k') | KeyCode::Char('K') => {
                self.kill_selected(libc::SIGKILL, "SIGKILL");
                self.mode = InputMode::Normal;
            }
            KeyCode::Esc => self.mode = InputMode::Normal,
            _ => {}
        }
    }

    fn open_kill(&mut self) {
        if self.selected_process.is_some() {
            self.mode = InputMode::Kill;
        } else {
            self.set_status("select a process first (↑/↓ or /)".to_string());
        }
    }

    /// Re-point the selection at the first process matching the current query,
    /// in display order — the incremental "jump as you type" behaviour. Clears
    /// the selection when the query is empty or nothing matches.
    fn reselect_first(&mut self) {
        let identity = {
            let Some(m) = &self.metrics else { return };
            if self.search_query.is_empty() {
                self.selected_process = None;
                return;
            }
            let q = self.search_query.to_lowercase();
            let axis = self.sort_axis(m);
            ui::ordered_procs(m, axis)
                .into_iter()
                .find(|p| matches(p, &q))
                .map(|p| p.identity)
        };
        self.selected_process = identity;
    }

    /// Advance the selection to the next (or previous) process matching the
    /// query, cycling with wraparound. This is F3's "switch between hits".
    fn cycle_hit(&mut self, forward: bool) {
        let next = {
            let Some(m) = &self.metrics else { return };
            let q = self.search_query.to_lowercase();
            if q.is_empty() {
                return;
            }
            let axis = self.sort_axis(m);
            let hits: Vec<ProcessIdentity> = ui::ordered_procs(m, axis)
                .into_iter()
                .filter(|p| matches(p, &q))
                .map(|p| p.identity)
                .collect();
            if hits.is_empty() {
                return;
            }
            let cur = self
                .selected_process
                .and_then(|identity| hits.iter().position(|&hit| hit == identity));
            let idx = match cur {
                Some(i) if forward => (i + 1) % hits.len(),
                Some(i) => (i + hits.len() - 1) % hits.len(),
                None if forward => 0,
                None => hits.len() - 1,
            };
            hits[idx]
        };
        self.selected_process = Some(next);
    }

    /// Select the first or last row in display order. Used by Home and End.
    fn select_edge(&mut self, last: bool) {
        let next = {
            let Some(m) = &self.metrics else { return };
            let axis = self.sort_axis(m);
            let order = ui::ordered_procs(m, axis);
            let Some(p) = (if last { order.last() } else { order.first() }) else {
                return;
            };
            p.identity
        };
        self.selected_process = Some(next);
    }

    /// Move the selection by one row or one page through the *full* sorted list
    /// (not just hits), independently of any search.
    fn move_sel(&mut self, delta: isize) {
        let next = {
            let Some(m) = &self.metrics else { return };
            let axis = self.sort_axis(m);
            let order = ui::ordered_procs(m, axis);
            if order.is_empty() {
                return;
            }
            let cur = self
                .selected_process
                .and_then(|identity| order.iter().position(|p| p.identity == identity));
            order[selection_index(order.len(), cur, delta)].identity
        };
        self.selected_process = Some(next);
    }

    fn kill_selected(&mut self, sig: i32, sig_name: &str) {
        let Some(identity) = self.selected_process else {
            return;
        };
        let Some(comm) = self.metrics.as_ref().and_then(|m| {
            m.procs
                .iter()
                .find(|p| p.identity == identity)
                .map(|p| p.comm.clone())
        }) else {
            return;
        };

        // A displayed row accounts for exactly one process lifetime. pidfd keeps
        // both that scope and the PID-reuse guarantee atomic at signal time.
        match crate::metrics::process::signal(identity, sig) {
            Ok(()) => self.set_status(format!("Sent {sig_name} to {} ({comm})", identity.pid)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.selected_process = None;
                self.set_status(format!("Process {} ({comm}) has exited", identity.pid));
            }
            Err(error) => self.set_status(format!(
                "Failed to send {sig_name} to {} ({comm}): {error}",
                identity.pid
            )),
        }
    }

    fn set_status(&mut self, msg: String) {
        self.status = Some((msg, STATUS_TICKS));
    }
}

/// Calculate the next selected row, clamping rather than wrapping at either
/// end. With no current selection, forward movement begins at the first row and
/// backward movement begins at the last.
fn selection_index(len: usize, current: Option<usize>, delta: isize) -> usize {
    debug_assert!(len > 0);
    match current {
        Some(i) => (i as isize + delta).clamp(0, len as isize - 1) as usize,
        None if delta > 0 => 0,
        None => len - 1,
    }
}

/// Does this process match the (already-lowercased) query? Matches a substring
/// of the command name, or of the PID's decimal form.
fn matches(p: &ProcSample, q: &str) -> bool {
    p.comm.to_lowercase().contains(q) || p.pid.to_string().contains(q)
}

#[cfg(test)]
mod tests {
    use super::selection_index;

    #[test]
    fn selection_navigation_clamps_at_list_edges() {
        assert_eq!(selection_index(20, Some(12), -10), 2);
        assert_eq!(selection_index(20, Some(12), 10), 19);
        assert_eq!(selection_index(20, Some(2), -10), 0);
        assert_eq!(selection_index(20, Some(18), 10), 19);
    }

    #[test]
    fn selection_navigation_starts_at_the_directional_edge() {
        assert_eq!(selection_index(20, None, 1), 0);
        assert_eq!(selection_index(20, None, -1), 19);
    }
}
