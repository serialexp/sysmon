//! Sampling layer: reads raw `/proc` counters and turns the delta between two
//! samples into human-meaningful, per-second rates and saturation fractions.

pub mod cpu;
pub mod disk;
pub mod mem;
pub mod net;
pub mod process;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

pub use process::ProcSample;

/// A fully-derived view of the system at one tick. All `*_frac` / saturation
/// fields are 0.0..=1.0 so the four axes are directly comparable.
#[derive(Clone)]
pub struct Metrics {
    pub cpu: CpuMetrics,
    pub mem: MemMetrics,
    pub disk: DiskMetrics,
    pub net: NetMetrics,
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
}

pub struct Sampler {
    prev: Option<Raw>,
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
            devices: disk::physical_devices(),
            ifaces: net::physical_ifaces(),
            clk_tck: if clk_tck > 0 { clk_tck as f64 } else { 100.0 },
            page_size: if page_size > 0 { page_size as u64 } else { 4096 },
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
        let out = self.prev.as_ref().map(|prev| {
            let mut m = self.compute(prev, &cur);
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

        Metrics {
            mem: compute_mem(prev, cur, dt),
            net: self.compute_net(prev, cur, dt),
            procs: self.compute_procs(prev, cur, dt),
            cpu,
            disk,
            io_restricted: false,   // set by `sample` from the latched flag
            io_hint: IoHint::Full,  // set by `sample`
        }
    }

    fn compute_net(&self, prev: &Raw, cur: &Raw, dt: f64) -> NetMetrics {
        let mut per_iface = Vec::new();
        let (mut rx_bps, mut tx_bps) = (0.0, 0.0);
        let mut sat: Option<f64> = None;
        for (name, c) in &cur.nets {
            let Some(p) = prev.nets.get(name) else { continue };
            let rx = c.rx_bytes.saturating_sub(p.rx_bytes) as f64 / dt;
            let tx = c.tx_bytes.saturating_sub(p.tx_bytes) as f64 / dt;
            rx_bps += rx;
            tx_bps += tx;
            let speed_mbps = self.ifaces.get(name).and_then(|i| i.speed_mbps);
            // Saturation = throughput bits / link bits. Only defined when the
            // NIC reports a link speed (wired); unknown for wifi/virtual.
            let iface_sat = speed_mbps
                .map(|s| ((rx + tx) * 8.0 / (s as f64 * 1e6)).clamp(0.0, 1.0));
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

        let frac = |num: f64| if total_d > 0.0 { (num / total_d).clamp(0.0, 1.0) } else { 0.0 };

        let per_core = cur
            .cpu
            .cores
            .iter()
            .zip(prev.cpu.cores.iter())
            .map(|(c, p)| {
                let t = c.total().saturating_sub(p.total()) as f64;
                let b = c.busy().saturating_sub(p.busy()) as f64;
                if t > 0.0 { (b / t).clamp(0.0, 1.0) } else { 0.0 }
            })
            .collect();

        CpuMetrics {
            usage: frac(busy_d),
            iowait: frac(iowait_d),
            per_core,
        }
    }

    fn compute_procs(&self, prev: &Raw, cur: &Raw, dt: f64) -> Vec<ProcSample> {
        cur.procs
            .iter()
            .map(|(&pid, c)| {
                let prev_p = prev.procs.get(&pid);
                let cpu_frac = prev_p
                    .map(|p| {
                        let dj = c.cpu_jiffies.saturating_sub(p.cpu_jiffies) as f64;
                        (dj / self.clk_tck) / dt
                    })
                    .unwrap_or(0.0);
                let (io_read_bps, io_write_bps) = match prev_p {
                    Some(p) => (
                        opt_rate(c.read_bytes, p.read_bytes, dt),
                        opt_rate(c.write_bytes, p.write_bytes, dt),
                    ),
                    None => (None, None),
                };
                ProcSample {
                    pid,
                    comm: c.comm.clone(),
                    cpu_frac,
                    rss: c.rss,
                    io_read_bps,
                    io_write_bps,
                }
            })
            .collect()
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
    let used_frac = if m.total > 0 { used as f64 / m.total as f64 } else { 0.0 };
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
    for (name, c) in &cur.disks {
        let Some(p) = prev.disks.get(name) else { continue };
        let rb = disk::sectors_to_bytes(c.sectors_read.saturating_sub(p.sectors_read)) as f64 / dt;
        let wb =
            disk::sectors_to_bytes(c.sectors_written.saturating_sub(p.sectors_written)) as f64 / dt;
        // io_ticks is in ms; utilisation = busy_ms / interval_ms.
        let u = (c.io_ticks.saturating_sub(p.io_ticks) as f64 / (dt * 1000.0)).clamp(0.0, 1.0);
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
        iowait: 0.0, // overwritten in `compute` with the CPU iowait fraction
        per_device,
    }
}
