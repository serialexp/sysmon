//! Per-process counters from `/proc/<pid>/stat` and `/proc/<pid>/io`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};

/// Raw cumulative counters for one process at one instant.
pub struct ProcRaw {
    pub comm: String,
    /// utime + stime, in jiffies.
    pub cpu_jiffies: u64,
    /// Resident set size, in bytes.
    pub rss: u64,
    /// Bytes actually fetched from / sent to the block layer. `None` when we
    /// lack permission (processes owned by other users, e.g. root daemons).
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
}

/// A process with per-second rates computed against the previous sample.
#[derive(Clone)]
pub struct ProcSample {
    pub pid: i32,
    pub comm: String,
    /// Fraction of a single core, 0..N (can exceed 1.0 when multithreaded),
    /// matching htop's per-core CPU% convention.
    pub cpu_frac: f64,
    pub rss: u64,
    /// `None` when `/proc/<pid>/io` is unreadable (a process owned by another
    /// user, typically root) — distinct from `Some(0.0)` meaning "readable and
    /// genuinely idle". We must not render the former as a fabricated zero.
    pub io_read_bps: Option<f64>,
    pub io_write_bps: Option<f64>,
}

/// The process table plus whether we were denied permission to read any
/// process's I/O accounting this pass. `io_denied == true` means at least one
/// `/proc/<pid>/io` returned `EACCES` — i.e. we lack `CAP_SYS_PTRACE` and aren't
/// that process's owner, so full per-process I/O attribution is unavailable.
pub struct Readout {
    pub procs: HashMap<i32, ProcRaw>,
    pub io_denied: bool,
}

pub fn read_all(page_size: u64) -> Readout {
    let mut map = HashMap::new();
    let mut io_denied = false;
    let Ok(dir) = fs::read_dir("/proc") else {
        return Readout {
            procs: map,
            io_denied,
        };
    };
    // Reuse one buffer across all pids to avoid a fresh allocation per process.
    let mut buf = String::with_capacity(4096);
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else { continue };

        buf.clear();
        // The process may exit between readdir and open; just skip on any error.
        if File::open(format!("/proc/{pid}/stat"))
            .and_then(|mut f| f.read_to_string(&mut buf))
            .is_err()
        {
            continue;
        }
        let Some((comm, cpu_jiffies, rss)) = parse_stat(&buf, page_size) else {
            continue;
        };
        let (read_bytes, write_bytes, denied) = read_io(pid);
        io_denied |= denied;
        map.insert(
            pid,
            ProcRaw {
                comm,
                cpu_jiffies,
                rss,
                read_bytes,
                write_bytes,
            },
        );
    }
    Readout {
        procs: map,
        io_denied,
    }
}

/// Parse the subset of `/proc/<pid>/stat` we need. The `comm` field is wrapped
/// in parentheses and may itself contain spaces and parentheses, so we split on
/// the *last* ')' before tokenising the numeric tail.
fn parse_stat(s: &str, page_size: u64) -> Option<(String, u64, u64)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let tail: Vec<&str> = s.get(close + 1..)?.split_whitespace().collect();
    // After ')', tail[0] is `state` (field 3). So field N lives at tail[N - 3]:
    //   utime = field 14 -> tail[11], stime = field 15 -> tail[12],
    //   rss (pages) = field 24 -> tail[21].
    let utime: u64 = tail.get(11)?.parse().ok()?;
    let stime: u64 = tail.get(12)?.parse().ok()?;
    let rss_pages: u64 = tail.get(21).and_then(|x| x.parse().ok()).unwrap_or(0);
    Some((comm, utime + stime, rss_pages * page_size))
}

/// Returns `(read_bytes, write_bytes, denied)`. `denied` is true only for a
/// genuine permission error (`EACCES`) — the kernel's `ptrace_may_access` gate
/// on `/proc/<pid>/io`. A process exiting mid-scan yields `ENOENT`, which is a
/// race, not a privilege signal, so it does not set `denied`.
fn read_io(pid: i32) -> (Option<u64>, Option<u64>, bool) {
    let content = match fs::read_to_string(format!("/proc/{pid}/io")) {
        Ok(c) => c,
        Err(e) => return (None, None, e.kind() == ErrorKind::PermissionDenied),
    };
    let mut read = None;
    let mut write = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            read = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            write = v.trim().parse().ok();
        }
    }
    (read, write, false)
}
