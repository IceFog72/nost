use crate::model::{GpuSnapshot, ProcessSnapshot, Snapshot, VramHolderSnapshot};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::{c_int, c_ulong, c_void};
use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU32, Ordering};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

const ACCENT: Rgb = Rgb(98, 255, 118);
const TEXT: Rgb = Rgb(218, 223, 232);
const MUTED: Rgb = Rgb(116, 124, 139);
const BORDER: Rgb = Rgb(72, 91, 118);
const CYAN: Rgb = Rgb(63, 218, 230);
const BLUE: Rgb = Rgb(94, 168, 255);
const AMBER: Rgb = Rgb(255, 191, 74);

// DEC synchronized output. Terminals that implement mode 2026 hold the frame
// until SYNC_END, which avoids exposing half-drawn resize frames. Unsupported
// terminals simply ignore these private-mode sequences.
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

// width in the high 16 bits, height in the low 16 bits. Zero means no frame
// has been rendered yet. This is only touched by the single render thread.
static LAST_TERMINAL_SIZE: AtomicU32 = AtomicU32::new(0);

// 1-based terminal row of the process table header, or 0 when it is not
// drawn. Written by the renderer, read by the mouse parser so clicks can be
// mapped to columns. Both run on the same thread.
static PROCESS_HEADER_ROW: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortColumn {
    Gpu,
    Pid,
    User,
    Kind,
    Name,
    Util,
    Memory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSort {
    pub column: SortColumn,
    pub descending: bool,
}

impl ProcessSort {
    /// Clicking a new column sorts by it (numbers biggest-first, text A-Z);
    /// clicking the active column reverses the direction.
    pub fn clicked(current: Option<Self>, column: SortColumn) -> Self {
        match current {
            Some(sort) if sort.column == column => Self {
                column,
                descending: !sort.descending,
            },
            _ => Self {
                column,
                descending: matches!(column, SortColumn::Util | SortColumn::Memory),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input {
    Key(u8),
    Sort(SortColumn),
}

fn sort_processes(processes: &[ProcessSnapshot], sort: ProcessSort) -> Vec<ProcessSnapshot> {
    let mut sorted = processes.to_vec();
    sorted.sort_by(|a, b| {
        let primary = match sort.column {
            SortColumn::Gpu => a.gpu_index.cmp(&b.gpu_index),
            SortColumn::Pid => a.pid.cmp(&b.pid),
            SortColumn::User => a.user.to_lowercase().cmp(&b.user.to_lowercase()),
            SortColumn::Kind => a.kind.short().cmp(b.kind.short()),
            SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortColumn::Util => a.gpu_util.cmp(&b.gpu_util),
            SortColumn::Memory => a.memory_bytes.cmp(&b.memory_bytes),
        };
        let primary = if sort.descending { primary.reverse() } else { primary };
        primary.then_with(|| a.pid.cmp(&b.pid)).then_with(|| a.gpu_index.cmp(&b.gpu_index))
    });
    sorted
}

fn process_header_hit(column: usize, inner: usize) -> Option<SortColumn> {
    // Content starts after the left border, i.e. at 1-based column 2.
    let c = process_columns(inner);
    let mut start = 2;
    for (width, id) in [
        (c.gpu, SortColumn::Gpu),
        (c.pid, SortColumn::Pid),
        (c.user, SortColumn::User),
        (c.kind, SortColumn::Kind),
        (c.name, SortColumn::Name),
        (c.util, SortColumn::Util),
        (c.memory, SortColumn::Memory),
    ] {
        if column >= start && column < start + width {
            return Some(id);
        }
        start += width;
    }
    None
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

#[derive(Clone, Copy, PartialEq)]
enum Grade {
    Heat,
    Memory,
    /// Samples are cap categories, drawn as full-height columns.
    Reason,
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
const MOUSE_MODES_ON: &str = "\x1b[?1000h\x1b[?1006h";

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
    fn tcgetattr(fd: c_int, termios: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, actions: c_int, termios: *const Termios) -> c_int;
    #[link_name = "read"]
    fn c_read(fd: c_int, buffer: *mut c_void, count: usize) -> isize;
}

const STDIN_FILENO: c_int = 0;
const STDOUT_FILENO: c_int = 1;
const TIOCGWINSZ: c_ulong = 0x5413;
const TCSANOW: c_int = 0;
const ICANON: u32 = 0o000002;
const ECHO: u32 = 0o000010;
const VTIME: usize = 5;
const VMIN: usize = 6;

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    input_flags: u32,
    output_flags: u32,
    control_flags: u32,
    local_flags: u32,
    line_discipline: u8,
    control_chars: [u8; 32],
    input_speed: u32,
    output_speed: u32,
}

pub struct TerminalGuard {
    inline: bool,
    interactive: bool,
    original_stdin: Option<Termios>,
    input_buffer: Vec<u8>,
}

impl TerminalGuard {
    pub fn enter(inline: bool, interactive: bool) -> io::Result<Self> {
        let original_stdin = interactive.then(enable_key_input).transpose()?.flatten();
        let guard = Self {
            inline,
            interactive,
            original_stdin,
            input_buffer: Vec::new(),
        };
        guard.write_enter_sequence()?;
        Ok(guard)
    }

    fn write_enter_sequence(&self) -> io::Result<()> {
        let mut out = io::stdout().lock();
        if !self.inline {
            write!(out, "\x1b[?1049h")?;
        }
        // Disable alternate-scroll *after* entering the alternate screen.
        // nost reads only the documented keyboard controls.
        // Disable auto-wrap while the dashboard owns the terminal. If the
        // window becomes narrower between TIOCGWINSZ and the actual write,
        // long rows are clipped instead of wrapping into unrelated rows.
        let mouse_modes = if self.original_stdin.is_some() { MOUSE_MODES_ON } else { "" };
        write!(
            out,
            "{}{mouse_modes}\x1b[?7l\x1b[?25l\x1b[2J\x1b[H",
            INPUT_MODES_OFF
        )?;
        out.flush()?;
        Ok(())
    }

    pub fn read_key(&mut self, interval_ms: u64) -> io::Result<Option<Input>> {
        if self.original_stdin.is_none() {
            return Ok(None);
        }
        let mut bytes = [0u8; 64];
        let read = unsafe { c_read(STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len()) };
        if read > 0 {
            self.input_buffer.extend_from_slice(&bytes[..read as usize]);
        } else if read < 0 {
            let error = io::Error::last_os_error();
            if !matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) {
                return Err(error);
            }
        }
        let width = terminal_size().0 as usize;
        Ok(parse_interval_input(
            &mut self.input_buffer,
            interval_ms,
            width.saturating_sub(1),
        ))
    }
}

fn parse_interval_input(input: &mut Vec<u8>, interval_ms: u64, width: usize) -> Option<Input> {
    while !input.is_empty() {
        match input[0] {
            key @ (b'-' | b'_' | b'+' | b'=' | b'q' | b'Q') => {
                input.remove(0);
                return Some(Input::Key(key));
            }
            b'\x1b' if input.len() < 3 => return None,
            b'\x1b' if input.starts_with(b"\x1b[<") => {
                let Some(end) = input.iter().position(|byte| matches!(byte, b'M' | b'm')) else {
                    if input.len() > 32 {
                        input.clear();
                    }
                    return None;
                };
                let event: Vec<u8> = input.drain(..=end).collect();
                if event[end] != b'M' {
                    continue;
                }
                let Ok(fields) = std::str::from_utf8(&event[3..end]) else {
                    continue;
                };
                let mut fields = fields.split(';');
                let (Some(button), Some(column), Some(row)) =
                    (
                        fields.next().and_then(|field| field.parse::<usize>().ok()),
                        fields.next().and_then(|field| field.parse::<usize>().ok()),
                        fields.next().and_then(|field| field.parse::<usize>().ok()),
                    )
                else {
                    continue;
                };
                if fields.next().is_some() || button & 3 != 0 || button & 64 != 0 {
                    continue;
                }
                let header_row = PROCESS_HEADER_ROW.load(Ordering::Relaxed) as usize;
                if header_row != 0 && row == header_row {
                    if let Some(column) = process_header_hit(column, width.saturating_sub(2)) {
                        return Some(Input::Sort(column));
                    }
                    continue;
                }
                if row != 1 {
                    continue;
                }
                let plus = width.saturating_sub(3);
                let minus = plus.saturating_sub(interval_ms.to_string().len() + 5);
                if column.abs_diff(plus) <= 1 {
                    return Some(Input::Key(b'+'));
                }
                if column.abs_diff(minus) <= 1 {
                    return Some(Input::Key(b'-'));
                }
            }
            _ => {
                input.remove(0);
            }
        }
    }
    None
}

fn enable_key_input() -> io::Result<Option<Termios>> {
    let mut original = MaybeUninit::<Termios>::uninit();
    if unsafe { tcgetattr(STDIN_FILENO, original.as_mut_ptr()) } != 0 {
        return Ok(None);
    }
    let original = unsafe { original.assume_init() };
    let mut interactive = original;
    interactive.local_flags &= !(ICANON | ECHO);
    interactive.control_chars[VMIN] = 0;
    interactive.control_chars[VTIME] = 0;
    if unsafe { tcsetattr(STDIN_FILENO, TCSANOW, &interactive) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(original))
}

impl TerminalGuard {
    fn leave(&mut self) {
        if let Some(original) = self.original_stdin {
            unsafe {
                let _ = tcsetattr(STDIN_FILENO, TCSANOW, &original);
            }
        }
        let mut out = io::stdout().lock();
        // Always leave synchronized-output mode, restore auto-wrap, and then
        // restore the cursor even if the previous frame was interrupted.
        let _ = write!(out, "{SYNC_END}{}\x1b[0m\x1b[?7h\x1b[?25h", INPUT_MODES_OFF);
        if !self.inline {
            let _ = write!(out, "\x1b[?1049l");
        } else {
            let _ = writeln!(out);
        }
        let _ = out.flush();
    }

    /// Hand the terminal back to the shell before the process stops (Ctrl-Z).
    pub fn suspend(&mut self) {
        self.leave();
    }

    /// Re-enter dashboard modes after SIGCONT. The shell may have changed the
    /// tty settings while we were stopped, so capture them again as the state
    /// to restore on the next leave.
    pub fn resume(&mut self) -> io::Result<()> {
        if self.interactive {
            self.original_stdin = enable_key_input()?;
        }
        self.input_buffer.clear();
        self.write_enter_sequence()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

#[derive(Default)]
struct GpuHistory {
    gpu: VecDeque<f64>,
    memory: VecDeque<f64>,
    power: VecDeque<f64>,
    thermal: VecDeque<f64>,
    cap: VecDeque<f64>,
}

pub struct History {
    max_len: usize,
    gpus: HashMap<u32, GpuHistory>,
}

impl History {
    pub fn new(max_len: usize) -> Self {
        Self {
            max_len,
            gpus: HashMap::new(),
        }
    }

    pub fn update(&mut self, snapshot: &Snapshot) {
        let max_len = self.max_len;
        for gpu in &snapshot.gpus {
            let h = self.gpus.entry(gpu.index).or_default();
            push_bounded(&mut h.gpu, gpu.gpu_util.unwrap_or(0) as f64, max_len);
            push_bounded(&mut h.memory, gpu.memory_percent(), max_len);
            push_bounded(&mut h.power, power_percent(gpu), max_len);
            push_bounded(&mut h.thermal, temperature_grade(gpu), max_len);
            push_bounded(&mut h.cap, gpu.clock_limit.as_deref().map_or(0.0, |limit| cap_category(limit) as f64), max_len);
        }
    }

    fn series(&self, index: u32, pick: fn(&GpuHistory) -> &VecDeque<f64>) -> Option<&VecDeque<f64>> {
        self.gpus.get(&index).map(pick)
    }
}

fn push_bounded(values: &mut VecDeque<f64>, value: f64, max_len: usize) {
    if values.len() >= max_len {
        values.pop_front();
    }
    values.push_back(value);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VerticalLayout {
    graph_rows: usize,
    show_holders: bool,
    ultra_compact: bool,
}

fn choose_vertical_layout(height: usize) -> Option<VerticalLayout> {
    // Hard visibility priorities under height pressure:
    //   1) one GPU summary remains visible,
    //   2) at least the top three processes remain visible,
    //   3) at least the top two VRAM holders remain visible,
    //   4) secondary GPU metrics and multi-row graphs consume spare height.
    //
    // Ultra-compact (13 rows): GPU 4 + processes 5 + holders 4 = 13.
    // Compact (19 rows): GPU 7 + processes 6 + holders 6 = 19.
    // Full (25 rows): GPU 13 + processes 6 + holders 6 = 25.
    if height < 13 {
        return None;
    }
    if height < 19 {
        return Some(VerticalLayout {
            graph_rows: 1,
            show_holders: true,
            ultra_compact: true,
        });
    }
    if height < 25 {
        return Some(VerticalLayout {
            graph_rows: 1,
            show_holders: true,
            ultra_compact: false,
        });
    }
    Some(VerticalLayout {
        graph_rows: 4,
        show_holders: true,
        ultra_compact: false,
    })
}

pub fn render(
    snapshot: &Snapshot,
    history: &History,
    interval_ms: u64,
    _backend_label: &str,
    sort: Option<ProcessSort>,
    color: bool,
) -> io::Result<()> {
    PROCESS_HEADER_ROW.store(0, Ordering::Relaxed);
    let (terminal_width, terminal_height) = terminal_size();
    let terminal_width = terminal_width as usize;
    let terminal_height = terminal_height as usize;
    // Leave the last physical column alone. A number of terminal emulators
    // auto-wrap when a printable glyph touches that column.
    let width = terminal_width.saturating_sub(1);
    let height = terminal_height;
    let truecolor = supports_truecolor();

    let mut frame = String::with_capacity(width.saturating_mul(height).saturating_mul(4));
    frame.push_str(SYNC_BEGIN);

    let size_key = terminal_size_key(terminal_width as u16, terminal_height as u16);
    let previous_size = LAST_TERMINAL_SIZE.swap(size_key, Ordering::Relaxed);
    if previous_size != 0 && previous_size != size_key {
        // CSI J at the end of a normal frame only clears *below* the final
        // cursor. It cannot erase stale right-hand tails on rows already
        // redrawn at a smaller width. A full clear on geometry changes removes
        // those tails before the new frame is painted.
        frame.push_str("\x1b[2J");
    }
    frame.push_str("\x1b[H");

    let vertical_layout = choose_vertical_layout(height);
    if width < 64 || vertical_layout.is_none() {
        let message = format!(
            "nost  resize terminal ({}x{}, need >= 65x13)",
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
        frame.push_str(SYNC_END);
        return flush_frame(frame);
    }

    let vertical_layout = vertical_layout.expect("height checked above");
    // At the minimum 13-row geometry, secondary GPU metric rows and table
    // headers are suppressed so we can preserve 3 process rows + 2 holder rows.
    // Once 19 rows are available, the normal compact panels and headers return.
    let gpu_panel_lines = if vertical_layout.ultra_compact {
        2usize + vertical_layout.graph_rows * 2
    } else {
        4usize + vertical_layout.graph_rows * 2
    };
    let process_min_lines = if vertical_layout.ultra_compact { 5usize } else { 6usize };
    let holders_min_lines = if vertical_layout.show_holders {
        if vertical_layout.ultra_compact { 4usize } else { 6usize }
    } else {
        0usize
    };
    let fixed_lines = process_min_lines + holders_min_lines;
    let max_gpu_panels = height
        .saturating_sub(fixed_lines)
        .checked_div(gpu_panel_lines)
        .unwrap_or(0)
        .max(1);

    for (position, gpu) in snapshot.gpus.iter().take(max_gpu_panels).enumerate() {
        push_gpu_panel(
            &mut frame,
            gpu,
            history,
            width,
            vertical_layout.graph_rows,
            vertical_layout.ultra_compact,
            (position == 0).then_some(interval_ms),
            color,
            truecolor,
        );
    }

    if snapshot.gpus.len() > max_gpu_panels {
        // A hidden-GPU notice is informational, so never let it consume one of
        // the six lines reserved for the top-three process table.
        let gpu_lines = max_gpu_panels.saturating_mul(gpu_panel_lines);
        if height >= gpu_lines.saturating_add(fixed_lines).saturating_add(1) {
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
    }

    let lines_before_process = count_newlines(&frame);
    let remaining = height.saturating_sub(lines_before_process);
    let process_budget = remaining.saturating_sub(holders_min_lines).max(process_min_lines);
    if process_budget >= process_min_lines {
        push_process_panel(
            &mut frame,
            &snapshot.processes,
            width,
            process_budget.min(remaining),
            vertical_layout.ultra_compact,
            sort,
            color,
            truecolor,
        );
    }

    if vertical_layout.show_holders {
        let lines_before_holders = count_newlines(&frame);
        let holder_budget = height.saturating_sub(lines_before_holders);
        if holder_budget >= holders_min_lines {
            push_vram_holders_panel(
                &mut frame,
                snapshot,
                width,
                holder_budget,
                vertical_layout.ultra_compact,
                color,
                truecolor,
            );
        }
    }

    // A line feed from the last physical row scrolls the entire dashboard.
    // Keep the cursor after the final border so CSI J can still clear its tail.
    if frame.ends_with('\n') {
        frame.pop();
    }
    frame.push_str("\x1b[J");
    frame.push_str(SYNC_END);
    flush_frame(frame)
}

fn terminal_size_key(width: u16, height: u16) -> u32 {
    ((width as u32) << 16) | height as u32
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
    graph_rows: usize,
    ultra_compact: bool,
    interval_ms: Option<u64>,
    color: bool,
    truecolor: bool,
) {
    let interval_plain = interval_ms.map(|ms| format!("- {ms}ms +"));
    let title_prefix = format!("GPU {}  ", gpu.index);
    let right_width = interval_plain.as_ref().map_or(0, |value| value.chars().count() + 3);
    let inner = width.saturating_sub(2);
    let mut name_budget = width.saturating_sub(title_prefix.chars().count() + right_width + 5);
    // The title must also stop before the first vertical rule (border index
    // sep + 1), leaving room for the " gpu NN% " title that hangs off it.
    let first_rule = graph_separator_columns(inner, 2)[0] + 1;
    let gpu_title_len = 3 + 1 + gpu.gpu_util.map_or(2, |v| v.to_string().len() + 1) + 2;
    let before_rule = first_rule.saturating_sub(5 + title_prefix.chars().count());
    let gpu_title_in_border = before_rule >= gpu_title_len + 10;
    name_budget = name_budget.min(if gpu_title_in_border {
        before_rule - gpu_title_len
    } else {
        before_rule
    });
    let name = truncate(&gpu.name, name_budget);
    let title_len = title_prefix.chars().count() + name.chars().count();

    // Five history graphs: gpu + mem on the first row, power + thermal +
    // performance-cap reason on the second. Each graph's label and reading is
    // written into the border line above it; if it does not fit there it is
    // overlaid on the graph instead. Under height pressure every graph
    // collapses to one line.
    let gpu_rgb = gpu.gpu_util.map_or(MUTED, |v| grade_rgb(v as f64, Grade::Heat));
    let gpu_text = gpu.gpu_util.map_or_else(|| "--".to_string(), |v| format!("{v}%"));

    // One short reading per graph, overlaid next to its label.
    let mem_percent = gpu.memory_percent();
    let mem_readings = vec![(
        format!(
            "{mem_percent:.0}% {}/{}",
            format_memory_compact(gpu.memory_used),
            format_memory_compact(gpu.memory_usable())
        ),
        grade_rgb(mem_percent, Grade::Memory),
    )];

    let power_rgb = grade_rgb(power_percent(gpu), Grade::Heat);
    let power_readings = vec![match (gpu.power_mw, gpu.power_limit_mw) {
        (Some(now), Some(limit)) => (
            format!("{:.0}W/{:.0}W", now as f64 / 1000.0, limit as f64 / 1000.0),
            power_rgb,
        ),
        (Some(now), None) => (format!("{:.0}W", now as f64 / 1000.0), power_rgb),
        _ => ("--".to_string(), MUTED),
    }];

    let thermal_readings = vec![match gpu.temperature_c {
        Some(v) => (format!("{v}°C"), grade_rgb(temperature_grade(gpu), Grade::Heat)),
        None => ("--".to_string(), MUTED),
    }];

    let cap_text = gpu.clock_limit.as_deref().unwrap_or("--");
    let cap_readings = vec![(cap_label(cap_text), clock_limit_color(cap_text))];

    let hist = |pick: fn(&GpuHistory) -> &VecDeque<f64>| history.series(gpu.index, pick);
    let top_cells = [
        GraphCell { label: "gpu", values: hist(|h| &h.gpu), grade: Grade::Heat, readings: vec![(gpu_text, gpu_rgb)] },
        GraphCell { label: "mem", values: hist(|h| &h.memory), grade: Grade::Memory, readings: mem_readings },
    ];
    let bottom_cells = [
        GraphCell { label: "power", values: hist(|h| &h.power), grade: Grade::Heat, readings: power_readings },
        GraphCell { label: "temp", values: hist(|h| &h.thermal), grade: Grade::Heat, readings: thermal_readings },
        GraphCell { label: "cap", values: hist(|h| &h.cap), grade: Grade::Reason, readings: cap_readings },
    ];

    // Top border: GPU name on the left, refresh interval on the right, and the
    // gpu/mem titles between them, hanging off the vertical rule.
    let mut top = BorderLine::new(width, '╭', '╮');
    top.put_titled(
        2,
        &[
            (format!("GPU {}", gpu.index), ACCENT, true),
            ("  ".to_string(), TEXT, false),
            (name.clone(), TEXT, false),
        ],
    );
    if let Some(plain) = &interval_plain {
        let len = plain.chars().count();
        let (minus, rest) = plain.split_at(1);
        let (middle, plus) = rest.split_at(rest.len() - 1);
        top.put_titled(
            width.saturating_sub(4 + len),
            &[
                (minus.to_string(), BLUE, false),
                (middle.to_string(), TEXT, true),
                (plus.to_string(), BLUE, false),
            ],
        );
    }
    for column in graph_separator_columns(inner, top_cells.len()) {
        top.junction(column + 1, '┬');
    }
    let top_in_border = top.place_cell_titles(&top_cells, inner, 5 + title_len, gpu_title_in_border);
    frame.push_str(&top.render(color, truecolor));
    push_graph_row(frame, &top_cells, &top_in_border, graph_rows, inner, color, truecolor);

    let bottom_in_border = if ultra_compact {
        vec![false; bottom_cells.len()]
    } else {
        let mut divider = BorderLine::new(width, '├', '┤');
        for column in graph_separator_columns(inner, top_cells.len()) {
            divider.junction(column + 1, '┴');
        }
        for column in graph_separator_columns(inner, bottom_cells.len()) {
            divider.junction(column + 1, '┬');
        }
        let placed = divider.place_cell_titles(&bottom_cells, inner, 2, true);
        frame.push_str(&divider.render(color, truecolor));
        placed
    };
    push_graph_row(frame, &bottom_cells, &bottom_in_border, graph_rows, inner, color, truecolor);

    if !ultra_compact {
            let fan_plain = gpu
                .fan_percent
                .map(|v| format!("{v}%"))
                .unwrap_or_else(|| "--".into());
            let fan_metric = metric("fan", &fan_plain, BLUE, color, truecolor);

            let core_plain = gpu
                .graphics_clock_mhz
                .map(|v| format!("{v} MHz"))
                .unwrap_or_else(|| "--".into());
            let memory_clock_plain = gpu
                .memory_clock_mhz
                .map(|v| format!("{v} MHz"))
                .unwrap_or_else(|| "--".into());
            let core_metric = metric("core clock", &core_plain, TEXT, color, truecolor);
            let memory_metric = metric("mem clock", &memory_clock_plain, TEXT, color, truecolor);
            push_panel_columns(frame, &[core_metric, memory_metric, fan_metric], inner, color, truecolor);

    }

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

fn power_percent(gpu: &GpuSnapshot) -> f64 {
    match (gpu.power_mw, gpu.power_limit_mw) {
        (Some(now), Some(limit)) if limit > 0 => (now as f64 * 100.0 / limit as f64).clamp(0.0, 100.0),
        _ => 0.0,
    }
}

/// Category of the primary (most actionable) clock-limit reason. 0 means
/// nothing limits the clocks. The cap graph stores one category per sample and
/// gives each its own color, like GPU-Z's PerfCap Reason.
fn cap_category(limit: &str) -> u8 {
    match limit.split('+').next().unwrap_or("") {
        "none" | "--" | "" => 0,
        "idle" => 1,
        "power" => 2,
        "thermal" => 3,
        "hw thermal" => 4,
        "power brake" => 5,
        "hw slowdown" => 6,
        "sync" => 7,
        "app clocks" => 8,
        "display" => 9,
        "reliability" => 10,
        "board limit" => 11,
        _ => 12,
    }
}

fn cap_color(category: u8) -> Rgb {
    match category {
        0 => ACCENT,
        1 => BLUE,
        2 => Rgb(255, 168, 64),
        3 => Rgb(255, 92, 72),
        4 => Rgb(224, 48, 64),
        5 => Rgb(214, 88, 214),
        6 => Rgb(176, 64, 255),
        7 => Rgb(150, 160, 255),
        8 => CYAN,
        9 => Rgb(96, 224, 190),
        10 => Rgb(255, 214, 64),
        11 => Rgb(255, 128, 160),
        _ => MUTED,
    }
}

/// GPU-Z style short names for the reasons NVML reports.
fn cap_label(limit: &str) -> String {
    limit
        .split('+')
        .map(|reason| match reason {
            "power" => "Pwr",
            "thermal" | "hw thermal" => "Thrm",
            "reliability" => "VRel",
            "board limit" => "Board",
            "idle" => "Idle",
            other => other,
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn clock_limit_color(limit: &str) -> Rgb {
    if limit == "--" {
        MUTED
    } else {
        cap_color(cap_category(limit))
    }
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
        line.push_str(clip_ansi(&item.styled, width.saturating_sub(1)).as_ref());
        let used = plain_len.min(width.saturating_sub(1)) + 1;
        if width > used {
            line.push_str(&" ".repeat(width - used));
        }
    }
    push_panel_line(frame, &line, inner, color, truecolor);
}

#[allow(clippy::too_many_arguments)]
fn push_process_panel(
    frame: &mut String,
    processes: &[ProcessSnapshot],
    width: usize,
    available_lines: usize,
    ultra_compact: bool,
    sort: Option<ProcessSort>,
    color: bool,
    truecolor: bool,
) {
    let sorted;
    let processes = match sort {
        Some(sort) => {
            sorted = sort_processes(processes, sort);
            &sorted[..]
        }
        None => processes,
    };
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
    push_panel_top(frame, &title_plain, &title_styled, None, width, color, truecolor);

    let inner = width.saturating_sub(2);
    let columns = process_columns(inner);
    if !ultra_compact {
        // Header sits one line below the top border (both 1-based rows).
        PROCESS_HEADER_ROW.store(count_newlines(frame) as u32 + 1, Ordering::Relaxed);
        let header = process_header(&columns, sort, color, truecolor);
        push_panel_line(frame, &header, inner, color, truecolor);
    }

    // Ultra-compact mode spends only top+bottom on chrome, preserving three
    // real process rows in a five-line panel. Normal mode also shows the header.
    let chrome_lines = if ultra_compact { 2 } else { 3 };
    let rows_available = available_lines.saturating_sub(chrome_lines);
    let reserve_summary = processes.len() > rows_available && rows_available >= 4;
    let data_rows = if reserve_summary {
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
    ultra_compact: bool,
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
    push_panel_top(frame, &title_plain, &title_styled, None, width, color, truecolor);

    let inner = width.saturating_sub(2);
    let columns = holder_columns(inner);

    if !ultra_compact {
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
            summary_plain
        };
        push_panel_line(frame, &summary, inner, color, truecolor);

        let header = holder_header(&columns, color, truecolor);
        push_panel_line(frame, &header, inner, color, truecolor);
    }

    // In ultra-compact mode top+bottom are the only chrome, so four lines are
    // enough for two actual holders. Normal mode keeps summary+header and needs
    // six lines for the same two-holder guarantee. Never replace either of the
    // two minimum holder rows with an "… more" summary.
    let chrome_lines = if ultra_compact { 2 } else { 4 };
    let rows_available = available_lines.saturating_sub(chrome_lines);
    let reserve_summary = snapshot.vram_holders.len() > rows_available && rows_available >= 3;
    let data_rows = if reserve_summary {
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

fn process_header(
    c: &ProcessColumns,
    sort: Option<ProcessSort>,
    color: bool,
    truecolor: bool,
) -> String {
    let label = |text: &str, column: SortColumn| match sort {
        Some(s) if s.column == column => {
            format!("{text} {}", if s.descending { '▼' } else { '▲' })
        }
        _ => text.to_string(),
    };
    let plain = format!(
        "{:<gw$}{:<pw$}{:<uw$}{:<kw$}{:<nw$}{:>xw$}{:>mw$}",
        label(" GPU", SortColumn::Gpu),
        label("PID", SortColumn::Pid),
        label("USER", SortColumn::User),
        label("TYPE", SortColumn::Kind),
        label("PROCESS", SortColumn::Name),
        label("GPU%", SortColumn::Util),
        label("VRAM", SortColumn::Memory),
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
    right: Option<(&str, &str)>,
    width: usize,
    color: bool,
    truecolor: bool,
) {
    let right_len = right.map_or(0, |(plain, _)| plain.chars().count() + 3);
    let fill = width.saturating_sub(title_plain.chars().count() + right_len + 5);
    let right_styled = right.map_or(String::new(), |(_, styled)| format!(" {styled} {border}─", border = fg(BORDER, truecolor)));
    let right_plain = right.map_or(String::new(), |(plain, _)| format!(" {plain} ─"));
    if color {
        let border = fg(BORDER, truecolor);
        let _ = writeln!(
            frame,
            "{border}╭─{RESET} {title_styled} {border}{}{right_styled}╮{RESET}",
            "─".repeat(fill)
        );
    } else {
        let _ = writeln!(frame, "╭─ {title_plain} {}{right_plain}╮", "─".repeat(fill));
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
    let content = clip_ansi(content, inner);
    let visible = visible_len_ansi(&content);
    let pad = inner.saturating_sub(visible);
    if color {
        let border = fg(BORDER, truecolor);
        let _ = writeln!(frame, "{border}│{RESET}{content}{}{border}│{RESET}", " ".repeat(pad));
    } else {
        let _ = writeln!(frame, "│{content}{}│", " ".repeat(pad));
    }
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

struct GraphCell<'a> {
    label: &'a str,
    values: Option<&'a VecDeque<f64>>,
    grade: Grade,
    /// One entry per terminal row, shown in the text column left of the graph.
    readings: Vec<(String, Rgb)>,
}

/// Draw several history graphs side by side, sharing the panel width equally.
/// A cell whose `in_border` flag is false has its label and reading overlaid on
/// the top-left corner of the graph (where the oldest samples are).
fn push_graph_row(
    frame: &mut String,
    cells: &[GraphCell],
    in_border: &[bool],
    rows: usize,
    inner: usize,
    color: bool,
    truecolor: bool,
) {
    let rows = rows.max(1);
    let widths = graph_cell_widths(inner, cells.len());
    let separator = style(" │ ", BORDER, color, truecolor);

    let mut columns: Vec<Vec<String>> = Vec::with_capacity(cells.len());
    for ((cell, &cell_w), &titled) in cells.iter().zip(&widths).zip(in_border) {
        let mut grid = graph_cells(cell.values, cell_w, rows, cell.grade);
        if !titled {
            let max_text = cell_w.saturating_sub(cell_w / 3);
            let mut x = 0;
            let mut put = |text: &str, rgb: Rgb| {
                for ch in truncate(text, max_text.saturating_sub(x)).chars() {
                    if x < grid[0].len() {
                        grid[0][x] = (ch, rgb);
                        x += 1;
                    }
                }
            };
            put(cell.label, MUTED);
            put(" ", MUTED);
            if let Some((text, rgb)) = cell.readings.first() {
                put(text, *rgb);
            }
        }
        columns.push(grid.iter().map(|row| cells_to_string(row, color, truecolor)).collect());
    }

    for row in 0..rows {
        let mut line = String::from(" ");
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                line.push_str(&separator);
            }
            line.push_str(&column[row]);
        }
        push_panel_line(frame, &line, inner, color, truecolor);
    }
}

/// A horizontal border (top edge or divider) built as a grid so titles and
/// T-junctions can be written into it.
struct BorderLine {
    cells: Vec<(char, Rgb, bool)>,
}

impl BorderLine {
    fn new(width: usize, left: char, right: char) -> Self {
        let mut cells = vec![('─', BORDER, false); width.max(2)];
        cells[0].0 = left;
        let last = cells.len() - 1;
        cells[last].0 = right;
        Self { cells }
    }

    fn junction(&mut self, index: usize, ch: char) {
        if index > 0 && index < self.cells.len() - 1 {
            self.cells[index].0 = ch;
        }
    }

    /// Write ` text ` (one space of padding each side) starting at `start`.
    /// Fails, changing nothing, unless the whole span is still plain border.
    fn put_titled(&mut self, start: usize, parts: &[(String, Rgb, bool)]) -> bool {
        let len: usize = parts.iter().map(|(t, _, _)| t.chars().count()).sum::<usize>() + 2;
        if start == 0 || start + len > self.cells.len() - 1 {
            return false;
        }
        if self.cells[start..start + len].iter().any(|c| c.0 != '─') {
            return false;
        }
        let mut x = start;
        self.cells[x] = (' ', BORDER, false);
        x += 1;
        for (text, rgb, bold) in parts {
            for ch in text.chars() {
                self.cells[x] = (ch, *rgb, *bold);
                x += 1;
            }
        }
        self.cells[x] = (' ', BORDER, false);
        true
    }

    /// Put each cell's `label reading` title just right of the junction that
    /// starts the cell (cell 0 goes at `first_start`). Returns which fit.
    fn place_cell_titles(
        &mut self,
        cells: &[GraphCell],
        inner: usize,
        first_start: usize,
        first_enabled: bool,
    ) -> Vec<bool> {
        let separators = graph_separator_columns(inner, cells.len());
        cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let start = if i == 0 { first_start } else { separators[i - 1] + 2 };
                let mut parts = vec![(cell.label.to_string(), MUTED, false)];
                if let Some((text, rgb)) = cell.readings.first() {
                    parts.push((" ".to_string(), MUTED, false));
                    parts.push((text.clone(), *rgb, false));
                }
                (i > 0 || first_enabled) && self.put_titled(start, &parts)
            })
            .collect()
    }

    fn render(&self, color: bool, truecolor: bool) -> String {
        let mut line = String::with_capacity(self.cells.len() * 4 + 32);
        let mut current: Option<((u8, u8, u8), bool)> = None;
        for &(ch, rgb, bold) in &self.cells {
            if color && current != Some(((rgb.0, rgb.1, rgb.2), bold)) {
                if current.is_some() {
                    line.push_str(RESET);
                }
                if bold {
                    line.push_str(BOLD);
                }
                line.push_str(&fg(rgb, truecolor));
                current = Some(((rgb.0, rgb.1, rgb.2), bold));
            }
            line.push(ch);
        }
        if color {
            line.push_str(RESET);
        }
        line.push('\n');
        line
    }
}

/// Cell widths for `count` graphs: a leading space, then cells joined by
/// 3-column separators (" │ ").
fn graph_cell_widths(inner: usize, count: usize) -> Vec<usize> {
    let count = count.max(1);
    let available = inner.saturating_sub(1 + 3 * (count - 1));
    let base = available / count;
    let mut widths = vec![base; count];
    widths[count - 1] = available - base * (count - 1);
    widths
}

/// 0-based columns (inside the panel) of the `│` between graph cells.
fn graph_separator_columns(inner: usize, count: usize) -> Vec<usize> {
    let mut x = 1;
    let widths = graph_cell_widths(inner, count);
    widths[..widths.len() - 1]
        .iter()
        .map(|w| {
            // cell, one space, then the rule; the next cell starts after " │ ".
            let column = x + w + 1;
            x = column + 2;
            column
        })
        .collect()
}

#[cfg(test)]
fn history_graph_rows(
    values: Option<&VecDeque<f64>>,
    width: usize,
    rows: usize,
    grade: Grade,
    color: bool,
    truecolor: bool,
) -> Vec<String> {
    graph_cells(values, width, rows, grade)
        .iter()
        .map(|row| cells_to_string(row, color, truecolor))
        .collect()
}

fn cells_to_string(cells: &[(char, Rgb)], color: bool, truecolor: bool) -> String {
    let mut line = String::with_capacity(cells.len() * 4 + 32);
    let mut current: Option<(u8, u8, u8)> = None;
    for &(ch, rgb) in cells {
        if color && current != Some((rgb.0, rgb.1, rgb.2)) {
            line.push_str(&fg(rgb, truecolor));
            current = Some((rgb.0, rgb.1, rgb.2));
        }
        line.push(ch);
    }
    if color && current.is_some() {
        line.push_str(RESET);
    }
    line
}

fn graph_cells(
    values: Option<&VecDeque<f64>>,
    width: usize,
    rows: usize,
    grade: Grade,
) -> Vec<Vec<(char, Rgb)>> {
    // Each terminal row contains 8 vertical sub-levels. For multi-row graphs we
    // keep btop-style row grading: color is tied to height bands. In compact
    // one-line mode, switch to per-sample/column grading so the single row still
    // carries a sense of intensity.
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

    if grade == Grade::Reason {
        // One full-height column per sample; category 0 (no limit) stays blank.
        let column = |value: &f64| match *value as u8 {
            0 => (' ', MUTED),
            category => ('█', cap_color(category)),
        };
        return vec![visible.iter().map(column).collect(); rows];
    }

    if rows == 1 {
        return vec![visible
            .iter()
            .map(|value| {
                let level = ((*value / 100.0) * 8.0).round() as usize;
                (BLOCKS[level.min(8)], grade_rgb(*value, grade))
            })
            .collect()];
    }

    (0..rows)
        .map(|screen_row| {
            // screen_row=0 is the hottest/top band; band_from_bottom=0 is bottom.
            let band_from_bottom = rows - 1 - screen_row;
            let band_low = band_from_bottom as f64 * 100.0 / rows as f64;
            let band_high = (band_from_bottom + 1) as f64 * 100.0 / rows as f64;
            let row_color = grade_rgb((band_low + band_high) * 0.5, grade);
            visible
                .iter()
                .map(|value| {
                    let occupancy = ((*value - band_low) / (band_high - band_low)).clamp(0.0, 1.0);
                    let level = (occupancy * 8.0).round() as usize;
                    (BLOCKS[level.min(8)], row_color)
                })
                .collect()
        })
        .collect()
}

fn grade_rgb(value: f64, grade: Grade) -> Rgb {
    let value = value.clamp(0.0, 100.0);
    match grade {
        Grade::Reason => cap_color(value as u8),
        Grade::Heat => gradient(
            value,
            &[
                (0.0, Rgb(92, 206, 118)),
                (18.0, Rgb(127, 214, 112)),
                (36.0, Rgb(165, 220, 106)),
                (54.0, Rgb(206, 214, 102)),
                (70.0, Rgb(232, 196, 99)),
                (84.0, Rgb(240, 167, 101)),
                (100.0, Rgb(238, 122, 113)),
            ],
        ),
        Grade::Memory => gradient(
            value,
            &[
                (0.0, Rgb(82, 206, 224)),
                (18.0, Rgb(82, 214, 204)),
                (36.0, Rgb(104, 220, 176)),
                (54.0, Rgb(160, 216, 130)),
                (70.0, Rgb(214, 202, 102)),
                (84.0, Rgb(236, 176, 97)),
                (100.0, Rgb(239, 132, 111)),
            ],
        ),
    }
}

fn gradient(value: f64, stops: &[(f64, Rgb)]) -> Rgb {
    debug_assert!(!stops.is_empty());
    let value = value.clamp(0.0, 100.0);
    if value <= stops[0].0 {
        return stops[0].1;
    }
    for pair in stops.windows(2) {
        let (left_at, left_rgb) = pair[0];
        let (right_at, right_rgb) = pair[1];
        if value <= right_at {
            let span = (right_at - left_at).max(f64::EPSILON);
            return mix(left_rgb, right_rgb, (value - left_at) / span);
        }
    }
    stops[stops.len() - 1].1
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

fn format_memory_compact(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * MIB;
    if bytes as f64 >= GIB {
        format!("{:.1}G", bytes as f64 / GIB)
    } else {
        format!("{:.0}M", bytes as f64 / MIB)
    }
}

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    // These values can come from process names and other external text.
    // Replace controls before they reach the terminal, including ESC and LF.
    let sanitized = || value.chars().map(|ch| if ch.is_control() { '?' } else { ch });
    let count = value.chars().count();
    if count <= width {
        return sanitized().collect();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut result: String = sanitized().take(width - 1).collect();
    result.push('…');
    result
}

// Internal styled strings contain CSI sequences; never cut an escape in half.
fn clip_ansi(value: &str, width: usize) -> Cow<'_, str> {
    if visible_len_ansi(value) <= width {
        return Cow::Borrowed(value);
    }
    let mut output = String::new();
    let mut chars = value.chars().peekable();
    let mut visible = 0usize;
    let mut styled = false;
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            styled = true;
            output.push(ch);
            output.push(chars.next().expect("CSI prefix checked"));
            for control in chars.by_ref() {
                output.push(control);
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            if visible >= width {
                break;
            }
            output.push(ch);
            visible += 1;
        }
    }
    if styled {
        output.push_str(RESET);
    }
    Cow::Owned(output)
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
        choose_vertical_layout, format_memory_compact, format_memory_pretty, grade_rgb,
        history_graph_rows, parse_interval_input, process_header_hit, sort_processes, Input, ProcessSort, SortColumn, terminal_size_key, truncate,
        visible_len_ansi, Grade,
    };
    use std::collections::VecDeque;

    #[test]
    fn missing_gpu_utilization_renders_dashes() {
        let mut snapshot = crate::demo::DemoCollector::new().collect();
        snapshot.gpus[0].gpu_util = None;
        let mut history = super::History::new(240);
        history.update(&snapshot);
        let mut frame = String::new();
        super::push_gpu_panel(&mut frame, &snapshot.gpus[0], &history, 100, 1, false, None, false, true);
        let gpu_row = frame.lines().find(|row| row.contains(" gpu ")).expect("gpu title");
        assert!(gpu_row.contains("gpu --"), "{gpu_row:?}");
        assert!(!gpu_row.contains("gpu 0%"), "{gpu_row:?}");
    }

    #[test]
    fn narrow_gpu_panel_keeps_metrics_and_borders_inside_width() {
        let mut snapshot = crate::demo::DemoCollector::new().collect();
        snapshot.gpus[0].name = "A long GPU name that exceeds the available header space".into();
        let mut history = super::History::new(240);
        history.update(&snapshot);
        let mut plain_header = None;
        for color in [false, true] {
            let mut frame = String::new();
            super::push_gpu_panel(&mut frame, &snapshot.gpus[0], &history, 64, 1, false, Some(60000), color, true);
            for row in frame.lines() {
                assert_eq!(visible_len_ansi(row), 64, "panel row exceeded its budget: {row:?}");
                assert!(row.contains('│') || row.contains('╭') || row.contains('╰') || row.contains('├'));
            }
            // The header's printable characters and controls occupy the same
            // columns whether color is enabled or disabled.
            let header = frame.lines().next().unwrap();
            let stripped = strip_ansi(header);
            assert!(stripped.ends_with("- 60000ms + ─╮"));
            if let Some(ref plain) = plain_header {
                assert_eq!(&stripped, plain);
            } else {
                plain_header = Some(stripped);
            }
        }
    }

    fn strip_ansi(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for control in chars.by_ref() {
                    if ('@'..='~').contains(&control) { break; }
                }
            } else {
                output.push(ch);
            }
        }
        output
    }

    #[test]
    fn panel_boundary_clips_styled_content_without_broken_escapes() {
        let mut frame = String::new();
        super::push_panel_line(&mut frame, "\x1b[38;2;1;2;3mabcdefgh\x1b[0m", 4, true, true);
        assert_eq!(visible_len_ansi(frame.trim_end()), 6);
        assert_eq!(strip_ansi(frame.trim_end()), "│abcd│");
        assert!(frame.contains("abcd\x1b[0m"));
        assert_eq!(super::clip_ansi("abcdef", 0), "");
    }

    #[test]
    fn external_text_cannot_emit_terminal_controls() {
        assert_eq!(truncate("a\x1b[2J\nb\rc\td", 50), "a?[2J?b?c?d");
        let mut snapshot = crate::demo::DemoCollector::new().collect();
        snapshot.processes[0].name = "process\x1b[2J\nnext".into();
        snapshot.processes[0].user = "user\rname".into();
        snapshot.vram_holders[0].name = snapshot.processes[0].name.clone();
        snapshot.vram_holders[0].user = snapshot.processes[0].user.clone();
        for color in [false, true] {
            let process = super::process_line(&snapshot.processes[0], &super::process_columns(100), color, true);
            let holder = super::holder_line(&snapshot.vram_holders[0], &super::holder_columns(100), color, true);
            for line in [process, holder] {
                assert!(!line.contains("\x1b[2J"));
                assert!(!line.contains('\n'));
                assert!(!line.contains('\r'));
            }
        }
    }

    #[test]
    fn history_graph_has_requested_rows_and_width() {
        let values = VecDeque::from(vec![0.0, 50.0, 100.0]);
        let output = history_graph_rows(Some(&values), 7, 4, Grade::Heat, true, true);
        assert_eq!(output.len(), 4);
        assert!(output.iter().all(|row| visible_len_ansi(row) == 7));
    }

    #[test]
    fn graph_dividers_line_up_with_vertical_rules() {
        let snapshot = crate::demo::DemoCollector::new().collect();
        let mut history = super::History::new(240);
        history.update(&snapshot);
        for width in [64usize, 80, 99, 119] {
            let mut frame = String::new();
            super::push_gpu_panel(&mut frame, &snapshot.gpus[0], &history, width, 4, false, None, false, true);
            let rows: Vec<Vec<char>> = frame.lines().map(|row| row.chars().collect()).collect();
            let divider = rows.iter().position(|row| row[0] == '├').expect("divider row");
            // rows[1..divider] have 2 graphs, rows[divider + 1..] have 3.
            for (range, count) in [(1..divider, 2), (divider + 1..divider + 5, 3)] {
                let columns = super::graph_separator_columns(width - 2, count);
                for row in &rows[range] {
                    for &column in &columns {
                        assert_eq!(row[column + 1], '│', "{width}: {}", row.iter().collect::<String>());
                    }
                }
            }
            for &column in &super::graph_separator_columns(width - 2, 2) {
                assert!(matches!(rows[divider][column + 1], '┴' | '┼'));
            }
            for &column in &super::graph_separator_columns(width - 2, 3) {
                assert!(matches!(rows[divider][column + 1], '┬' | '┼'));
            }
        }
    }

    #[test]
    fn cap_reasons_get_distinct_colors_and_full_height_columns() {
        use super::{cap_category, cap_color};
        assert_eq!(cap_category("none"), 0);
        assert_eq!(cap_category("power+thermal"), cap_category("power"));
        let reasons = ["idle", "power", "thermal", "hw thermal", "power brake", "hw slowdown", "sync", "app clocks", "display", "reliability", "board limit", "other"];
        let colors: Vec<(u8, u8, u8)> = reasons
            .iter()
            .map(|r| {
                let c = cap_color(cap_category(r));
                (c.0, c.1, c.2)
            })
            .collect();
        for (i, a) in colors.iter().enumerate() {
            assert!(colors[i + 1..].iter().all(|b| a != b), "{reasons:?}");
        }

        let values = VecDeque::from(vec![0.0, 2.0, 3.0]);
        let rows = super::graph_cells(Some(&values), 3, 4, Grade::Reason);
        assert_eq!(rows.len(), 4);
        for row in &rows {
            assert_eq!(row[0].0, ' ');
            assert_eq!(row[1].0, '█');
            assert_eq!(row[2].0, '█');
            assert_ne!((row[1].1 .0, row[1].1 .1), (row[2].1 .0, row[2].1 .1));
        }
    }

    #[test]
    fn quit_key_is_reported() {
        let mut input = b"xq".to_vec();
        assert_eq!(parse_interval_input(&mut input, 500, 79), Some(Input::Key(b'q')));
        assert!(input.is_empty());
    }

    #[test]
    fn mouse_clicks_hit_interval_buttons_only() {
        let mut plus = b"\x1b[<0;76;1M".to_vec();
        assert_eq!(parse_interval_input(&mut plus, 500, 79), Some(Input::Key(b'+')));

        let mut minus = b"\x1b[<0;68;1M".to_vec();
        assert_eq!(parse_interval_input(&mut minus, 500, 79), Some(Input::Key(b'-')));

        let mut outside = b"\x1b[<0;60;1M".to_vec();
        assert_eq!(parse_interval_input(&mut outside, 500, 79), None);
    }

    #[test]
    fn header_clicks_select_sort_columns() {
        use crate::ui::PROCESS_HEADER_ROW;
        PROCESS_HEADER_ROW.store(14, std::sync::atomic::Ordering::Relaxed);
        // width 79 -> inner 77; PID spans 1-based columns 7..16.
        let mut pid = b"\x1b[<0;8;14M".to_vec();
        assert_eq!(parse_interval_input(&mut pid, 500, 79), Some(Input::Sort(SortColumn::Pid)));
        let mut vram = b"\x1b[<0;77;14M".to_vec();
        assert_eq!(parse_interval_input(&mut vram, 500, 79), Some(Input::Sort(SortColumn::Memory)));
        let mut body = b"\x1b[<0;8;15M".to_vec();
        assert_eq!(parse_interval_input(&mut body, 500, 79), None);
        PROCESS_HEADER_ROW.store(0, std::sync::atomic::Ordering::Relaxed);
        assert!(process_header_hit(1, 77).is_none());
    }

    #[test]
    fn sorting_toggles_and_orders_processes() {
        let first = ProcessSort::clicked(None, SortColumn::Memory);
        assert!(first.descending);
        let flipped = ProcessSort::clicked(Some(first), SortColumn::Memory);
        assert!(!flipped.descending);
        assert!(!ProcessSort::clicked(Some(first), SortColumn::Name).descending);

        let snapshot = crate::demo::DemoCollector::new().collect();
        let sorted = sort_processes(&snapshot.processes, first);
        assert!(sorted.windows(2).all(|w| w[0].memory_bytes >= w[1].memory_bytes));
        let sorted = sort_processes(&snapshot.processes, ProcessSort { column: SortColumn::Pid, descending: false });
        assert!(sorted.windows(2).all(|w| w[0].pid <= w[1].pid));
    }

    #[test]
    fn sorted_header_shows_arrow() {
        let header = super::process_header(
            &super::process_columns(77),
            Some(ProcessSort { column: SortColumn::Util, descending: true }),
            false,
            false,
        );
        assert!(header.contains("GPU% ▼"));
        assert!(!header.contains('▲'));
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
    fn compact_history_graph_colors_are_column_based() {
        let values = VecDeque::from(vec![10.0, 55.0, 90.0, 100.0]);
        let output = history_graph_rows(Some(&values), 4, 1, Grade::Heat, true, true);
        assert_eq!(output.len(), 1);
        let row = &output[0];
        // One-line graphs should color each sample independently.
        assert_eq!(row.matches("\x1b[38;").count(), 4);
    }

    #[test]
    fn truncates_unicode_by_characters() {
        assert_eq!(truncate("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn memory_format_is_human_readable() {
        assert_eq!(format_memory_pretty(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_memory_pretty(512 * 1024 * 1024), "512 MiB");
        assert_eq!(format_memory_compact(1024 * 1024 * 1024), "1.0G");
        assert_eq!(format_memory_compact(512 * 1024 * 1024), "512M");
    }

    #[test]
    fn terminal_size_key_changes_with_either_dimension() {
        let base = terminal_size_key(120, 40);
        assert_ne!(base, terminal_size_key(119, 40));
        assert_ne!(base, terminal_size_key(120, 39));
        assert_eq!(base, terminal_size_key(120, 40));
    }


    #[test]
    fn height_layout_preserves_three_processes_and_two_holders_at_minimum() {
        assert_eq!(choose_vertical_layout(12), None);

        let minimum = choose_vertical_layout(13).unwrap();
        assert_eq!(minimum.graph_rows, 1);
        assert!(minimum.show_holders);
        assert!(minimum.ultra_compact);
        // Minimum geometry budgets exactly: GPU 4 + processes 5 + holders 4.
        assert_eq!(4 + 5 + 4, 13);

        let compact = choose_vertical_layout(19).unwrap();
        assert_eq!(compact.graph_rows, 1);
        assert!(compact.show_holders);
        assert!(!compact.ultra_compact);

        let full = choose_vertical_layout(25).unwrap();
        assert_eq!(full.graph_rows, 4);
        assert!(full.show_holders);
        assert!(!full.ultra_compact);
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
}
