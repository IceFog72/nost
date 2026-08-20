use crate::model::{GpuSnapshot, ProcessKind, ProcessSnapshot, Snapshot};
use std::time::Instant;

pub struct DemoCollector {
    started: Instant,
}

impl DemoCollector {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    pub fn collect(&self) -> Snapshot {
        let t = self.started.elapsed().as_secs_f64();
        let wave = |base: f64, amplitude: f64, speed: f64, phase: f64| -> u32 {
            (base + amplitude * (t * speed + phase).sin())
                .round()
                .clamp(0.0, 100.0) as u32
        };

        let gpu_util = wave(62.0, 24.0, 0.72, 0.0);
        let memory_used = ((2.6 + 0.35 * (t * 0.23).sin()) * 1024.0 * 1024.0 * 1024.0) as u64;
        let total = 6 * 1024 * 1024 * 1024u64;

        let gpus = vec![GpuSnapshot {
            index: 0,
            name: "NVIDIA GeForce RTX 3060 Laptop GPU".to_string(),
            gpu_util,
            memory_total: total,
            memory_used,
            temperature_c: Some((55.0 + 7.0 * (t * 0.31).sin()).round() as u32),
            power_mw: Some((62_000.0 + 18_000.0 * (t * 0.41).sin()) as u32),
            power_limit_mw: Some(105_000),
            fan_percent: Some(wave(44.0, 12.0, 0.28, 1.0)),
            graphics_clock_mhz: Some((1665.0 + 240.0 * (t * 0.47).sin()) as u32),
            memory_clock_mhz: Some(7001),
        }];

        let mut processes = vec![
            ProcessSnapshot {
                gpu_index: 0,
                pid: 14122,
                name: "llama-server".to_string(),
                user: "ice".to_string(),
                kind: ProcessKind::Compute,
                gpu_util: Some(wave(46.0, 18.0, 0.81, 0.3)),
                memory_bytes: Some(1720 * 1024 * 1024),
            },
            ProcessSnapshot {
                gpu_index: 0,
                pid: 3915,
                name: "firefox".to_string(),
                user: "ice".to_string(),
                kind: ProcessKind::Graphics,
                gpu_util: Some(wave(13.0, 9.0, 1.11, 1.7)),
                memory_bytes: Some(486 * 1024 * 1024),
            },
            ProcessSnapshot {
                gpu_index: 0,
                pid: 2271,
                name: "kwin_wayland".to_string(),
                user: "ice".to_string(),
                kind: ProcessKind::Graphics,
                gpu_util: Some(wave(8.0, 5.0, 1.37, 2.2)),
                memory_bytes: Some(214 * 1024 * 1024),
            },
            ProcessSnapshot {
                gpu_index: 0,
                pid: 15581,
                name: "python comfyui.py".to_string(),
                user: "ice".to_string(),
                kind: ProcessKind::ComputeGraphics,
                gpu_util: Some(wave(5.0, 5.0, 0.91, 4.2)),
                memory_bytes: Some(362 * 1024 * 1024),
            },
        ];

        processes.sort_by(|a, b| {
            b.gpu_util
                .unwrap_or(0)
                .cmp(&a.gpu_util.unwrap_or(0))
                .then_with(|| b.memory_bytes.unwrap_or(0).cmp(&a.memory_bytes.unwrap_or(0)))
        });

        Snapshot { gpus, processes }
    }
}
