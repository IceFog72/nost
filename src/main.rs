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
use std::thread;
use std::time::{Duration, Instant};
use ui::{History, TerminalGuard};

static RUNNING: AtomicBool = AtomicBool::new(true);

const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;

extern "C" {
    #[link_name = "signal"]
    fn c_signal(signal: c_int, handler: usize) -> usize;
}

extern "C" fn stop_signal(_signal: c_int) {
    RUNNING.store(false, Ordering::Relaxed);
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
        let _terminal = TerminalGuard::enter(config.inline || config.once).map_err(|e| e.to_string())?;
        loop_frames(
            &config,
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
        let _terminal = TerminalGuard::enter(config.inline || config.once).map_err(|e| e.to_string())?;
        loop_frames(
            &config,
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
    history: &mut History,
    mut collect: F,
    backend: &str,
    color: bool,
) -> Result<(), String>
where
    F: FnMut() -> model::Snapshot,
{
    while RUNNING.load(Ordering::Relaxed) {
        let frame_started = Instant::now();
        let snapshot = collect();
        history.update(&snapshot);
        ui::render(
            &snapshot,
            history,
            config.interval.as_millis().min(u64::MAX as u128) as u64,
            backend,
            color,
        )
        .map_err(|error| format!("terminal write failed: {error}"))?;

        if config.once {
            break;
        }

        let elapsed = frame_started.elapsed();
        if elapsed < config.interval {
            sleep_interruptible(config.interval - elapsed);
        }
    }
    Ok(())
}

fn sleep_interruptible(duration: Duration) {
    let deadline = Instant::now() + duration;
    while RUNNING.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

fn install_signal_handlers() {
    unsafe {
        let handler = stop_signal as usize;
        let _ = c_signal(SIGINT, handler);
        let _ = c_signal(SIGTERM, handler);
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
Input:\n  nost does not read stdin, enable raw mode, or register mouse/keyboard events.\n  Ctrl-C is handled as SIGINT only."
    );
}
