# nost

Minimal btop-style NVIDIA GPU monitor written in Rust.

 It dynamically loads the NVIDIA driver's `libnvidia-ml.so.1` and queries NVML directly, so there are no subprocesses, CSV/XML parsing, Python, virtualenvs, or runtime package dependencies.
 
<img width="823" height="774" alt="image" src="https://github.com/user-attachments/assets/e45a4971-f4ff-4264-aa18-814ee4861b3e" />

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
