use crate::model::Snapshot;
use std::fmt::Write;

fn push_str(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn push_opt_u32(out: &mut String, value: Option<u32>) {
    match value {
        Some(v) => {
            let _ = write!(out, "{v}");
        }
        None => out.push_str("null"),
    }
}

fn push_opt_u64(out: &mut String, value: Option<u64>) {
    match value {
        Some(v) => {
            let _ = write!(out, "{v}");
        }
        None => out.push_str("null"),
    }
}

fn push_opt_str(out: &mut String, value: Option<&str>) {
    match value {
        Some(v) => push_str(out, v),
        None => out.push_str("null"),
    }
}

fn push_opt_u32_fields(out: &mut String, fields: &[(&str, Option<u32>)]) {
    for (key, value) in fields {
        let _ = write!(out, ",\"{key}\":");
        push_opt_u32(out, *value);
    }
}

/// Serialize a snapshot as a single-line JSON object (schema version 1).
pub fn snapshot_to_json(snapshot: &Snapshot) -> String {
    let mut out = String::from("{\"schema\":1,\"gpus\":[");
    for (i, gpu) in snapshot.gpus.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"index\":{},\"name\":", gpu.index);
        push_str(&mut out, &gpu.name);
        out.push_str(",\"gpu_util_percent\":");
        push_opt_u32(&mut out, gpu.gpu_util);
        let _ = write!(
            out,
            ",\"memory_total_bytes\":{},\"memory_used_bytes\":{},\"memory_reserved_bytes\":",
            gpu.memory_total, gpu.memory_used
        );
        push_opt_u64(&mut out, gpu.memory_reserved);
        let _ = write!(
            out,
            ",\"memory_usable_bytes\":{},\"memory_percent\":{:.1},\"memory_processes_bytes\":{}",
            gpu.memory_usable(),
            gpu.memory_percent(),
            gpu.memory_processes
        );
        push_opt_u32_fields(
            &mut out,
            &[
                ("temperature_c", gpu.temperature_c),
                ("temperature_slowdown_c", gpu.temperature_slowdown_c),
                ("temperature_shutdown_c", gpu.temperature_shutdown_c),
                ("temperature_max_operating_c", gpu.temperature_max_operating_c),
                ("power_mw", gpu.power_mw),
                ("power_limit_mw", gpu.power_limit_mw),
                ("fan_percent", gpu.fan_percent),
                ("graphics_clock_mhz", gpu.graphics_clock_mhz),
                ("memory_clock_mhz", gpu.memory_clock_mhz),
            ],
        );
        out.push_str(",\"clock_limit\":");
        push_opt_str(&mut out, gpu.clock_limit.as_deref());
        push_opt_u32_fields(
            &mut out,
            &[
                ("encoder_util_percent", gpu.encoder_util_percent),
                ("decoder_util_percent", gpu.decoder_util_percent),
                ("pcie_tx_kb_per_s", gpu.pcie_tx_kb_per_s),
                ("pcie_rx_kb_per_s", gpu.pcie_rx_kb_per_s),
            ],
        );
        out.push('}');
    }
    out.push_str("],\"processes\":[");
    for (i, p) in snapshot.processes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"gpu_index\":{},\"pid\":{},\"name\":", p.gpu_index, p.pid);
        push_str(&mut out, &p.name);
        out.push_str(",\"user\":");
        push_str(&mut out, &p.user);
        let _ = write!(out, ",\"kind\":\"{}\",\"gpu_util_percent\":", p.kind.short());
        push_opt_u32(&mut out, p.gpu_util);
        out.push_str(",\"memory_bytes\":");
        push_opt_u64(&mut out, p.memory_bytes);
        out.push('}');
    }
    out.push_str("],\"vram_holders\":[");
    for (i, h) in snapshot.vram_holders.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"pid\":{},\"name\":", h.pid);
        push_str(&mut out, &h.name);
        out.push_str(",\"user\":");
        push_str(&mut out, &h.user);
        out.push_str(",\"nvml_memory_bytes\":");
        push_opt_u64(&mut out, h.nvml_memory_bytes);
        out.push_str(",\"gpu_indices\":[");
        for (j, idx) in h.gpu_indices.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            let _ = write!(out, "{idx}");
        }
        out.push_str("]}");
    }
    out.push_str("],\"uvm_module_refcount\":");
    push_opt_u64(&mut out, snapshot.uvm_module_refcount);
    let _ = write!(out, ",\"uvm_scan_partial\":{}}}", snapshot.uvm_scan_partial);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_control_and_quote_characters() {
        let mut out = String::new();
        push_str(&mut out, "a\"b\\c\nd\u{1b}");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\u001b\"");
    }

    #[test]
    fn demo_snapshot_has_expected_shape() {
        let json = snapshot_to_json(&crate::demo::DemoCollector::new().collect());
        assert!(json.starts_with("{\"schema\":1,\"gpus\":[{\"index\":0,"), "{json}");
        assert!(json.contains("\"kind\":\"C\""));
        assert!(json.contains("\"nvml_memory_bytes\":null"));
        assert!(json.ends_with('}'));
        assert!(!json.contains('\n'));
    }
}
