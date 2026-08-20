#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrmStatus {
    /// At least one DRM client exported usable resident device-memory counters.
    Available,
    /// DRM clients are visible for this GPU, but the driver exports no DRM
    /// resident/device-memory accounting keys through fdinfo.
    Unsupported,
    /// Relevant procfs fdinfo entries could not be read with current permissions.
    PermissionDenied,
    /// No matching DRM client could be observed.
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct GpuSnapshot {
    pub index: u32,
    pub name: String,
    pub gpu_util: u32,
    pub memory_total: u64,
    /// Non-reserved framebuffer allocations. NVML v2 is normalized so this
    /// value has the same meaning even on drivers that disagree about whether
    /// `used` includes `reserved`.
    pub memory_used: u64,
    /// System-reserved framebuffer memory reported by NVML memory-info v2.
    pub memory_reserved: Option<u64>,
    /// Sum of NVML-attributed per-process framebuffer allocations for this GPU.
    pub memory_processes: u64,
    /// Residual allocated VRAM after subtracting NVML-attributed process allocations.
    /// Reserved framebuffer is a separate bucket and is never subtracted twice.
    pub memory_other: Option<u64>,
    /// Optional DRM fdinfo resident device-memory accounting, deduplicated by DRM client id.
    pub drm_resident: Option<u64>,
    pub drm_clients: usize,
    pub drm_status: DrmStatus,
    /// True when DRM fdinfo was readable for only a subset of relevant processes.
    pub drm_partial: bool,
    /// Normalized PCI BDF used to match DRM fdinfo clients to this GPU.
    pub pci_bus_id: Option<String>,
    pub temperature_c: Option<u32>,
    pub temperature_slowdown_c: Option<u32>,
    pub temperature_shutdown_c: Option<u32>,
    pub temperature_max_operating_c: Option<u32>,
    pub power_mw: Option<u32>,
    pub power_limit_mw: Option<u32>,
    pub fan_percent: Option<u32>,
    pub graphics_clock_mhz: Option<u32>,
    pub memory_clock_mhz: Option<u32>,
    pub clock_limit: Option<String>,
    pub encoder_util_percent: Option<u32>,
    pub decoder_util_percent: Option<u32>,
    pub pcie_current_gen: Option<u32>,
    pub pcie_current_width: Option<u32>,
    pub pcie_max_gen: Option<u32>,
    pub pcie_max_width: Option<u32>,
    pub pcie_tx_kb_per_s: Option<u32>,
    pub pcie_rx_kb_per_s: Option<u32>,
}

impl GpuSnapshot {
    pub fn memory_percent(&self) -> f64 {
        if self.memory_total == 0 {
            0.0
        } else {
            (self.memory_used as f64 * 100.0 / self.memory_total as f64).clamp(0.0, 100.0)
        }
    }

    pub fn temperature_limit_c(&self) -> Option<u32> {
        self.temperature_max_operating_c
            .or(self.temperature_slowdown_c)
            .or(self.temperature_shutdown_c)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessKind {
    Compute,
    Graphics,
    ComputeGraphics,
}

impl ProcessKind {
    pub fn short(self) -> &'static str {
        match self {
            Self::Compute => "C",
            Self::Graphics => "G",
            Self::ComputeGraphics => "C+G",
        }
    }

    pub fn merge(self, other: Self) -> Self {
        if self == other {
            self
        } else {
            Self::ComputeGraphics
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessSnapshot {
    pub gpu_index: u32,
    pub pid: u32,
    pub name: String,
    pub user: String,
    pub kind: ProcessKind,
    pub gpu_util: Option<u32>,
    pub memory_bytes: Option<u64>,
    /// Optional resident device-memory bytes exported through Linux DRM fdinfo.
    pub drm_resident_bytes: Option<u64>,
}


#[derive(Clone, Debug)]
pub struct VramHolderSnapshot {
    pub pid: u32,
    pub name: String,
    pub user: String,
    /// Sum of framebuffer bytes NVML attributes to this PID across GPUs.
    /// UVM holders can legitimately have no attributable NVML allocation.
    pub nvml_memory_bytes: Option<u64>,
    /// GPU indices on which NVML currently sees this PID. Empty means the
    /// process only surfaced through /dev/nvidia-uvm ownership.
    pub gpu_indices: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub gpus: Vec<GpuSnapshot>,
    pub processes: Vec<ProcessSnapshot>,
    /// Processes that currently hold /dev/nvidia-uvm open. This is an
    /// independent ownership signal and intentionally includes processes that
    /// NVML reports with no per-process framebuffer allocation.
    pub vram_holders: Vec<VramHolderSnapshot>,
    pub uvm_module_refcount: Option<u64>,
    /// True when one or more /proc/<pid>/fd directories could not be inspected.
    pub uvm_scan_partial: bool,
}
