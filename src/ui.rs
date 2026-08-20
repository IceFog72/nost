use crate::model::{GpuSnapshot, ProcessSnapshot, Snapshot};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::{c_char, c_int, c_long, c_ulong, c_void};
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
    fn time(timer: *mut c_long) -> c_long;
    fn localtime(timer: *const c_long) -> *mut c_void;
    fn strftime(
        output: *mut c_char,
        max: usize,
        format: *const c_char,
        tm: *const c_void,
    ) -> usize;
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
    interval_ms: u64,
    backend_label: &str,
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

    if width < 64 || height < 16 {
        let message = format!(
            "nost  resize terminal ({}x{}, need >= 65x16)",
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

    push_header(&mut frame, width, backend_label, interval_ms, snapshot.gpus.len(), color, truecolor);

    // Keep enough room for a useful process box even when several GPUs exist.
    let gpu_panel_lines = 10usize;
    let process_min_lines = 5usize;
    let fixed_lines = 1 /* header */ + 1 /* gap */ + process_min_lines;
    let max_gpu_panels = height
        .saturating_sub(fixed_lines)
        .checked_div(gpu_panel_lines + 1)
        .unwrap_or(0)
        .max(1);

    for gpu in snapshot.gpus.iter().take(max_gpu_panels) {
        push_gpu_panel(&mut frame, gpu, history, width, color, truecolor);
        frame.push('\n');
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
    if remaining >= 4 {
        push_process_panel(
            &mut frame,
            &snapshot.processes,
            width,
            remaining,
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

fn push_header(
    frame: &mut String,
    width: usize,
    backend: &str,
    interval_ms: u64,
    gpu_count: usize,
    color: bool,
    truecolor: bool,
) {
    let left_plain = "nost  nvidia";
    let backend_short = if backend == "NVML direct" { "NVML" } else { backend };
    let right = format!(
        "{}   {}   {} ms   {} GPU{}",
        local_clock(),
        backend_short,
        interval_ms,
        gpu_count,
        if gpu_count == 1 { "" } else { "s" }
    );
    let gap = width
        .saturating_sub(left_plain.chars().count() + right.chars().count())
        .max(1);

    if color {
        let accent = fg(ACCENT, truecolor);
        let muted = fg(MUTED, truecolor);
        let text = fg(TEXT, truecolor);
        let _ = writeln!(
            frame,
            "{BOLD}{accent}nost{RESET}  {muted}nvidia{RESET}{}{}{}{right}{RESET}",
            " ".repeat(gap),
            DIM,
            text,
        );
    } else {
        let _ = writeln!(frame, "nost  nvidia{}{}", " ".repeat(gap), right);
    }
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

    let graph_width = inner.saturating_sub(12).max(12);
    let gpu_graph = spark_graded(history.gpu(gpu.index), graph_width, Grade::Heat, color, truecolor);
    let mem_graph = spark_graded(history.memory(gpu.index), graph_width, Grade::Memory, color, truecolor);
    let gpu_graph_line = if color {
        format!(" {}gpu{RESET}  {gpu_graph}", fg(MUTED, truecolor))
    } else {
        format!(" gpu  {gpu_graph}")
    };
    let mem_graph_line = if color {
        format!(" {}mem{RESET}  {mem_graph}", fg(MUTED, truecolor))
    } else {
        format!(" mem  {mem_graph}")
    };
    push_panel_line(frame, &gpu_graph_line, inner, color, truecolor);
    push_panel_line(frame, &mem_graph_line, inner, color, truecolor);

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
    let video_plain = format_video_util(gpu.encoder_util_percent, gpu.decoder_util_percent);
    let video_peak = gpu
        .encoder_util_percent
        .into_iter()
        .chain(gpu.decoder_util_percent)
        .max()
        .unwrap_or(0) as f64;
    let video_metric = metric(
        "video",
        &video_plain,
        grade_rgb(video_peak, Grade::Memory),
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
    let traffic_plain = format_pcie_traffic(gpu.pcie_tx_kb_per_s, gpu.pcie_rx_kb_per_s);
    let pcie_metric = metric("pcie", &pcie_plain, BLUE, color, truecolor);
    let traffic_metric = metric("traffic", &traffic_plain, CYAN, color, truecolor);
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

fn format_video_util(encoder: Option<u32>, decoder: Option<u32>) -> String {
    let enc = encoder
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "--".to_string());
    let dec = decoder
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "--".to_string());
    format!("ENC{enc} DEC{dec}")
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
        format!("{:.1}GB/s", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.0}MB/s", value / 1_000.0)
    } else {
        format!("{kb_per_s}KB/s")
    }
}

fn format_pcie_traffic(tx: Option<u32>, rx: Option<u32>) -> String {
    let tx = tx.map(format_pcie_rate).unwrap_or_else(|| "--".to_string());
    let rx = rx.map(format_pcie_rate).unwrap_or_else(|| "--".to_string());
    format!("TX{tx} RX{rx}")
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

fn spark_graded(
    values: Option<&VecDeque<f64>>,
    width: usize,
    grade: Grade,
    color: bool,
    truecolor: bool,
) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let Some(values) = values else {
        return if color {
            format!("{}{}{}", fg(EMPTY, truecolor), "▁".repeat(width), RESET)
        } else {
            "▁".repeat(width)
        };
    };

    let skip = values.len().saturating_sub(width);
    let pad = width.saturating_sub(values.len().min(width));
    let mut output = String::with_capacity(width * 20);
    if pad > 0 {
        if color {
            output.push_str(&fg(EMPTY, truecolor));
        }
        output.push_str(&"▁".repeat(pad));
        if color {
            output.push_str(RESET);
        }
    }

    for value in values.iter().skip(skip) {
        let value = value.clamp(0.0, 100.0);
        let index = ((value / 100.0) * 7.0).round() as usize;
        if color {
            output.push_str(&fg(grade_rgb(value, grade), truecolor));
        }
        output.push(BLOCKS[index.min(7)]);
    }
    if color {
        output.push_str(RESET);
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

fn local_clock() -> String {
    let mut now: c_long = 0;
    unsafe {
        let _ = time(&mut now as *mut c_long);
        let tm = localtime(&now as *const c_long);
        if tm.is_null() {
            return "--:--:--".to_string();
        }
        let mut buffer = [0i8; 16];
        let format = b"%H:%M:%S\0";
        let written = strftime(
            buffer.as_mut_ptr(),
            buffer.len(),
            format.as_ptr() as *const c_char,
            tm,
        );
        if written == 0 {
            return "--:--:--".to_string();
        }
        let bytes = std::slice::from_raw_parts(buffer.as_ptr() as *const u8, written);
        String::from_utf8_lossy(bytes).into_owned()
    }
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
        graded_bar, spark_graded, truncate, visible_len_ansi, Grade,
    };
    use std::collections::VecDeque;

    #[test]
    fn bar_has_requested_width_without_color() {
        assert_eq!(graded_bar(50.0, 10, Grade::Heat, false, false).chars().count(), 10);
    }

    #[test]
    fn spark_has_requested_visible_width() {
        let values = VecDeque::from(vec![0.0, 50.0, 100.0]);
        let output = spark_graded(Some(&values), 7, Grade::Heat, true, true);
        assert_eq!(visible_len_ansi(&output), 7);
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
        assert_eq!(format_pcie_rate(185_000), "185MB/s");
        assert_eq!(format_pcie_rate(1_250_000), "1.2GB/s");
        assert_eq!(
            format_pcie_traffic(Some(185_000), Some(460_000)),
            "TX185MB/s RX460MB/s"
        );
    }
}
