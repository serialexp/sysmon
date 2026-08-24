# sysmon

A terminal system monitor that shows **CPU, memory, disk, and network at the
same time** — and, more importantly, tells you *which one is making your machine
slow* right now.

`htop` shows you CPU and memory, one graph style at a time, and nothing about
disk or network. When your box is slow, that's usually not enough to tell you
*why*. `sysmon` puts all four axes on screen at once, normalises each to its own
ceiling so they're directly comparable, and names the current bottleneck.

```
╭ sysmon — what's slowing you down ─────────────────────────────────────╮
│ [!] BOTTLENECK: Disk I/O saturated at 96%                             │
╰───────────────────────────────────────────────────────────────────────╯
╭ CPU ──────────────────╮╭ Memory ───────────────╮
│ 41%                   ││ 23.3 GiB / 60.5 GiB   │
│ ████████       41%    ││ ███████████    39%    │
│ per-core ▃▂▄▂▁▁▁█▂▂   ││ swap 11 GiB / 20 GiB  │
│ ▁▂▃▄▃▂▁▁▂▃▅▆▇▆▅▃      ││ ▁▁▁▁▂▂▂▂▂▂▂▂▂▂        │
╰───────────────────────╯╰───────────────────────╯
╭ Disk I/O ─────────────╮╭ Network ──────────────╮
│ 1.4 MB/s R  920 MB/s W││ Rx 88 Mb/s  Tx 4 Mb/s │
│ ██████████████  96%   ││ ██████         88%    │
│ util 96%  iowait 71%  ││ 88% of link capacity  │
│ ▂▃▅▆▇███████          ││ ▁▂▃▄▅▆▇█▇▆▅           │
╰───────────────────────╯╰───────────────────────╯
╭ Top processes by Disk I/O ────────────────────────────────────────────╮
│ PID     COMMAND          CPU%    RSS       RD/s        WR/s            │
│ 51234   postgres         12.0    533 MiB   1.2 MB/s    918 MB/s        │
│ ...                                                                    │
╰───────────────────────────────────────────────────────────────────────╯
```

## Installation

Install the latest published release on Linux (x86-64 or ARM64):

```sh
curl -fsSL https://raw.githubusercontent.com/serialexp/sysmon/master/install.sh | sh
```

The installer downloads the matching statically linked binary from the latest
[GitHub release](https://github.com/serialexp/sysmon/releases), verifies it
against the release's SHA-256 checksums, and installs it to `/usr/local/bin` when
that directory is writable or to `~/.local/bin` otherwise. Set
`SYSMON_INSTALL_DIR` to choose another location:

```sh
curl -fsSL https://raw.githubusercontent.com/serialexp/sysmon/master/install.sh \
  | SYSMON_INSTALL_DIR="$HOME/bin" sh
```

For full per-process disk-I/O attribution, run `sysmon --grant` once after
installing or upgrading; replacing the executable removes its file capability.
See [Full I/O attribution](#full-io-attribution) for the permission model.

### Building from source

Requires Linux and a current stable Rust toolchain:

```sh
git clone https://github.com/serialexp/sysmon.git
cd sysmon
cargo build --release --locked
./target/release/sysmon
```

`just install` builds from the current checkout and installs with Cargo.

### Publishing a release

Release tags are `v<version>` and must match the version in `Cargo.toml`. Pushing
a tag builds statically linked x86-64 and ARM64 Linux archives, generates
`SHA256SUMS`, and publishes a GitHub release with generated notes:

```sh
# after updating Cargo.toml and Cargo.lock and committing that change
git tag v0.1.0
git push origin v0.1.0
```

## What makes it different: saturation, not throughput

The hard part of "what's slow" is that each resource means "busy" differently,
and raw numbers lie:

| Axis    | Saturation signal                          | Why not just throughput / usage |
|---------|--------------------------------------------|---------------------------------|
| CPU     | busy % (`100 − idle − iowait`)             | —                               |
| Memory  | used %, **and active swapping**            | Linux fills RAM with cache on purpose; 90% used isn't pain, *swapping* is |
| Disk    | `%util` (time with I/O in flight)          | 2 GB/s sequential can be fine; a trickle of random I/O can freeze you. Throughput alone misleads. |
| Network | throughput ÷ link speed                     | 50 Mb/s means nothing until you know it's a 100 Mb/s NIC |

Each axis is reduced to a `0–100%` saturation so the four are comparable, and the
verdict line names whichever is highest. Colours: green `<50%`, yellow `<80%`,
red `≥80%`.

Because `%util` saturates the moment the device is simply *never idle* — it hits
100% for a trickle of slow random I/O just as easily as for a full sequential
stream — the disk pane also shows **`await`** (average service latency per
completed request) and **`aqu`** (average queue depth). Together they make the
number legible: 100% util with sub-millisecond `await` and `aqu ≈ 1` is a device
that's busy but keeping up; high `await` with a deep queue is genuine backlog.
The `stall` (PSI-io) figure is the tiebreaker for whether that occupancy is
actually delaying anyone.

### PSI: the kernel's own "am I stalling?" signal

Where the kernel provides it (`/proc/pressure/{cpu,memory,io}`, Linux 4.20+),
sysmon reads **Pressure Stall Information** — the fraction of recent wall-clock
time during which tasks were *actually delayed* waiting on a resource (`some
avg10`). It's shown as `stall N%` on the CPU, Memory, and Disk panes.

PSI is strictly more honest than the level/throughput proxies: a box with RAM
full of cache reads ~0% memory stall, and a disk pinned at 100% `%util` that's
still keeping up reads ~0% I/O stall. Because of that, **the verdict for CPU,
memory and disk is driven by PSI** — the tool asks "were tasks actually
*delayed*?", not "is the resource busy?". So it no longer calls memory the
bottleneck just because RAM is full of cache, nor disk because `%util` is high
while it keeps up. On kernels without PSI each axis falls back to its utilization
proxy (`busy%` / `used% + swapping` / `%util`).

This is deliberately a *degradation* model, not a *utilization* one, and it cuts
the other way too: a box pegged at 100% CPU doing real work with no run-queue
backlog stalls no one, so PSI reads ~0 and the verdict stays clear even though
the CPU gauge is full. The gauges still show utilization (with the `stall N%`
figure beside them), so you always see both the level and whether it's hurting —
they can legitimately disagree, and that disagreement is the point.

> **Note on thresholds.** The same `50% / 80%` elevated/saturated cut-offs are
> applied to PSI as to utilization, even though PSI runs on a different natural
> scale (20–30% stall is already real pain). So today the verdict is
> conservative — it names a bottleneck only under heavy stall. Retuning the PSI
> thresholds is a tracked follow-up.

**Load average** (`/proc/loadavg`, shown by the CPU pane as `load 1min / cores`)
complements instantaneous CPU busy%: it counts runnable *and* uninterruptible
(D-state) tasks, so a load well above the core count with only moderate CPU% is
the tell-tale of I/O contention rather than compute.

## The process table follows the bottleneck

The bottom pane shows **one row per userspace process**. CPU, resident memory,
and readable disk-I/O rates belong only to the displayed PID; a process's
children are never rolled into it merely because it launched them. Linux
userspace threads are already represented by their thread-group leader in
`/proc/<pid>`, while the complete process forest below `kthreadd` (PID 2) is
hidden. This matches htop's useful default distinction without guessing which
processes constitute a conceptual application.

The table sorts processes by *whichever resource is currently the bottleneck* —
CPU-bound? top CPU processes. Disk-bound? top I/O processes. CPU, RSS, and I/O
are three-sample rolling averages (about three seconds at the default refresh),
so a single one-second burst has less power to reshuffle the table. New processes
warm up from the observations available so genuine new work appears immediately.
Ties fall back to CPU then RSS, so active processes naturally rise above idle
ones. Rolling histories are keyed by PID plus `/proc/<pid>/stat` start time, so a
reused PID never inherits its predecessor's values.

The command label is derived from `/proc/<pid>/cmdline` (executable plus its
first useful argument) rather than relying only on the kernel task name. That
distinguishes runtimes that otherwise use generic names such as `MainThread` —
for example, `node server.mjs` versus `node vite.js`. The `S` column is the
process's scheduler state, so a process stuck **`D`** (uninterruptible sleep —
blocked in the kernel on I/O) stands out in red. `R` running is green, `Z`
zombie magenta.

Killing a selected row signals only that displayed PID. It does not implicitly
signal descendants whose resource usage is shown in their own rows.

## Honesty about what it can and can't see

- **Per-process disk I/O needs privilege.** `/proc/<pid>/io` is gated by the
  kernel's `ptrace_may_access` check, so unprivileged you can only read your own
  processes; another user's process (usually root) shows `—` (unknown), never a
  fabricated `0 B/s`. `sysmon` detects *why* it's restricted and shows the fix
  that will actually work on your kernel (see
  [Full I/O attribution](#full-io-attribution)): `sysmon --grant` (a one-time
  `CAP_SYS_PTRACE` grant that suffices on stock kernels) or, when that capability
  is present but the kernel ignores it, `sysmon --sudo` (run as real root). Under
  root the banner disappears because it's no longer relevant. This is the same
  limitation `iotop` has — even taskstats, the netlink interface `iotop` uses,
  requires `CAP_NET_ADMIN`; there is no way to read another user's per-process
  I/O without elevated privilege.
- **Per-process network isn't available** from `/proc` without eBPF, so when the
  network is the bottleneck the table sorts by CPU and says so.
- **Physical devices only.** Disk stats cover real block devices
  (`nvme*`, `sd*`), skipping `dm-*`/`loop*`/`zram*` which double-count or aren't
  disks. Network covers physical NICs only, skipping `docker0`, bridges, `veth*`,
  `tailscale0`, and `lo` — all of which report bogus link speeds and would
  double-count container traffic.

## Usage

```sh
cargo run --release          # launch the TUI
```

Keys:

| Key      | Action                                                              |
|----------|---------------------------------------------------------------------|
| `q`      | quit                                                                |
| `1–4`    | force sort by CPU / Memory / Disk / Network                         |
| `0`      | auto — follow the current bottleneck (default)                      |
| `/`      | incremental process search (matches command or PID)                |
| `F3`     | jump to the next search hit (`Shift+F3` for the previous)           |
| `n` `N`  | next / previous hit — aliases for `F3` when the terminal eats it    |
| `↑` `↓`  | move the selection through the list                                 |
| `Home` `End` | select the first / last visible process                          |
| `Page Up` `Page Down` | move the selection by ten rows, clamped at the list ends    |
| `F9`/`k` | kill the selected process (`Enter` = SIGTERM, `k` = SIGKILL)        |
| `space`  | freeze / unfreeze the display (a `PAUSED` badge appears)            |
| `Esc`    | close search / kill, then clear the selection, then quit           |

There's also `sysmon --help` and `sysmon --version`; an unrecognised flag now
exits with an error instead of silently launching the TUI.

The selection is **locked to a PID and process start time**, so once you've
searched for a process the highlight follows that exact process lifetime as the
table reshuffles each second — a recycled PID cannot inherit the highlight or a
kill action. `F3` walks matches in display order; killing another user's process
needs privilege (run under `sudo`, or the kill reports the kernel's `EPERM`).

### Full I/O attribution

By default the binary is unprivileged and can only attribute I/O to your own
processes; everything else shows `—`. There are two ways to get full
attribution, and `sysmon` tells you which one you need:

```sh
sysmon --grant     # one-time CAP_SYS_PTRACE grant (self-elevates via sudo)
sysmon             # …then run normally — full attribution on most kernels
# or, when the capability is ignored by your kernel:
sysmon --sudo      # run the TUI as real root (self-elevates; always works)
```

**`sysmon --grant`** is the cheap one-time fix. Run it as your **normal user**
(not `sudo sysmon --grant` — root's `secure_path` doesn't include `~/.cargo/bin`,
so that's "command not found"). It's on your PATH, so it resolves; seeing it
isn't root, it re-execs under `sudo` with its **absolute** path, prompts for your
password, and writes the `security.capability` xattr on its own executable
(equivalent to `setcap cap_sys_ptrace+ep`). On stock kernels that's enough to
pass the `ptrace_may_access` gate on `/proc/<pid>/io` for every process, with no
`sudo` thereafter. Re-run it after any `cargo install` (which replaces the file
and drops the capability).

**Caveat — some kernels ignore the capability.** On certain distro kernels
(observed on Pop!_OS 6.17) a `CAP_SYS_PTRACE`-holding *non-root* process still
can't read other users' `/proc/<pid>/io` — the grant is inert. `sysmon` detects
this at runtime (it holds the capability yet reads are still denied) and switches
the banner to recommend **`sysmon --sudo`**, which re-execs the TUI as real root
(the only thing guaranteed to work). This is especially likely on boxes running
containers (Docker/k8s), where the heavy writers are root-owned.

Note that granting lets the binary inspect any process's I/O accounting for
anyone who runs it — fine on a single-user box, a real (if modest) privilege on a
shared one.

<details><summary>Equivalent manual commands</summary>

```sh
sudo setcap cap_sys_ptrace+ep "$(command -v sysmon)"   # same effect as --grant
sudo "$(command -v sysmon)"                            # same effect as --sudo
```

Here `$(command -v sysmon)` expands to the absolute path *in your shell* before
`sudo` runs, so `setcap` receives a real path rather than relying on root's PATH.

</details>

### Headless modes (no TTY needed)

```sh
cargo run -- --dump        # print one derived sample as text
cargo run -- --snapshot    # render one real UI frame to a plain-text grid
```

## How it works

Everything comes from `/proc` and `/sys`, sampled once per second; rates are the
delta between two samples:

- **CPU** — `/proc/stat` (aggregate + per-core jiffies, iowait)
- **Memory** — `/proc/meminfo` (levels) + `/proc/vmstat` (`pswpin`/`pswpout` for
  swap activity)
- **Disk** — `/proc/diskstats` (`sectors_read/written`, `io_ticks` for `%util`),
  filtered by the `device` symlink under `/sys/block`
- **Network** — `/proc/net/dev` (rx/tx bytes) + `/sys/class/net/*/speed`,
  filtered to interfaces with a `device` symlink
- **Pressure** — `/proc/pressure/{cpu,memory,io}` (`some avg10`) and
  `/proc/loadavg` (1/5/15-minute load)
- **Processes** — `/proc/<pid>/stat` (parent PID, CPU, RSS, state),
  `/proc/<pid>/cmdline` (human-useful command labels), and `/proc/<pid>/io`
  (block I/O); each userspace PID remains independently accountable, while the
  process forest rooted at `kthreadd` is hidden

Linux only (it's `/proc`-native by design). Built with
[ratatui](https://ratatui.rs).
