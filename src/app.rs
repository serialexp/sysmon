//! Application state: owns the sampler, the latest metrics, and the sparkline
//! histories. Also owns the interactive process selector (search / follow /
//! kill), whose keystrokes are dispatched here via [`App::on_key`].

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::bottleneck::{self, Axis};
use crate::history::History;
use crate::metrics::{Metrics, ProcSample, Sampler};
use crate::ui;

const HISTORY_LEN: usize = 240;
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
    /// The kill confirmation is open for [`App::selected_pid`].
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
    /// The process the selector is locked onto, tracked by PID so the highlight
    /// *follows that process* across samples even as the sort reshuffles rows.
    pub selected_pid: Option<i32>,
    /// The current incremental-search query (matched against command and PID).
    pub search_query: String,
    /// Which input overlay is active.
    pub mode: InputMode,
    /// A transient one-line status (e.g. the result of a kill), with a countdown
    /// of ticks remaining before it clears.
    pub status: Option<(String, u8)>,
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
            selected_pid: None,
            search_query: String::new(),
            mode: InputMode::Normal,
            status: None,
            should_quit: false,
        }
    }

    pub fn on_tick(&mut self) {
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
        if let (Some(pid), Some(m)) = (self.selected_pid, &self.metrics) {
            if !m.procs.iter().any(|p| p.pid == pid) {
                self.selected_pid = None;
            }
        }

        // Age out any transient status message.
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
                if self.selected_pid.is_some() || !self.search_query.is_empty() {
                    self.selected_pid = None;
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
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
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
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
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
        if self.selected_pid.is_some() {
            self.mode = InputMode::Kill;
        } else {
            self.set_status("select a process first (↑/↓ or /)".to_string());
        }
    }

    /// Re-point the selection at the first process matching the current query,
    /// in display order — the incremental "jump as you type" behaviour. Clears
    /// the selection when the query is empty or nothing matches.
    fn reselect_first(&mut self) {
        let pid = {
            let Some(m) = &self.metrics else { return };
            if self.search_query.is_empty() {
                self.selected_pid = None;
                return;
            }
            let q = self.search_query.to_lowercase();
            let axis = self.sort_axis(m);
            ui::ordered_procs(m, axis)
                .into_iter()
                .find(|p| matches(p, &q))
                .map(|p| p.pid)
        };
        self.selected_pid = pid;
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
            let hits: Vec<i32> = ui::ordered_procs(m, axis)
                .into_iter()
                .filter(|p| matches(p, &q))
                .map(|p| p.pid)
                .collect();
            if hits.is_empty() {
                return;
            }
            let cur = self
                .selected_pid
                .and_then(|pid| hits.iter().position(|&h| h == pid));
            let idx = match cur {
                Some(i) if forward => (i + 1) % hits.len(),
                Some(i) => (i + hits.len() - 1) % hits.len(),
                None if forward => 0,
                None => hits.len() - 1,
            };
            hits[idx]
        };
        self.selected_pid = Some(next);
    }

    /// Move the selection one row up/down through the *full* sorted list (not
    /// just hits) — plain keyboard navigation, independent of any search.
    fn move_sel(&mut self, delta: isize) {
        let next = {
            let Some(m) = &self.metrics else { return };
            let axis = self.sort_axis(m);
            let order = ui::ordered_procs(m, axis);
            if order.is_empty() {
                return;
            }
            let cur = self
                .selected_pid
                .and_then(|pid| order.iter().position(|p| p.pid == pid));
            let idx = match cur {
                Some(i) => (i as isize + delta).clamp(0, order.len() as isize - 1) as usize,
                None if delta > 0 => 0,
                None => order.len() - 1,
            };
            order[idx].pid
        };
        self.selected_pid = Some(next);
    }

    fn kill_selected(&mut self, sig: i32, sig_name: &str) {
        let Some(pid) = self.selected_pid else { return };
        let comm = self
            .metrics
            .as_ref()
            .and_then(|m| m.procs.iter().find(|p| p.pid == pid))
            .map(|p| p.comm.clone())
            .unwrap_or_default();

        let ret = unsafe { libc::kill(pid, sig) };
        if ret == 0 {
            self.set_status(format!("Sent {sig_name} to {pid} ({comm})"));
        } else {
            let e = std::io::Error::last_os_error();
            self.set_status(format!("kill {pid} ({comm}): {e}"));
        }
    }

    fn set_status(&mut self, msg: String) {
        self.status = Some((msg, STATUS_TICKS));
    }
}

/// Does this process match the (already-lowercased) query? Matches a substring
/// of the command name, or of the PID's decimal form.
fn matches(p: &ProcSample, q: &str) -> bool {
    p.comm.to_lowercase().contains(q) || p.pid.to_string().contains(q)
}
