#[cfg(not(target_os = "linux"))]
compile_error!("nost currently supports Linux only");

mod demo;
mod model;
mod nvml;
mod procfs;
mod ui;

use demo::DemoCollector;
use nvml::NvmlCollector;
use std::env;
use std::ffi::c_int;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use ui::{History, TerminalGuard};

static RUNNING: AtomicBool = AtomicBool::new(true);
static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);
static SUSPEND_PENDING: AtomicBool = AtomicBool::new(false);

const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;
const SIGWINCH: c_int = 28;
const SIGTSTP: c_int = 20;
const SIG_DFL: usize = 0;
const MIN_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INTERVAL: Duration = Duration::from_secs(60);
const INTERVAL_STEP: Duration = Duration::from_millis(100);

extern "C" {
    #[link_name = "signal"]
    fn c_signal(signal: c_int, handler: usize) -> usize;
    fn raise(signal: c_int) -> c_int;
}

extern "C" fn stop_signal(_signal: c_int) {
    RUNNING.store(false, Ordering::Relaxed);
}

extern "C" fn resize_signal(_signal: c_int) {
    // Do not perform terminal I/O from a signal handler. The render loop polls
    // this flag and redraws within at most one short sleep slice.
    RESIZE_PENDING.store(true, Ordering::Relaxed);
}

extern "C" fn suspend_signal(_signal: c_int) {
    // Terminal I/O is not async-signal-safe; the loop performs the suspend.
    SUSPEND_PENDING.store(true, Ordering::Relaxed);
}

fn suspend_process(terminal: &mut TerminalGuard) -> Result<(), String> {
    terminal.suspend();
    unsafe {
        // Stop with the default action so the shell sees a normal job stop,
        // then reinstall our handler once SIGCONT resumes us here.
        let _ = c_signal(SIGTSTP, SIG_DFL);
        let _ = raise(SIGTSTP);
        let _ = c_signal(SIGTSTP, suspend_signal as *const () as usize);
    }
    terminal
        .resume()
        .map_err(|error| format!("terminal resume failed: {error}"))
}

#[derive(Debug)]
struct Config {
    interval: Duration,
    once: bool,
    inline: bool,
    demo: bool,
    no_color: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            once: false,
            inline: false,
            demo: false,
            no_color: false,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nost: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let config = parse_args()?;
    install_signal_handlers();

    let color = ui::should_use_color(config.no_color);
    let mut history = History::new(240);

    if config.demo {
        let collector = DemoCollector::new();
        let mut terminal = TerminalGuard::enter(config.inline || config.once, !config.once)
            .map_err(|e| e.to_string())?;
        loop_frames(
            &config,
            &mut terminal,
            &mut history,
            || collector.collect(),
            "demo",
            color,
        )?;
    } else {
        // Initialize NVML before switching screens so driver/library failures are
        // printed normally and never leave the terminal in an alternate state.
        let mut collector = NvmlCollector::new().map_err(|error| {
            format!("{error}. Install/load the NVIDIA driver, or use ./run-demo.sh to preview the UI")
        })?;
        let mut terminal = TerminalGuard::enter(config.inline || config.once, !config.once)
            .map_err(|e| e.to_string())?;
        loop_frames(
            &config,
            &mut terminal,
            &mut history,
            || collector.collect(),
            "NVML direct",
            color,
        )?;
    }

    Ok(())
}

fn loop_frames<F>(
    config: &Config,
    terminal: &mut TerminalGuard,
    history: &mut History,
    mut collect: F,
    backend: &str,
    color: bool,
) -> Result<(), String>
where
    F: FnMut() -> model::Snapshot,
{
    let mut interval = config.interval;
    while RUNNING.load(Ordering::Relaxed) {
        // Consume the resize wake-up that brought us here. A new SIGWINCH that
        // arrives during collection/rendering will set the flag again and keep
        // us from sleeping after this frame.
        RESIZE_PENDING.store(false, Ordering::Relaxed);
        if SUSPEND_PENDING.swap(false, Ordering::Relaxed) {
            suspend_process(terminal)?;
        }
        let frame_started = Instant::now();
        let snapshot = collect();
        history.update(&snapshot);
        ui::render(
            &snapshot,
            history,
            interval.as_millis().min(u64::MAX as u128) as u64,
            backend,
            color,
        )
        .map_err(|error| format!("terminal write failed: {error}"))?;

        if config.once {
            break;
        }

        // The UI itself detects changed terminal geometry and performs the
        // hard clear. If SIGWINCH arrived during this frame, sleep_interruptible
        // sees the still-set flag and immediately starts another frame.

        let elapsed = frame_started.elapsed();
        if elapsed < interval {
            sleep_interruptible(interval - elapsed, terminal, &mut interval)?;
        }
    }
    Ok(())
}

fn sleep_interruptible(
    duration: Duration,
    terminal: &mut TerminalGuard,
    interval: &mut Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + duration;
    while RUNNING.load(Ordering::Relaxed) {
        if SUSPEND_PENDING.swap(false, Ordering::Relaxed) {
            suspend_process(terminal)?;
            break; // redraw immediately after resume
        }
        if RESIZE_PENDING.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if let Some(key) = terminal
            .read_key(interval.as_millis().min(u64::MAX as u128) as u64)
            .map_err(|error| format!("terminal input failed: {error}"))?
        {
            if let Some(next) = adjusted_interval(*interval, key) {
                *interval = next;
                break;
            }
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(40)));
    }
    Ok(())
}

fn adjusted_interval(interval: Duration, key: u8) -> Option<Duration> {
    match key {
        b'-' | b'_' => Some(interval.saturating_sub(INTERVAL_STEP).max(MIN_INTERVAL)),
        b'+' | b'=' => Some((interval + INTERVAL_STEP).min(MAX_INTERVAL)),
        _ => None,
    }
}

fn install_signal_handlers() {
    unsafe {
        let handler = stop_signal as *const () as usize;
        let resize_handler = resize_signal as *const () as usize;
        let _ = c_signal(SIGINT, handler);
        let _ = c_signal(SIGTERM, handler);
        let _ = c_signal(SIGWINCH, resize_handler);
        let _ = c_signal(SIGTSTP, suspend_signal as *const () as usize);
    }
}

fn parse_args() -> Result<Config, String> {
    let mut config = Config::default();
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--interval" | "-i" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--interval requires a number of seconds".to_string())?;
                let seconds: f64 = value
                    .parse()
                    .map_err(|_| format!("invalid interval: {value}"))?;
                if !seconds.is_finite() || !(0.1..=60.0).contains(&seconds) {
                    return Err("interval must be between 0.1 and 60 seconds".to_string());
                }
                config.interval = Duration::from_secs_f64(seconds);
            }
            "--once" => config.once = true,
            "--inline" => config.inline = true,
            "--demo" => config.demo = true,
            "--no-color" => config.no_color = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("nost {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => return Err(format!("unknown option: {arg} (try --help)")),
        }
    }

    Ok(config)
}

fn print_help() {
    println!(
        "nost - minimal direct-NVML NVIDIA GPU monitor\n\n\
Usage:\n  nost [options]\n\n\
Options:\n  -i, --interval SECONDS  refresh interval (default: 0.5)\n\
      --once              render one frame and exit\n\
      --inline            use the normal terminal screen\n\
      --demo              animated UI without NVIDIA hardware\n\
      --no-color          disable ANSI colors\n\
  -h, --help              show this help\n\
  -V, --version           show version\n\n\
Input:\n  - / +                 decrease / increase refresh interval by 100 ms\n  left-click - / +      decrease / increase refresh interval by 100 ms\n  Ctrl-Z                suspend (resume with fg)\n  Ctrl-C                quit"
    );
}

#[cfg(test)]
mod tests {
    use super::{adjusted_interval, MAX_INTERVAL, MIN_INTERVAL};
    use std::time::Duration;

    #[test]
    fn interval_keys_adjust_and_clamp() {
        assert_eq!(
            adjusted_interval(Duration::from_millis(500), b'-'),
            Some(Duration::from_millis(400))
        );
        assert_eq!(
            adjusted_interval(Duration::from_millis(500), b'+'),
            Some(Duration::from_millis(600))
        );
        assert_eq!(adjusted_interval(MIN_INTERVAL, b'-'), Some(MIN_INTERVAL));
        assert_eq!(adjusted_interval(MAX_INTERVAL, b'+'), Some(MAX_INTERVAL));
        assert_eq!(adjusted_interval(Duration::from_millis(500), b'x'), None);
    }
}
