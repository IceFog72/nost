# nost

Minimal btop-style NVIDIA GPU monitor written in Rust.

This version bypasses `nvidia-smi` completely. It dynamically loads the NVIDIA
driver's `libnvidia-ml.so.1` and queries NVML directly, so there are no subprocesses,
CSV/XML parsing, Python, virtualenvs, or runtime package dependencies.
<img width="832" height="444" alt="image" src="https://github.com/user-attachments/assets/e45396e7-b443-4aa0-b3d1-c43bd4d29653" />


## Scripts

Runtime/build scripts:

```bash
./build.sh       # build + run Rust tests
./run.sh         # build if needed, then run against real NVML
./run-demo.sh    # build if needed, then run the animated demo backend
```

`build.sh` uses an existing Cargo/Rust installation when present. If Rust is missing, it bootstraps a **local** minimal stable toolchain under `.tools/` using the official rustup installer; it does not modify your shell profile. The build runs release-mode unit and CLI tests, compiles the binary, then performs a `--demo --once` smoke test.

## What the screen shows

- **GPU panel**: five history graphs separated by thin rules, each titled in the border line above it.
  - First row: `gpu` utilization and `mem` (used / usable VRAM).
  - Second row: `power` (draw as a share of the power limit), `temp` (closeness to the thermal limit) and `cap`.
  - `cap` is a performance-cap reason bar in the style of GPU-Z: every sample is a full-height column in the colour of its main reason, and no limit stays blank. Reasons are shown as `Pwr`, `Thrm`, `VRel`, `Idle`, `Board`, plus `hw slowdown`, `power brake`, `sync`, `app clocks`, `display` and `other`.
  - Below the graphs: core clock, memory clock and fan.
- **processes**: per-process GPU% and VRAM, sorted by VRAM by default.
- **vram holders**: processes holding `/dev/nvidia-uvm`, including ones NVML does not attribute.

On very short terminals each graph collapses to one line and the secondary rows disappear; titles that do not fit in a border are drawn over the graph instead.

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
- active clock-event / performance-limit reasons (power, thermal, hardware slowdown, reliability, board limit, idle, etc.)
- NVENC encoder and NVDEC decoder utilization (in `--json`; not drawn on screen)
- PCIe TX/RX throughput (in `--json`; not drawn on screen)
- compute and graphics process PIDs
- per-process framebuffer memory
- per-process SM utilization when `nvmlDeviceGetProcessUtilization` is supported; sample timestamps are advanced between refreshes so old non-zero utilization is not held forever

Process user/name metadata comes from Linux `/proc` and `/etc/passwd`, not from `nvidia-smi`.


Unsupported optional metrics display as `--` instead of failing the monitor.

## Options

```text
./run.sh --interval 0.25   # refresh every 250 ms
./run.sh --inline          # render on the normal terminal screen
./run.sh --once            # one frame, then exit
./run.sh --no-color        # disable colors; screen-control escapes remain
./run.sh --json            # print one snapshot as JSON and exit
./run-demo.sh              # UI preview without an NVIDIA GPU
```

Default refresh is 1000 ms.

The JSON schema is versioned by its "schema" field; fields NVML cannot provide are null.

While nost is running, press or left-click `-` or `+` to change the refresh interval by 100 ms. The current interval appears in the first GPU panel's top-right corner. The allowed range is 100 ms to 60 seconds. Hold Shift while dragging to select terminal text. Ctrl-Z suspends nost and restores the terminal; `fg` resumes the dashboard. Press `q` (or Ctrl-C) to quit. 

Left-click a column header in the processes table to sort by it; the default is VRAM, largest first. Click the same header again to reverse the order. `▲` marks ascending and `▼` descending.

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
       ├── PCIe TX/RX traffic (background sampler thread)
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

## Internal VRAM accounting

nost normalizes NVML memory-v2 accounting internally and shows reserved, usable, and used VRAM separately. Memory percentage is used VRAM divided by usable VRAM. It can use DRM `fdinfo` as a cross-check, but `proc` and `drm` are not rendered in the main GPU panel. Per-process VRAM stays in the process table; the third `vram holders` panel is focused specifically on finding UVM/CUDA owners that may be absent from NVML's attributable VRAM rows.

GPU metrics and NVML per-process VRAM refresh every frame. The internal DRM
cross-check is cached for up to two seconds per PID/GPU pair, and the UVM owner
scan runs every two seconds, reducing repeated walks through `/proc`. Newly
observed PID/GPU pairs are probed immediately. Process names and users are read
once per PID within a frame and refreshed on the next frame.
