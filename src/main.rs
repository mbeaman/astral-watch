//! astral-watch CLI: live display, CSV logging, and Prometheus export for ASUS ROG
//! Astral GPUs — one sampling loop feeding whichever sinks the mode enables.

use anyhow::{bail, Context, Result};
use astral_watch::alert::evaluate;
use astral_watch::cards::{gpu_at, nvidia_gpus};
use astral_watch::config::{self, Config};
use astral_watch::exporter;
use astral_watch::i2c::{
    bus_pci_id, detect_bus, read_reading, redetect_card, Detect, CHIP_ADDR_STR,
};
use astral_watch::lifecycle::{condition_of, Condition, Lifecycle};
use astral_watch::logger::CsvLogger;
use astral_watch::metrics::Metrics;
use astral_watch::notify::{self, Dispatcher};
use chrono::Local;
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Time the notify queues get to drain on SIGTERM/SIGINT before the process exits — enough
/// for a small backlog against a slow-but-responsive endpoint, well under systemd's stop
/// timeout. Workers skip retry backoff while draining (see [`notify::Dispatcher::shutdown`]).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Consecutive unusable samples on an auto-detected bus before re-running detection — covers
/// the GPU resetting and the kernel re-enumerating the i2c bus under a new number.
const REDETECT_AFTER: u32 = 10;

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
    /// Serve Prometheus metrics (no CSV); monitor/log modes can also export via config
    Export {
        /// listen address for GET /metrics (default: config [export].listen, else 127.0.0.1:9942)
        #[arg(long)]
        listen: Option<String>,
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
    let cmd = cli.cmd.unwrap_or(Cmd::Monitor);

    // SIGTERM (systemctl stop) and SIGINT (Ctrl-C) flip this; the loops return promptly and
    // flush queued notifications instead of being killed mid-delivery
    let shutdown = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, Arc::clone(&shutdown))
            .context("registering signal handler")?;
    }

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

    let gpus = nvidia_gpus();
    for g in &gpus {
        let model = g
            .model()
            .unwrap_or("unknown — not in card DB (still works if the chip answers)");
        eprintln!(
            "# GPU {}  subsystem {:04x}:{:04x}  -> {model}",
            g.pci, g.subsystem_vendor, g.subsystem_device
        );
    }
    if !gpus.is_empty() && !gpus.iter().any(|g| g.is_asus()) {
        eprintln!(
            "# note: no ASUS GPU here; per-pin telemetry is an ASUS ROG Astral/Matrix feature"
        );
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

    // The exporter binds before bus detection so scrapes see up=0 (not connection
    // refused) while we wait for a GPU.
    let cfg_listen = cfg.export.as_ref().map(|e| e.listen.clone());
    let export_listen = match &cmd {
        Cmd::Export { listen } => Some(
            listen
                .clone()
                .or(cfg_listen)
                .unwrap_or_else(|| "127.0.0.1:9942".into()),
        ),
        _ => cfg_listen,
    };
    let metrics = match export_listen {
        Some(listen) => {
            let metrics = Arc::new(Metrics::new());
            match exporter::spawn(&listen, Arc::clone(&metrics)) {
                Ok(addr) => {
                    eprintln!("# metrics: http://{addr}/metrics");
                    Some(metrics)
                }
                // in export mode the listener IS the deliverable — fail fast; in
                // monitor/log the watchdog must keep sampling without it
                Err(e) if matches!(cmd, Cmd::Export { .. }) => return Err(e),
                Err(e) => {
                    eprintln!("# warning: metrics exporter disabled: {e:#} (watchdog continues)");
                    None
                }
            }
        }
        None => None,
    };

    // The dispatcher and lifecycle exist BEFORE bus detection: a watchdog started during
    // an outage (host reboot after a GPU crash, driver loading late, deeply idle card)
    // must wait and tell someone — not exit into a silent systemd restart loop.
    let mut dispatcher = Dispatcher::from_config(&cfg.notify);
    let mut lifecycle = Lifecycle::new(cfg.alerts);
    let Some(bus) = acquire_bus(
        cli.bus,
        cli.addr,
        &mut lifecycle,
        &dispatcher,
        &metrics,
        &shutdown,
    ) else {
        dispatcher.shutdown(SHUTDOWN_GRACE);
        return Ok(());
    };
    eprintln!(
        "# i2c-{bus} @ {:#04x}  interval {}s",
        cli.addr, cli.interval
    );
    // name the card actually backing this bus (not just the first VGA), and flag the
    // multi-GPU case where only one of several cards is being watched
    if let Some(pci) = bus_pci_id(bus) {
        let model = gpu_at(&pci)
            .and_then(|g| g.model())
            .unwrap_or("unknown SKU");
        eprintln!("# monitoring {pci} ({model})");
        if gpus.len() > 1 {
            eprintln!(
                "# note: {} NVIDIA GPUs present — only {pci} is monitored; pass --bus N to pick another",
                gpus.len()
            );
        }
    }

    let csv = match &cmd {
        Cmd::Log { file, max_mb, keep } => {
            if !max_mb.is_finite() || *max_mb < 0.0 {
                bail!("--max-mb must be >= 0 (got {max_mb}); 0 disables rotation");
            }
            let max_bytes = (max_mb * 1024.0 * 1024.0) as u64;
            let log = CsvLogger::open(file, max_bytes, *keep)?;
            eprintln!(
                "# logging -> {file}  (Ctrl-C to stop){}",
                if max_bytes > 0 {
                    format!("  rotate>{max_mb}MB keep={keep}")
                } else {
                    String::new()
                }
            );
            Some(log)
        }
        _ => None,
    };

    let sinks = Sinks {
        display: matches!(cmd, Cmd::Monitor),
        csv,
        metrics,
    };
    // only re-detect the bus if we picked it; a pinned --bus is the user's explicit choice
    let auto = cli.bus.is_none();
    run(
        bus,
        cli.addr,
        interval,
        &cfg,
        &dispatcher,
        lifecycle,
        sinks,
        auto,
        &shutdown,
    )?;
    eprintln!("# shutting down — flushing notifications");
    dispatcher.shutdown(SHUTDOWN_GRACE);
    Ok(())
}

/// Sleep up to `dur`, returning early once shutdown is signalled — polled in small steps so
/// SIGTERM/SIGINT is honored within ~200 ms even with a long `--interval`.
fn interruptible_sleep(dur: Duration, shutdown: &AtomicBool) {
    let step = Duration::from_millis(200);
    let mut left = dur;
    while !left.is_zero() && !shutdown.load(Ordering::Relaxed) {
        let nap = left.min(step);
        sleep(nap);
        left -= nap;
    }
}

/// Detection-retry pause — gentle on the GPU i2c bus, ~15 s to the first notification.
const ACQUIRE_RETRY: Duration = Duration::from_secs(5);

/// Find the telemetry bus, waiting (and alerting through the lifecycle) instead of
/// exiting while no GPU answers. A pinned `--bus` is returned as-is; the run loop
/// handles its read failures the same way. Returns `None` if shutdown is signalled
/// before a bus is found.
fn acquire_bus(
    pinned: Option<u32>,
    addr: u16,
    lifecycle: &mut Lifecycle,
    dispatcher: &Dispatcher,
    metrics: &Option<Arc<Metrics>>,
    shutdown: &AtomicBool,
) -> Option<u32> {
    if let Some(b) = pinned {
        return Some(b);
    }
    // announce each distinct cause once; it can change between iterations (driver loads,
    // GPU comes under load), so re-announce when it does
    let mut announced: Option<&'static str> = None;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return None;
        }
        let hint = match detect_bus(addr) {
            Detect::Found(b) => return Some(b),
            Detect::NoBuses => {
                "no NVIDIA i2c buses found — is i2c-dev loaded?  try `sudo modprobe i2c-dev`"
            }
            Detect::PermissionDenied => {
                "permission denied opening /dev/i2c-* — run with sudo, or `sudo make install` \
                 (creates the `i2c` group + udev rule); after that, add your user to the `i2c` \
                 group and re-login to run without sudo"
            }
            Detect::NoTelemetry => {
                "no per-pin telemetry on any NVIDIA bus yet — GPU deeply idle? run under load, \
                 or pass --bus N"
            }
        };
        if announced != Some(hint) {
            eprintln!("# {hint}  (will keep checking)");
            announced = Some(hint);
        }
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let waiting = (
            Condition::TelemetryLost,
            format!("waiting for GPU telemetry: {hint}"),
        );
        for ev in lifecycle.observe(Instant::now(), std::slice::from_ref(&waiting)) {
            if let Some(m) = metrics {
                m.on_event(&ev);
            }
            eprintln!("{ts}  {ev}");
            dispatcher.publish(notify::render(&ev, &ts));
        }
        interruptible_sleep(ACQUIRE_RETRY, shutdown);
    }
}

/// Where each sample goes; the loop itself is mode-agnostic.
struct Sinks {
    /// Live refreshing terminal line.
    display: bool,
    /// Per-sample forensic CSV.
    csv: Option<CsvLogger>,
    /// Prometheus cache (shared with the exporter thread).
    metrics: Option<Arc<Metrics>>,
}

/// The sampling loop: read → evaluate → feed sinks → debounce → notify.
///
/// Per-sample alert text goes to the CSV (forensics); stderr and notifications carry the
/// debounced lifecycle events. A failing CSV write must never kill the watchdog: it
/// degrades to a deduplicated warning, and while the CSV is unwritable the per-sample
/// record is mirrored to stderr so the forensic trail survives a full disk.
#[allow(clippy::too_many_arguments)]
fn run(
    mut bus: u32,
    addr: u16,
    interval: Duration,
    cfg: &Config,
    dispatcher: &Dispatcher,
    mut lifecycle: Lifecycle,
    mut sinks: Sinks,
    auto: bool,
    shutdown: &AtomicBool,
) -> Result<()> {
    let mut log_failing = false;
    let mut misses = 0u32; // consecutive unusable samples, for bus re-detection
                           // pin re-detection to the card we started on, so a renumber after a GPU reset can't
                           // migrate the watchdog onto a different Astral (which would falsely resolve the alert)
    let card = if auto { bus_pci_id(bus) } else { None };
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        let now = Local::now();
        let ts = now.format("%Y-%m-%dT%H:%M:%S").to_string();
        // the refreshing display line uses the short form; everything durable gets ISO
        let ts_disp = now.format("%H:%M:%S").to_string();
        let mut conditions: Vec<(Condition, String)> = Vec::new();
        let mut csv_result: Result<()> = Ok(());
        let mut sample_note: Option<String> = None;

        match read_reading(bus, addr) {
            Ok(r) if r.plausible() => {
                misses = 0;
                let alerts = evaluate(&r, &cfg.thresholds);
                conditions.extend(alerts.iter().map(|a| (condition_of(a), a.to_string())));
                if let Some(m) = &sinks.metrics {
                    m.on_good_sample(&r);
                }
                if sinks.display {
                    let mut line = format!("\r{ts_disp}  ");
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
                    if !alerts.is_empty() {
                        line.push_str(&format!("  !! {}", join(&alerts)));
                    }
                    print!("{line}\x1b[K");
                    std::io::stdout().flush().ok();
                }
                if let Some(log) = &mut sinks.csv {
                    sample_note = (!alerts.is_empty()).then(|| format!("ALERT: {}", join(&alerts)));
                    csv_result = log.log(&ts, &r, &alerts);
                }
            }
            Ok(_) => {
                misses += 1;
                // monitor keeps its v0.2.0 wording (interactive diagnosis hint)
                let msg = if sinks.display {
                    "implausible reading (chip answered; wrong device or GPU resetting?)"
                } else {
                    "implausible reading (chip answered but data failed sanity checks)"
                };
                conditions.push((Condition::TelemetryLost, msg.into()));
                if let Some(m) = &sinks.metrics {
                    m.on_implausible_sample();
                }
                if sinks.display {
                    eprintln!("\n{ts_disp}  *** {msg} ***");
                }
                if let Some(log) = &mut sinks.csv {
                    csv_result = log.log_unreachable(
                        &ts,
                        "IMPLAUSIBLE_READING (chip answered but data failed sanity checks)",
                    );
                    sample_note = Some("IMPLAUSIBLE_READING".to_string());
                }
            }
            Err(e) => {
                misses += 1;
                conditions.push((Condition::TelemetryLost, format!("read failed: {e:#}")));
                if let Some(m) = &sinks.metrics {
                    m.on_read_error();
                }
                if sinks.display {
                    eprintln!("\n{ts_disp}  *** read failed: {e:#} ***");
                }
                if let Some(log) = &mut sinks.csv {
                    csv_result = log.log_unreachable(
                        &ts,
                        &format!(
                            "GPU_UNREACHABLE (read failed: {e:#} - GPU may have fallen off the bus)"
                        ),
                    );
                    sample_note = Some(format!("GPU_UNREACHABLE (read failed: {e:#})"));
                }
            }
        }

        for ev in lifecycle.observe(Instant::now(), &conditions) {
            if let Some(m) = &sinks.metrics {
                m.on_event(&ev);
            }
            if sinks.display {
                eprintln!("\n{ts_disp}  {ev}");
            } else {
                eprintln!("{ts}  {ev}");
            }
            dispatcher.publish(notify::render(&ev, &ts));
        }

        match csv_result {
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

        // Sustained failure on an auto-detected bus may mean the GPU reset and the kernel
        // renumbered the i2c bus — re-run the same scoped probe to reattach. Throttled to
        // once per REDETECT_AFTER misses; never for a pinned --bus.
        if auto && misses >= REDETECT_AFTER {
            misses = 0;
            // restrict to the original card's PCI id when we know it; fall back to a plain
            // probe only if identity was unavailable (single-GPU best effort)
            let found = match &card {
                Some(pci) => redetect_card(addr, pci),
                None => match detect_bus(addr) {
                    Detect::Found(b) => Some(b),
                    _ => None,
                },
            };
            if let Some(b2) = found {
                if b2 != bus {
                    eprintln!("{ts}  i2c bus changed {bus} -> {b2}, reattached");
                }
                bus = b2;
            }
        }

        interruptible_sleep(interval, shutdown);
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
