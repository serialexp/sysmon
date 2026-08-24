//! Human-friendly number formatting.

/// Bytes as an IEC size, e.g. `3.4 GiB`. Used for absolute quantities (RAM, RSS).
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// A byte-rate as decimal MB/s etc. Used for disk throughput, where people read
/// in decimal ("500 MB/s SSD").
pub fn fmt_rate(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 4] = ["B/s", "kB/s", "MB/s", "GB/s"];
    let mut v = bytes_per_sec.max(0.0);
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// A latency given in milliseconds, rendered at a readable scale: sub-ms as
/// microseconds, then ms, then seconds for pathological queues. Used for disk
/// `await` where values span µs (NVMe cache hit) to seconds (thrashing).
pub fn fmt_latency(ms: f64) -> String {
    let ms = ms.max(0.0);
    if ms < 1.0 {
        format!("{:.0}µs", ms * 1000.0)
    } else if ms < 10.0 {
        format!("{ms:.1}ms")
    } else if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.1}s", ms / 1000.0)
    }
}

/// A byte-rate expressed in *bits* per second (Mb/s), the convention for
/// network links.
pub fn fmt_bits(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 4] = ["b/s", "kb/s", "Mb/s", "Gb/s"];
    let mut v = bytes_per_sec.max(0.0) * 8.0;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
