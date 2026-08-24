//! Pressure Stall Information from `/proc/pressure/{cpu,memory,io}`.
//!
//! PSI is the kernel's own answer to "is this resource making tasks wait?".
//! `some avg10` is the fraction of the last 10 seconds during which *at least
//! one* task was stalled waiting on the resource; `full avg10` is the fraction
//! during which *every* non-idle task was stalled (not emitted for CPU). Unlike
//! `%util` or `used%`, it only rises when work is actually delayed — so a
//! cache-heavy but healthy box reads ~0, and a 100%-util-but-keeping-up disk
//! reads ~0 too.

use std::fs;

/// One resource's pressure, as fractions in 0..1 (the kernel reports percent).
#[derive(Clone, Copy, Default)]
pub struct Psi {
    /// `some avg10`: share of time at least one task stalled on this resource.
    pub some: Option<f64>,
    /// `full avg10`: share of time all non-idle tasks stalled. `None` for CPU
    /// (the kernel doesn't emit a `full` line there) or when unavailable.
    pub full: Option<f64>,
}

/// Read one pressure file (`cpu`, `memory`, or `io`). Returns an all-`None`
/// `Psi` when PSI is unavailable — not compiled in, not mounted, or the kernel
/// predates it — so callers transparently fall back to their proxy signals.
pub fn read(resource: &str) -> Psi {
    match fs::read_to_string(format!("/proc/pressure/{resource}")) {
        Ok(s) => parse(&s),
        Err(_) => Psi::default(),
    }
}

/// Parse the `some`/`full` lines of a pressure file, taking the `avg10=` field
/// and converting percent → fraction.
fn parse(s: &str) -> Psi {
    let mut p = Psi::default();
    for line in s.lines() {
        // e.g. "some avg10=1.23 avg60=0.80 avg300=0.31 total=12345678"
        let mut it = line.split_whitespace();
        let kind = it.next().unwrap_or("");
        let avg10 = it
            .find_map(|f| f.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f64>().ok())
            .map(|pct| (pct / 100.0).clamp(0.0, 1.0));
        match kind {
            "some" => p.some = avg10,
            "full" => p.full = avg10,
            _ => {}
        }
    }
    p
}

/// Read all three resources at once.
pub fn read_all() -> Pressure {
    Pressure {
        cpu: read("cpu"),
        mem: read("memory"),
        io: read("io"),
    }
}

/// Pressure across the three resources PSI tracks.
#[derive(Clone, Copy, Default)]
pub struct Pressure {
    pub cpu: Psi,
    pub mem: Psi,
    pub io: Psi,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_some_and_full() {
        // memory/io files carry both lines; cpu carries only `some`.
        let p = parse(
            "some avg10=12.34 avg60=5.00 avg300=1.00 total=99\n\
             full avg10=6.00 avg60=2.00 avg300=0.50 total=42\n",
        );
        assert!((p.some.unwrap() - 0.1234).abs() < 1e-9);
        assert!((p.full.unwrap() - 0.06).abs() < 1e-9);
    }

    #[test]
    fn cpu_has_no_full_line() {
        let p = parse("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
        assert_eq!(p.some, Some(0.0));
        assert_eq!(p.full, None);
    }

    #[test]
    fn empty_or_garbage_is_none() {
        assert_eq!(parse("").some, None);
        assert_eq!(parse("garbage\n").some, None);
    }
}
