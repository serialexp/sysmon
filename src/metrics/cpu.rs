//! Raw CPU time counters from `/proc/stat`.

use std::fs;

/// Cumulative jiffie counters for one CPU (aggregate or a single core).
#[derive(Clone, Copy, Default)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    fn parse(fields: &[&str]) -> Self {
        let g = |i: usize| fields.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        CpuTimes {
            user: g(0),
            nice: g(1),
            system: g(2),
            idle: g(3),
            iowait: g(4),
            irq: g(5),
            softirq: g(6),
            steal: g(7),
        }
    }

    pub fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }

    /// Time counted as "not doing useful work": true idle plus iowait.
    pub fn idle_all(&self) -> u64 {
        self.idle + self.iowait
    }

    pub fn busy(&self) -> u64 {
        self.total().saturating_sub(self.idle_all())
    }
}

#[derive(Clone, Default)]
pub struct CpuSnapshot {
    pub total: CpuTimes,
    pub cores: Vec<CpuTimes>,
}

pub fn read() -> std::io::Result<CpuSnapshot> {
    let content = fs::read_to_string("/proc/stat")?;
    let mut snap = CpuSnapshot::default();
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            // The `cpu*` lines are always first; once we pass them we're done.
            break;
        };
        if let Some(agg) = rest.strip_prefix(' ') {
            // Aggregate line: "cpu  <fields>".
            let fields: Vec<&str> = agg.split_whitespace().collect();
            snap.total = CpuTimes::parse(&fields);
        } else {
            // Per-core line: "cpuN <fields>".
            let mut it = rest.split_whitespace();
            if it.next().is_some() {
                let fields: Vec<&str> = it.collect();
                snap.cores.push(CpuTimes::parse(&fields));
            }
        }
    }
    Ok(snap)
}
