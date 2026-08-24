//! Turns the four saturation fractions into a single verdict: which axis, if
//! any, is the current bottleneck.

use crate::metrics::Metrics;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Cpu,
    Memory,
    Disk,
    Network,
}

impl Axis {
    pub fn label(self) -> &'static str {
        match self {
            Axis::Cpu => "CPU",
            Axis::Memory => "Memory",
            Axis::Disk => "Disk I/O",
            Axis::Network => "Network",
        }
    }
}

/// Below this, nothing is meaningfully constrained.
pub const CLEAR: f64 = 0.5;
/// At/above this, the axis is effectively saturated.
pub const SATURATED: f64 = 0.8;

pub struct Assessment {
    /// Saturation per axis in [Cpu, Memory, Disk, Network] order, 0..1. CPU /
    /// memory / disk are PSI stall fractions (utilization proxy only when PSI is
    /// unavailable); network is link-speed saturation.
    pub sats: [f64; 4],
    pub worst: Axis,
    pub worst_sat: f64,
}

pub fn assess(m: &Metrics) -> Assessment {
    // The verdict is about *degradation* — were tasks actually delayed? — not
    // raw utilization. So CPU, memory and disk are all driven by the kernel's
    // PSI stall signal (`some avg10`), falling back to their utilization proxy
    // only on kernels without PSI.
    //
    // This deliberately makes "busy but not hurting" read as clear:
    //   * CPU  — 100% busy with no run-queue backlog stalls no one → ~0.
    //   * Disk — 100% %util that keeps up (fast-and-shallow) stalls no one → ~0.
    //   * Mem  — cache-full, or merely swap-holding, stalls no one → ~0.
    // Utilization stays visible on the gauges; PSI just decides the bottleneck.
    let cpu = m.psi.cpu.some.unwrap_or(m.cpu.usage);
    let mem = match m.psi.mem.some {
        Some(p) => p,
        // The old "swapping ⇒ near-saturated" heuristic trips on a single
        // reclaimed page, so it survives only as a no-PSI fallback.
        None if m.mem.swapping => m.mem.used_frac.max(0.9),
        None => m.mem.used_frac,
    };
    let disk = m.psi.io.some.unwrap_or(m.disk.util);
    // Network has no PSI; use link-speed saturation. Unknown (wifi / no link
    // speed) counts as 0 so we never blame a network we can't measure.
    let net = m.net.sat.unwrap_or(0.0);

    let sats = [cpu, mem, disk, net];
    let axes = [Axis::Cpu, Axis::Memory, Axis::Disk, Axis::Network];

    let (idx, &worst_sat) = sats
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0, &0.0));

    Assessment {
        sats,
        worst: axes[idx],
        worst_sat,
    }
}
