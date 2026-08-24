//! Sampling layer: reads raw `/proc` counters and turns the delta between two
//! samples into human-meaningful, per-second rates and saturation fractions.

pub mod cpu;
pub mod disk;
pub mod mem;
pub mod net;
pub mod process;
pub mod psi;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

pub use process::{ProcSample, ProcessIdentity};
pub use psi::Pressure;

/// A fully-derived view of the system at one tick. All `*_frac` / saturation
/// fields are 0.0..=1.0 so the four axes are directly comparable.
#[derive(Clone)]
pub struct Metrics {
    pub cpu: CpuMetrics,
    pub mem: MemMetrics,
    pub disk: DiskMetrics,
    pub net: NetMetrics,
    pub load: LoadMetrics,
    /// Kernel pressure-stall info per resource (the honest "am I stalling?"
    /// signal). All-`None` when PSI is unavailable on this kernel.
    pub psi: Pressure,
    /// One row per userspace process. CPU, RSS and I/O belong only to that PID;
    /// descendants are deliberately not charged to their parents.
    pub procs: Vec<ProcSample>,
    /// True when we lack permission to read some processes' I/O accounting, so
    /// per-process I/O attribution is incomplete. False when running as root or
    /// when nothing was denied.
    pub io_restricted: bool,
    /// What (if anything) to suggest to the user to get full I/O attribution.
    pub io_hint: IoHint,
}

/// Guidance derived from *why* I/O attribution is restricted, so the UI can
/// suggest the action that will actually help on this system.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IoHint {
    /// Full attribution — running as root, or nothing was denied. No banner.
    Full,
    /// Restricted and we hold no `CAP_SYS_PTRACE`: suggest the one-time
    /// `sysmon --grant` (works on stock kernels).
    TryGrant,
    /// Restricted *despite* holding `CAP_SYS_PTRACE` — this kernel ignores the
    /// capability for `/proc/<pid>/io` (observed on some distro kernels), so
    /// only real root works. Suggest `sysmon --sudo`.
    CapInert,
}

#[derive(Clone)]
pub struct CpuMetrics {
    pub usage: f64,
    pub iowait: f64,
    pub per_core: Vec<f64>,
}

/// Run-queue pressure from `/proc/loadavg`, plus the core count to normalise
/// against. `one / cores > 1` means more runnable+blocked work than the machine
/// can service — includes D-state, so it catches I/O contention that
/// instantaneous CPU busy% misses.
#[derive(Clone, Copy, Default)]
pub struct LoadMetrics {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
    pub cores: usize,
}

#[derive(Clone)]
pub struct MemMetrics {
    pub used: u64,
    pub total: u64,
    pub used_frac: f64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub swapping: bool,
}

#[derive(Clone)]
pub struct DiskMetrics {
    pub read_bps: f64,
    pub write_bps: f64,
    /// Max `%util` across physical devices, 0..1 — the saturation signal.
    pub util: f64,
    /// Average service latency per completed request, in ms, op-count-weighted
    /// across devices (`await`). Distinguishes slow-and-shallow (high await, low
    /// throughput, still 100% util) from fast-and-deep at the same %util. 0 when
    /// no requests completed this interval.
    pub await_ms: f64,
    /// Average number of outstanding I/Os across all devices (`aqu-sz`) — the
    /// time-averaged queue depth. Near 1 means the device is busy but barely
    /// queued; deep queues mean genuine backlog.
    pub aqu_sz: f64,
    /// System-wide iowait fraction (copied from CPU) shown alongside disk as a
    /// corroborating signal.
    pub iowait: f64,
    pub per_device: Vec<DeviceRate>,
}

#[derive(Clone)]
pub struct DeviceRate {
    pub name: String,
    pub read_bps: f64,
    pub write_bps: f64,
    pub util: f64,
}

#[derive(Clone)]
pub struct NetMetrics {
    pub rx_bps: f64,
    pub tx_bps: f64,
    /// Max link utilisation across NICs with a known speed, 0..1. `None` when
    /// no physical NIC reports a link speed (e.g. wifi-only).
    pub sat: Option<f64>,
    pub per_iface: Vec<IfaceRate>,
}

#[derive(Clone)]
pub struct IfaceRate {
    pub name: String,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub speed_mbps: Option<u64>,
    pub sat: Option<f64>,
}

/// One instant's raw counters, kept so the next sample can diff against it.
struct Raw {
    at: Instant,
    cpu: cpu::CpuSnapshot,
    mem: mem::MemInfo,
    swap_in: u64,
    swap_out: u64,
    disks: HashMap<String, disk::DiskCounters>,
    nets: HashMap<String, net::NetCounters>,
    procs: HashMap<i32, process::ProcRaw>,
    /// PSI is a windowed average the kernel maintains, so we don't diff it — we
    /// just snapshot the current sample's reading.
    psi: Pressure,
    /// `/proc/loadavg` 1/5/15-minute figures; also a snapshot, not a delta.
    load: (f64, f64, f64),
}

const PROCESS_AVERAGE_SAMPLES: usize = 3;

#[derive(Clone, Copy, Default)]
struct ProcessObservation {
    cpu_frac: f64,
    rss: u64,
    io_read_bps: Option<f64>,
    io_write_bps: Option<f64>,
}

struct ProcessWindow {
    observations: [ProcessObservation; PROCESS_AVERAGE_SAMPLES],
    next: usize,
    len: usize,
    last_seen: u64,
}

impl Default for ProcessWindow {
    fn default() -> Self {
        Self {
            observations: [ProcessObservation::default(); PROCESS_AVERAGE_SAMPLES],
            next: 0,
            len: 0,
            last_seen: 0,
        }
    }
}

pub struct Sampler {
    prev: Option<Raw>,
    process_windows: HashMap<ProcessIdentity, ProcessWindow>,
    process_sample_generation: u64,
    devices: HashSet<String>,
    ifaces: HashMap<String, net::IfaceInfo>,
    clk_tck: f64,
    page_size: u64,
    /// Latches true once any `/proc/<pid>/io` read is denied. Privilege only
    /// ever drops within a run (never gains), so a sticky flag is stable against
    /// a transient pass where every unreadable process happened to have exited.
    io_restricted: bool,
    /// Effective euid==0 at startup — root sees every process's I/O.
    is_root: bool,
    /// Whether we hold `CAP_SYS_PTRACE` (effective). Distinguishes "no privilege
    /// yet, try --grant" from "have the cap but it's inert, need real root".
    has_ptrace_cap: bool,
}

impl Sampler {
    pub fn new() -> Self {
        // Safe: these sysconf names are always valid; a negative result just
        // means "unknown", for which we substitute the universal defaults.
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Self {
            prev: None,
            process_windows: HashMap::new(),
            process_sample_generation: 0,
            devices: disk::physical_devices(),
            ifaces: net::physical_ifaces(),
            clk_tck: if clk_tck > 0 { clk_tck as f64 } else { 100.0 },
            page_size: if page_size > 0 {
                page_size as u64
            } else {
                4096
            },
            io_restricted: false,
            is_root: unsafe { libc::geteuid() } == 0,
            has_ptrace_cap: has_cap_sys_ptrace(),
        }
    }

    fn read_raw(&self) -> (Raw, bool) {
        let (swap_in, swap_out) = mem::swap_counters();
        let procs = process::read_all(self.page_size);
        let raw = Raw {
            at: Instant::now(),
            cpu: cpu::read().unwrap_or_default(),
            mem: mem::read(),
            swap_in,
            swap_out,
            disks: disk::read(&self.devices),
            nets: net::read(&self.ifaces),
            procs: procs.procs,
            psi: psi::read_all(),
            load: read_loadavg(),
        };
        (raw, procs.io_denied)
    }

    /// Take a sample. Returns `None` the very first time (no previous sample to
    /// diff against); every subsequent call returns derived metrics.
    pub fn sample(&mut self) -> Option<Metrics> {
        let (cur, io_denied) = self.read_raw();
        self.io_restricted |= io_denied;
        let restricted = self.io_restricted;
        // Root always has full attribution; if nothing was denied we're also
        // full. Otherwise steer to the fix that will actually work here.
        let hint = if self.is_root || !restricted {
            IoHint::Full
        } else if self.has_ptrace_cap {
            IoHint::CapInert
        } else {
            IoHint::TryGrant
        };
        let out = self.prev.take().map(|prev| {
            let mut m = self.compute(&prev, &cur);
            self.process_sample_generation = self.process_sample_generation.wrapping_add(1);
            smooth_process_samples(
                &mut m.procs,
                &mut self.process_windows,
                self.process_sample_generation,
            );
            m.io_restricted = restricted;
            m.io_hint = hint;
            m
        });
        self.prev = Some(cur);
        out
    }

    fn compute(&self, prev: &Raw, cur: &Raw) -> Metrics {
        let dt = (cur.at - prev.at).as_secs_f64().max(1e-3);

        let cpu = self.compute_cpu(prev, cur);
        let mut disk = compute_disk(prev, cur, dt);
        // iowait is a CPU-side counter but we surface it on the disk pane as a
        // corroborating "the CPU is stalled waiting on I/O" signal.
        disk.iowait = cpu.iowait;

        let load = LoadMetrics {
            one: cur.load.0,
            five: cur.load.1,
            fifteen: cur.load.2,
            cores: cpu.per_core.len(),
        };

        let procs = self.compute_procs(prev, cur, dt);
        Metrics {
            mem: compute_mem(prev, cur, dt),
            net: self.compute_net(prev, cur, dt),
            procs,
            cpu,
            disk,
            load,
            psi: cur.psi,
            io_restricted: false,  // set by `sample` from the latched flag
            io_hint: IoHint::Full, // set by `sample`
        }
    }

    fn compute_net(&self, prev: &Raw, cur: &Raw, dt: f64) -> NetMetrics {
        let mut per_iface = Vec::new();
        let (mut rx_bps, mut tx_bps) = (0.0, 0.0);
        let mut sat: Option<f64> = None;
        for (name, c) in &cur.nets {
            let Some(p) = prev.nets.get(name) else {
                continue;
            };
            let rx = c.rx_bytes.saturating_sub(p.rx_bytes) as f64 / dt;
            let tx = c.tx_bytes.saturating_sub(p.tx_bytes) as f64 / dt;
            rx_bps += rx;
            tx_bps += tx;
            let speed_mbps = self.ifaces.get(name).and_then(|i| i.speed_mbps);
            // Saturation = throughput bits / link bits. Only defined when the
            // NIC reports a link speed (wired); unknown for wifi/virtual.
            let iface_sat =
                speed_mbps.map(|s| ((rx + tx) * 8.0 / (s as f64 * 1e6)).clamp(0.0, 1.0));
            if let Some(s) = iface_sat {
                sat = Some(sat.unwrap_or(0.0).max(s));
            }
            per_iface.push(IfaceRate {
                name: name.clone(),
                rx_bps: rx,
                tx_bps: tx,
                speed_mbps,
                sat: iface_sat,
            });
        }
        per_iface.sort_by(|a, b| {
            (b.rx_bps + b.tx_bps)
                .partial_cmp(&(a.rx_bps + a.tx_bps))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        NetMetrics {
            rx_bps,
            tx_bps,
            sat,
            per_iface,
        }
    }

    fn compute_cpu(&self, prev: &Raw, cur: &Raw) -> CpuMetrics {
        let (ct, pt) = (cur.cpu.total, prev.cpu.total);
        let total_d = ct.total().saturating_sub(pt.total()) as f64;
        let busy_d = ct.busy().saturating_sub(pt.busy()) as f64;
        let iowait_d = ct.iowait.saturating_sub(pt.iowait) as f64;

        let frac = |num: f64| {
            if total_d > 0.0 {
                (num / total_d).clamp(0.0, 1.0)
            } else {
                0.0
            }
        };

        let per_core = cur
            .cpu
            .cores
            .iter()
            .zip(prev.cpu.cores.iter())
            .map(|(c, p)| {
                let t = c.total().saturating_sub(p.total()) as f64;
                let b = c.busy().saturating_sub(p.busy()) as f64;
                if t > 0.0 {
                    (b / t).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            })
            .collect();

        CpuMetrics {
            usage: frac(busy_d),
            iowait: frac(iowait_d),
            per_core,
        }
    }

    fn compute_procs(&self, prev: &Raw, cur: &Raw, dt: f64) -> Vec<ProcSample> {
        compute_process_samples(&prev.procs, &cur.procs, dt, self.clk_tck)
    }
}

impl ProcessWindow {
    fn push(&mut self, observation: ProcessObservation, generation: u64) -> ProcessObservation {
        self.observations[self.next] = observation;
        self.next = (self.next + 1) % PROCESS_AVERAGE_SAMPLES;
        self.len = (self.len + 1).min(PROCESS_AVERAGE_SAMPLES);
        self.last_seen = generation;

        let observations = &self.observations[..self.len];
        ProcessObservation {
            cpu_frac: observations.iter().map(|v| v.cpu_frac).sum::<f64>() / self.len as f64,
            rss: observations
                .iter()
                .map(|v| v.rss as u128)
                .sum::<u128>()
                .div_ceil(self.len as u128) as u64,
            io_read_bps: average_optional(observations.iter().map(|v| v.io_read_bps)),
            io_write_bps: average_optional(observations.iter().map(|v| v.io_write_bps)),
        }
    }
}

fn average_optional(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0;
    for value in values {
        sum += value?;
        count += 1;
    }
    (count > 0).then_some(sum / count as f64)
}

fn smooth_process_samples(
    samples: &mut [ProcSample],
    windows: &mut HashMap<ProcessIdentity, ProcessWindow>,
    generation: u64,
) {
    for sample in samples {
        if sample.rate_valid {
            let averaged = windows.entry(sample.identity).or_default().push(
                ProcessObservation {
                    cpu_frac: sample.cpu_frac,
                    rss: sample.rss,
                    io_read_bps: sample.io_read_bps,
                    io_write_bps: sample.io_write_bps,
                },
                generation,
            );
            sample.cpu_frac = averaged.cpu_frac;
            sample.rss = averaged.rss;
            sample.io_read_bps = averaged.io_read_bps;
            sample.io_write_bps = averaged.io_write_bps;
        } else {
            // The first sighting has no rate interval yet. Keep its current RSS
            // visible but wait for the next tick before starting its rate window.
            windows.entry(sample.identity).or_default().last_seen = generation;
        }
    }
    windows.retain(|_, window| window.last_seen == generation);
}

fn compute_process_samples(
    prev: &HashMap<i32, process::ProcRaw>,
    cur: &HashMap<i32, process::ProcRaw>,
    dt: f64,
    clk_tck: f64,
) -> Vec<ProcSample> {
    let mut samples = Vec::with_capacity(cur.len());
    let mut kernel_cache = HashMap::with_capacity(cur.len());
    let mut ancestry = Vec::new();
    for (&pid, c) in cur {
        // htop's useful default is one row per userspace process: hide the
        // kthreadd forest, but never fold an application's descendants into
        // the launcher that happened to create them.
        if is_kernel_process(pid, cur, &mut kernel_cache, &mut ancestry) {
            continue;
        }

        let identity = ProcessIdentity {
            pid,
            start_time: c.start_time,
        };
        let prev_p = prev
            .get(&pid)
            .filter(|previous| previous.start_time == c.start_time);
        let rate_valid = prev_p.is_some();
        let cpu_frac = prev_p
            .map(|p| {
                let dj = c.cpu_jiffies.saturating_sub(p.cpu_jiffies) as f64;
                (dj / clk_tck) / dt
            })
            .unwrap_or(0.0);
        let (io_read_bps, io_write_bps) = match prev_p {
            Some(p) => (
                opt_rate(c.read_bytes, p.read_bytes, dt),
                opt_rate(c.write_bytes, p.write_bytes, dt),
            ),
            None => (None, None),
        };
        samples.push(ProcSample {
            identity,
            pid,
            comm: c.label.clone(),
            rate_valid,
            cpu_frac,
            rss: c.rss,
            state: c.state,
            io_read_bps,
            io_write_bps,
        });
    }
    samples
}

/// Linux kernel threads belong to the process forest rooted at kthreadd (PID 2).
/// Follow PPIDs rather than guessing from state, RSS, or command spelling: active
/// kernel workers remain kernel threads, while an idle userspace process remains
/// visible and naturally sorts below resource consumers.
///
/// `ancestry` is shared scratch so this once-per-process hot path does not create
/// and free a small allocation for every PID.
fn is_kernel_process(
    pid: i32,
    procs: &HashMap<i32, process::ProcRaw>,
    cache: &mut HashMap<i32, bool>,
    ancestry: &mut Vec<i32>,
) -> bool {
    if let Some(&is_kernel) = cache.get(&pid) {
        return is_kernel;
    }

    ancestry.clear();
    let mut current = pid;
    let is_kernel = loop {
        if current == 2 {
            break true;
        }
        if current <= 1 {
            break false;
        }
        if let Some(&cached) = cache.get(&current) {
            break cached;
        }
        if ancestry.contains(&current) {
            break false;
        }
        ancestry.push(current);
        let Some(proc) = procs.get(&current) else {
            break false;
        };
        current = proc.ppid;
    };

    for ancestor in ancestry.iter().copied() {
        cache.insert(ancestor, is_kernel);
    }
    is_kernel
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(ppid: i32) -> process::ProcRaw {
        process::ProcRaw {
            label: "proc".into(),
            state: 'S',
            ppid,
            start_time: 1,
            cpu_jiffies: 0,
            rss: 0,
            read_bytes: Some(0),
            write_bytes: Some(0),
        }
    }

    fn classified_as_kernel(pid: i32, procs: &HashMap<i32, process::ProcRaw>) -> bool {
        is_kernel_process(pid, procs, &mut HashMap::new(), &mut Vec::new())
    }

    #[test]
    fn identifies_the_complete_kthreadd_forest_as_kernel_processes() {
        let procs = HashMap::from([
            (1, raw(0)),
            (2, raw(0)),
            (10, raw(2)),
            (11, raw(10)),
            (20, raw(1)),
            (21, raw(20)),
        ]);

        assert!(classified_as_kernel(2, &procs));
        assert!(classified_as_kernel(10, &procs));
        assert!(classified_as_kernel(11, &procs));
        assert!(!classified_as_kernel(1, &procs));
        assert!(!classified_as_kernel(20, &procs));
        assert!(!classified_as_kernel(21, &procs));
    }

    #[test]
    fn missing_or_cyclic_ancestry_is_not_guessed_to_be_kernel_owned() {
        let procs = HashMap::from([(10, raw(999)), (20, raw(21)), (21, raw(20))]);

        assert!(!classified_as_kernel(10, &procs));
        assert!(!classified_as_kernel(20, &procs));
        assert!(!classified_as_kernel(21, &procs));
    }

    #[test]
    fn process_samples_keep_parent_and_child_accounting_separate() {
        let mut parent_prev = raw(1);
        parent_prev.label = "plasmashell".into();
        parent_prev.cpu_jiffies = 100;
        parent_prev.rss = 200;
        parent_prev.read_bytes = Some(1_000);
        let mut child_prev = raw(10);
        child_prev.label = "firefox".into();
        child_prev.cpu_jiffies = 200;
        child_prev.rss = 800;
        child_prev.read_bytes = Some(2_000);

        let mut parent_cur = raw(1);
        parent_cur.label = "plasmashell".into();
        parent_cur.cpu_jiffies = 101;
        parent_cur.rss = 210;
        parent_cur.read_bytes = Some(1_100);
        let mut child_cur = raw(10);
        child_cur.label = "firefox".into();
        child_cur.cpu_jiffies = 225;
        child_cur.rss = 820;
        child_cur.read_bytes = Some(3_000);

        let prev = HashMap::from([(10, parent_prev), (11, child_prev)]);
        let cur = HashMap::from([(10, parent_cur), (11, child_cur)]);
        let samples = compute_process_samples(&prev, &cur, 1.0, 100.0);
        let parent = samples.iter().find(|p| p.pid == 10).expect("parent");
        let child = samples.iter().find(|p| p.pid == 11).expect("child");

        assert_eq!(parent.comm, "plasmashell");
        assert_eq!(parent.cpu_frac, 0.01);
        assert_eq!(parent.rss, 210);
        assert_eq!(parent.io_read_bps, Some(100.0));
        assert_eq!(child.comm, "firefox");
        assert_eq!(child.cpu_frac, 0.25);
        assert_eq!(child.rss, 820);
        assert_eq!(child.io_read_bps, Some(1_000.0));
    }

    #[test]
    fn rolling_average_uses_three_observations_and_resets_for_reused_pid() {
        let old_identity = ProcessIdentity {
            pid: 10,
            start_time: 100,
        };
        let new_identity = ProcessIdentity {
            pid: 10,
            start_time: 200,
        };
        let mut windows = HashMap::new();

        for (generation, cpu, rss) in [(1, 0.0, 100), (2, 0.9, 400), (3, 0.0, 100)] {
            let mut samples = vec![ProcSample {
                identity: old_identity,
                pid: 10,
                comm: "old".into(),
                rate_valid: true,
                cpu_frac: cpu,
                rss,
                state: 'S',
                io_read_bps: Some(cpu * 100.0),
                io_write_bps: Some(0.0),
            }];
            smooth_process_samples(&mut samples, &mut windows, generation);
            if generation == 3 {
                assert_eq!(samples[0].cpu_frac, 0.3);
                assert_eq!(samples[0].rss, 200);
                assert_eq!(samples[0].io_read_bps, Some(30.0));
            }
        }

        let mut replacement = vec![ProcSample {
            identity: new_identity,
            pid: 10,
            comm: "new".into(),
            rate_valid: true,
            cpu_frac: 0.6,
            rss: 700,
            state: 'R',
            io_read_bps: Some(60.0),
            io_write_bps: None,
        }];
        smooth_process_samples(&mut replacement, &mut windows, 4);
        assert_eq!(replacement[0].cpu_frac, 0.6);
        assert_eq!(replacement[0].rss, 700);
        assert_eq!(replacement[0].io_read_bps, Some(60.0));
        assert_eq!(replacement[0].io_write_bps, None);
        assert_eq!(windows.len(), 1);
        assert!(windows.contains_key(&new_identity));
    }

    #[test]
    fn first_sighting_does_not_add_a_synthetic_zero_to_the_rate_window() {
        let identity = ProcessIdentity {
            pid: 10,
            start_time: 100,
        };
        let mut windows = HashMap::new();
        let mut first = vec![ProcSample {
            identity,
            pid: 10,
            comm: "new".into(),
            rate_valid: false,
            cpu_frac: 0.0,
            rss: 500,
            state: 'S',
            io_read_bps: None,
            io_write_bps: None,
        }];
        smooth_process_samples(&mut first, &mut windows, 1);

        let mut measured = vec![ProcSample {
            identity,
            pid: 10,
            comm: "new".into(),
            rate_valid: true,
            cpu_frac: 0.9,
            rss: 600,
            state: 'R',
            io_read_bps: Some(90.0),
            io_write_bps: Some(0.0),
        }];
        smooth_process_samples(&mut measured, &mut windows, 2);
        assert_eq!(measured[0].cpu_frac, 0.9);
        assert_eq!(measured[0].rss, 600);
        assert_eq!(measured[0].io_read_bps, Some(90.0));
    }

    #[test]
    fn process_samples_do_not_diff_a_reused_pid_against_its_predecessor() {
        let mut old = raw(1);
        old.start_time = 100;
        old.cpu_jiffies = 10_000;
        old.read_bytes = Some(50_000);
        let mut replacement = raw(1);
        replacement.start_time = 200;
        replacement.cpu_jiffies = 5;
        replacement.read_bytes = Some(20);

        let samples = compute_process_samples(
            &HashMap::from([(10, old)]),
            &HashMap::from([(10, replacement)]),
            1.0,
            100.0,
        );
        assert_eq!(samples[0].identity.start_time, 200);
        assert!(!samples[0].rate_valid);
        assert_eq!(samples[0].cpu_frac, 0.0);
        assert_eq!(samples[0].io_read_bps, None);
    }

    #[test]
    fn process_samples_omit_kernel_forest_even_when_active() {
        let prev = HashMap::from([(2, raw(0)), (10, raw(2)), (20, raw(1))]);
        let mut active_worker = raw(2);
        active_worker.cpu_jiffies = 100;
        active_worker.rss = 4096;
        active_worker.read_bytes = Some(10_000);
        let cur = HashMap::from([(2, raw(0)), (10, active_worker), (20, raw(1))]);

        let samples = compute_process_samples(&prev, &cur, 1.0, 100.0);
        assert!(!samples.iter().any(|p| matches!(p.pid, 2 | 10)));
        assert!(samples.iter().any(|p| p.pid == 20));
    }
}

/// Whether this process holds `CAP_SYS_PTRACE` in its effective set, read from
/// `/proc/self/status` (so we need no libcap dependency). Bit 19 is
/// `CAP_SYS_PTRACE`. Used only to pick which hint to show, never to gate reads.
fn has_cap_sys_ptrace() -> bool {
    const CAP_SYS_PTRACE: u32 = 19;
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("CapEff:") {
            if let Ok(v) = u64::from_str_radix(rest.trim(), 16) {
                return v & (1 << CAP_SYS_PTRACE) != 0;
            }
        }
    }
    false
}

/// The 1/5/15-minute load averages from `/proc/loadavg` (its first three
/// fields). Zeroes if the file is unreadable or malformed.
fn read_loadavg() -> (f64, f64, f64) {
    let Ok(s) = std::fs::read_to_string("/proc/loadavg") else {
        return (0.0, 0.0, 0.0);
    };
    let mut it = s.split_whitespace();
    let mut next = || it.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (next(), next(), next())
}

/// Per-second rate between two cumulative counters, or `None` if either
/// endpoint was unreadable (so callers can distinguish "unknown" from "zero").
fn opt_rate(cur: Option<u64>, prev: Option<u64>, dt: f64) -> Option<f64> {
    match (cur, prev) {
        (Some(c), Some(p)) => Some(c.saturating_sub(p) as f64 / dt),
        _ => None,
    }
}

fn compute_mem(prev: &Raw, cur: &Raw, dt: f64) -> MemMetrics {
    let m = cur.mem;
    let used = m.total.saturating_sub(m.available);
    let used_frac = if m.total > 0 {
        used as f64 / m.total as f64
    } else {
        0.0
    };
    let swap_used = m.swap_total.saturating_sub(m.swap_free);
    let swap_rate = (cur.swap_in.saturating_sub(prev.swap_in)
        + cur.swap_out.saturating_sub(prev.swap_out)) as f64
        / dt;
    MemMetrics {
        used,
        total: m.total,
        used_frac,
        swap_used,
        swap_total: m.swap_total,
        swapping: swap_rate > 0.0,
    }
}

fn compute_disk(prev: &Raw, cur: &Raw, dt: f64) -> DiskMetrics {
    let mut per_device = Vec::new();
    let (mut read_bps, mut write_bps, mut util) = (0.0, 0.0, 0.0_f64);
    // `await` aggregates op-count-weighted across devices (total service ms ÷
    // total completed requests); `aqu-sz` sums per-device average queue depths.
    let (mut svc_ms, mut ops, mut aqu_sz) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (name, c) in &cur.disks {
        let Some(p) = prev.disks.get(name) else {
            continue;
        };
        let rb = disk::sectors_to_bytes(c.sectors_read.saturating_sub(p.sectors_read)) as f64 / dt;
        let wb =
            disk::sectors_to_bytes(c.sectors_written.saturating_sub(p.sectors_written)) as f64 / dt;
        // io_ticks is in ms; utilisation = busy_ms / interval_ms.
        let u = (c.io_ticks.saturating_sub(p.io_ticks) as f64 / (dt * 1000.0)).clamp(0.0, 1.0);
        let d_ops =
            c.reads.saturating_sub(p.reads) as f64 + c.writes.saturating_sub(p.writes) as f64;
        let d_svc = c.ms_read.saturating_sub(p.ms_read) as f64
            + c.ms_written.saturating_sub(p.ms_written) as f64;
        // weighted_io_ticks is in ms; ÷ interval-ms gives average queue depth.
        aqu_sz += c.weighted_io_ticks.saturating_sub(p.weighted_io_ticks) as f64 / (dt * 1000.0);
        ops += d_ops;
        svc_ms += d_svc;
        read_bps += rb;
        write_bps += wb;
        util = util.max(u);
        per_device.push(DeviceRate {
            name: name.clone(),
            read_bps: rb,
            write_bps: wb,
            util: u,
        });
    }
    per_device.sort_by(|a, b| {
        (b.read_bps + b.write_bps)
            .partial_cmp(&(a.read_bps + a.write_bps))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    DiskMetrics {
        read_bps,
        write_bps,
        util,
        await_ms: if ops > 0.0 { svc_ms / ops } else { 0.0 },
        aqu_sz,
        iowait: 0.0, // overwritten in `compute` with the CPU iowait fraction
        per_device,
    }
}
