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
    /// Saturation per axis in [Cpu, Memory, Disk, Network] order, 0..1.
    pub sats: [f64; 4],
    pub worst: Axis,
    pub worst_sat: f64,
}

pub fn assess(m: &Metrics) -> Assessment {
    // Memory is special: high *usage* alone isn't pain (Linux fills RAM with
    // cache on purpose), but active swapping is. When swapping, treat memory as
    // at least near-saturated so it can win the verdict.
    let mem = if m.mem.swapping {
        m.mem.used_frac.max(0.9)
    } else {
        m.mem.used_frac
    };
    // Unknown network saturation (wifi/no link speed) counts as 0 so we never
    // falsely blame the network we can't measure.
    let net = m.net.sat.unwrap_or(0.0);

    let sats = [m.cpu.usage, mem, m.disk.util, net];
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
