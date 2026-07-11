//! Raw disk counters from `/proc/diskstats`, restricted to physical devices.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Default)]
pub struct DiskCounters {
    pub sectors_read: u64,
    pub sectors_written: u64,
    /// `io_ticks`: milliseconds during which the device had I/O in flight.
    /// Its delta over an interval, divided by the interval, is device
    /// utilisation (the `%util` iostat reports) — the honest saturation signal.
    pub io_ticks: u64,
}

/// A disk sector is 512 bytes in `/proc/diskstats`, always, regardless of the
/// device's physical/logical block size.
const SECTOR_BYTES: u64 = 512;

/// Whole physical block devices only. We use the presence of a `device`
/// symlink under `/sys/block/<name>` to separate real hardware (nvme0n1, sda)
/// from virtual/stacked devices (dm-*, loop*, zram*) which either double-count
/// the underlying disk or aren't disks at all.
pub fn physical_devices() -> HashSet<String> {
    let mut set = HashSet::new();
    let Ok(entries) = fs::read_dir("/sys/block") else {
        return set;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if Path::new(&format!("/sys/block/{name}/device")).exists() {
            set.insert(name);
        }
    }
    set
}

pub fn read(devices: &HashSet<String>) -> HashMap<String, DiskCounters> {
    let mut map = HashMap::new();
    let Ok(content) = fs::read_to_string("/proc/diskstats") else {
        return map;
    };
    for line in content.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // Fields: 0 major, 1 minor, 2 name, 3 reads, 4 rmerged, 5 sectors_read,
        // 6 ms_read, 7 writes, 8 wmerged, 9 sectors_written, 10 ms_write,
        // 11 in_flight, 12 io_ticks, 13 weighted_io_ticks, ...
        if f.len() < 14 {
            continue;
        }
        let name = f[2];
        if !devices.contains(name) {
            continue;
        }
        let g = |i: usize| f.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        map.insert(
            name.to_string(),
            DiskCounters {
                sectors_read: g(5),
                sectors_written: g(9),
                io_ticks: g(12),
            },
        );
    }
    map
}

pub fn sectors_to_bytes(sectors: u64) -> u64 {
    sectors * SECTOR_BYTES
}
