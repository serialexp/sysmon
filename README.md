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

## The process table follows the bottleneck

Instead of a generic process list, the bottom pane sorts by *whichever resource
is currently the bottleneck* — CPU-bound? top CPU hogs. Disk-bound? top I/O
writers. It answers the natural follow-up: "disk is slow — **who's doing it?**"

Ties fall back to CPU then RSS, so you always see *active* processes rather than
idle kernel threads in random order.

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
| `↑` `↓`  | move the selection through the list                                 |
| `F9`/`k` | kill the selected process (`Enter` = SIGTERM, `k` = SIGKILL)        |
| `Esc`    | close search / kill, then clear the selection, then quit           |

The selection is **locked to a PID**, so once you've searched for a process the
highlight follows it as the table reshuffles each second — you're always looking
at the same process, not whatever happens to sit on that row. `F3` walks the
matches in display order; killing another user's process needs privilege (run
under `sudo`, or the kill reports the kernel's `EPERM`).

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
- **Processes** — `/proc/<pid>/stat` (CPU, RSS) and `/proc/<pid>/io` (block I/O)

Linux only (it's `/proc`-native by design). Built with
[ratatui](https://ratatui.rs).
