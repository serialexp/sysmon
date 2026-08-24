//! Per-process counters from `/proc/<pid>/stat` and `/proc/<pid>/io`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};

/// Stable identity for one lifetime of a PID. Linux can reuse a numeric PID;
/// `/proc/<pid>/stat` field 22 distinguishes the replacement from its predecessor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub start_time: u64,
}

/// Raw cumulative counters for one process at one instant.
pub struct ProcRaw {
    /// A human-useful command label from `/proc/<pid>/cmdline`: executable plus
    /// its first argument when useful. Avoids ambiguous thread-style names such
    /// as `MainThread` in the process table.
    pub label: String,
    /// Scheduler state char from `/proc/<pid>/stat` field 3: `R` running,
    /// `D` uninterruptible sleep (blocked on I/O — the interesting one),
    /// `S` sleeping, `Z` zombie, `T`/`t` stopped/traced, `I` idle kernel thread.
    pub state: char,
    /// Parent PID from `/proc/<pid>/stat` field 4. Used to identify kernel
    /// threads by ancestry under kthreadd (PID 2).
    pub ppid: i32,
    /// Process start time since boot, in jiffies (`/proc/<pid>/stat` field 22).
    pub start_time: u64,
    /// utime + stime, in jiffies.
    pub cpu_jiffies: u64,
    /// Resident set size, in bytes.
    pub rss: u64,
    /// Bytes actually fetched from / sent to the block layer. `None` when we
    /// lack permission (processes owned by other users, e.g. root daemons).
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
}

/// One userspace process with per-second rates computed against the previous
/// sample. Linux exposes each process as its thread-group leader in `/proc`; we
/// intentionally do not scan `/proc/<pid>/task`, so worker threads are not rows.
#[derive(Clone)]
pub struct ProcSample {
    pub identity: ProcessIdentity,
    pub pid: i32,
    pub comm: String,
    /// Whether CPU/I/O rates had a matching previous sample for this exact
    /// process lifetime. New processes can show RSS immediately without a
    /// synthetic zero polluting their rolling rate average.
    pub(crate) rate_valid: bool,
    /// Fraction of a single core, 0..N (can exceed 1.0 when multithreaded),
    /// matching htop's per-core CPU% convention.
    pub cpu_frac: f64,
    pub rss: u64,
    /// Scheduler state char (see [`ProcRaw::state`]). `D` means blocked on I/O.
    pub state: char,
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
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };

        buf.clear();
        // The process may exit between readdir and open; just skip on any error.
        if File::open(format!("/proc/{pid}/stat"))
            .and_then(|mut f| f.read_to_string(&mut buf))
            .is_err()
        {
            continue;
        }
        let Some((comm, state, ppid, cpu_jiffies, start_time, rss)) = parse_stat(&buf, page_size)
        else {
            continue;
        };
        let label = read_label(pid, &comm);
        let (read_bytes, write_bytes, denied) = read_io(pid);
        io_denied |= denied;
        map.insert(
            pid,
            ProcRaw {
                label,
                state,
                ppid,
                start_time,
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
fn parse_stat(s: &str, page_size: u64) -> Option<(String, char, i32, u64, u64, u64)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    // No collection: this runs once per PID on each sample, so parse just the
    // fields we need from the tail iterator.
    let mut tail = s.get(close + 1..)?.split_whitespace();
    // After ')', tail[0] is `state` (field 3). So field N lives at tail[N - 3]:
    // state = field 3, ppid = field 4, utime = field 14, stime = field 15,
    // starttime = field 22, rss (pages) = field 24.
    let state = tail.next()?.chars().next().unwrap_or('?');
    let ppid: i32 = tail.next()?.parse().ok()?;
    let utime: u64 = tail.nth(9)?.parse().ok()?;
    let stime: u64 = tail.next()?.parse().ok()?;
    let start_time: u64 = tail.nth(6)?.parse().ok()?;
    let rss_pages: u64 = tail.nth(1)?.parse().ok()?;
    Some((
        comm,
        state,
        ppid,
        utime + stime,
        start_time,
        rss_pages * page_size,
    ))
}

/// Read a compact label from the NUL-separated argv vector. `/proc/<pid>/comm`
/// may be a runtime thread name (Node uses `MainThread`), whereas argv normally
/// identifies the actual program. Keep at most the executable and first
/// argument: enough to distinguish `node server.mjs` from `node vite.js` while
/// remaining readable in a narrow table.
/// Read the start time for the process currently occupying `pid`. Used directly
/// before signalling to ensure a recycled PID cannot inherit a kill selection.
pub fn start_time(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat, 1).map(|(_, _, _, _, start_time, _)| start_time)
}

/// Signal exactly one process lifetime using Linux's stable pidfd handle. Open
/// the handle first, then verify starttime: if the PID was recycled before or
/// during this operation, either the identity mismatches or the old pidfd refers
/// to the exited predecessor and no replacement is signalled.
pub fn signal(identity: ProcessIdentity, signal: i32) -> std::io::Result<()> {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) as i32 };
    if pidfd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = if start_time(identity.pid) != Some(identity.start_time) {
        Err(std::io::Error::new(
            ErrorKind::NotFound,
            "process exited or PID was reused",
        ))
    } else if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    };
    unsafe { libc::close(pidfd) };
    result
}

fn read_label(pid: i32, fallback: &str) -> String {
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return fallback.to_owned();
    };
    command_label(&cmdline, fallback)
}

fn command_label(cmdline: &[u8], fallback: &str) -> String {
    let mut args = cmdline
        .split(|&byte| byte == 0)
        .filter(|arg| !arg.is_empty())
        .filter_map(|arg| std::str::from_utf8(arg).ok());
    let Some(executable) = args.next() else {
        return fallback.to_owned();
    };
    let executable = executable.rsplit('/').next().unwrap_or(executable);
    match args.next() {
        Some(first_arg) if !first_arg.starts_with('-') => {
            format!(
                "{executable} {}",
                first_arg.rsplit('/').next().unwrap_or(first_arg)
            )
        }
        _ => executable.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{command_label, parse_stat};

    #[test]
    fn command_label_distinguishes_mainthread_node_processes() {
        assert_eq!(
            command_label(b"/usr/bin/node\0/home/bart/api/server.mjs\0", "MainThread"),
            "node server.mjs"
        );
        assert_eq!(
            command_label(b"node\0/home/bart/web/vite.js\0", "MainThread"),
            "node vite.js"
        );
    }

    #[test]
    fn command_label_falls_back_for_kernel_threads_and_invalid_utf8() {
        assert_eq!(command_label(b"", "kworker/0:1"), "kworker/0:1");
        assert_eq!(command_label(b"\xff\0", "worker"), "worker");
    }

    #[test]
    fn parses_identity_and_counters_when_command_contains_parentheses() {
        let stat = "123 (worker (alpha)) R 42 0 0 0 0 0 0 0 0 0 10 20 0 0 0 0 0 0 12345 0 7 0 0";
        let (comm, state, ppid, cpu, start_time, rss) = parse_stat(stat, 4096).expect("valid stat");
        assert_eq!(comm, "worker (alpha)");
        assert_eq!(state, 'R');
        assert_eq!(ppid, 42);
        assert_eq!(cpu, 30);
        assert_eq!(start_time, 12345);
        assert_eq!(rss, 7 * 4096);
    }
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
