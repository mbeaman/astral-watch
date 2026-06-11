//! astral-watch CLI: live per-pin display and CSV logging for ASUS ROG Astral GPUs.

use anyhow::{bail, Result};
use astral_watch::alert::evaluate;
use astral_watch::cards::{detect_gpu, model_for, ASUS_VENDOR};
use astral_watch::config::{self, Config};
use astral_watch::i2c::{autodetect_bus, nvidia_buses, read_reading, CHIP_ADDR_STR};
use astral_watch::lifecycle::{condition_of, Condition, Lifecycle};
use astral_watch::logger::CsvLogger;
use astral_watch::notify::{self, Dispatcher};
use chrono::Local;
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

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

    /// config file (default: ~/.config/astral-watch/config.toml, then /etc/astral-watch.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
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

    // config and flag validation first: a typo'd config must fail fast
    let (cfg, cfg_path) = config::load(cli.config.as_deref())?;
    for w in cfg.warnings() {
        eprintln!("# warning: {w}");
    }
    let interval = parse_interval(cli.interval)?;
    if cli.interval < 0.05 {
        eprintln!(
            "# warning: --interval {} hammers the GPU i2c bus (shared with display traffic)",
            cli.interval
        );
    }

    if let Some((pci, sv, sd)) = detect_gpu() {
        let model =
            model_for(sd).unwrap_or("unknown — not in card DB (still works if the chip answers)");
        eprintln!("# GPU {pci}  subsystem {sv:04x}:{sd:04x}  -> {model}");
        if sv != ASUS_VENDOR {
            eprintln!("# note: subsystem vendor isn't ASUS; per-pin telemetry is an ASUS ROG Astral/Matrix feature");
        }
    }

    eprintln!(
        "# config: {}",
        cfg_path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "built-in defaults".into())
    );
    let enabled = cfg.notify.enabled();
    if !enabled.is_empty() {
        eprintln!("# notify: {}", enabled.join(" + "));
    }

    // The dispatcher and lifecycle exist BEFORE bus detection: a watchdog started during
    // an outage (host reboot after a GPU crash, driver loading late, deeply idle card)
    // must wait and tell someone — not exit into a silent systemd restart loop.
    let dispatcher = Dispatcher::from_config(&cfg.notify);
    let mut lifecycle = Lifecycle::new(cfg.alerts);
    let bus = acquire_bus(cli.bus, cli.addr, &mut lifecycle, &dispatcher);
    eprintln!(
        "# i2c-{bus} @ {:#04x}  interval {}s",
        cli.addr, cli.interval
    );

    match cli.cmd.unwrap_or(Cmd::Monitor) {
        Cmd::Monitor => run_monitor(bus, cli.addr, interval, &cfg, &dispatcher, lifecycle),
        Cmd::Log { file, max_mb, keep } => {
            let target = LogTarget {
                file: &file,
                max_mb,
                keep,
            };
            run_log(
                bus,
                cli.addr,
                interval,
                target,
                &cfg,
                &dispatcher,
                lifecycle,
            )
        }
    }
}

/// Detection-retry pause — gentle on the GPU i2c bus, ~15 s to the first notification.
const ACQUIRE_RETRY: Duration = Duration::from_secs(5);

/// Find the telemetry bus, waiting (and alerting through the lifecycle) instead of
/// exiting while no GPU answers. A pinned `--bus` is returned as-is; the run loops
/// handle its read failures the same way.
fn acquire_bus(
    pinned: Option<u32>,
    addr: u16,
    lifecycle: &mut Lifecycle,
    dispatcher: &Dispatcher,
) -> u32 {
    if let Some(b) = pinned {
        return b;
    }
    let mut quiet = false;
    loop {
        if nvidia_buses().is_empty() {
            if !quiet {
                eprintln!("# no NVIDIA i2c buses found — is i2c-dev loaded?  `sudo modprobe i2c-dev`  (will keep checking)");
            }
        } else if let Some(b) = autodetect_bus(addr) {
            return b;
        } else if !quiet {
            eprintln!("# no per-pin telemetry on any NVIDIA bus yet (GPU deeply idle? run under load, or pass --bus N)  (will keep checking)");
        }
        quiet = true;
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let waiting = (
            Condition::TelemetryLost,
            "waiting for GPU telemetry (no readable bus)".to_string(),
        );
        for ev in lifecycle.observe(Instant::now(), std::slice::from_ref(&waiting)) {
            eprintln!("{ts}  {ev}");
            dispatcher.publish(notify::render(&ev, &ts));
        }
        sleep(ACQUIRE_RETRY);
    }
}

fn run_monitor(
    bus: u32,
    addr: u16,
    interval: Duration,
    cfg: &Config,
    dispatcher: &Dispatcher,
    mut lifecycle: Lifecycle,
) -> Result<()> {
    loop {
        let now = Local::now();
        let ts = now.format("%H:%M:%S").to_string();
        // webhook consumers get a full ISO timestamp; the short form is display-only
        let ts_full = now.format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut conditions: Vec<(Condition, String)> = Vec::new();
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
                let alerts = evaluate(&r, &cfg.thresholds);
                conditions.extend(alerts.iter().map(|a| (condition_of(a), a.to_string())));
                if !alerts.is_empty() {
                    line.push_str(&format!("  !! {}", join(&alerts)));
                }
                print!("{line}\x1b[K");
                std::io::stdout().flush().ok();
            }
            Ok(_) => {
                let msg = "implausible reading (chip answered; wrong device or GPU resetting?)";
                conditions.push((Condition::TelemetryLost, msg.into()));
                eprintln!("\n{ts}  *** {msg} ***");
            }
            Err(e) => {
                conditions.push((Condition::TelemetryLost, format!("read failed: {e:#}")));
                eprintln!("\n{ts}  *** read failed: {e:#} ***");
            }
        }
        for ev in lifecycle.observe(Instant::now(), &conditions) {
            eprintln!("\n{ts}  {ev}");
            dispatcher.publish(notify::render(&ev, &ts_full));
        }
        sleep(interval);
    }
}

struct LogTarget<'a> {
    file: &'a str,
    max_mb: f64,
    keep: u32,
}

fn run_log(
    bus: u32,
    addr: u16,
    interval: Duration,
    target: LogTarget,
    cfg: &Config,
    dispatcher: &Dispatcher,
    mut lifecycle: Lifecycle,
) -> Result<()> {
    if !target.max_mb.is_finite() || target.max_mb < 0.0 {
        bail!(
            "--max-mb must be >= 0 (got {}); 0 disables rotation",
            target.max_mb
        );
    }
    let max_bytes = (target.max_mb * 1024.0 * 1024.0) as u64;
    let mut log = CsvLogger::open(target.file, max_bytes, target.keep)?;
    eprintln!(
        "# logging -> {}  (Ctrl-C to stop){}",
        target.file,
        if max_bytes > 0 {
            format!("  rotate>{}MB keep={}", target.max_mb, target.keep)
        } else {
            String::new()
        }
    );
    // Per-sample alert text goes to the CSV (forensics); stderr and notifications carry the
    // debounced lifecycle events. A failing CSV write must never kill the watchdog: it
    // degrades to a deduplicated warning, and while the CSV is unwritable the per-sample
    // record is mirrored to stderr so the forensic trail survives a full disk.
    let mut log_failing = false;
    loop {
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut conditions: Vec<(Condition, String)> = Vec::new();
        let (written, sample_note) = match read_reading(bus, addr) {
            Ok(r) if r.plausible() => {
                let alerts = evaluate(&r, &cfg.thresholds);
                conditions.extend(alerts.iter().map(|a| (condition_of(a), a.to_string())));
                let note = (!alerts.is_empty()).then(|| format!("ALERT: {}", join(&alerts)));
                (log.log(&ts, &r, &alerts), note)
            }
            Ok(_) => {
                let msg = "implausible reading (chip answered but data failed sanity checks)";
                conditions.push((Condition::TelemetryLost, msg.into()));
                let written = log.log_unreachable(
                    &ts,
                    "IMPLAUSIBLE_READING (chip answered but data failed sanity checks)",
                );
                (written, Some("IMPLAUSIBLE_READING".to_string()))
            }
            Err(e) => {
                conditions.push((Condition::TelemetryLost, format!("read failed: {e:#}")));
                let written = log.log_unreachable(
                    &ts,
                    &format!(
                        "GPU_UNREACHABLE (read failed: {e:#} - GPU may have fallen off the bus)"
                    ),
                );
                (
                    written,
                    Some(format!("GPU_UNREACHABLE (read failed: {e:#})")),
                )
            }
        };
        for ev in lifecycle.observe(Instant::now(), &conditions) {
            eprintln!("{ts}  {ev}");
            dispatcher.publish(notify::render(&ev, &ts));
        }
        match written {
            Ok(()) => {
                if log_failing {
                    eprintln!("{ts}  csv logging recovered");
                    log_failing = false;
                }
            }
            Err(e) => {
                if !log_failing {
                    eprintln!("{ts}  csv write failed: {e:#} (mirroring per-sample alerts to stderr until logging recovers)");
                    log_failing = true;
                }
                if let Some(note) = sample_note {
                    eprintln!("{ts}  {note}");
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
