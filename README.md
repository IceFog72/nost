# nost

Minimal btop-style NVIDIA GPU monitor written in Rust.

This version bypasses `nvidia-smi` completely. It dynamically loads the NVIDIA driver's `libnvidia-ml.so.1` and queries NVML directly, so there are no subprocesses, CSV/XML parsing, Python, virtualenvs, or runtime package dependencies.

## Scripts

Runtime/build scripts:

```bash
./build.sh       # build + run Rust tests
./run.sh         # build if needed, then run against real NVML
./run-demo.sh    # build if needed, then run the animated demo backend
```

`build.sh` uses an existing Cargo/Rust installation when present. If Rust is missing, it bootstraps a **local** minimal stable toolchain under `.tools/` using the official rustup installer; it does not modify your shell profile. The build runs release-mode unit tests, compiles the binary, then performs a `--demo --once` smoke test.

## Install as a CLI command + desktop app

There are exactly two installation scripts:

- `install-root.sh` — complete system-wide installer.
- `install-user.sh` — complete per-user installer.

They do not call or depend on another installer script.

The installer builds `nost`, installs the release binary as the `nost` command, and registers a terminal application named **nost GPU Monitor** in the desktop application menu.

System-wide install (uses root/sudo):

```bash
./install-root.sh
```

Per-user install (no root, no sudo):

```bash
./install-user.sh
```

The two installers are **standalone scripts**. There is no shared `install.sh`: `install-root.sh` contains the complete system-wide installation path, while `install-user.sh` contains the complete non-root installation path. Both accept `--no-build`.

The root/system-wide install places files at:

```text
/usr/local/bin/nost
/usr/local/share/applications/nost.desktop
```

Afterward, run it from any terminal with:

```bash
nost
```

Or launch **nost GPU Monitor** from your desktop application menu. The desktop entry uses `Terminal=true`, so it opens in your configured terminal emulator.

The non-root script installs under `~/.local`. If `~/.local/bin` is not already in your shell `PATH`, the installer prints the exact line to add.

To install an already-built release without rebuilding:

```bash
./install-root.sh --no-build
# or
./install-user.sh --no-build
```

## Runtime requirements

For the real monitor:

- Linux
- NVIDIA proprietary/open kernel driver userspace stack with `libnvidia-ml.so.1`

`nvidia-smi` is **not used or required by nost**. It may still be installed by the same NVIDIA driver package, but nost never executes it.

## What it reads directly from NVML

- GPU utilization
- framebuffer/VRAM total + used + explicit system-reserved bytes through `nvmlDeviceGetMemoryInfo_v2` when available
- GPU temperature plus supported slowdown / maximum-operating / shutdown thresholds
- power draw + power limit
- fan speed
- graphics and memory clocks
- active clock-event / performance-limit reasons (power, thermal, hardware slowdown, idle, etc.)
- NVENC encoder and NVDEC decoder utilization
- PCIe TX/RX throughput
- compute and graphics process PIDs
- per-process framebuffer memory
- per-process SM utilization when `nvmlDeviceGetProcessUtilization` is supported; sample timestamps are advanced between refreshes so old non-zero utilization is not held forever

Process user/name metadata comes from Linux `/proc` and `/etc/passwd`, not from `nvidia-smi`.


Unsupported optional metrics display as `--` instead of failing the monitor.

## UI / color grading

The renderer is intentionally small and dependency-free, but the visual hierarchy is now closer to btop:

- bordered GPU and process panels
- GPU and VRAM multi-row history graphs
- current GPU/MEM utilization values are integrated directly into their history graphs; no separate utilization-meter rows
- compact metric rows without duplicate temperature/VRAM readouts
- btop-style **row-based** history color grading: bottom rows stay cool/green, middle rows yellow/amber, and upper rows orange/red; the color does not change from time-column to time-column
- the VRAM graph uses the same row-based geometry with its cyan/teal → yellow → orange/red palette
- temperature is graded against NVML-reported hardware limits when the GPU exposes them, instead of fixed generic thresholds
- temperature, power and per-process GPU percentages use the same severity grades
- compact `limit`, `ENC`/`DEC`, hardware thermal-limit and PCIe TX/RX traffic rows
- 24-bit color when the terminal advertises truecolor, with an automatic ANSI-256 fallback

No TUI framework is required: the app still writes a single ANSI frame directly and never registers keyboard or mouse input.

## Terminal/input behavior

nost is intentionally a passive dashboard:

- never reads stdin
- never enters raw/cbreak mode
- never enables mouse reporting
- has no selection cursor or process-list scrolling
- handles only `SIGINT`/`SIGTERM` for clean exit
- explicitly resets xterm alternate-scroll mode (`DEC 1007`) **after entering the alternate screen**
- reasserts the mouse/alternate-scroll reset every rendered frame

That prevents wheel motion from being translated into `ESC[A` / `ESC[B` cursor-key sequences and appearing as `^[[A` / `^[[B` text.

`Ctrl-C` exits and restores the cursor/screen.

## Options

```text
./run.sh --interval 0.25   # refresh every 250 ms
./run.sh --inline          # render on the normal terminal screen
./run.sh --once            # one frame, then exit
./run.sh --no-color        # disable colors; screen-control escapes remain
./run-demo.sh              # UI preview without an NVIDIA GPU
```

Default refresh is 500 ms.

## Architecture

```text
NVIDIA driver
    │
    └── libnvidia-ml.so.1
             │
             ▼
      Rust NVML FFI loader
       ├── device metrics + thermal limits
       ├── clock-event reasons + video engines
       ├── PCIe TX/RX traffic
       ├── running processes
       └── process utilization
             │
        /proc metadata + DRM fdinfo
             │
             ▼
      snapshot + history
             │
             ▼
      ANSI TUI renderer
```

The Rust project has **zero Cargo dependencies**. NVML is loaded with `dlopen()` at runtime, so the project can be built even on a machine without NVIDIA hardware/driver installed; use `./run-demo.sh` there.


## v0.4.2 UI cleanup

- Removed the global `nost / nvidia / clock / NVML / interval / GPU count` header row.
- Removed blank rows between the GPU and process panels.
- Video telemetry is rendered as parsed fields: `video  ENC 0%  DEC 0%`.
- PCIe traffic is rendered as parsed fields with readable units: `traffic  TX 1 MB/s  RX 585 KB/s`.

## Internal VRAM accounting

nost still normalizes NVML memory-v2 accounting internally and can use DRM `fdinfo` as a cross-check, but `proc`, `reserved`, and `drm` are intentionally not rendered in the main GPU panel. Per-process VRAM stays in the process table; the third `vram holders` panel is focused specifically on finding UVM/CUDA owners that may be absent from NVML's attributable VRAM rows.

## v0.4.7 VRAM holders panel

A third panel now cross-checks unexplained framebuffer usage against processes that keep `/dev/nvidia-uvm` open:

```text
╭─ vram holders  2 UVM ───────────────────────────────╮
│ unattributed 445 MiB   UVM module refs 4   scan ... │
│ PID       USER         GPU      PROCESS    NVML VRAM │
│ 1745052   icefog       --       Discord           -- │
│ 1884401   icefog       --       python3 pocket... -- │
╰─────────────────────────────────────────────────────╯
```

The panel uses two independent signals:

1. **NVML gap analysis**: active framebuffer usage minus the sum of NVML-attributed process VRAM gives an `unattributed` residual. This residual is diagnostic; it is not assumed to belong to UVM.
2. **UVM ownership scan**: every two seconds nost walks `/proc/<pid>/fd` and finds processes whose file descriptors point to `/dev/nvidia-uvm`, equivalent to the useful part of `lsof | grep nvidia-uvm` but without spawning `lsof`. It also reads `/sys/module/nvidia_uvm/refcnt` when available.

A UVM holder with `NVML VRAM --` is particularly useful: it means the process has an active UVM/CUDA ownership signal even though NVML's per-process framebuffer table does not currently attribute bytes to it. The panel does **not** claim that every UVM holder owns the entire unattributed gap.

If `/proc/<pid>/fd` is hidden by permissions, the summary says `scan partial`; visible holders remain useful but the list is not exhaustive.


## v0.4.8 compact metric cleanup

- Removed the duplicate `temperature` and `vram` detail rows.
- Removed the visible PCIe link-generation/width row. PCIe TX/RX traffic remains available.
- Repacked the lower telemetry into three rows to reclaim one line of vertical space.

## v0.4.10 graph-only utilization

- Removed the separate horizontal `GPU` and `MEM` utilization-meter rows.
- The live utilization value is now right-aligned on the top row of each history graph.
- GPU graph keeps the single temperature readout beside its current percentage.
- MEM graph keeps the single current/total VRAM readout beside its current percentage.
- GPU panel shrank from 15 to 13 rows; minimum terminal height is now 24 rows.

## v0.4.11 stacked graph readings

- Split the MEM graph status into three right-edge rows: utilization %, used VRAM, total VRAM.
- Split the GPU graph status into separate right-edge rows for utilization % and temperature.
- The graph width now reserves space only for the longest individual reading, giving the history plot more horizontal room.
- No separate GPU/MEM meter rows were reintroduced.


## Resize behavior

`nost` treats `SIGWINCH` as a redraw wake-up, detects geometry changes on every
frame, clears stale row tails after a resize, disables terminal auto-wrap while
the dashboard is active, and brackets frames with DEC synchronized-output mode
when the terminal supports it. These measures prevent old right-edge text and
borders from being left behind while a terminal window is dragged narrower or
wider.


## v0.4.12 resize-safe redraw

- Wakes the renderer on Linux `SIGWINCH` instead of waiting for the normal sample interval.
- Performs a full-screen clear only when terminal geometry changes, removing stale right-edge tails.
- Disables DEC auto-wrap while the dashboard is active so a mid-frame shrink clips instead of wrapping.
- Uses DEC synchronized-output mode (`2026`) when supported to avoid displaying half-painted frames.
- Restores wrap, cursor, alternate-screen, mouse/scroll and sync modes on exit.


## v0.4.13 adaptive height

- When terminal height drops below the full 24-row layout, both utilization histories collapse from four rows to a single graph line instead of replacing the dashboard with a resize warning.
- Compact graph lines keep their live values inline (`GPU% + temperature`, `MEM% + used/total VRAM`).
- The process panel has hard priority for at least the top three process rows whenever the dashboard can render at all.
- From 18–23 rows the VRAM-holder box remains visible with one-line graphs; from 13–17 rows the holder box is temporarily hidden so GPU history + top-three processes stay visible.
- Minimum supported geometry is now 65x13.
