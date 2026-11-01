// SPDX-License-Identifier: GPL-2.0-only
//! xtop — an htop-style monitor for Intel GPUs.
//!
//! Standalone: reads hwmon, GT sysfs, DRM fdinfo and the xe query ioctl
//! directly. No xpu-smi, Level Zero or root required.

mod app;
mod collector;
mod device;
mod fdinfo;
mod sysfs;
mod theme;
mod ui;
mod util;
mod xequery;

use std::process::exit;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use app::App;
use device::Device;

const USAGE: &str = "\
xtop — htop for Intel GPUs (xe / i915)

USAGE:
    xtop [OPTIONS]

OPTIONS:
    -i, --interval <MS>    Refresh interval in milliseconds [default: 1000]
    -d, --device <ID|BDF>  Start on this GPU (index from --list, or PCI address)
    -l, --list             List detected GPUs and exit
    -s, --snapshot         Print one plain-text sample and exit
    -h, --help             Show this help
    -V, --version          Show version

KEYS (in the TUI): ? for help, q to quit.";

struct Args {
    interval_ms: u64,
    device: Option<String>,
    list: bool,
    snapshot: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        interval_ms: 1000,
        device: None,
        list: false,
        snapshot: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or(format!("{name} needs a value"))
        };
        match flag.as_str() {
            "-i" | "--interval" => {
                let v = value("--interval")?;
                a.interval_ms = v.parse().map_err(|_| format!("invalid interval '{v}'"))?;
                a.interval_ms = a.interval_ms.clamp(100, 10_000);
            }
            "-d" | "--device" => a.device = Some(value("--device")?),
            "-l" | "--list" => a.list = true,
            "-s" | "--snapshot" => a.snapshot = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                exit(0);
            }
            "-V" | "--version" => {
                println!("xtop {}", env!("CARGO_PKG_VERSION"));
                exit(0);
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    Ok(a)
}

fn pick_device(devices: &[Device], sel: &Option<String>) -> Result<usize, String> {
    let Some(s) = sel else { return Ok(0) };
    if let Ok(i) = s.parse::<usize>() {
        return if i < devices.len() {
            Ok(i)
        } else {
            Err(format!("no device {i} (see --list)"))
        };
    }
    let s = s.to_lowercase();
    devices
        .iter()
        .position(|d| d.bdf == s || d.bdf.ends_with(&s) || d.card == s)
        .ok_or(format!("no device matching '{s}' (see --list)"))
}

fn main() {
    let args = parse_args().unwrap_or_else(|e| {
        eprintln!("xtop: {e}\n\n{USAGE}");
        exit(2);
    });
    let devices = device::discover();
    if devices.is_empty() {
        eprintln!("xtop: no Intel GPU bound to xe or i915 found under /sys/class/drm");
        exit(1);
    }
    if args.list {
        for (i, d) in devices.iter().enumerate() {
            println!(
                "{i}  {}  {}  {:<5} {}",
                d.bdf,
                d.card,
                d.driver.name(),
                d.name
            );
        }
        return;
    }
    let initial = pick_device(&devices, &args.device).unwrap_or_else(|e| {
        eprintln!("xtop: {e}");
        exit(2);
    });
    if args.snapshot {
        snapshot(&devices[initial], args.interval_ms);
        return;
    }
    if let Err(e) = run_tui(devices, initial, args.interval_ms) {
        eprintln!("xtop: {e}");
        exit(1);
    }
}

fn run_tui(devices: Vec<Device>, initial: usize, interval_ms: u64) -> std::io::Result<()> {
    let interval = Arc::new(AtomicU64::new(interval_ms));
    let detail = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let (ctl_tx, ctl_rx) = mpsc::channel();

    collector::spawn_sampler(
        devices.clone(),
        interval.clone(),
        detail.clone(),
        stop.clone(),
        tx.clone(),
        ctl_rx,
    );
    let mut app = App::new(devices, initial, interval, detail, ctl_tx);

    let mut terminal = ratatui::init();
    collector::spawn_input(tx);
    let result = (|| {
        loop {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            let Ok(msg) = rx.recv() else { break };
            app.update(msg);
            // Coalesce bursts (key repeat, multi-device samples) into one frame.
            while let Ok(m) = rx.try_recv() {
                app.update(m);
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    ratatui::restore();
    result
}

/// Non-interactive text output: two samples `interval_ms` apart so rates exist.
fn snapshot(dev: &Device, interval_ms: u64) {
    use util::{fmt_bytes, fmt_kib};
    let detail = Arc::new(AtomicBool::new(true));
    let mut sampler = sysfs::SysfsSampler::new(dev.clone(), detail);
    let mut scanner = fdinfo::FdinfoScanner::new();
    let bdfs = vec![dev.bdf.clone()];
    scanner.scan(&bdfs);
    sampler.sample();
    std::thread::sleep(Duration::from_millis(interval_ms));
    let mut procs = scanner.scan(&bdfs).remove(&dev.bdf).unwrap_or_default();
    let hw = sampler.sample();

    println!(
        "{}  {}  {}  {}",
        dev.name,
        dev.bdf,
        dev.driver.name(),
        dev.card
    );
    let gpu = procs.engine_totals.values().cloned().fold(0.0, f64::max);
    let mut eng: Vec<String> = procs
        .engine_names
        .iter()
        .map(|e| {
            format!(
                "{} {:.1}%",
                fdinfo::engine_label(e),
                procs.engine_totals.get(e).unwrap_or(&0.0)
            )
        })
        .collect();
    if eng.is_empty() {
        eng.push("no visible clients".into());
    }
    println!("  GPU      {gpu:.1}%   [{}]", eng.join(", "));
    if let Some((used, total, vis)) = hw.vram() {
        println!(
            "  VRAM     {} / {} ({:.1}%), CPU-visible {}",
            fmt_bytes(used as f64),
            fmt_bytes(total as f64),
            used as f64 / total.max(1) as f64 * 100.0,
            fmt_bytes(vis as f64)
        );
    }
    let temps: Vec<String> = hw
        .temps
        .iter()
        .filter(|t| !t.is_vram_channel())
        .map(|t| format!("{} {:.0}°C", t.label, t.celsius))
        .collect();
    println!("  Temp     {}", temps.join(", "));
    let ch: Vec<f64> = hw
        .temps
        .iter()
        .filter(|t| t.is_vram_channel())
        .map(|t| t.celsius)
        .collect();
    if !ch.is_empty() {
        let lo = ch.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = ch.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        println!("  VRAM ch  {} channels, {lo:.0}–{hi:.0}°C", ch.len());
    }
    for r in &hw.rails {
        let mut s = format!("  Power    {:<5}", r.label);
        match r.watts {
            Some(w) => s.push_str(&format!(" {w:.1} W")),
            None => s.push_str(" n/a"),
        }
        if let Some(c) = r.cap_w {
            s.push_str(&format!("  (cap {c:.0} W)"));
        }
        println!("{s}");
    }
    for (n, rpm) in &hw.fans {
        println!("  Fan      {n} {rpm} RPM");
    }
    for g in &hw.gts {
        println!(
            "  gt{} {:<6} act {} MHz, req {} MHz, range {}–{} MHz, awake {}, throttle {}",
            g.id,
            g.kind.label(),
            g.act_mhz.unwrap_or(0),
            g.cur_mhz.map(|v| v.to_string()).unwrap_or("?".into()),
            g.rpn_mhz.unwrap_or(0),
            g.rp0_mhz.unwrap_or(0),
            g.awake_pct
                .map(|v| format!("{v:.1}%"))
                .unwrap_or("?".into()),
            if g.throttle_reasons.is_empty() {
                "none".into()
            } else {
                g.throttle_reasons.join(" ")
            },
        );
    }
    procs.procs.sort_by_key(|p| std::cmp::Reverse(p.vram_kib));
    if !procs.procs.is_empty() {
        println!(
            "\n  {:>7} {:<10} {:>6} {:>8} {:>8} {:>6}  COMMAND",
            "PID", "USER", "GPU%", "VRAM", "GTT", "CPU%"
        );
        for p in &procs.procs {
            let cmd: String = p.cmdline.chars().take(60).collect();
            println!(
                "  {:>7} {:<10} {:>5.1}% {:>8} {:>8} {:>6.1}  {}",
                p.pid,
                p.user.chars().take(10).collect::<String>(),
                p.gpu_pct(),
                fmt_kib(p.vram_kib),
                fmt_kib(p.gtt_kib),
                p.cpu_pct.unwrap_or(0.0),
                cmd
            );
        }
    }
}
