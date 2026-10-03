use crate::model::{DrmStatus, GpuSnapshot, ProcessKind, ProcessSnapshot, Snapshot, VramHolderSnapshot};
use crate::procfs::{normalize_pci_bus_id, Procfs, UvmProbe};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::mem;
use std::ptr;
use std::time::{Duration, Instant};

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
struct NvmlMemoryV2 {
    version: u32,
    total: u64,
    reserved: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvmlPciInfo {
    bus_id_legacy: [c_char; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_subsystem_id: u32,
    bus_id: [c_char; 32],
}

impl Default for NvmlPciInfo {
    fn default() -> Self {
        Self {
            bus_id_legacy: [0; 16], domain: 0, bus: 0, device: 0,
            pci_device_id: 0, pci_subsystem_id: 0, bus_id: [0; 32],
        }
    }
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
type GetMemoryV2Fn = unsafe extern "C" fn(NvmlDevice, *mut NvmlMemoryV2) -> NvmlReturn;
type GetPciInfoFn = unsafe extern "C" fn(NvmlDevice, *mut NvmlPciInfo) -> NvmlReturn;
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
    get_memory_v2: Option<GetMemoryV2Fn>,
    get_pci_info: Option<GetPciInfoFn>,
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
            get_memory_v2: optional_symbol!(library, "nvmlDeviceGetMemoryInfo_v2", GetMemoryV2Fn),
            get_pci_info: load_pci_info_symbol(&library),
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


fn load_pci_info_symbol(library: &DynamicLibrary) -> Option<GetPciInfoFn> {
    for name in ["nvmlDeviceGetPciInfo_v3", "nvmlDeviceGetPciInfo_v2", "nvmlDeviceGetPciInfo"] {
        if let Some(pointer) = library.symbol_optional(name) {
            return Some(unsafe { mem::transmute::<*mut c_void, GetPciInfoFn>(pointer) });
        }
    }
    None
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

#[derive(Clone, Copy)]
struct DeviceHandle {
    index: u32,
    handle: NvmlDevice,
}

pub struct NvmlCollector {
    api: NvmlApi,
    devices: Vec<DeviceHandle>,
    procfs: Procfs,
    process_last_seen: Vec<u64>,
    uvm_probe: UvmProbe,
    uvm_last_scan: Option<Instant>,
}

impl NvmlCollector {
    pub fn new() -> Result<Self, String> {
        Self::from_api(NvmlApi::load()?)
    }

    fn from_api(api: NvmlApi) -> Result<Self, String> {
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
                devices.push(DeviceHandle { index, handle: device });
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
            uvm_probe: UvmProbe::default(),
            uvm_last_scan: None,
        })
    }

    pub fn collect(&mut self) -> Snapshot {
        self.procfs.begin_snapshot();
        let mut snapshot = Snapshot::default();

        for slot in 0..self.devices.len() {
            let DeviceHandle { index, handle: device } = self.devices[slot];
            let pci_bus_id = query_pci_bus_id(self.api.get_pci_info, device);
            let (processes, drm_resident, drm_clients, drm_status, drm_partial) =
                self.collect_processes(slot, index, device, pci_bus_id.as_deref());
            let process_memory = processes
                .iter()
                .filter_map(|process| process.memory_bytes)
                .fold(0u64, u64::saturating_add);
            snapshot.gpus.push(self.collect_gpu(
                index, device, pci_bus_id, process_memory, drm_resident, drm_clients, drm_status, drm_partial,
            ));
            snapshot.processes.extend(processes);
        }

        snapshot.processes.sort_by(|a, b| {
            b.gpu_util
                .unwrap_or(0)
                .cmp(&a.gpu_util.unwrap_or(0))
                .then_with(|| b.memory_bytes.unwrap_or(0).cmp(&a.memory_bytes.unwrap_or(0)))
                .then_with(|| a.pid.cmp(&b.pid))
        });

        // /dev/nvidia-uvm ownership is an independent signal from NVML's
        // per-process framebuffer accounting. Some CUDA/TTS/browser processes
        // keep a UVM context open while NVML attributes little or no VRAM to
        // them. Refresh this more expensive /proc scan at a lower cadence.
        let refresh_uvm = self
            .uvm_last_scan
            .map(|last| last.elapsed() >= Duration::from_secs(2))
            .unwrap_or(true);
        if refresh_uvm {
            self.uvm_probe = self.procfs.uvm_holders();
            self.uvm_last_scan = Some(Instant::now());
        }

        let mut nvml_by_pid: BTreeMap<u32, (Option<u64>, BTreeSet<u32>)> = BTreeMap::new();
        for process in &snapshot.processes {
            let entry = nvml_by_pid
                .entry(process.pid)
                .or_insert_with(|| (None, BTreeSet::new()));
            if let Some(bytes) = process.memory_bytes {
                entry.0 = Some(entry.0.unwrap_or(0).saturating_add(bytes));
            }
            entry.1.insert(process.gpu_index);
        }

        snapshot.vram_holders = self
            .uvm_probe
            .pids
            .iter()
            .copied()
            .map(|pid| {
                let (user, name) = self.procfs.metadata(pid);
                let (memory, gpus) = nvml_by_pid
                    .get(&pid)
                    .map(|(memory, gpus)| {
                        (*memory, gpus.iter().copied().collect())
                    })
                    .unwrap_or((None, Vec::new()));
                VramHolderSnapshot {
                    pid,
                    name,
                    user,
                    nvml_memory_bytes: memory,
                    gpu_indices: gpus,
                }
            })
            .collect();
        snapshot.vram_holders.sort_by(|a, b| {
            b.nvml_memory_bytes
                .unwrap_or(0)
                .cmp(&a.nvml_memory_bytes.unwrap_or(0))
                .then_with(|| a.pid.cmp(&b.pid))
        });
        snapshot.uvm_module_refcount = self.uvm_probe.module_refcount;
        snapshot.uvm_scan_partial = self.uvm_probe.permission_denied;

        snapshot
    }

    fn collect_gpu(&self, index: u32, device: NvmlDevice, pci_bus_id: Option<String>, process_memory: u64, drm_resident: Option<u64>, drm_clients: usize, drm_status: DrmStatus, drm_partial: bool) -> GpuSnapshot {
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

        let (memory_total, memory_used, memory_reserved) = query_memory(
            self.api.get_memory_v2, self.api.get_memory, device,
        );
        let memory_other = memory_reserved.map(|_| {
            residual_vram(memory_used, process_memory)
        });

        GpuSnapshot {
            index,
            name,
            gpu_util,
            memory_total,
            memory_used,
            memory_reserved,
            memory_processes: process_memory,
            memory_other,
            drm_resident,
            drm_clients,
            drm_status,
            drm_partial,
            pci_bus_id,
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

    fn collect_processes(&mut self, slot: usize, gpu_index: u32, device: NvmlDevice, pci_bus_id: Option<&str>) -> (Vec<ProcessSnapshot>, Option<u64>, usize, DrmStatus, bool) {
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

        let mut seen_drm_clients = BTreeSet::new();
        let mut matched_drm_clients = BTreeSet::new();
        let mut drm_total = 0u64;
        let mut drm_available = false;
        let mut drm_partial = false;
        let mut output = Vec::with_capacity(rows.len());
        for (pid, row) in rows {
            let (user, name) = self.procfs.metadata(pid);
            let drm_probe = pci_bus_id
                .map(|bdf| self.procfs.drm_clients(pid, bdf))
                .unwrap_or_default();
            drm_partial |= drm_probe.permission_denied;
            matched_drm_clients.extend(drm_probe.matched_client_ids.iter().copied());
            let drm_resident_bytes = if drm_probe.clients.is_empty() {
                None
            } else {
                drm_available = true;
                Some(drm_probe.clients.iter().map(|client| client.resident_device_bytes).fold(0u64, u64::saturating_add))
            };
            for client in drm_probe.clients {
                if seen_drm_clients.insert(client.client_id) {
                    drm_total = drm_total.saturating_add(client.resident_device_bytes);
                }
            }
            output.push(ProcessSnapshot {
                gpu_index, pid, name, user, kind: row.kind,
                gpu_util: utilization.as_ref().map(|values| *values.get(&pid).unwrap_or(&0)),
                memory_bytes: row.memory,
                drm_resident_bytes,
            });
        }

        let drm_status = classify_drm_status(
            drm_available,
            !matched_drm_clients.is_empty(),
            drm_partial,
        );
        let drm_resident = drm_available.then_some(drm_total);
        (output, drm_resident, seen_drm_clients.len(), drm_status, drm_partial)
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

fn residual_vram(allocated: u64, processes: u64) -> u64 {
    allocated.saturating_sub(processes)
}

fn classify_drm_status(memory_available: bool, matching_clients: bool, permission_denied: bool) -> DrmStatus {
    if memory_available {
        DrmStatus::Available
    } else if matching_clients {
        // Generic DRM fdinfo identification is present, but this driver did
        // not export any drm-resident-* / drm-memory-* VRAM counters.
        DrmStatus::Unsupported
    } else if permission_denied {
        DrmStatus::PermissionDenied
    } else {
        DrmStatus::Unavailable
    }
}


/// Normalize the two NVML memory-v2 behaviours seen in released drivers.
///
/// NVIDIA's documentation says `used` includes `reserved`, but released NVML
/// implementations have also returned the reserved framebuffer as a separate
/// bucket (`total == free + used + reserved`).  Detect the actual ABI semantics
/// from the accounting identity instead of assuming either interpretation.
/// The returned `allocated` value always means non-reserved, actively allocated
/// framebuffer memory, so the UI remains consistent across driver versions.
fn normalize_memory_v2(memory: NvmlMemoryV2) -> (u64, u64, u64) {
    let separate_sum = memory
        .free
        .saturating_add(memory.used)
        .saturating_add(memory.reserved);
    let included_sum = memory.free.saturating_add(memory.used);
    let separate_error = memory.total.abs_diff(separate_sum);
    let included_error = memory.total.abs_diff(included_sum);

    let allocated = if included_error < separate_error {
        // `used` includes reserved: strip it to get process/other allocations.
        memory.used.saturating_sub(memory.reserved)
    } else {
        // `used` and `reserved` are already disjoint (common released-driver
        // behaviour and what nvidia-smi reports as Used + Reserved).
        memory.used
    };

    (memory.total, allocated, memory.reserved)
}

fn query_memory(v2: Option<GetMemoryV2Fn>, v1: GetMemoryFn, device: NvmlDevice) -> (u64, u64, Option<u64>) {
    if let Some(function) = v2 {
        let mut memory = NvmlMemoryV2::default();
        memory.version = (mem::size_of::<NvmlMemoryV2>() as u32) | (2u32 << 24);
        if unsafe { function(device, &mut memory) } == NVML_SUCCESS {
            let (total, allocated, reserved) = normalize_memory_v2(memory);
            return (total, allocated, Some(reserved));
        }
    }
    let mut memory = NvmlMemory::default();
    if unsafe { v1(device, &mut memory) } == NVML_SUCCESS {
        (memory.total, memory.used, None)
    } else {
        (0, 0, None)
    }
}

fn query_pci_bus_id(function: Option<GetPciInfoFn>, device: NvmlDevice) -> Option<String> {
    let function = function?;
    let mut info = NvmlPciInfo::default();
    if unsafe { function(device, &mut info) } != NVML_SUCCESS {
        return None;
    }
    let raw = unsafe { CStr::from_ptr(info.bus_id.as_ptr()) }.to_string_lossy();
    if raw.is_empty() {
        None
    } else {
        Some(normalize_pci_bus_id(&raw))
    }
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
    use super::{DrmStatus, format_clock_limit, classify_drm_status, normalize_memory_v2, residual_vram, NvmlMemory, NvmlMemoryV2, NvmlPciInfo, NvmlProcessInfo, NvmlProcessUtilizationSample, NvmlUtilization, NVML_CLOCK_EVENT_REASON_GPU_IDLE, NVML_CLOCK_EVENT_REASON_HW_THERMAL, NVML_CLOCK_EVENT_REASON_SW_POWER_CAP};
    use std::mem::{align_of, size_of};

    unsafe extern "C" fn fake_shutdown() -> i32 { 0 }
    unsafe extern "C" fn fake_count(count: *mut u32) -> i32 {
        unsafe { *count = 2; }
        0
    }
    unsafe extern "C" fn fake_handle(index: u32, handle: *mut super::NvmlDevice) -> i32 {
        if index == 0 { return 4; }
        unsafe { *handle = std::ptr::dangling_mut::<std::ffi::c_void>(); }
        0
    }
    unsafe extern "C" fn fake_name(_: super::NvmlDevice, name: *mut std::ffi::c_char, _: u32) -> i32 {
        unsafe { *name = 0; }
        0
    }
    unsafe extern "C" fn fake_utilization(_: super::NvmlDevice, value: *mut NvmlUtilization) -> i32 {
        unsafe { *value = NvmlUtilization::default(); }
        0
    }
    unsafe extern "C" fn fake_memory(_: super::NvmlDevice, value: *mut NvmlMemory) -> i32 {
        unsafe { *value = NvmlMemory { total: 100, free: 90, used: 10 }; }
        0
    }
    unsafe extern "C" fn fake_processes(_: super::NvmlDevice, count: *mut u32, infos: *mut NvmlProcessInfo) -> i32 {
        unsafe { *count = 1; }
        if infos.is_null() { return super::NVML_ERROR_INSUFFICIENT_SIZE; }
        unsafe { *infos = NvmlProcessInfo { pid: std::process::id(), used_gpu_memory: 10, ..NvmlProcessInfo::default() }; }
        0
    }
    unsafe extern "C" fn fake_process_utilization(_: super::NvmlDevice, samples: *mut NvmlProcessUtilizationSample, count: *mut u32, last_seen: u64) -> i32 {
        unsafe { *count = 1; }
        if samples.is_null() { return super::NVML_ERROR_INSUFFICIENT_SIZE; }
        unsafe { *samples = NvmlProcessUtilizationSample { pid: std::process::id(), timestamp: last_seen + 10, sm_util: 42, ..NvmlProcessUtilizationSample::default() }; }
        0
    }

    #[test]
    fn skipped_device_keeps_nvml_index_and_advances_internal_timestamp_slot() {
        let api = super::NvmlApi {
            _library: super::DynamicLibrary { handle: std::ptr::null_mut() },
            shutdown: fake_shutdown,
            get_count: fake_count,
            get_handle_by_index: fake_handle,
            get_name: fake_name,
            get_utilization: fake_utilization,
            get_memory: fake_memory,
            get_memory_v2: None,
            get_pci_info: None,
            get_temperature: None,
            get_power: None,
            get_power_limit: None,
            get_fan: None,
            get_clock: None,
            get_temperature_threshold: None,
            get_clock_event_reasons: None,
            get_encoder_utilization: None,
            get_decoder_utilization: None,
            get_pcie_current_gen: None,
            get_pcie_current_width: None,
            get_pcie_max_gen: None,
            get_pcie_max_width: None,
            get_pcie_throughput: None,
            get_compute_processes: Some(fake_processes),
            get_graphics_processes: None,
            get_process_utilization: Some(fake_process_utilization),
            error_string: None,
        };
        let mut collector = super::NvmlCollector::from_api(api).expect("accessible GPU 1");
        collector.uvm_probe.pids = vec![std::process::id()];
        collector.uvm_last_scan = Some(std::time::Instant::now());
        for timestamp in [10, 20] {
            let snapshot = collector.collect();
            assert_eq!(snapshot.gpus.len(), 1);
            assert_eq!(snapshot.gpus[0].index, 1);
            assert_eq!(snapshot.processes[0].gpu_index, 1);
            assert_eq!(snapshot.processes[0].gpu_util, Some(42));
            assert_eq!(snapshot.vram_holders[0].gpu_indices, vec![1]);
            assert_eq!(collector.process_last_seen, vec![timestamp]);
        }
    }

    #[test]
    fn nvml_ffi_layout_sizes_match_linux_c_abi() {
        assert_eq!(size_of::<NvmlUtilization>(), 8);
        assert_eq!(size_of::<NvmlMemory>(), 24);
        assert_eq!(size_of::<NvmlMemoryV2>(), 40);
        assert_eq!(size_of::<NvmlPciInfo>(), 68);
        assert_eq!(size_of::<NvmlProcessInfo>(), 24);
        assert_eq!(size_of::<NvmlProcessUtilizationSample>(), 32);
        assert_eq!(align_of::<NvmlProcessInfo>(), 8);
        assert_eq!(align_of::<NvmlProcessUtilizationSample>(), 8);
    }


    #[test]
    fn residual_vram_is_allocated_minus_processes() {
        assert_eq!(residual_vram(345, 318), 27);
        assert_eq!(residual_vram(100, 120), 0);
    }

    #[test]
    fn memory_v2_normalizes_drivers_where_reserved_is_separate() {
        // Matches released-driver behaviour reported by nvidia-smi/NVML:
        // total = free + used + reserved.
        let mib = 1024u64 * 1024;
        let memory = NvmlMemoryV2 {
            version: 0,
            total: 6144 * mib,
            reserved: 419 * mib,
            free: 5380 * mib,
            used: 345 * mib,
        };
        let (_, allocated, reserved) = normalize_memory_v2(memory);
        assert_eq!(allocated, 345 * mib);
        assert_eq!(reserved, 419 * mib);
    }

    #[test]
    fn memory_v2_normalizes_drivers_where_used_includes_reserved() {
        let mib = 1024u64 * 1024;
        let memory = NvmlMemoryV2 {
            version: 0,
            total: 6144 * mib,
            reserved: 419 * mib,
            free: 5380 * mib,
            used: 764 * mib,
        };
        let (_, allocated, reserved) = normalize_memory_v2(memory);
        assert_eq!(allocated, 345 * mib);
        assert_eq!(reserved, 419 * mib);
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
    #[test]
    fn classifies_drm_fdinfo_states_without_ambiguous_dash() {
        assert_eq!(classify_drm_status(true, true, false), DrmStatus::Available);
        assert_eq!(classify_drm_status(false, true, false), DrmStatus::Unsupported);
        assert_eq!(classify_drm_status(false, false, true), DrmStatus::PermissionDenied);
        assert_eq!(classify_drm_status(false, false, false), DrmStatus::Unavailable);
        // Usable counters win even when some other fdinfo entries were denied.
        assert_eq!(classify_drm_status(true, true, true), DrmStatus::Available);
    }

}
