//! astral-watch CLI: live per-pin display and CSV logging for ASUS ROG Astral GPUs.

use anyhow::{bail, Result};
use astral_watch::alert::evaluate;
use astral_watch::cards::{detect_gpu, model_for, ASUS_VENDOR};
use astral_watch::i2c::{autodetect_bus, nvidia_buses, read_reading};
use astral_watch::logger::CsvLogger;
use chrono::Local;
use clap::{Parser, Subcommand};
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "astral-watch",
    version,
    about = "Per-pin 12V-2x6 power monitor for ASUS ROG Astral GPUs"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// i2c bus number (default: auto-detect the NVIDIA bus carrying the chip)
    #[arg(long, global = true)]
    bus: Option<u32>,

    /// i2c address of the telemetry chip (decimal or 0x-hex)
    #[arg(long, global = true, default_value = "0x2b", value_parser = parse_u16)]
    addr: u16,

    /// seconds between samples
    #[arg(long, global = true, default_value_t = 1.0)]
    interval: f64,
}

#[derive(Subcommand)]
enum Cmd {
    /// Live refreshing per-pin display (default)
    Monitor,
    /// Append per-pin readings to a CSV (auto-rotating) with overload/imbalance alerts
    Log {
        /// output CSV path
        #[arg(default_value = "gpu-pins.csv")]
        file: String,
        /// rotate once the log passes this many MB (0 disables rotation)
        #[arg(long, default_value_t = 50.0)]
        max_mb: f64,
        /// number of rotated backups to keep
        #[arg(long, default_value_t = 5)]
        keep: u32,
    },
}

fn parse_u16(s: &str) -> Result<u16, String> {
    let s = s.trim();
    match s.strip_prefix("0x") {
        Some(h) => u16::from_str_radix(h, 16),
        None => s.parse(),
    }
    .map_err(|e| format!("{e}"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some((pci, sv, sd)) = detect_gpu() {
        let model =
            model_for(sd).unwrap_or("unknown — not in card DB (still works if the chip answers)");
        eprintln!("# GPU {pci}  subsystem {sv:04x}:{sd:04x}  -> {model}");
        if sv != ASUS_VENDOR {
            eprintln!("# note: subsystem vendor isn't ASUS; per-pin telemetry is an ASUS ROG Astral/Matrix feature");
        }
    }

    if nvidia_buses().is_empty() {
        bail!("no NVIDIA i2c buses found — is i2c-dev loaded?  `sudo modprobe i2c-dev`");
    }

    let bus = match cli.bus {
        Some(b) => b,
        None => autodetect_bus(cli.addr).ok_or_else(|| {
            anyhow::anyhow!(
                "no valid per-pin telemetry on any NVIDIA bus (GPU deeply idle? run under load, or pass --bus N)"
            )
        })?,
    };
    eprintln!(
        "# i2c-{bus} @ {:#04x}  interval {}s",
        cli.addr, cli.interval
    );

    let interval = Duration::from_secs_f64(cli.interval);
    match cli.cmd.unwrap_or(Cmd::Monitor) {
        Cmd::Monitor => run_monitor(bus, cli.addr, interval),
        Cmd::Log { file, max_mb, keep } => run_log(bus, cli.addr, interval, &file, max_mb, keep),
    }
}

fn run_monitor(bus: u32, addr: u16, interval: Duration) -> Result<()> {
    loop {
        let ts = Local::now().format("%H:%M:%S");
        match read_reading(bus, addr) {
            Ok(r) if r.plausible() => {
                let mut line = format!("\r{ts}  ");
                for (i, p) in r.pins.iter().enumerate() {
                    line.push_str(&format!("p{} {:5.2}V {:5.2}A  ", i + 1, p.volts, p.amps));
                }
                let bal = r
                    .balance()
                    .map(|b| format!("{b:.2}"))
                    .unwrap_or_else(|| "-".into());
                line.push_str(&format!(
                    "| {:5.1}A ~{:4.0}W bal {bal}",
                    r.total_amps(),
                    r.total_watts()
                ));
                let alerts = evaluate(&r);
                if !alerts.is_empty() {
                    line.push_str(&format!("  !! {}", join(&alerts)));
                }
                print!("{line}    ");
                std::io::stdout().flush().ok();
            }
            _ => eprintln!("\n{ts}  *** GPU unreachable (read failed) ***"),
        }
        sleep(interval);
    }
}

fn run_log(
    bus: u32,
    addr: u16,
    interval: Duration,
    file: &str,
    max_mb: f64,
    keep: u32,
) -> Result<()> {
    let max_bytes = (max_mb * 1024.0 * 1024.0) as u64;
    let mut log = CsvLogger::open(file, max_bytes, keep)?;
    eprintln!(
        "# logging -> {file}  (Ctrl-C to stop){}",
        if max_bytes > 0 {
            format!("  rotate>{max_mb}MB keep={keep}")
        } else {
            String::new()
        }
    );
    loop {
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        match read_reading(bus, addr) {
            Ok(r) if r.plausible() => {
                let alerts = evaluate(&r);
                log.log(&ts, &r, &alerts)?;
                if !alerts.is_empty() {
                    eprintln!("{ts}  ALERT: {}", join(&alerts));
                }
            }
            _ => {
                log.log_unreachable(
                    &ts,
                    "GPU_UNREACHABLE (i2c read failed - GPU may have fallen off the bus)",
                )?;
                eprintln!("\n{ts}  *** GPU_UNREACHABLE ***");
            }
        }
        sleep(interval);
    }
}

fn join(alerts: &[astral_watch::alert::Alert]) -> String {
    alerts
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}
