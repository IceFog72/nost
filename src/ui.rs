use crate::model::{GpuSnapshot, ProcessSnapshot, Snapshot, VramHolderSnapshot};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::{c_int, c_ulong};
use std::fmt::Write as FmtWrite;
use std::io::{self, Write};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

const ACCENT: Rgb = Rgb(98, 255, 118);
const TEXT: Rgb = Rgb(218, 223, 232);
const MUTED: Rgb = Rgb(116, 124, 139);
const BORDER: Rgb = Rgb(72, 91, 118);
const EMPTY: Rgb = Rgb(48, 54, 66);
const CYAN: Rgb = Rgb(63, 218, 230);
const BLUE: Rgb = Rgb(94, 168, 255);
const AMBER: Rgb = Rgb(255, 191, 74);

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

#[derive(Clone, Copy)]
enum Grade {
    Heat,
    Memory,
}

pub const INPUT_MODES_OFF: &str = concat!(
    "\x1b[?1007l", // alternate-scroll -> cursor keys
    "\x1b[?1000l", // X10 mouse reporting
    "\x1b[?1002l", // button-event tracking
    "\x1b[?1003l", // any-event tracking
    "\x1b[?1005l", // UTF-8 mouse encoding
    "\x1b[?1006l", // SGR mouse encoding
    "\x1b[?1015l", // urxvt mouse encoding
    "\x1b[?1016l"  // SGR pixel mouse encoding
);

#[repr(C)]
#[derive(Default)]
struct WinSize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

const STDOUT_FILENO: c_int = 1;
const TIOCGWINSZ: c_ulong = 0x5413;

pub struct TerminalGuard {
    inline: bool,
}

impl TerminalGuard {
    pub fn enter(inline: bool) -> io::Result<Self> {
        let mut out = io::stdout().lock();
        if !inline {
            write!(out, "\x1b[?1049h")?;
        }
        // Disable alternate-scroll *after* entering the alternate screen.
        // This stops wheel events being translated into ESC[A / ESC[B while
        // nost itself remains completely non-interactive.
        write!(out, "{}\x1b[?25l\x1b[2J\x1b[H", INPUT_MODES_OFF)?;
        out.flush()?;
        Ok(Self { inline })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout().lock();
        let _ = write!(out, "{}\x1b[0m\x1b[?25h", INPUT_MODES_OFF);
        if !self.inline {
            let _ = write!(out, "\x1b[?1049l");
        } else {
            let _ = writeln!(out);
        }
        let _ = out.flush();
    }
}

pub struct History {
    max_len: usize,
    gpu: HashMap<u32, VecDeque<f64>>,
    memory: HashMap<u32, VecDeque<f64>>,
}

impl History {
    pub fn new(max_len: usize) -> Self {
        Self {
            max_len,
            gpu: HashMap::new(),
            memory: HashMap::new(),
        }
    }

    pub fn update(&mut self, snapshot: &Snapshot) {
        for gpu in &snapshot.gpus {
            push_bounded(
                self.gpu.entry(gpu.index).or_default(),
                gpu.gpu_util as f64,
                self.max_len,
            );
            push_bounded(
                self.memory.entry(gpu.index).or_default(),
                gpu.memory_percent(),
                self.max_len,
            );
        }
    }

    fn gpu(&self, index: u32) -> Option<&VecDeque<f64>> {
        self.gpu.get(&index)
    }

    fn memory(&self, index: u32) -> Option<&VecDeque<f64>> {
        self.memory.get(&index)
    }
}

fn push_bounded(values: &mut VecDeque<f64>, value: f64, max_len: usize) {
    if values.len() >= max_len {
        values.pop_front();
    }
    values.push_back(value);
}

pub fn render(
    snapshot: &Snapshot,
    history: &History,
    _interval_ms: u64,
    _backend_label: &str,
    color: bool,
) -> io::Result<()> {
    let (terminal_width, terminal_height) = terminal_size();
    let terminal_width = terminal_width as usize;
    let terminal_height = terminal_height as usize;
    // Leave the last physical column alone. A number of terminal emulators
    // auto-wrap when a printable glyph touches that column.
    let width = terminal_width.saturating_sub(1);
    let height = terminal_height;
    let truecolor = supports_truecolor();

    let mut frame = String::with_capacity(width.saturating_mul(height).saturating_mul(4));
    frame.push_str(INPUT_MODES_OFF);
    frame.push_str("\x1b[H");

    if width < 64 || height < 27 {
        let message = format!(
            "nost  resize terminal ({}x{}, need >= 65x27)",
            terminal_width, terminal_height
        );
        let message = truncate(&message, width.saturating_sub(1));
        if color {
            let _ = write!(
                frame,
                "{BOLD}{}{message}{RESET}",
                fg(ACCENT, truecolor)
            );
        } else {
            frame.push_str(&message);
        }
        frame.push_str("\x1b[J");
        return flush_frame(frame);
    }

    // Keep enough room for the two lower boxes even when several GPUs exist.
    // GPU panel: top + 2 meters + 4-row GPU history + 4-row MEM history +
    // 4 metric rows + bottom = 16 physical lines. The third box is a direct
    // /dev/nvidia-uvm ownership view for processes that can retain CUDA/UVM
    // state without showing attributable VRAM in NVML's process table.
    let gpu_panel_lines = 16usize;
    let process_min_lines = 5usize;
    let holders_min_lines = 6usize;
    let fixed_lines = process_min_lines + holders_min_lines;
    let max_gpu_panels = height
        .saturating_sub(fixed_lines)
        .checked_div(gpu_panel_lines)
        .unwrap_or(0)
        .max(1);

    for gpu in snapshot.gpus.iter().take(max_gpu_panels) {
        push_gpu_panel(&mut frame, gpu, history, width, color, truecolor);
    }

    if snapshot.gpus.len() > max_gpu_panels {
        let hidden = snapshot.gpus.len() - max_gpu_panels;
        push_plain_line(
            &mut frame,
            &format!("  … {hidden} more GPU{} hidden", if hidden == 1 { "" } else { "s" }),
            width,
            MUTED,
            color,
            truecolor,
        );
    }

    let lines_before_process = count_newlines(&frame);
    let remaining = height.saturating_sub(lines_before_process);
    let process_budget = remaining.saturating_sub(holders_min_lines);
    if process_budget >= 4 {
        push_process_panel(
            &mut frame,
            &snapshot.processes,
            width,
            process_budget,
            color,
            truecolor,
        );
    }

    let lines_before_holders = count_newlines(&frame);
    let holder_budget = height.saturating_sub(lines_before_holders);
    if holder_budget >= 5 {
        push_vram_holders_panel(
            &mut frame,
            snapshot,
            width,
            holder_budget,
            color,
            truecolor,
        );
    }

    frame.push_str("\x1b[J");
    flush_frame(frame)
}

fn flush_frame(frame: String) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(frame.as_bytes())?;
    out.flush()
}

fn push_gpu_panel(
    frame: &mut String,
    gpu: &GpuSnapshot,
    history: &History,
    width: usize,
    color: bool,
    truecolor: bool,
) {
    let title_plain = format!("GPU {}  {}", gpu.index, truncate(&gpu.name, width.saturating_sub(16)));
    let title_styled = if color {
        format!(
            "{BOLD}{}GPU {}{RESET}  {}{}{RESET}",
            fg(ACCENT, truecolor),
            gpu.index,
            fg(TEXT, truecolor),
            truncate(&gpu.name, width.saturating_sub(16))
        )
    } else {
        title_plain.clone()
    };
    push_panel_top(frame, &title_plain, &title_styled, width, color, truecolor);

    let inner = width.saturating_sub(2);
    let bar_width = ((inner * 44) / 100).clamp(18, 42);

    let gpu_bar = graded_bar(gpu.gpu_util as f64, bar_width, Grade::Heat, color, truecolor);
    let mem_bar = graded_bar(gpu.memory_percent(), bar_width, Grade::Memory, color, truecolor);

    let gpu_left = if color {
        format!(
            " GPU  {gpu_bar}  {}{:>3}%{RESET}",
            maybe_grade(gpu.gpu_util as f64, Grade::Heat, color, truecolor),
            gpu.gpu_util
        )
    } else {
        format!(" GPU  {gpu_bar}  {:>3}%", gpu.gpu_util)
    };
    let temp_plain = gpu
        .temperature_c
        .map(|v| format!("{v}°C"))
        .unwrap_or_else(|| "--".into());
    let temp_styled = gpu.temperature_c.map_or_else(
        || style("--", MUTED, color, truecolor),
        |v| {
            style(
                &format!("{v}°C"),
                grade_rgb(temperature_grade(gpu), Grade::Heat),
                color,
                truecolor,
            )
        },
    );
    push_panel_lr(frame, &gpu_left, &temp_plain, &temp_styled, inner, color, truecolor);

    let mem_left = if color {
        format!(
            " MEM  {mem_bar}  {}{:>3.0}%{RESET}",
            maybe_grade(gpu.memory_percent(), Grade::Memory, color, truecolor),
            gpu.memory_percent()
        )
    } else {
        format!(" MEM  {mem_bar}  {:>3.0}%", gpu.memory_percent())
    };
    let mem_plain = format_memory_pair_pretty(gpu.memory_used, gpu.memory_total);
    let mem_styled = style(&mem_plain, CYAN, color, truecolor);
    push_panel_lr(frame, &mem_left, &mem_plain, &mem_styled, inner, color, truecolor);

    // History graphs are vertically graded like btop: color is tied to the
    // utilization row (low rows cool, high rows hot), not to each time column.
    // This makes a spike climb through green/yellow/orange/red bands instead of
    // painting the whole spike a single severity color.
    let graph_width = inner.saturating_sub(8).max(12);
    push_history_graph(
        frame,
        "gpu",
        history.gpu(gpu.index),
        graph_width,
        4,
        Grade::Heat,
        inner,
        color,
        truecolor,
    );
    push_history_graph(
        frame,
        "mem",
        history.memory(gpu.index),
        graph_width,
        4,
        Grade::Memory,
        inner,
        color,
        truecolor,
    );

    let temp_metric = metric(
        "temperature",
        &temp_plain,
        gpu.temperature_c
            .map(|_| grade_rgb(temperature_grade(gpu), Grade::Heat))
            .unwrap_or(MUTED),
        color,
        truecolor,
    );
    let power_plain = match (gpu.power_mw, gpu.power_limit_mw) {
        (Some(now), Some(limit)) => format!("{:.0}W / {:.0}W", now as f64 / 1000.0, limit as f64 / 1000.0),
        (Some(now), None) => format!("{:.0}W", now as f64 / 1000.0),
        _ => "--".to_string(),
    };
    let power_percent = match (gpu.power_mw, gpu.power_limit_mw) {
        (Some(now), Some(limit)) if limit > 0 => now as f64 * 100.0 / limit as f64,
        _ => 0.0,
    };
    let power_metric = metric(
        "power",
        &power_plain,
        grade_rgb(power_percent, Grade::Heat),
        color,
        truecolor,
    );
    let fan_plain = gpu
        .fan_percent
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "--".into());
    let fan_metric = metric("fan", &fan_plain, BLUE, color, truecolor);
    push_panel_columns(frame, &[temp_metric, power_metric, fan_metric], inner, color, truecolor);

    let core_plain = gpu
        .graphics_clock_mhz
        .map(|v| format!("{v} MHz"))
        .unwrap_or_else(|| "--".into());
    let memory_clock_plain = gpu
        .memory_clock_mhz
        .map(|v| format!("{v} MHz"))
        .unwrap_or_else(|| "--".into());
    let vram_plain = format_memory_pair_compact(gpu.memory_used, gpu.memory_total);
    let core_metric = metric("core clock", &core_plain, TEXT, color, truecolor);
    let memory_metric = metric("mem clock", &memory_clock_plain, TEXT, color, truecolor);
    let vram_metric = metric("vram", &vram_plain, CYAN, color, truecolor);
    push_panel_columns(frame, &[core_metric, memory_metric, vram_metric], inner, color, truecolor);

    let limit_plain = gpu.clock_limit.as_deref().unwrap_or("--");
    let limit_display = truncate(limit_plain, 12);
    let limit_metric = metric(
        "limit",
        &limit_display,
        clock_limit_color(limit_plain),
        color,
        truecolor,
    );
    let video_metric = video_metric(
        gpu.encoder_util_percent,
        gpu.decoder_util_percent,
        color,
        truecolor,
    );
    let thermal_plain = format_temperature_thresholds(gpu);
    let thermal_metric = metric("thermal", &thermal_plain, temperature_limit_color(gpu), color, truecolor);
    push_panel_columns(frame, &[limit_metric, video_metric, thermal_metric], inner, color, truecolor);

    let pcie_current = format_pcie_link(gpu.pcie_current_gen, gpu.pcie_current_width);
    let pcie_max = format_pcie_link(gpu.pcie_max_gen, gpu.pcie_max_width);
    let pcie_plain = if pcie_current == "--" && pcie_max == "--" {
        "--".to_string()
    } else if pcie_max == "--" {
        pcie_current
    } else {
        format!("{pcie_current} / {pcie_max}")
    };
    let pcie_metric = metric("pcie", &pcie_plain, BLUE, color, truecolor);
    let traffic_metric = traffic_metric(
        gpu.pcie_tx_kb_per_s,
        gpu.pcie_rx_kb_per_s,
        color,
        truecolor,
    );
    push_panel_columns(frame, &[pcie_metric, traffic_metric], inner, color, truecolor);

    push_panel_bottom(frame, width, color, truecolor);
}

fn temperature_grade(gpu: &GpuSnapshot) -> f64 {
    let Some(current) = gpu.temperature_c else {
        return 0.0;
    };
    let limit = gpu.temperature_limit_c().unwrap_or(90).max(45);
    let floor = 30u32.min(limit.saturating_sub(1));
    let span = limit.saturating_sub(floor).max(1) as f64;
    ((current.saturating_sub(floor) as f64 / span) * 100.0).clamp(0.0, 100.0)
}

fn temperature_limit_color(gpu: &GpuSnapshot) -> Rgb {
    if gpu.temperature_limit_c().is_some() {
        grade_rgb(72.0, Grade::Heat)
    } else {
        MUTED
    }
}

fn clock_limit_color(limit: &str) -> Rgb {
    if limit == "--" {
        MUTED
    } else if limit == "none" {
        ACCENT
    } else if limit == "idle" {
        BLUE
    } else if limit.contains("thermal")
        || limit.contains("power brake")
        || limit.contains("hw slowdown")
    {
        grade_rgb(95.0, Grade::Heat)
    } else if limit.contains("power") {
        grade_rgb(78.0, Grade::Heat)
    } else {
        grade_rgb(62.0, Grade::Heat)
    }
}

fn video_metric(
    encoder: Option<u32>,
    decoder: Option<u32>,
    color: bool,
    truecolor: bool,
) -> Metric {
    let enc = encoder
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "--".to_string());
    let dec = decoder
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "--".to_string());
    let plain = format!("video  ENC {enc}  DEC {dec}");
    let styled = if color {
        let enc_color = encoder
            .map(|v| grade_rgb(v as f64, Grade::Memory))
            .unwrap_or(MUTED);
        let dec_color = decoder
            .map(|v| grade_rgb(v as f64, Grade::Memory))
            .unwrap_or(MUTED);
        format!(
            "{}{label}{RESET}  {}ENC{RESET} {}{enc}{RESET}  {}DEC{RESET} {}{dec}{RESET}",
            fg(MUTED, truecolor),
            fg(MUTED, truecolor),
            fg(enc_color, truecolor),
            fg(MUTED, truecolor),
            fg(dec_color, truecolor),
            label = "video",
        )
    } else {
        plain.clone()
    };
    Metric { plain, styled }
}

fn format_temperature_thresholds(gpu: &GpuSnapshot) -> String {
    let operating = gpu
        .temperature_max_operating_c
        .or(gpu.temperature_slowdown_c);
    match (operating, gpu.temperature_shutdown_c) {
        (Some(limit), Some(shutdown)) if limit != shutdown => format!("{limit}/{shutdown}°C"),
        (Some(limit), _) => format!("{limit}°C"),
        (None, Some(shutdown)) => format!("--/{shutdown}°C"),
        (None, None) => "--".to_string(),
    }
}

fn format_pcie_link(generation: Option<u32>, width: Option<u32>) -> String {
    match (generation.filter(|v| *v > 0), width.filter(|v| *v > 0)) {
        (Some(generation), Some(width)) => format!("Gen{generation} x{width}"),
        (Some(generation), None) => format!("Gen{generation}"),
        (None, Some(width)) => format!("x{width}"),
        (None, None) => "--".to_string(),
    }
}

fn format_pcie_rate(kb_per_s: u32) -> String {
    let value = kb_per_s as f64;
    if value >= 1_000_000.0 {
        format!("{:.1} GB/s", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.0} MB/s", value / 1_000.0)
    } else {
        format!("{kb_per_s} KB/s")
    }
}

fn format_pcie_traffic(tx: Option<u32>, rx: Option<u32>) -> String {
    let tx = tx.map(format_pcie_rate).unwrap_or_else(|| "--".to_string());
    let rx = rx.map(format_pcie_rate).unwrap_or_else(|| "--".to_string());
    format!("TX {tx}  RX {rx}")
}

fn traffic_metric(
    tx: Option<u32>,
    rx: Option<u32>,
    color: bool,
    truecolor: bool,
) -> Metric {
    let tx_value = tx.map(format_pcie_rate).unwrap_or_else(|| "--".to_string());
    let rx_value = rx.map(format_pcie_rate).unwrap_or_else(|| "--".to_string());
    let plain = format!("traffic  TX {tx_value}  RX {rx_value}");
    let styled = if color {
        format!(
            "{}{label}{RESET}  {}TX{RESET} {}{tx_value}{RESET}  {}RX{RESET} {}{rx_value}{RESET}",
            fg(MUTED, truecolor),
            fg(BLUE, truecolor),
            fg(CYAN, truecolor),
            fg(BLUE, truecolor),
            fg(CYAN, truecolor),
            label = "traffic",
        )
    } else {
        plain.clone()
    };
    Metric { plain, styled }
}

#[derive(Clone)]
struct Metric {
    plain: String,
    styled: String,
}

fn metric(
    label: &str,
    value: &str,
    value_color: Rgb,
    color: bool,
    truecolor: bool,
) -> Metric {
    let plain = format!("{label}  {value}");
    let styled = if color {
        format!(
            "{}{}{RESET}  {}{}{RESET}",
            fg(MUTED, truecolor),
            label,
            fg(value_color, truecolor),
            value
        )
    } else {
        plain.clone()
    };
    Metric { plain, styled }
}

fn push_panel_columns(
    frame: &mut String,
    metrics: &[Metric],
    inner: usize,
    color: bool,
    truecolor: bool,
) {
    let count = metrics.len().max(1);
    let base = inner / count;
    let mut line = String::new();
    for (index, item) in metrics.iter().enumerate() {
        let width = if index + 1 == count {
            inner.saturating_sub(base * index)
        } else {
            base
        };
        let plain_len = item.plain.chars().count();
        line.push(' ');
        line.push_str(&item.styled);
        let used = plain_len + 1;
        if width > used {
            line.push_str(&" ".repeat(width - used));
        }
    }
    push_panel_line(frame, &line, inner, color, truecolor);
}

fn push_process_panel(
    frame: &mut String,
    processes: &[ProcessSnapshot],
    width: usize,
    available_lines: usize,
    color: bool,
    truecolor: bool,
) {
    let title_plain = format!("processes  {} active", processes.len());
    let title_styled = if color {
        format!(
            "{BOLD}{}processes{RESET}  {}{} active{RESET}",
            fg(ACCENT, truecolor),
            fg(MUTED, truecolor),
            processes.len()
        )
    } else {
        title_plain.clone()
    };
    push_panel_top(frame, &title_plain, &title_styled, width, color, truecolor);

    let inner = width.saturating_sub(2);
    let columns = process_columns(inner);
    let header = process_header(&columns, color, truecolor);
    push_panel_line(frame, &header, inner, color, truecolor);

    // top + header + bottom consume 3 physical lines.
    let rows_available = available_lines.saturating_sub(3);
    let data_rows = if processes.len() > rows_available && rows_available > 0 {
        rows_available.saturating_sub(1)
    } else {
        rows_available
    };

    for process in processes.iter().take(data_rows) {
        let line = process_line(process, &columns, color, truecolor);
        push_panel_line(frame, &line, inner, color, truecolor);
    }

    if processes.len() > data_rows && rows_available > data_rows {
        let hidden = processes.len() - data_rows;
        let plain = format!(" … {hidden} more processes");
        let line = style(&plain, MUTED, color, truecolor);
        push_panel_line(frame, &line, inner, color, truecolor);
    }

    push_panel_bottom(frame, width, color, truecolor);
}

fn push_vram_holders_panel(
    frame: &mut String,
    snapshot: &Snapshot,
    width: usize,
    available_lines: usize,
    color: bool,
    truecolor: bool,
) {
    let holder_count = snapshot.vram_holders.len();
    let title_plain = format!("vram holders  {holder_count} UVM");
    let title_styled = if color {
        format!(
            "{BOLD}{}vram holders{RESET}  {}{holder_count} UVM{RESET}",
            fg(ACCENT, truecolor),
            fg(MUTED, truecolor),
        )
    } else {
        title_plain.clone()
    };
    push_panel_top(frame, &title_plain, &title_styled, width, color, truecolor);

    let inner = width.saturating_sub(2);
    let unattributed = snapshot
        .gpus
        .iter()
        .map(|gpu| gpu.memory_used.saturating_sub(gpu.memory_processes))
        .fold(0u64, u64::saturating_add);
    let refs = snapshot
        .uvm_module_refcount
        .map(|value| value.to_string())
        .unwrap_or_else(|| "--".to_string());
    let scan = if snapshot.uvm_scan_partial { "partial" } else { "complete" };
    let summary_plain = format!(
        " unattributed {}   UVM module refs {refs}   scan {scan}",
        format_memory_pretty(unattributed)
    );
    let summary = if color {
        format!(
            " {}unattributed{RESET} {}{}{RESET}   {}UVM module refs{RESET} {}{refs}{RESET}   {}scan{RESET} {}{}{RESET}",
            fg(MUTED, truecolor),
            fg(AMBER, truecolor),
            format_memory_pretty(unattributed),
            fg(MUTED, truecolor),
            fg(TEXT, truecolor),
            fg(MUTED, truecolor),
            fg(if snapshot.uvm_scan_partial { AMBER } else { ACCENT }, truecolor),
            scan,
        )
    } else {
        summary_plain.clone()
    };
    push_panel_line(frame, &summary, inner, color, truecolor);

    let columns = holder_columns(inner);
    let header = holder_header(&columns, color, truecolor);
    push_panel_line(frame, &header, inner, color, truecolor);

    // top + summary + header + bottom consume four lines.
    let rows_available = available_lines.saturating_sub(4);
    let data_rows = if snapshot.vram_holders.len() > rows_available && rows_available > 0 {
        rows_available.saturating_sub(1)
    } else {
        rows_available
    };

    for holder in snapshot.vram_holders.iter().take(data_rows) {
        let line = holder_line(holder, &columns, color, truecolor);
        push_panel_line(frame, &line, inner, color, truecolor);
    }

    if snapshot.vram_holders.is_empty() && rows_available > 0 {
        let plain = " no visible /dev/nvidia-uvm holders";
        push_panel_line(
            frame,
            &style(plain, MUTED, color, truecolor),
            inner,
            color,
            truecolor,
        );
    } else if snapshot.vram_holders.len() > data_rows && rows_available > data_rows {
        let hidden = snapshot.vram_holders.len() - data_rows;
        let plain = format!(" … {hidden} more UVM holder{}", if hidden == 1 { "" } else { "s" });
        push_panel_line(
            frame,
            &style(&plain, MUTED, color, truecolor),
            inner,
            color,
            truecolor,
        );
    }

    push_panel_bottom(frame, width, color, truecolor);
}

struct HolderColumns {
    pid: usize,
    user: usize,
    gpu: usize,
    name: usize,
    memory: usize,
}

fn holder_columns(inner: usize) -> HolderColumns {
    let pid = 10;
    let user = 13;
    let gpu = 9;
    let memory = 13;
    let fixed = pid + user + gpu + memory;
    let name = inner.saturating_sub(fixed).max(10);
    HolderColumns { pid, user, gpu, name, memory }
}

fn holder_header(c: &HolderColumns, color: bool, truecolor: bool) -> String {
    let plain = format!(
        " {:<pw$}{:<uw$}{:<gw$}{:<nw$}{:>mw$}",
        "PID", "USER", "GPU", "PROCESS", "NVML VRAM",
        pw = c.pid.saturating_sub(1),
        uw = c.user,
        gw = c.gpu,
        nw = c.name,
        mw = c.memory,
    );
    style(&plain, MUTED, color, truecolor)
}

fn holder_line(h: &VramHolderSnapshot, c: &HolderColumns, color: bool, truecolor: bool) -> String {
    let user = truncate(&h.user, c.user.saturating_sub(1));
    let name = truncate(&h.name, c.name.saturating_sub(1));
    let gpu_plain = if h.gpu_indices.is_empty() {
        "--".to_string()
    } else {
        h.gpu_indices
            .iter()
            .map(|gpu| gpu.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let gpu_plain = truncate(&gpu_plain, c.gpu.saturating_sub(1));
    let memory_plain = match h.nvml_memory_bytes {
        Some(0) => "0 MiB".to_string(),
        Some(bytes) => format_memory_pretty(bytes),
        None => "--".to_string(),
    };

    if !color {
        return format!(
            " {:<pw$}{:<uw$}{:<gw$}{:<nw$}{:>mw$}",
            h.pid,
            user,
            gpu_plain,
            name,
            memory_plain,
            pw = c.pid.saturating_sub(1),
            uw = c.user,
            gw = c.gpu,
            nw = c.name,
            mw = c.memory,
        );
    }

    let pid_field = format!(" {:<pw$}", h.pid, pw = c.pid.saturating_sub(1));
    let user_field = format!("{:<uw$}", user, uw = c.user);
    let gpu_field = format!("{:<gw$}", gpu_plain, gw = c.gpu);
    let name_field = format!("{:<nw$}", name, nw = c.name);
    let memory_field = format!("{:>mw$}", memory_plain, mw = c.memory);
    let memory_color = if h.nvml_memory_bytes.is_some() { CYAN } else { AMBER };

    format!(
        "{}{}{RESET}{}{}{RESET}{}{}{RESET}{}{}{RESET}{}{}{RESET}",
        fg(MUTED, truecolor), pid_field,
        fg(TEXT, truecolor), user_field,
        fg(BLUE, truecolor), gpu_field,
        fg(TEXT, truecolor), name_field,
        fg(memory_color, truecolor), memory_field,
    )
}

struct ProcessColumns {
    gpu: usize,
    pid: usize,
    user: usize,
    kind: usize,
    name: usize,
    util: usize,
    memory: usize,
}

fn process_columns(inner: usize) -> ProcessColumns {
    let gpu = 5;
    let pid = 9;
    let user = 12;
    let kind = 6;
    let util = 7;
    let memory = 12;
    let fixed = gpu + pid + user + kind + util + memory;
    let name = inner.saturating_sub(fixed).max(8);
    ProcessColumns {
        gpu,
        pid,
        user,
        kind,
        name,
        util,
        memory,
    }
}

fn process_header(c: &ProcessColumns, color: bool, truecolor: bool) -> String {
    let plain = format!(
        "{:<gw$}{:<pw$}{:<uw$}{:<kw$}{:<nw$}{:>xw$}{:>mw$}",
        " GPU", "PID", "USER", "TYPE", "PROCESS", "GPU%", "VRAM",
        gw = c.gpu,
        pw = c.pid,
        uw = c.user,
        kw = c.kind,
        nw = c.name,
        xw = c.util,
        mw = c.memory,
    );
    style(&plain, MUTED, color, truecolor)
}

fn process_line(p: &ProcessSnapshot, c: &ProcessColumns, color: bool, truecolor: bool) -> String {
    let name = truncate(&p.name, c.name.saturating_sub(1));
    let user = truncate(&p.user, c.user.saturating_sub(1));
    let util_plain = p
        .gpu_util
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "--".to_string());
    let memory_plain = p
        .memory_bytes
        .map(format_memory_pretty)
        .unwrap_or_else(|| "--".to_string());

    if !color {
        return format!(
            "{:<gw$}{:<pw$}{:<uw$}{:<kw$}{:<nw$}{:>xw$}{:>mw$}",
            format!(" {}", p.gpu_index),
            p.pid,
            user,
            p.kind.short(),
            name,
            util_plain,
            memory_plain,
            gw = c.gpu,
            pw = c.pid,
            uw = c.user,
            kw = c.kind,
            nw = c.name,
            xw = c.util,
            mw = c.memory,
        );
    }

    let gpu_field = format!("{:<gw$}", format!(" {}", p.gpu_index), gw = c.gpu);
    let pid_field = format!("{:<pw$}", p.pid, pw = c.pid);
    let user_field = format!("{:<uw$}", user, uw = c.user);
    let kind_field = format!("{:<kw$}", p.kind.short(), kw = c.kind);
    let name_field = format!("{:<nw$}", name, nw = c.name);
    let util_field = format!("{:>xw$}", util_plain, xw = c.util);
    let memory_field = format!("{:>mw$}", memory_plain, mw = c.memory);
    let util_color = p
        .gpu_util
        .map(|v| grade_rgb(v as f64, Grade::Heat))
        .unwrap_or(MUTED);

    format!(
        "{}{}{RESET}{}{}{RESET}{}{}{RESET}{}{}{RESET}{}{}{RESET}{}{}{RESET}{}{}{RESET}",
        fg(BLUE, truecolor), gpu_field,
        fg(MUTED, truecolor), pid_field,
        fg(TEXT, truecolor), user_field,
        fg(MUTED, truecolor), kind_field,
        fg(TEXT, truecolor), name_field,
        fg(util_color, truecolor), util_field,
        fg(CYAN, truecolor), memory_field,
    )
}

fn push_panel_top(
    frame: &mut String,
    title_plain: &str,
    title_styled: &str,
    width: usize,
    color: bool,
    truecolor: bool,
) {
    let fill = width.saturating_sub(title_plain.chars().count() + 5);
    if color {
        let border = fg(BORDER, truecolor);
        let _ = writeln!(
            frame,
            "{border}╭─{RESET} {title_styled} {border}{}╮{RESET}",
            "─".repeat(fill)
        );
    } else {
        let _ = writeln!(frame, "╭─ {title_plain} {}╮", "─".repeat(fill));
    }
}

fn push_panel_bottom(frame: &mut String, width: usize, color: bool, truecolor: bool) {
    if color {
        let border = fg(BORDER, truecolor);
        let _ = writeln!(frame, "{border}╰{}╯{RESET}", "─".repeat(width.saturating_sub(2)));
    } else {
        let _ = writeln!(frame, "╰{}╯", "─".repeat(width.saturating_sub(2)));
    }
}

fn push_panel_line(
    frame: &mut String,
    content: &str,
    inner: usize,
    color: bool,
    truecolor: bool,
) {
    let visible = visible_len_ansi(content);
    let pad = inner.saturating_sub(visible);
    if color {
        let border = fg(BORDER, truecolor);
        let _ = writeln!(frame, "{border}│{RESET}{content}{}{border}│{RESET}", " ".repeat(pad));
    } else {
        let _ = writeln!(frame, "│{content}{}│", " ".repeat(pad));
    }
}

fn push_panel_lr(
    frame: &mut String,
    left: &str,
    right_plain: &str,
    right_styled: &str,
    inner: usize,
    color: bool,
    truecolor: bool,
) {
    let left_len = visible_len_ansi(left);
    let right_len = right_plain.chars().count();
    let gap = inner.saturating_sub(left_len + right_len).max(1);
    let content = format!("{left}{}{right_styled}", " ".repeat(gap));
    push_panel_line(frame, &content, inner, color, truecolor);
}

fn push_plain_line(
    frame: &mut String,
    line: &str,
    width: usize,
    rgb: Rgb,
    color: bool,
    truecolor: bool,
) {
    let line = truncate(line, width);
    if color {
        let _ = writeln!(frame, "{}{}{RESET}", fg(rgb, truecolor), line);
    } else {
        let _ = writeln!(frame, "{line}");
    }
}

fn graded_bar(percent: f64, width: usize, grade: Grade, color: bool, truecolor: bool) -> String {
    let percent = percent.clamp(0.0, 100.0);
    let full = ((percent / 100.0) * width as f64).round() as usize;
    if !color {
        return format!(
            "{}{}",
            "━".repeat(full.min(width)),
            "─".repeat(width.saturating_sub(full))
        );
    }

    let mut output = String::with_capacity(width * 4 + 64);
    if full > 0 {
        output.push_str(&fg(grade_rgb(percent, grade), truecolor));
        output.push_str(&"━".repeat(full.min(width)));
    }
    if full < width {
        output.push_str(&fg(EMPTY, truecolor));
        output.push_str(&"─".repeat(width - full));
    }
    output.push_str(RESET);
    output
}

fn push_history_graph(
    frame: &mut String,
    label: &str,
    values: Option<&VecDeque<f64>>,
    width: usize,
    rows: usize,
    grade: Grade,
    inner: usize,
    color: bool,
    truecolor: bool,
) {
    let rows = rows.max(1);
    let rendered = history_graph_rows(values, width, rows, grade, color, truecolor);

    for (screen_row, graph_row) in rendered.into_iter().enumerate() {
        // Only the top line needs the label; keeping the remaining left margin
        // blank makes the vertical severity bands visually continuous.
        let prefix = if screen_row == 0 {
            if color {
                format!(" {}{label}{RESET}  ", fg(MUTED, truecolor))
            } else {
                format!(" {label}  ")
            }
        } else {
            "      ".to_string()
        };
        let line = format!("{prefix}{graph_row}");
        push_panel_line(frame, &line, inner, color, truecolor);
    }
}

fn history_graph_rows(
    values: Option<&VecDeque<f64>>,
    width: usize,
    rows: usize,
    grade: Grade,
    color: bool,
    truecolor: bool,
) -> Vec<String> {
    // Each terminal row contains 8 vertical sub-levels. A sample therefore has
    // rows*8 possible heights while the row's color remains fixed horizontally.
    const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let rows = rows.max(1);
    let visible: Vec<f64> = match values {
        Some(values) => {
            let skip = values.len().saturating_sub(width);
            let mut samples = vec![0.0; width.saturating_sub(values.len().min(width))];
            samples.extend(values.iter().skip(skip).map(|value| value.clamp(0.0, 100.0)));
            samples
        }
        None => vec![0.0; width],
    };

    let mut output = Vec::with_capacity(rows);
    for screen_row in 0..rows {
        // screen_row=0 is the hottest/top band; band_from_bottom=0 is bottom.
        let band_from_bottom = rows - 1 - screen_row;
        let band_low = band_from_bottom as f64 * 100.0 / rows as f64;
        let band_high = (band_from_bottom + 1) as f64 * 100.0 / rows as f64;
        let band_mid = (band_low + band_high) * 0.5;
        let row_color = grade_rgb(band_mid, grade);

        let mut line = String::with_capacity(width * 4 + 32);
        if color {
            line.push_str(&fg(row_color, truecolor));
        }

        for value in &visible {
            let occupancy = ((*value - band_low) / (band_high - band_low)).clamp(0.0, 1.0);
            let level = (occupancy * 8.0).round() as usize;
            line.push(BLOCKS[level.min(8)]);
        }

        if color {
            line.push_str(RESET);
        }
        output.push(line);
    }

    output
}

fn maybe_grade(value: f64, grade: Grade, color: bool, truecolor: bool) -> String {
    if color {
        fg(grade_rgb(value, grade), truecolor)
    } else {
        String::new()
    }
}

fn grade_rgb(value: f64, grade: Grade) -> Rgb {
    let value = value.clamp(0.0, 100.0);
    match grade {
        Grade::Heat => {
            // btop-like cool -> lime -> amber -> coral progression.
            if value <= 35.0 {
                mix(Rgb(76, 220, 126), Rgb(177, 238, 101), value / 35.0)
            } else if value <= 65.0 {
                mix(Rgb(177, 238, 101), Rgb(246, 205, 92), (value - 35.0) / 30.0)
            } else if value <= 85.0 {
                mix(Rgb(246, 205, 92), Rgb(249, 143, 92), (value - 65.0) / 20.0)
            } else {
                mix(Rgb(249, 143, 92), Rgb(247, 91, 110), (value - 85.0) / 15.0)
            }
        }
        Grade::Memory => {
            // Memory starts cyan, reaches the btop-like yellow band earlier,
            // then grades through amber into red near saturation.
            if value <= 20.0 {
                mix(Rgb(61, 207, 232), Rgb(68, 224, 190), value / 20.0)
            } else if value <= 40.0 {
                mix(Rgb(68, 224, 190), Rgb(239, 207, 92), (value - 20.0) / 20.0)
            } else if value <= 75.0 {
                mix(Rgb(239, 207, 92), Rgb(249, 148, 88), (value - 40.0) / 35.0)
            } else {
                mix(Rgb(249, 148, 88), Rgb(247, 91, 110), (value - 75.0) / 25.0)
            }
        }
    }
}

fn mix(left: Rgb, right: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| -> u8 {
        (a as f64 + (b as f64 - a as f64) * t).round().clamp(0.0, 255.0) as u8
    };
    Rgb(channel(left.0, right.0), channel(left.1, right.1), channel(left.2, right.2))
}

fn fg(rgb: Rgb, truecolor: bool) -> String {
    if truecolor {
        format!("\x1b[38;2;{};{};{}m", rgb.0, rgb.1, rgb.2)
    } else {
        let r = ((rgb.0 as u16 * 5 + 127) / 255) as u8;
        let g = ((rgb.1 as u16 * 5 + 127) / 255) as u8;
        let b = ((rgb.2 as u16 * 5 + 127) / 255) as u8;
        let code = 16 + 36 * r + 6 * g + b;
        format!("\x1b[38;5;{code}m")
    }
}

fn style(value: &str, rgb: Rgb, color: bool, truecolor: bool) -> String {
    if color {
        format!("{}{}{RESET}", fg(rgb, truecolor), value)
    } else {
        value.to_string()
    }
}

fn format_memory_pretty(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * MIB;
    if bytes as f64 >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB)
    }
}

fn format_memory_pair_pretty(used: u64, total: u64) -> String {
    format!("{} / {}", format_memory_pretty(used), format_memory_pretty(total))
}

fn format_memory_compact(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * MIB;
    if bytes as f64 >= GIB {
        format!("{:.1}G", bytes as f64 / GIB)
    } else {
        format!("{:.0}M", bytes as f64 / MIB)
    }
}

fn format_memory_pair_compact(used: u64, total: u64) -> String {
    format!("{}/{}", format_memory_compact(used), format_memory_compact(total))
}

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = value.chars().count();
    if count <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut result: String = value.chars().take(width - 1).collect();
    result.push('…');
    result
}

fn visible_len_ansi(value: &str) -> usize {
    let mut count = 0usize;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for control in chars.by_ref() {
                    if ('@'..='~').contains(&control) {
                        break;
                    }
                }
            }
            continue;
        }
        count += 1;
    }
    count
}

fn count_newlines(value: &str) -> usize {
    value.bytes().filter(|byte| *byte == b'\n').count()
}

fn terminal_size() -> (u16, u16) {
    let mut size = WinSize::default();
    let ok = unsafe { ioctl(STDOUT_FILENO, TIOCGWINSZ, &mut size) } == 0;
    if ok && size.cols > 0 && size.rows > 0 {
        return (size.cols, size.rows);
    }

    let columns = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120);
    let lines = env::var("LINES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(40);
    (columns, lines)
}

fn supports_truecolor() -> bool {
    let colorterm = env::var("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return true;
    }
    let term = env::var("TERM").unwrap_or_default().to_ascii_lowercase();
    term.contains("direct") || term.contains("kitty") || term.contains("wezterm")
}

pub fn should_use_color(force_no_color: bool) -> bool {
    !force_no_color
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").map(|value| value != "dumb").unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::{
        format_memory_pretty, format_pcie_link, format_pcie_rate, format_pcie_traffic, grade_rgb,
        graded_bar, history_graph_rows, truncate, visible_len_ansi, Grade,
    };
    use std::collections::VecDeque;

    #[test]
    fn bar_has_requested_width_without_color() {
        assert_eq!(graded_bar(50.0, 10, Grade::Heat, false, false).chars().count(), 10);
    }

    #[test]
    fn history_graph_has_requested_rows_and_width() {
        let values = VecDeque::from(vec![0.0, 50.0, 100.0]);
        let output = history_graph_rows(Some(&values), 7, 4, Grade::Heat, true, true);
        assert_eq!(output.len(), 4);
        assert!(output.iter().all(|row| visible_len_ansi(row) == 7));
    }

    #[test]
    fn history_graph_colors_are_row_based_not_sample_based() {
        let values = VecDeque::from(vec![20.0, 90.0, 20.0, 90.0]);
        let output = history_graph_rows(Some(&values), 4, 4, Grade::Heat, true, true);
        // Each row emits one foreground-color escape for the whole row. If the
        // renderer regresses to per-sample grading there will be many more.
        for row in output {
            assert_eq!(row.matches("\x1b[38;").count(), 1);
        }
    }

    #[test]
    fn truncates_unicode_by_characters() {
        assert_eq!(truncate("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn memory_format_is_human_readable() {
        assert_eq!(format_memory_pretty(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_memory_pretty(512 * 1024 * 1024), "512 MiB");
    }

    #[test]
    fn ansi_width_ignores_control_sequences() {
        assert_eq!(visible_len_ansi("\x1b[38;2;1;2;3mhello\x1b[0m"), 5);
    }

    #[test]
    fn heat_grade_gets_hotter() {
        let cool = grade_rgb(10.0, Grade::Heat);
        let hot = grade_rgb(95.0, Grade::Heat);
        assert!(hot.0 > cool.0);
    }

    #[test]
    fn pcie_link_and_traffic_format_are_compact_and_explicit() {
        assert_eq!(format_pcie_link(Some(4), Some(16)), "Gen4 x16");
        assert_eq!(format_pcie_rate(185_000), "185 MB/s");
        assert_eq!(format_pcie_rate(1_250_000), "1.2 GB/s");
        assert_eq!(
            format_pcie_traffic(Some(185_000), Some(460_000)),
            "TX 185 MB/s  RX 460 MB/s"
        );
    }
}
