//! Raw network counters from `/proc/net/dev`, restricted to physical NICs.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Default)]
pub struct NetCounters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// A physical interface plus its link speed (Mbit/s) if the kernel reports one.
#[derive(Clone, Default)]
pub struct IfaceInfo {
    pub speed_mbps: Option<u64>,
}

/// Physical interfaces only, keyed by name. Same `device`-symlink trick as for
/// disks: it keeps eno1/wlp10s0 and drops docker0, bridges, the ~18 veth pairs,
/// tailscale0 and lo — all of which report bogus 10000 Mb/s speeds and would
/// double-count container traffic if summed into the "network" signal.
pub fn physical_ifaces() -> HashMap<String, IfaceInfo> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir("/sys/class/net") else {
        return map;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !Path::new(&format!("/sys/class/net/{name}/device")).exists() {
            continue;
        }
        // `speed` is in Mbit/s; wired NICs report it, wifi/virtual often return
        // -1 or EINVAL. Treat anything <= 0 or unreadable as "unknown".
        let speed_mbps = fs::read_to_string(format!("/sys/class/net/{name}/speed"))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .map(|v| v as u64);
        map.insert(name, IfaceInfo { speed_mbps });
    }
    map
}

pub fn read(ifaces: &HashMap<String, IfaceInfo>) -> HashMap<String, NetCounters> {
    let mut map = HashMap::new();
    let Ok(content) = fs::read_to_string("/proc/net/dev") else {
        return map;
    };
    for line in content.lines() {
        // Data lines look like "  eth0: <rx...> <tx...>"; header lines have no colon.
        let Some(colon) = line.find(':') else {
            continue;
        };
        let name = line[..colon].trim();
        if !ifaces.contains_key(name) {
            continue;
        }
        let f: Vec<&str> = line[colon + 1..].split_whitespace().collect();
        // Receive block is fields 0..8, transmit block 8..16. Byte counts are
        // the first column of each block: rx = f[0], tx = f[8].
        if f.len() < 16 {
            continue;
        }
        map.insert(
            name.to_string(),
            NetCounters {
                rx_bytes: f[0].parse().unwrap_or(0),
                tx_bytes: f[8].parse().unwrap_or(0),
            },
        );
    }
    map
}
