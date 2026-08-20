use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrmClientMemory {
    pub client_id: u64,
    pub resident_device_bytes: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DrmProbe {
    /// DRM clients matched to the requested PCI GPU that exported resident
    /// device-memory counters.
    pub clients: Vec<DrmClientMemory>,
    /// IDs of all DRM clients matched to the requested PCI GPU, including
    /// clients for which the driver exported no memory counters.
    pub matched_client_ids: Vec<u64>,
    /// True when one or more fdinfo entries could not be read because of
    /// permissions. The probe remains useful when other entries were readable.
    pub permission_denied: bool,
}


#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UvmProbe {
    pub pids: Vec<u32>,
    pub module_refcount: Option<u64>,
    pub permission_denied: bool,
}

pub struct Procfs {
    users: HashMap<u32, String>,
}

impl Procfs {
    pub fn new() -> Self {
        Self {
            users: load_users(),
        }
    }

    pub fn metadata(&self, pid: u32) -> (String, String) {
        let user = self
            .uid_for_pid(pid)
            .and_then(|uid| self.users.get(&uid).cloned())
            .unwrap_or_else(|| "?".to_string());
        let name = process_name(pid).unwrap_or_else(|| "?".to_string());
        (user, name)
    }

    /// Best-effort Linux DRM fdinfo accounting for one process on one PCI GPU.
    ///
    /// The kernel fdinfo ABI is optional: a driver may expose no DRM memory
    /// counters at all. We only accept resident *device/local/VRAM* regions and
    /// deliberately ignore `system`, `memory`, `gtt`, and `stolen` regions.
    /// Multiple file descriptors can refer to the same DRM client, so results
    /// are deduplicated by `drm-client-id` and the largest observation wins.
    pub fn drm_clients(&self, pid: u32, pci_bus_id: &str) -> DrmProbe {
        let expected_bdf = normalize_pci_bus_id(pci_bus_id);
        let entries = match fs::read_dir(format!("/proc/{pid}/fdinfo")) {
            Ok(entries) => entries,
            Err(error) => {
                return DrmProbe {
                    permission_denied: error.kind() == std::io::ErrorKind::PermissionDenied,
                    ..DrmProbe::default()
                };
            }
        };

        let mut matched_client_ids = std::collections::BTreeSet::new();
        let mut clients: BTreeMap<u64, u64> = BTreeMap::new();
        let mut permission_denied = false;
        for entry in entries.flatten() {
            let text = match fs::read_to_string(entry.path()) {
                Ok(text) => text,
                Err(error) => {
                    permission_denied |= error.kind() == std::io::ErrorKind::PermissionDenied;
                    continue;
                }
            };
            let Some(record) = parse_drm_fdinfo(&text) else {
                continue;
            };
            let record_bdf = record
                .pci_bus_id
                .as_deref()
                .map(normalize_pci_bus_id)
                .or_else(|| drm_fd_pci_bus_id(pid, &entry.file_name()));
            if record_bdf.as_deref() != Some(expected_bdf.as_str()) {
                continue;
            }
            matched_client_ids.insert(record.client_id);
            let Some(resident) = record.resident_device_bytes else {
                continue;
            };
            clients
                .entry(record.client_id)
                .and_modify(|value| *value = (*value).max(resident))
                .or_insert(resident);
        }

        DrmProbe {
            clients: clients
                .into_iter()
                .map(|(client_id, resident_device_bytes)| DrmClientMemory {
                    client_id,
                    resident_device_bytes,
                })
                .collect(),
            matched_client_ids: matched_client_ids.into_iter().collect(),
            permission_denied,
        }
    }


    /// Find processes that currently hold /dev/nvidia-uvm open.
    ///
    /// NVML per-process framebuffer accounting can miss CUDA/UVM users that
    /// still keep a CUDA context alive. Walking /proc/*/fd gives us an
    /// independent ownership signal without invoking lsof. The scan is
    /// best-effort: unreadable fd directories mark the result as partial.
    pub fn uvm_holders(&self) -> UvmProbe {
        let mut pids = std::collections::BTreeSet::new();
        let mut permission_denied = false;

        let entries = match fs::read_dir("/proc") {
            Ok(entries) => entries,
            Err(_) => return UvmProbe::default(),
        };

        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };

            let fd_entries = match fs::read_dir(format!("/proc/{pid}/fd")) {
                Ok(entries) => entries,
                Err(error) => {
                    permission_denied |= error.kind() == std::io::ErrorKind::PermissionDenied;
                    continue;
                }
            };

            for fd in fd_entries.flatten() {
                match fs::read_link(fd.path()) {
                    Ok(target) if is_nvidia_uvm_target(&target) => {
                        pids.insert(pid);
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        permission_denied |=
                            error.kind() == std::io::ErrorKind::PermissionDenied;
                    }
                }
            }
        }

        let module_refcount = fs::read_to_string("/sys/module/nvidia_uvm/refcnt")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok());

        UvmProbe {
            pids: pids.into_iter().collect(),
            module_refcount,
            permission_denied,
        }
    }

    fn uid_for_pid(&self, pid: u32) -> Option<u32> {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
        None
    }
}

#[derive(Debug)]
struct DrmFdinfoRecord {
    client_id: u64,
    pci_bus_id: Option<String>,
    resident_device_bytes: Option<u64>,
}

fn parse_drm_fdinfo(text: &str) -> Option<DrmFdinfoRecord> {
    let mut client_id = None;
    let mut pci_bus_id = None;
    let mut resident_by_region: BTreeMap<String, u64> = BTreeMap::new();
    let mut legacy_memory_by_region: BTreeMap<String, u64> = BTreeMap::new();

    for line in text.lines() {
        let Some((raw_key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let key = raw_key.trim();
        let value = raw_value.trim();

        match key {
            "drm-client-id" => client_id = value.split_whitespace().next()?.parse::<u64>().ok(),
            "drm-pdev" => pci_bus_id = Some(value.to_string()),
            _ => {
                if let Some(region) = key.strip_prefix("drm-resident-") {
                    if is_device_memory_region(region) {
                        if let Some(bytes) = parse_memory_value(value) {
                            resident_by_region.insert(region.to_string(), bytes);
                        }
                    }
                } else if let Some(region) = key.strip_prefix("drm-memory-") {
                    // Deprecated alias used by some drivers. Prefer the
                    // `drm-resident-*` value when both are present.
                    if is_device_memory_region(region) {
                        if let Some(bytes) = parse_memory_value(value) {
                            legacy_memory_by_region.insert(region.to_string(), bytes);
                        }
                    }
                }
            }
        }
    }

    let mut resident = 0u64;
    let mut any = false;
    for (region, bytes) in &resident_by_region {
        resident = resident.saturating_add(*bytes);
        any = true;
        legacy_memory_by_region.remove(region);
    }
    // If a driver only exposes the deprecated key for a region, still use it.
    for bytes in legacy_memory_by_region.values() {
        resident = resident.saturating_add(*bytes);
        any = true;
    }

    Some(DrmFdinfoRecord {
        client_id: client_id?,
        pci_bus_id,
        resident_device_bytes: any.then_some(resident),
    })
}

fn drm_fd_pci_bus_id(pid: u32, fd_name: &std::ffi::OsStr) -> Option<String> {
    let fd_target = fs::read_link(Path::new(&format!("/proc/{pid}/fd")).join(fd_name)).ok()?;
    let node = fd_target.file_name()?.to_str()?;
    if !(node.starts_with("card") || node.starts_with("renderD")) {
        return None;
    }
    let device = fs::canonicalize(Path::new("/sys/class/drm").join(node).join("device")).ok()?;
    let bdf = device.file_name()?.to_str()?;
    Some(normalize_pci_bus_id(bdf))
}

fn is_device_memory_region(region: &str) -> bool {
    let region = region.to_ascii_lowercase();
    if matches!(region.as_str(), "system" | "memory" | "gtt" | "stolen") {
        return false;
    }
    region.contains("vram")
        || region.contains("local")
        || region.contains("device")
        || region.contains("fb")
}

fn parse_memory_value(value: &str) -> Option<u64> {
    let mut fields = value.split_whitespace();
    let number = fields.next()?.parse::<u64>().ok()?;
    let unit = fields.next().unwrap_or("B");
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "b" | "bytes" => 1u64,
        "kb" | "kib" => 1024u64,
        "mb" | "mib" => 1024u64.pow(2),
        "gb" | "gib" => 1024u64.pow(3),
        _ => return None,
    };
    number.checked_mul(multiplier)
}

pub fn normalize_pci_bus_id(value: &str) -> String {
    // NVML commonly uses 8 hex digits for the domain while Linux DRM fdinfo
    // commonly uses 4. Parse the tuple and normalize both forms to 4:2:2.1.
    let lower = value.trim().to_ascii_lowercase();
    let mut parts = lower.split(':');
    let Some(domain) = parts.next() else { return lower };
    let Some(bus) = parts.next() else { return lower };
    let Some(device_function) = parts.next() else { return lower };
    if parts.next().is_some() {
        return lower;
    }
    let Some((device, function)) = device_function.split_once('.') else {
        return lower;
    };
    let Ok(domain) = u32::from_str_radix(domain, 16) else { return lower };
    let Ok(bus) = u32::from_str_radix(bus, 16) else { return lower };
    let Ok(device) = u32::from_str_radix(device, 16) else { return lower };
    let Ok(function) = u32::from_str_radix(function, 16) else { return lower };
    format!("{domain:04x}:{bus:02x}:{device:02x}.{function:x}")
}


fn is_nvidia_uvm_target(target: &Path) -> bool {
    target == Path::new("/dev/nvidia-uvm")
}

fn load_users() -> HashMap<u32, String> {
    let mut users = HashMap::new();
    let Ok(passwd) = fs::read_to_string("/etc/passwd") else {
        return users;
    };

    for line in passwd.lines() {
        let mut fields = line.split(':');
        let Some(name) = fields.next() else { continue };
        let _password = fields.next();
        let Some(uid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        users.insert(uid, name.to_string());
    }
    users
}

fn process_name(pid: u32) -> Option<String> {
    if let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) {
        if !cmdline.is_empty() {
            let args: Vec<&[u8]> = cmdline
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .collect();
            if let Some(first) = args.first() {
                let first_text = String::from_utf8_lossy(first);
                let executable = basename(first_text.as_ref());
                if executable == "python" || executable.starts_with("python3") {
                    if let Some(second) = args.get(1) {
                        let second_text = String::from_utf8_lossy(second);
                        let script = basename(second_text.as_ref());
                        if !script.is_empty() {
                            return Some(format!("{executable} {script}"));
                        }
                    }
                }
                if !executable.is_empty() {
                    return Some(executable);
                }
            }
        }
    }

    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

fn basename(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{basename, is_nvidia_uvm_target, normalize_pci_bus_id, parse_drm_fdinfo, parse_memory_value};

    #[test]
    fn recognizes_only_the_nvidia_uvm_device() {
        assert!(is_nvidia_uvm_target(std::path::Path::new("/dev/nvidia-uvm")));
        assert!(!is_nvidia_uvm_target(std::path::Path::new("/dev/nvidia0")));
        assert!(!is_nvidia_uvm_target(std::path::Path::new("/dev/nvidia-uvm-tools")));
    }

    #[test]
    fn extracts_basename() {
        assert_eq!(basename("/usr/bin/python3"), "python3");
        assert_eq!(basename("firefox"), "firefox");
    }

    #[test]
    fn normalizes_nvml_and_drm_pci_domains() {
        assert_eq!(normalize_pci_bus_id("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalize_pci_bus_id("0000:01:00.0"), "0000:01:00.0");
    }

    #[test]
    fn parses_memory_units() {
        assert_eq!(parse_memory_value("512 KiB"), Some(512 * 1024));
        assert_eq!(parse_memory_value("2 MiB"), Some(2 * 1024 * 1024));
        assert_eq!(parse_memory_value("4096"), Some(4096));
    }

    #[test]
    fn parses_and_filters_drm_resident_device_memory() {
        let text = r#"
            drm-driver: nvidia-drm
            drm-client-id: 17
            drm-pdev: 0000:01:00.0
            drm-resident-vram0: 200 MiB
            drm-resident-system: 50 MiB
            drm-total-vram0: 300 MiB
        "#;
        let record = parse_drm_fdinfo(text).expect("record");
        assert_eq!(record.client_id, 17);
        assert_eq!(record.pci_bus_id.as_deref(), Some("0000:01:00.0"));
        assert_eq!(record.resident_device_bytes, Some(200 * 1024 * 1024));
    }

    #[test]
    fn prefers_resident_over_legacy_alias_for_same_region() {
        let text = r#"
            drm-client-id: 3
            drm-pdev: 0000:01:00.0
            drm-resident-vram: 80 MiB
            drm-memory-vram: 90 MiB
        "#;
        let record = parse_drm_fdinfo(text).expect("record");
        assert_eq!(record.resident_device_bytes, Some(80 * 1024 * 1024));
    }
    #[test]
    fn accepts_drm_fdinfo_without_pdev_for_sysfs_fallback() {
        let text = r#"
            drm-driver: nvidia-drm
            drm-client-id: 9
            drm-memory-vram: 12 MiB
        "#;
        let record = parse_drm_fdinfo(text).expect("record");
        assert_eq!(record.client_id, 9);
        assert_eq!(record.pci_bus_id, None);
        assert_eq!(record.resident_device_bytes, Some(12 * 1024 * 1024));
    }

}
