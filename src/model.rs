#[derive(Clone, Debug)]
pub struct GpuSnapshot {
    pub index: u32,
    pub name: String,
    pub gpu_util: u32,
    pub memory_total: u64,
    pub memory_used: u64,
    pub temperature_c: Option<u32>,
    pub power_mw: Option<u32>,
    pub power_limit_mw: Option<u32>,
    pub fan_percent: Option<u32>,
    pub graphics_clock_mhz: Option<u32>,
    pub memory_clock_mhz: Option<u32>,
}

impl GpuSnapshot {
    pub fn memory_percent(&self) -> f64 {
        if self.memory_total == 0 {
            0.0
        } else {
            (self.memory_used as f64 * 100.0 / self.memory_total as f64).clamp(0.0, 100.0)
        }
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
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub gpus: Vec<GpuSnapshot>,
    pub processes: Vec<ProcessSnapshot>,
}
