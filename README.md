# nost

Minimal btop-style NVIDIA GPU monitor written in Rust.

This version bypasses `nvidia-smi` completely. It dynamically loads the NVIDIA driver's `libnvidia-ml.so.1` and queries NVML directly, so there are no subprocesses, CSV/XML parsing, Python, virtualenvs, or runtime package dependencies.

## Scripts

Exactly three entry scripts are included:

```bash
./build.sh       # build + run Rust tests
./run.sh         # build if needed, then run against real NVML
./run-demo.sh    # build if needed, then run the animated demo backend
```

`build.sh` uses an existing Cargo/Rust installation when present. If Rust is missing, it bootstraps a **local** minimal stable toolchain under `.tools/` using the official rustup installer; it does not modify your shell profile. The build runs release-mode unit tests, compiles the binary, then performs a `--demo --once` smoke test.

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
- current and maximum PCIe link generation + width
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
- long utilization bars rather than diagnostic-sized meters
- three-column thermals/power/fan and clocks/VRAM rows
- btop-style **row-based** history color grading: bottom rows stay cool/green, middle rows yellow/amber, and upper rows orange/red; the color does not change from time-column to time-column
- the VRAM graph uses the same row-based geometry with its cyan/teal → yellow → orange/red palette
- temperature is graded against NVML-reported hardware limits when the GPU exposes them, instead of fixed generic thresholds
- temperature, power and per-process GPU percentages use the same severity grades
- compact `limit`, `ENC`/`DEC`, hardware thermal-limit, PCIe link and PCIe TX/RX rows
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
       ├── PCIe link + TX/RX traffic
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

