# sysmon — TODO

Functional gaps found reviewing the app against its own thesis ("name the
resource that's making me slow"). Roughly priority-ordered.

## High priority (do first)

- [x] **PSI (Pressure Stall Information)** — read `/proc/pressure/{cpu,memory,io}`
      (`some`/`full` avg10). Now surfaced as a "stall N%" figure on the CPU /
      Memory / Disk panes, and drives the memory verdict (below).
      *Not yet* driving the CPU/Disk verdict — that redefines the product's
      "saturated at X%" headline, so left as a decision (see Open questions).
- [x] **Memory cries wolf** — `assess()` now uses PSI mem-stall as the memory
      saturation, falling back to `swapping ? used%.max(0.9) : used%` only when
      PSI is absent. A cache-heavy or merely swap-holding box no longer gets
      falsely blamed (verified live: 15 GiB in swap, PSI 0% → "All clear").
- [x] **Load average** — `/proc/loadavg` 1/5/15 vs core count, shown in the CPU
      pane headline (`17%   load 9.54 / 24`).
- [x] **Process state / D-state** — new `S` column in the process table; `D`
      (blocked on I/O) bold red, `R` green, `Z` magenta, else dim.

## Open questions (need Bart's call)

- [x] Should PSI also drive the **CPU and Disk** verdict (not just memory)?
      **Decided: yes, uniform PSI-preferred for CPU/Mem/Disk** (fallback to the
      utilization proxy only when PSI is absent). Degradation model, not
      utilization — a 100%-busy-but-uncontended box reads clear.
- [x] Should gauges show saturation (PSI) instead of / alongside util-usage?
      **Decided: no — gauges stay on utilization**, with PSI shown as `stall N%`
      beside them. Verdict (PSI) and gauge (util) can disagree by design.
- [ ] **Retune PSI thresholds.** `CLEAR=0.5 / SATURATED=0.8` were tuned for
      utilization; PSI runs lower (20–30% stall is already painful), so the
      verdict is currently conservative. Consider a PSI-specific mapping so
      moderate stall registers, without making the util-based net axis (still on
      the same 0..1 scale, max-wins) incomparable.

## Medium priority

- [ ] **Thermal throttling** — `/sys/class/thermal` or cpufreq scaling. A
      throttled CPU is slow while CPU% looks normal — currently invisible.
- [ ] **CPU steal** — surface separately ("X% stolen by hypervisor") instead of
      folding it into busy%; a real "why am I slow" on VMs.
- [x] **Freeze / pause** — space toggles a frozen frame (PAUSED badge on the
      verdict line); sampling is suspended so the 1 Hz display holds still.
- [ ] **Configurable refresh rate** — speed up / slow down the 1s tick.
- [x] **Disk latency / await** — disk pane now shows `await` (avg ms/request)
      and `aqu` (avg queue depth) beside `%util`, so 97%-util-at-a-trickle reads
      as slow-and-shallow instead of just "busy". Also in `--dump`.

## Low priority / nice to have

- [ ] Memory detail: cache / dirty / writeback (dirty+writeback ties to disk
      write pressure).
- [ ] Zombie / thread / process counts.
- [ ] Per-process network (needs eBPF — documented limitation).
- [ ] Sparkline time-axis labels; history length configurable.

## Repo hygiene (separate from the app)

- [ ] Add MIT `LICENSE` file (Cargo.toml declares MIT, no license text present).
- [ ] `cargo fmt` the tree (first commit landed unformatted).
- [ ] Flesh out `Cargo.toml` (`repository`, `authors`, `readme`, `keywords`,
      `categories`, `rust-version`).
- [ ] CI: `.github/workflows` running fmt --check + clippy -D warnings + test.
- [ ] Unit tests for the selector logic (search / follow-by-PID / cycle / kill).
- [ ] Kill dialog: don't render `Kill 0 (?)` if the followed PID exits while the
      confirm is open (freeze selection in Kill mode, or auto-close).
- [x] `n`/`N` aliases for next/prev hit (in case the terminal eats F3).
- [x] `--help` / `--version`; unknown flags now exit 2 with a message instead of
      silently launching the TUI.
