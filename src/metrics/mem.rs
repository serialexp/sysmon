//! Memory levels from `/proc/meminfo` and swap activity from `/proc/vmstat`.

use std::fs;

/// Instantaneous memory levels, in bytes.
#[derive(Clone, Copy, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

pub fn read() -> MemInfo {
    let mut mi = MemInfo::default();
    let Ok(content) = fs::read_to_string("/proc/meminfo") else {
        return mi;
    };
    for line in content.lines() {
        let mut it = line.split_whitespace();
        let key = it.next().unwrap_or("");
        // meminfo values are in kB.
        let bytes = it.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) * 1024;
        match key {
            "MemTotal:" => mi.total = bytes,
            "MemAvailable:" => mi.available = bytes,
            "SwapTotal:" => mi.swap_total = bytes,
            "SwapFree:" => mi.swap_free = bytes,
            _ => {}
        }
    }
    mi
}

/// Cumulative pages swapped in / out since boot (`pswpin` / `pswpout`).
/// A nonzero delta between samples means the machine is actively swapping —
/// the clearest sign that memory pressure is hurting, distinct from merely
/// "lots of RAM used".
pub fn swap_counters() -> (u64, u64) {
    let mut pin = 0;
    let mut pout = 0;
    if let Ok(content) = fs::read_to_string("/proc/vmstat") {
        for line in content.lines() {
            if let Some(v) = line.strip_prefix("pswpin ") {
                pin = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("pswpout ") {
                pout = v.trim().parse().unwrap_or(0);
            }
        }
    }
    (pin, pout)
}
