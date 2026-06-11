//! astral-watch CLI: live per-pin display and CSV logging for ASUS ROG Astral GPUs.

use anyhow::{bail, Result};
use astral_watch::alert::evaluate;
use astral_watch::cards::{detect_gpu, model_for, ASUS_VENDOR};
use astral_watch::i2c::{autodetect_bus, nvidia_buses, read_reading, CHIP_ADDR_STR};
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
    #[arg(long, global = true, default_value = CHIP_ADDR_STR, value_parser = parse_u16)]
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
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u16::from_str_radix(h, 16),
        None => s.parse(),
    }
    .map_err(|e| format!("{e}"))
}

/// Validate `--interval`: `Duration::from_secs_f64` panics on negative/NaN, and 0 would
/// busy-loop the GPU's i2c bus.
fn parse_interval(secs: f64) -> Result<Duration> {
    if !secs.is_finite() || secs <= 0.0 {
        bail!("--interval must be a positive number of seconds (got {secs})");
    }
    Ok(Duration::from_secs_f64(secs))
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

    let interval = parse_interval(cli.interval)?;
    if cli.interval < 0.05 {
        eprintln!(
            "# warning: --interval {} hammers the GPU i2c bus (shared with display traffic)",
            cli.interval
        );
    }
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
                print!("{line}\x1b[K");
                std::io::stdout().flush().ok();
            }
            Ok(_) => eprintln!("\n{ts}  *** implausible reading (chip answered; wrong device or GPU resetting?) ***"),
            Err(e) => eprintln!("\n{ts}  *** read failed: {e:#} ***"),
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
    if !max_mb.is_finite() || max_mb < 0.0 {
        bail!("--max-mb must be >= 0 (got {max_mb}); 0 disables rotation");
    }
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
    // A failing CSV write must never kill the watchdog or eat an alert: alerts go to
    // stderr first, write errors degrade to a (de-duplicated) warning and we keep sampling.
    let mut log_failing = false;
    loop {
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let written = match read_reading(bus, addr) {
            Ok(r) if r.plausible() => {
                let alerts = evaluate(&r);
                if !alerts.is_empty() {
                    eprintln!("{ts}  ALERT: {}", join(&alerts));
                }
                log.log(&ts, &r, &alerts)
            }
            Ok(_) => {
                eprintln!("\n{ts}  *** IMPLAUSIBLE_READING ***");
                log.log_unreachable(
                    &ts,
                    "IMPLAUSIBLE_READING (chip answered but data failed sanity checks)",
                )
            }
            Err(e) => {
                eprintln!("\n{ts}  *** GPU_UNREACHABLE ***");
                log.log_unreachable(
                    &ts,
                    &format!(
                        "GPU_UNREACHABLE (read failed: {e:#} - GPU may have fallen off the bus)"
                    ),
                )
            }
        };
        match written {
            Ok(()) => {
                if log_failing {
                    eprintln!("{ts}  csv logging recovered");
                    log_failing = false;
                }
            }
            Err(e) => {
                if !log_failing {
                    eprintln!("{ts}  csv write failed: {e:#} (alerts still reach stderr; retrying every sample)");
                    log_failing = true;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_u16_accepts_hex_and_decimal() {
        assert_eq!(parse_u16("0x2b").unwrap(), 0x2b);
        assert_eq!(parse_u16("0X2B").unwrap(), 0x2b);
        assert_eq!(parse_u16(" 43 ").unwrap(), 43);
        assert!(parse_u16("zz").is_err());
        assert!(parse_u16("0x10000").is_err());
    }

    #[test]
    fn interval_rejects_nonpositive_and_nan() {
        assert!(parse_interval(-1.0).is_err());
        assert!(parse_interval(0.0).is_err());
        assert!(parse_interval(f64::NAN).is_err());
        assert!(parse_interval(f64::INFINITY).is_err());
        assert_eq!(parse_interval(0.5).unwrap(), Duration::from_millis(500));
    }
}
