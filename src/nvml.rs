use crate::model::{GpuSnapshot, ProcessKind, ProcessSnapshot, Snapshot};
use crate::procfs::Procfs;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::mem;
use std::ptr;

const NVML_SUCCESS: i32 = 0;
const NVML_ERROR_NOT_SUPPORTED: i32 = 3;
const NVML_ERROR_NOT_FOUND: i32 = 6;
const NVML_ERROR_INSUFFICIENT_SIZE: i32 = 7;
const NVML_VALUE_NOT_AVAILABLE: u64 = u64::MAX;

const NVML_TEMPERATURE_GPU: u32 = 0;
const NVML_CLOCK_GRAPHICS: u32 = 0;
const NVML_CLOCK_MEM: u32 = 2;

const NVML_TEMPERATURE_THRESHOLD_SHUTDOWN: u32 = 0;
const NVML_TEMPERATURE_THRESHOLD_SLOWDOWN: u32 = 1;
const NVML_TEMPERATURE_THRESHOLD_GPU_MAX: u32 = 3;

const NVML_PCIE_UTIL_TX_BYTES: u32 = 0;
const NVML_PCIE_UTIL_RX_BYTES: u32 = 1;

const NVML_CLOCK_EVENT_REASON_GPU_IDLE: u64 = 0x0000_0000_0000_0001;
const NVML_CLOCK_EVENT_REASON_APPLICATION_CLOCKS: u64 = 0x0000_0000_0000_0002;
const NVML_CLOCK_EVENT_REASON_SW_POWER_CAP: u64 = 0x0000_0000_0000_0004;
const NVML_CLOCK_EVENT_REASON_HW_SLOWDOWN: u64 = 0x0000_0000_0000_0008;
const NVML_CLOCK_EVENT_REASON_SYNC_BOOST: u64 = 0x0000_0000_0000_0010;
const NVML_CLOCK_EVENT_REASON_SW_THERMAL: u64 = 0x0000_0000_0000_0020;
const NVML_CLOCK_EVENT_REASON_HW_THERMAL: u64 = 0x0000_0000_0000_0040;
const NVML_CLOCK_EVENT_REASON_HW_POWER_BRAKE: u64 = 0x0000_0000_0000_0080;
const NVML_CLOCK_EVENT_REASON_DISPLAY_CLOCK: u64 = 0x0000_0000_0000_0100;
const NVML_CLOCK_EVENT_REASON_KNOWN: u64 =
    NVML_CLOCK_EVENT_REASON_GPU_IDLE
        | NVML_CLOCK_EVENT_REASON_APPLICATION_CLOCKS
        | NVML_CLOCK_EVENT_REASON_SW_POWER_CAP
        | NVML_CLOCK_EVENT_REASON_HW_SLOWDOWN
        | NVML_CLOCK_EVENT_REASON_SYNC_BOOST
        | NVML_CLOCK_EVENT_REASON_SW_THERMAL
        | NVML_CLOCK_EVENT_REASON_HW_THERMAL
        | NVML_CLOCK_EVENT_REASON_HW_POWER_BRAKE
        | NVML_CLOCK_EVENT_REASON_DISPLAY_CLOCK;

const RTLD_NOW: c_int = 2;
const RTLD_LOCAL: c_int = 0;

type NvmlReturn = i32;
type NvmlDevice = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlProcessInfo {
    pid: u32,
    used_gpu_memory: u64,
    gpu_instance_id: u32,
    compute_instance_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlProcessUtilizationSample {
    pid: u32,
    timestamp: u64,
    sm_util: u32,
    mem_util: u32,
    enc_util: u32,
    dec_util: u32,
}

type InitFn = unsafe extern "C" fn() -> NvmlReturn;
type ShutdownFn = unsafe extern "C" fn() -> NvmlReturn;
type GetCountFn = unsafe extern "C" fn(*mut u32) -> NvmlReturn;
type GetHandleByIndexFn = unsafe extern "C" fn(u32, *mut NvmlDevice) -> NvmlReturn;
type GetNameFn = unsafe extern "C" fn(NvmlDevice, *mut c_char, u32) -> NvmlReturn;
type GetUtilizationFn = unsafe extern "C" fn(NvmlDevice, *mut NvmlUtilization) -> NvmlReturn;
type GetMemoryFn = unsafe extern "C" fn(NvmlDevice, *mut NvmlMemory) -> NvmlReturn;
type GetTemperatureFn = unsafe extern "C" fn(NvmlDevice, u32, *mut u32) -> NvmlReturn;
type GetU32Fn = unsafe extern "C" fn(NvmlDevice, *mut u32) -> NvmlReturn;
type GetU64Fn = unsafe extern "C" fn(NvmlDevice, *mut u64) -> NvmlReturn;
type GetFanFn = unsafe extern "C" fn(NvmlDevice, *mut u32) -> NvmlReturn;
type GetClockFn = unsafe extern "C" fn(NvmlDevice, u32, *mut u32) -> NvmlReturn;
type GetTemperatureThresholdFn = unsafe extern "C" fn(NvmlDevice, u32, *mut u32) -> NvmlReturn;
type GetVideoUtilizationFn = unsafe extern "C" fn(NvmlDevice, *mut u32, *mut u32) -> NvmlReturn;
type GetPcieThroughputFn = unsafe extern "C" fn(NvmlDevice, u32, *mut u32) -> NvmlReturn;
type GetRunningProcessesFn = unsafe extern "C" fn(NvmlDevice, *mut u32, *mut NvmlProcessInfo) -> NvmlReturn;
type GetProcessUtilizationFn = unsafe extern "C" fn(
    NvmlDevice,
    *mut NvmlProcessUtilizationSample,
    *mut u32,
    u64,
) -> NvmlReturn;
type ErrorStringFn = unsafe extern "C" fn(NvmlReturn) -> *const c_char;

#[link(name = "dl")]
extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *const c_char;
}

macro_rules! required_symbol {
    ($library:expr, $name:literal, $ty:ty) => {{
        let pointer = $library.symbol($name)?;
        unsafe { mem::transmute::<*mut c_void, $ty>(pointer) }
    }};
}

macro_rules! optional_symbol {
    ($library:expr, $name:literal, $ty:ty) => {{
        $library
            .symbol_optional($name)
            .map(|pointer| unsafe { mem::transmute::<*mut c_void, $ty>(pointer) })
    }};
}

struct DynamicLibrary {
    handle: *mut c_void,
}

impl DynamicLibrary {
    fn open() -> Result<Self, String> {
        for name in ["libnvidia-ml.so.1", "libnvidia-ml.so"] {
            let c_name = CString::new(name).expect("library name has no NUL");
            let handle = unsafe { dlopen(c_name.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
            if !handle.is_null() {
                return Ok(Self { handle });
            }
        }
        Err(format!(
            "could not load libnvidia-ml.so.1 ({})",
            dl_last_error().unwrap_or_else(|| "NVIDIA driver/NVML is not installed".to_string())
        ))
    }

    fn symbol(&self, name: &str) -> Result<*mut c_void, String> {
        self.symbol_optional(name)
            .ok_or_else(|| format!("NVML is missing required symbol {name}"))
    }

    fn symbol_optional(&self, name: &str) -> Option<*mut c_void> {
        let c_name = CString::new(name).ok()?;
        unsafe {
            let _ = dlerror();
            let pointer = dlsym(self.handle, c_name.as_ptr());
            if pointer.is_null() {
                None
            } else {
                Some(pointer)
            }
        }
    }
}

impl Drop for DynamicLibrary {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = dlclose(self.handle);
            }
        }
    }
}

fn dl_last_error() -> Option<String> {
    unsafe {
        let pointer = dlerror();
        if pointer.is_null() {
            None
        } else {
            Some(CStr::from_ptr(pointer).to_string_lossy().into_owned())
        }
    }
}

struct NvmlApi {
    _library: DynamicLibrary,
    shutdown: ShutdownFn,
    get_count: GetCountFn,
    get_handle_by_index: GetHandleByIndexFn,
    get_name: GetNameFn,
    get_utilization: GetUtilizationFn,
    get_memory: GetMemoryFn,
    get_temperature: Option<GetTemperatureFn>,
    get_power: Option<GetU32Fn>,
    get_power_limit: Option<GetU32Fn>,
    get_fan: Option<GetFanFn>,
    get_clock: Option<GetClockFn>,
    get_temperature_threshold: Option<GetTemperatureThresholdFn>,
    get_clock_event_reasons: Option<GetU64Fn>,
    get_encoder_utilization: Option<GetVideoUtilizationFn>,
    get_decoder_utilization: Option<GetVideoUtilizationFn>,
    get_pcie_current_gen: Option<GetU32Fn>,
    get_pcie_current_width: Option<GetU32Fn>,
    get_pcie_max_gen: Option<GetU32Fn>,
    get_pcie_max_width: Option<GetU32Fn>,
    get_pcie_throughput: Option<GetPcieThroughputFn>,
    get_compute_processes: Option<GetRunningProcessesFn>,
    get_graphics_processes: Option<GetRunningProcessesFn>,
    get_process_utilization: Option<GetProcessUtilizationFn>,
    error_string: Option<ErrorStringFn>,
}

impl NvmlApi {
    fn load() -> Result<Self, String> {
        let library = DynamicLibrary::open()?;

        let init: InitFn = if let Some(pointer) = library.symbol_optional("nvmlInit_v2") {
            unsafe { mem::transmute::<*mut c_void, InitFn>(pointer) }
        } else {
            required_symbol!(library, "nvmlInit", InitFn)
        };
        let shutdown = required_symbol!(library, "nvmlShutdown", ShutdownFn);
        let get_count: GetCountFn = if let Some(pointer) = library.symbol_optional("nvmlDeviceGetCount_v2") {
            unsafe { mem::transmute::<*mut c_void, GetCountFn>(pointer) }
        } else {
            required_symbol!(library, "nvmlDeviceGetCount", GetCountFn)
        };
        let get_handle_by_index: GetHandleByIndexFn = if let Some(pointer) = library.symbol_optional("nvmlDeviceGetHandleByIndex_v2") {
            unsafe { mem::transmute::<*mut c_void, GetHandleByIndexFn>(pointer) }
        } else {
            required_symbol!(library, "nvmlDeviceGetHandleByIndex", GetHandleByIndexFn)
        };

        let get_compute_processes = load_first_running_process_symbol(
            &library,
            &[
                "nvmlDeviceGetComputeRunningProcesses_v3",
                "nvmlDeviceGetComputeRunningProcesses_v2",
            ],
        );
        let get_graphics_processes = load_first_running_process_symbol(
            &library,
            &[
                "nvmlDeviceGetGraphicsRunningProcesses_v3",
                "nvmlDeviceGetGraphicsRunningProcesses_v2",
            ],
        );

        let api = Self {
            shutdown,
            get_count,
            get_handle_by_index,
            get_name: required_symbol!(library, "nvmlDeviceGetName", GetNameFn),
            get_utilization: required_symbol!(library, "nvmlDeviceGetUtilizationRates", GetUtilizationFn),
            get_memory: required_symbol!(library, "nvmlDeviceGetMemoryInfo", GetMemoryFn),
            get_temperature: optional_symbol!(library, "nvmlDeviceGetTemperature", GetTemperatureFn),
            get_power: optional_symbol!(library, "nvmlDeviceGetPowerUsage", GetU32Fn),
            get_power_limit: optional_symbol!(library, "nvmlDeviceGetPowerManagementLimit", GetU32Fn),
            get_fan: optional_symbol!(library, "nvmlDeviceGetFanSpeed", GetFanFn),
            get_clock: optional_symbol!(library, "nvmlDeviceGetClockInfo", GetClockFn),
            get_temperature_threshold: optional_symbol!(
                library,
                "nvmlDeviceGetTemperatureThreshold",
                GetTemperatureThresholdFn
            ),
            get_clock_event_reasons: load_clock_event_reasons_symbol(&library),
            get_encoder_utilization: optional_symbol!(
                library,
                "nvmlDeviceGetEncoderUtilization",
                GetVideoUtilizationFn
            ),
            get_decoder_utilization: optional_symbol!(
                library,
                "nvmlDeviceGetDecoderUtilization",
                GetVideoUtilizationFn
            ),
            get_pcie_current_gen: optional_symbol!(
                library,
                "nvmlDeviceGetCurrPcieLinkGeneration",
                GetU32Fn
            ),
            get_pcie_current_width: optional_symbol!(
                library,
                "nvmlDeviceGetCurrPcieLinkWidth",
                GetU32Fn
            ),
            get_pcie_max_gen: optional_symbol!(
                library,
                "nvmlDeviceGetMaxPcieLinkGeneration",
                GetU32Fn
            ),
            get_pcie_max_width: optional_symbol!(
                library,
                "nvmlDeviceGetMaxPcieLinkWidth",
                GetU32Fn
            ),
            get_pcie_throughput: optional_symbol!(
                library,
                "nvmlDeviceGetPcieThroughput",
                GetPcieThroughputFn
            ),
            get_compute_processes,
            get_graphics_processes,
            get_process_utilization: optional_symbol!(library, "nvmlDeviceGetProcessUtilization", GetProcessUtilizationFn),
            error_string: optional_symbol!(library, "nvmlErrorString", ErrorStringFn),
            _library: library,
        };

        let result = unsafe { init() };
        if result != NVML_SUCCESS {
            return Err(format!("nvmlInit failed: {}", api.error_text(result)));
        }
        Ok(api)
    }

    fn error_text(&self, code: NvmlReturn) -> String {
        if let Some(error_string) = self.error_string {
            let pointer = unsafe { error_string(code) };
            if !pointer.is_null() {
                return unsafe { CStr::from_ptr(pointer) }
                    .to_string_lossy()
                    .into_owned();
            }
        }
        format!("NVML error {code}")
    }
}

fn load_clock_event_reasons_symbol(library: &DynamicLibrary) -> Option<GetU64Fn> {
    for name in [
        "nvmlDeviceGetCurrentClocksEventReasons",
        "nvmlDeviceGetCurrentClocksThrottleReasons",
    ] {
        if let Some(pointer) = library.symbol_optional(name) {
            return Some(unsafe { mem::transmute::<*mut c_void, GetU64Fn>(pointer) });
        }
    }
    None
}

fn load_first_running_process_symbol(
    library: &DynamicLibrary,
    names: &[&str],
) -> Option<GetRunningProcessesFn> {
    for name in names {
        if let Some(pointer) = library.symbol_optional(name) {
            return Some(unsafe { mem::transmute::<*mut c_void, GetRunningProcessesFn>(pointer) });
        }
    }
    None
}

pub struct NvmlCollector {
    api: NvmlApi,
    devices: Vec<NvmlDevice>,
    procfs: Procfs,
    process_last_seen: Vec<u64>,
}

impl NvmlCollector {
    pub fn new() -> Result<Self, String> {
        let api = NvmlApi::load()?;
        let mut count = 0u32;
        let result = unsafe { (api.get_count)(&mut count) };
        if result != NVML_SUCCESS {
            unsafe {
                let _ = (api.shutdown)();
            }
            return Err(format!("nvmlDeviceGetCount failed: {}", api.error_text(result)));
        }

        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut device = ptr::null_mut();
            let result = unsafe { (api.get_handle_by_index)(index, &mut device) };
            if result == NVML_SUCCESS && !device.is_null() {
                devices.push(device);
            }
        }

        if devices.is_empty() {
            unsafe {
                let _ = (api.shutdown)();
            }
            return Err("NVML initialized but no accessible NVIDIA GPUs were found".to_string());
        }

        let process_last_seen = vec![0; devices.len()];
        Ok(Self {
            api,
            devices,
            procfs: Procfs::new(),
            process_last_seen,
        })
    }

    pub fn collect(&mut self) -> Snapshot {
        let mut snapshot = Snapshot::default();

        for index in 0..self.devices.len() {
            let device = self.devices[index];
            snapshot.gpus.push(self.collect_gpu(index as u32, device));
            snapshot
                .processes
                .extend(self.collect_processes(index as u32, device));
        }

        snapshot.processes.sort_by(|a, b| {
            b.gpu_util
                .unwrap_or(0)
                .cmp(&a.gpu_util.unwrap_or(0))
                .then_with(|| b.memory_bytes.unwrap_or(0).cmp(&a.memory_bytes.unwrap_or(0)))
                .then_with(|| a.pid.cmp(&b.pid))
        });
        snapshot
    }

    fn collect_gpu(&self, index: u32, device: NvmlDevice) -> GpuSnapshot {
        let mut name_buffer = [0 as c_char; 256];
        let name = if unsafe {
            (self.api.get_name)(device, name_buffer.as_mut_ptr(), name_buffer.len() as u32)
        } == NVML_SUCCESS
        {
            unsafe { CStr::from_ptr(name_buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            format!("NVIDIA GPU {index}")
        };

        let mut utilization = NvmlUtilization::default();
        let gpu_util = if unsafe { (self.api.get_utilization)(device, &mut utilization) } == NVML_SUCCESS {
            utilization.gpu.min(100)
        } else {
            0
        };

        let mut memory = NvmlMemory::default();
        if unsafe { (self.api.get_memory)(device, &mut memory) } != NVML_SUCCESS {
            memory = NvmlMemory::default();
        }

        GpuSnapshot {
            index,
            name,
            gpu_util,
            memory_total: memory.total,
            memory_used: memory.used,
            temperature_c: query_temperature(self.api.get_temperature, device),
            temperature_slowdown_c: query_temperature_threshold(
                self.api.get_temperature_threshold,
                device,
                NVML_TEMPERATURE_THRESHOLD_SLOWDOWN,
            ),
            temperature_shutdown_c: query_temperature_threshold(
                self.api.get_temperature_threshold,
                device,
                NVML_TEMPERATURE_THRESHOLD_SHUTDOWN,
            ),
            temperature_max_operating_c: query_temperature_threshold(
                self.api.get_temperature_threshold,
                device,
                NVML_TEMPERATURE_THRESHOLD_GPU_MAX,
            ),
            power_mw: query_u32(self.api.get_power, device),
            power_limit_mw: query_u32(self.api.get_power_limit, device),
            fan_percent: query_u32(self.api.get_fan, device),
            graphics_clock_mhz: query_clock(self.api.get_clock, device, NVML_CLOCK_GRAPHICS),
            memory_clock_mhz: query_clock(self.api.get_clock, device, NVML_CLOCK_MEM),
            clock_limit: query_clock_limit(self.api.get_clock_event_reasons, device),
            encoder_util_percent: query_video_utilization(self.api.get_encoder_utilization, device),
            decoder_util_percent: query_video_utilization(self.api.get_decoder_utilization, device),
            pcie_current_gen: query_u32(self.api.get_pcie_current_gen, device),
            pcie_current_width: query_u32(self.api.get_pcie_current_width, device),
            pcie_max_gen: query_u32(self.api.get_pcie_max_gen, device),
            pcie_max_width: query_u32(self.api.get_pcie_max_width, device),
            pcie_tx_kb_per_s: query_pcie_throughput(
                self.api.get_pcie_throughput,
                device,
                NVML_PCIE_UTIL_TX_BYTES,
            ),
            pcie_rx_kb_per_s: query_pcie_throughput(
                self.api.get_pcie_throughput,
                device,
                NVML_PCIE_UTIL_RX_BYTES,
            ),
        }
    }

    fn collect_processes(&mut self, gpu_index: u32, device: NvmlDevice) -> Vec<ProcessSnapshot> {
        let mut rows: BTreeMap<u32, ProcessGpuRow> = BTreeMap::new();

        if let Some(function) = self.api.get_compute_processes {
            for process in query_running_processes(function, device) {
                merge_process(&mut rows, process, ProcessKind::Compute);
            }
        }
        if let Some(function) = self.api.get_graphics_processes {
            for process in query_running_processes(function, device) {
                merge_process(&mut rows, process, ProcessKind::Graphics);
            }
        }

        let utilization = if let Some(function) = self.api.get_process_utilization {
            let slot = gpu_index as usize;
            let last_seen = self.process_last_seen.get(slot).copied().unwrap_or(0);
            match query_process_utilization(function, device, last_seen) {
                Some((values, newest_timestamp)) => {
                    if let Some(stored) = self.process_last_seen.get_mut(slot) {
                        *stored = newest_timestamp.max(*stored);
                    }
                    Some(values)
                }
                None => None,
            }
        } else {
            None
        };

        rows.into_iter()
            .map(|(pid, row)| {
                let (user, name) = self.procfs.metadata(pid);
                ProcessSnapshot {
                    gpu_index,
                    pid,
                    name,
                    user,
                    kind: row.kind,
                    gpu_util: utilization.as_ref().map(|values| *values.get(&pid).unwrap_or(&0)),
                    memory_bytes: row.memory,
                }
            })
            .collect()
    }
}

impl Drop for NvmlCollector {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.api.shutdown)();
        }
    }
}

#[derive(Clone, Copy)]
struct ProcessGpuRow {
    kind: ProcessKind,
    memory: Option<u64>,
}

fn merge_process(
    rows: &mut BTreeMap<u32, ProcessGpuRow>,
    process: NvmlProcessInfo,
    kind: ProcessKind,
) {
    let memory = memory_value(process.used_gpu_memory);
    rows.entry(process.pid)
        .and_modify(|row| {
            row.kind = row.kind.merge(kind);
            row.memory = match (row.memory, memory) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            };
        })
        .or_insert(ProcessGpuRow { kind, memory });
}

fn query_u32(function: Option<GetU32Fn>, device: NvmlDevice) -> Option<u32> {
    let function = function?;
    let mut value = 0u32;
    if unsafe { function(device, &mut value) } == NVML_SUCCESS {
        Some(value)
    } else {
        None
    }
}

fn query_temperature_threshold(
    function: Option<GetTemperatureThresholdFn>,
    device: NvmlDevice,
    threshold: u32,
) -> Option<u32> {
    let function = function?;
    let mut value = 0u32;
    if unsafe { function(device, threshold, &mut value) } == NVML_SUCCESS {
        Some(value)
    } else {
        None
    }
}

fn query_video_utilization(
    function: Option<GetVideoUtilizationFn>,
    device: NvmlDevice,
) -> Option<u32> {
    let function = function?;
    let mut utilization = 0u32;
    let mut sampling_period_us = 0u32;
    if unsafe { function(device, &mut utilization, &mut sampling_period_us) } == NVML_SUCCESS {
        Some(utilization.min(100))
    } else {
        None
    }
}

fn query_pcie_throughput(
    function: Option<GetPcieThroughputFn>,
    device: NvmlDevice,
    counter: u32,
) -> Option<u32> {
    let function = function?;
    let mut value = 0u32;
    if unsafe { function(device, counter, &mut value) } == NVML_SUCCESS {
        Some(value)
    } else {
        None
    }
}

fn query_clock_limit(function: Option<GetU64Fn>, device: NvmlDevice) -> Option<String> {
    let function = function?;
    let mut mask = 0u64;
    if unsafe { function(device, &mut mask) } != NVML_SUCCESS {
        return None;
    }
    Some(format_clock_limit(mask))
}

fn format_clock_limit(mask: u64) -> String {
    if mask == 0 {
        return "none".to_string();
    }

    let mut reasons = Vec::new();
    let mut push = |bit: u64, label: &'static str| {
        if mask & bit != 0 {
            reasons.push(label);
        }
    };

    // Put actionable limiters first. Idle is intentionally last and is only
    // shown when no actual limiter is active.
    push(NVML_CLOCK_EVENT_REASON_SW_POWER_CAP, "power");
    push(NVML_CLOCK_EVENT_REASON_SW_THERMAL, "thermal");
    push(NVML_CLOCK_EVENT_REASON_HW_THERMAL, "hw thermal");
    push(NVML_CLOCK_EVENT_REASON_HW_POWER_BRAKE, "power brake");
    push(NVML_CLOCK_EVENT_REASON_HW_SLOWDOWN, "hw slowdown");
    push(NVML_CLOCK_EVENT_REASON_SYNC_BOOST, "sync");
    push(NVML_CLOCK_EVENT_REASON_APPLICATION_CLOCKS, "app clocks");
    push(NVML_CLOCK_EVENT_REASON_DISPLAY_CLOCK, "display");

    let unknown = mask & !NVML_CLOCK_EVENT_REASON_KNOWN;
    if unknown != 0 {
        reasons.push("other");
    }

    if reasons.is_empty() && mask & NVML_CLOCK_EVENT_REASON_GPU_IDLE != 0 {
        reasons.push("idle");
    }

    if reasons.is_empty() {
        "none".to_string()
    } else {
        reasons.join("+")
    }
}

fn query_temperature(function: Option<GetTemperatureFn>, device: NvmlDevice) -> Option<u32> {
    let function = function?;
    let mut value = 0u32;
    if unsafe { function(device, NVML_TEMPERATURE_GPU, &mut value) } == NVML_SUCCESS {
        Some(value)
    } else {
        None
    }
}

fn query_clock(function: Option<GetClockFn>, device: NvmlDevice, clock: u32) -> Option<u32> {
    let function = function?;
    let mut value = 0u32;
    if unsafe { function(device, clock, &mut value) } == NVML_SUCCESS {
        Some(value)
    } else {
        None
    }
}

fn query_running_processes(function: GetRunningProcessesFn, device: NvmlDevice) -> Vec<NvmlProcessInfo> {
    let mut count = 0u32;
    let first = unsafe { function(device, &mut count, ptr::null_mut()) };
    if first == NVML_SUCCESS && count == 0 {
        return Vec::new();
    }
    if first != NVML_ERROR_INSUFFICIENT_SIZE && first != NVML_SUCCESS {
        return Vec::new();
    }

    let mut capacity = count.saturating_add(8).max(8);
    for _ in 0..2 {
        let mut infos = vec![NvmlProcessInfo::default(); capacity as usize];
        let mut actual = capacity;
        let result = unsafe { function(device, &mut actual, infos.as_mut_ptr()) };
        if result == NVML_SUCCESS {
            infos.truncate(actual as usize);
            return infos;
        }
        if result != NVML_ERROR_INSUFFICIENT_SIZE {
            break;
        }
        capacity = actual.saturating_add(8).max(capacity.saturating_mul(2));
    }
    Vec::new()
}

fn query_process_utilization(
    function: GetProcessUtilizationFn,
    device: NvmlDevice,
    last_seen_timestamp: u64,
) -> Option<(BTreeMap<u32, u32>, u64)> {
    let mut count = 0u32;
    let first = unsafe {
        function(
            device,
            ptr::null_mut(),
            &mut count,
            last_seen_timestamp,
        )
    };
    if first == NVML_ERROR_NOT_SUPPORTED {
        return None;
    }
    if first == NVML_ERROR_NOT_FOUND || (first == NVML_SUCCESS && count == 0) {
        return Some((BTreeMap::new(), last_seen_timestamp));
    }
    if first != NVML_ERROR_INSUFFICIENT_SIZE && first != NVML_SUCCESS {
        return None;
    }

    let mut capacity = count.saturating_add(8).max(8);
    for _ in 0..2 {
        let mut samples = vec![NvmlProcessUtilizationSample::default(); capacity as usize];
        let mut actual = capacity;
        let result = unsafe {
            function(
                device,
                samples.as_mut_ptr(),
                &mut actual,
                last_seen_timestamp,
            )
        };
        if result == NVML_SUCCESS {
            samples.truncate(actual as usize);
            let mut latest: BTreeMap<u32, (u64, u32)> = BTreeMap::new();
            for sample in samples {
                let entry = latest.entry(sample.pid).or_insert((0, 0));
                if sample.timestamp >= entry.0 {
                    *entry = (sample.timestamp, sample.sm_util.min(100));
                }
            }
            let newest_timestamp = latest
                .values()
                .map(|(timestamp, _)| *timestamp)
                .max()
                .unwrap_or(last_seen_timestamp);
            let values = latest
                .into_iter()
                .map(|(pid, (_, util))| (pid, util))
                .collect();
            return Some((values, newest_timestamp));
        }
        if result == NVML_ERROR_NOT_FOUND {
            return Some((BTreeMap::new(), last_seen_timestamp));
        }
        if result == NVML_ERROR_NOT_SUPPORTED {
            return None;
        }
        if result != NVML_ERROR_INSUFFICIENT_SIZE {
            return None;
        }
        capacity = actual.saturating_add(8).max(capacity.saturating_mul(2));
    }
    None
}

fn memory_value(value: u64) -> Option<u64> {
    if value == NVML_VALUE_NOT_AVAILABLE {
        None
    } else {
        Some(value)
    }
}


#[cfg(test)]
mod tests {
    use super::{format_clock_limit, NvmlMemory, NvmlProcessInfo, NvmlProcessUtilizationSample, NvmlUtilization, NVML_CLOCK_EVENT_REASON_GPU_IDLE, NVML_CLOCK_EVENT_REASON_HW_THERMAL, NVML_CLOCK_EVENT_REASON_SW_POWER_CAP};
    use std::mem::{align_of, size_of};

    #[test]
    fn nvml_ffi_layout_sizes_match_linux_c_abi() {
        assert_eq!(size_of::<NvmlUtilization>(), 8);
        assert_eq!(size_of::<NvmlMemory>(), 24);
        assert_eq!(size_of::<NvmlProcessInfo>(), 24);
        assert_eq!(size_of::<NvmlProcessUtilizationSample>(), 32);
        assert_eq!(align_of::<NvmlProcessInfo>(), 8);
        assert_eq!(align_of::<NvmlProcessUtilizationSample>(), 8);
    }

    #[test]
    fn clock_limit_decoding_prefers_actionable_reasons() {
        assert_eq!(format_clock_limit(0), "none");
        assert_eq!(format_clock_limit(NVML_CLOCK_EVENT_REASON_GPU_IDLE), "idle");
        assert_eq!(
            format_clock_limit(
                NVML_CLOCK_EVENT_REASON_GPU_IDLE
                    | NVML_CLOCK_EVENT_REASON_SW_POWER_CAP
                    | NVML_CLOCK_EVENT_REASON_HW_THERMAL
            ),
            "power+hw thermal"
        );
    }
}
