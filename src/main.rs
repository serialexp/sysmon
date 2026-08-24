mod app;
mod bottleneck;
mod history;
mod metrics;
mod ui;
mod util;

use std::time::{Duration, Instant};

use anyhow::Context;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::App;

const TICK: Duration = Duration::from_millis(1000);

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("sysmon {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // One-time privilege setup: grant this binary CAP_SYS_PTRACE so an
    // unprivileged user gets full per-process I/O attribution afterwards.
    if std::env::args().any(|a| a == "--grant") {
        return grant();
    }
    // Convenience: run the normal TUI with full attribution by re-execing under
    // sudo with our absolute path (so root's PATH is irrelevant and the user
    // needn't type the path). If already root, fall through and just run.
    if std::env::args().any(|a| a == "--sudo") && unsafe { libc::geteuid() } != 0 {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().context("resolving own executable path")?;
        let err = std::process::Command::new("sudo").arg(&exe).exec();
        // exec only returns on failure.
        return Err(anyhow::Error::new(err).context("failed to invoke sudo"));
    }
    // Headless self-check: sample twice a second apart and print the derived
    // metrics as plain text. Lets us validate /proc parsing without a TTY.
    if std::env::args().any(|a| a == "--dump") {
        return dump();
    }
    // Render one real frame to a text grid and print it — a TTY-free preview of
    // the actual UI layout.
    if std::env::args().any(|a| a == "--snapshot") {
        return snapshot();
    }

    // Anything left that looks like a flag is a typo — fail loudly instead of
    // silently launching the TUI (which hid `--dumpp` and friends before).
    const KNOWN: &[&str] = &[
        "--grant",
        "--sudo",
        "--dump",
        "--snapshot",
        "--help",
        "-h",
        "--version",
        "-V",
    ];
    if let Some(bad) = args
        .iter()
        .find(|a| a.starts_with('-') && !KNOWN.contains(&a.as_str()))
    {
        eprintln!("sysmon: unknown option '{bad}'\nTry 'sysmon --help' for usage.");
        std::process::exit(2);
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal);
    ratatui::restore();
    result
}

/// Usage text for `--help`. Kept terse; the README has the full story.
fn print_help() {
    println!(
        "sysmon {} — show CPU, memory, disk and network saturation at once,\n\
         and name whichever one is the current bottleneck.\n\
         \n\
         USAGE:\n    \
         sysmon [OPTIONS]\n\
         \n\
         OPTIONS:\n    \
         --grant       grant this binary CAP_SYS_PTRACE (one-time, via sudo) for\n                  \
         full per-process I/O attribution, then exit\n    \
         --sudo        run the TUI as root (full attribution on any kernel)\n    \
         --dump        print one derived sample as text and exit (no TTY needed)\n    \
         --snapshot    render one UI frame to a text grid and exit\n    \
         -h, --help    print this help and exit\n    \
         -V, --version print version and exit\n\
         \n\
         KEYS (in the TUI):\n    \
         q quit   1-4 sort CPU/Mem/Disk/Net   0 auto   / search   F3 next hit\n    \
         (n/N)   ↑↓ select   Home/End top/bottom   PgUp/PgDn page   F9/k kill process\n    \
         space pause   Esc back/quit",
        env!("CARGO_PKG_VERSION"),
    );
}

/// Grant this executable `CAP_SYS_PTRACE` (effective+permitted) by writing the
/// `security.capability` extended attribute directly — no dependency on the
/// `setcap` binary. Needs `CAP_SETFCAP` (root); if not already root we re-exec
/// ourselves under `sudo` with our absolute path (so root's PATH is irrelevant).
/// Afterwards an unprivileged `sysmon` can read every process's `/proc/<pid>/io`.
fn grant() -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().context("resolving own executable path")?;

    // Setting a file capability needs CAP_SETFCAP (root). Rather than tell the
    // user to `sudo sysmon` — which fails because root's secure_path doesn't
    // include ~/.cargo/bin — re-exec ourselves under sudo with our *absolute*
    // path, so PATH is irrelevant. sudo prompts for the password on this TTY.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("Setting a file capability needs root — re-running under sudo…");
        let err = std::process::Command::new("sudo")
            .arg(&exe)
            .arg("--grant")
            .exec();
        return Err(anyhow::Error::new(err).context("failed to invoke sudo"));
    }

    let c_path =
        std::ffi::CString::new(exe.as_os_str().as_bytes()).context("executable path has a NUL")?;

    // struct vfs_cap_data, revision 2: __le32 magic_etc; then data[2] of
    // { __le32 permitted; __le32 inheritable; }. CAP_SYS_PTRACE is bit 19, in
    // the low 32-bit word. The effective flag in magic_etc means "+e".
    const VFS_CAP_REVISION_2: u32 = 0x0200_0000;
    const VFS_CAP_FLAGS_EFFECTIVE: u32 = 0x0000_0001;
    const CAP_SYS_PTRACE: u32 = 19;
    let blob: [u32; 5] = [
        (VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE).to_le(),
        (1u32 << CAP_SYS_PTRACE).to_le(), // data[0].permitted
        0,                                // data[0].inheritable
        0,                                // data[1].permitted
        0,                                // data[1].inheritable
    ];

    let ret = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            c"security.capability".as_ptr(),
            blob.as_ptr() as *const libc::c_void,
            std::mem::size_of_val(&blob),
            0,
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EPERM) {
            anyhow::bail!(
                "kernel refused the file capability on {} (EPERM).\n\
                 The filesystem may not support security.* xattrs (e.g. some \
                 overlay/tmpfs mounts). Move the binary onto a normal disk and retry.",
                exe.display()
            );
        }
        return Err(anyhow::Error::new(err)
            .context(format!("writing security.capability on {}", exe.display())));
    }

    println!(
        "Granted CAP_SYS_PTRACE to {}.\n\
         Run `sysmon` normally — on most kernels that's full per-process I/O, no sudo.\n\
         If attribution still shows 'restricted', this kernel ignores the capability\n\
         for /proc/<pid>/io — use `sysmon --sudo` for full attribution instead.\n\
         Re-run this grant after any `cargo install`, which replaces the binary.",
        exe.display()
    );
    Ok(())
}

fn run(terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
    let mut app = App::new();
    app.on_tick(); // prime raw counters (produces no metrics yet)
    let mut last = Instant::now();

    loop {
        terminal.draw(|f| ui::render(f, &app))?;

        let timeout = TICK.saturating_sub(last.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.on_key(k);
                }
            }
        }

        if last.elapsed() >= TICK {
            app.on_tick();
            last = Instant::now();
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn snapshot() -> anyhow::Result<()> {
    use ratatui::{backend::TestBackend, Terminal};

    let mut app = App::new();
    app.on_tick();
    std::thread::sleep(TICK);
    app.on_tick();

    let (w, h) = (100u16, 34u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h))?;
    terminal.draw(|f| ui::render(f, &app))?;

    // Flatten the cell buffer into text rows (colour is dropped in this preview).
    let buffer = terminal.backend().buffer().clone();
    let border = "+".to_string() + &"-".repeat(w as usize) + "+";
    println!("{border}");
    for y in 0..h {
        let mut line = String::with_capacity(w as usize);
        for x in 0..w {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("|{line}|");
    }
    println!("{border}");
    Ok(())
}

fn dump() -> anyhow::Result<()> {
    use util::{fmt_bits, fmt_bytes, fmt_latency, fmt_rate};

    let mut app = App::new();
    app.on_tick();
    std::thread::sleep(TICK);
    app.on_tick();

    let Some(m) = &app.metrics else {
        println!("no metrics produced");
        return Ok(());
    };
    let a = bottleneck::assess(m);

    println!("=== sysmon --dump ===");
    println!(
        "I/O attr: {}",
        match m.io_hint {
            metrics::IoHint::Full => "full (root, or nothing denied)",
            metrics::IoHint::TryGrant =>
                "restricted — try `sysmon --grant` (one-time), else `sysmon --sudo`",
            metrics::IoHint::CapInert =>
                "restricted — capability inert on this kernel; use `sysmon --sudo`",
        }
    );
    println!(
        "CPU     {:>5.1}%   iowait {:.1}%   cores={}",
        m.cpu.usage * 100.0,
        m.cpu.iowait * 100.0,
        m.cpu.per_core.len()
    );
    let psi = |p: Option<f64>| {
        p.map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".into())
    };
    println!(
        "Load    {:.2} / {:.2} / {:.2}  ({} cores)",
        m.load.one, m.load.five, m.load.fifteen, m.load.cores
    );
    println!(
        "PSI     cpu(some) {}   mem(some) {}   io(some) {}",
        psi(m.psi.cpu.some),
        psi(m.psi.mem.some),
        psi(m.psi.io.some)
    );
    println!(
        "Memory  {:>5.1}%   {} / {}   swap {} / {}{}",
        m.mem.used_frac * 100.0,
        fmt_bytes(m.mem.used),
        fmt_bytes(m.mem.total),
        fmt_bytes(m.mem.swap_used),
        fmt_bytes(m.mem.swap_total),
        if m.mem.swapping { "  (swapping)" } else { "" }
    );
    println!(
        "Disk    util {:>4.1}%   R {}  W {}   await {}  aqu {:.2}  iowait {:.1}%",
        m.disk.util * 100.0,
        fmt_rate(m.disk.read_bps),
        fmt_rate(m.disk.write_bps),
        fmt_latency(m.disk.await_ms),
        m.disk.aqu_sz,
        m.disk.iowait * 100.0,
    );
    for d in &m.disk.per_device {
        println!(
            "          {:<10} util {:>4.1}%  R {}  W {}",
            d.name,
            d.util * 100.0,
            fmt_rate(d.read_bps),
            fmt_rate(d.write_bps)
        );
    }
    match m.net.sat {
        Some(s) => println!(
            "Network  {:>4.1}% of link   Rx {}  Tx {}",
            s * 100.0,
            fmt_bits(m.net.rx_bps),
            fmt_bits(m.net.tx_bps)
        ),
        None => println!(
            "Network  link n/a          Rx {}  Tx {}",
            fmt_bits(m.net.rx_bps),
            fmt_bits(m.net.tx_bps)
        ),
    }
    for i in &m.net.per_iface {
        let sat = i
            .sat
            .map(|s| format!("{:.1}%", s * 100.0))
            .unwrap_or_else(|| "n/a".into());
        println!(
            "          {:<10} speed {:?}  sat {}  Rx {}  Tx {}",
            i.name,
            i.speed_mbps,
            sat,
            fmt_bits(i.rx_bps),
            fmt_bits(i.tx_bps)
        );
    }
    println!(
        "\nVERDICT: {} at {:.0}%   [cpu {:.0} mem {:.0} disk {:.0} net {:.0}]",
        a.worst.label(),
        a.worst_sat * 100.0,
        a.sats[0] * 100.0,
        a.sats[1] * 100.0,
        a.sats[2] * 100.0,
        a.sats[3] * 100.0,
    );

    let mut procs: Vec<_> = m.procs.iter().collect();
    procs.sort_by(|x, y| {
        y.cpu_frac
            .partial_cmp(&x.cpu_frac)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!("\nTop 5 processes by CPU:");
    for p in procs.iter().take(5) {
        println!(
            "  {:>7} {:<28} {:>5.1}%  rss {}",
            p.pid,
            p.comm,
            p.cpu_frac * 100.0,
            fmt_bytes(p.rss)
        );
    }
    Ok(())
}
