//! Live full-screen terminal dashboard (opt-in `tui` feature).
//!
//! A foreground viewer: per-pin current as a bar chart (the balance is read at a glance),
//! totals, the debounced alert state, and a rolling watts sparkline. It reuses the same
//! read/decode/evaluate path, alert lifecycle, and card-pinned bus re-detection as the
//! headless reader, and feeds the same metrics cache (so a configured Prometheus exporter
//! still serves live data) — it just renders to the screen instead of CSV, and doesn't send
//! notifications (you're watching it).
//!
//! `ratatui::init`/`restore` install a panic hook that restores the terminal, so a panic
//! mid-render won't leave the user's terminal in raw mode.

use crate::alert::{evaluate, IMBALANCE_RATIO, OVERLOAD_A};
use crate::config::Config;
use crate::decode::{Reading, PIN_COUNT};
use crate::i2c::{bus_pci_id, read_reading, redetect_card, REDETECT_AFTER};
use crate::lifecycle::{condition_of, Condition, Event, Lifecycle};
use crate::metrics::Metrics;
use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, Paragraph, Sparkline};
use ratatui::Frame;
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Samples of total watts kept for the sparkline.
const HISTORY: usize = 240;
/// Bar value scale: centi-amps, capped a hair above the overload threshold so an overloaded
/// pin visibly pegs the bar.
const AMPS_FULL_SCALE_CENTI: u64 = 1000; // 10.00 A

struct App {
    bus: u32,
    addr: u16,
    /// PCI id of the card we started on; `Some` only when the bus was auto-detected, so
    /// re-detection follows the headless rule of never overriding a pinned `--bus`.
    card: Option<String>,
    label: String,
    reading: Option<Reading>,
    /// Whether the *most recent* sample was usable — distinguishes live data from stale.
    live: bool,
    active: Vec<Condition>,
    watts: VecDeque<u64>,
    status: String,
    misses: u32,
    samples: u64,
    metrics: Option<Arc<Metrics>>,
}

impl App {
    fn push_watts(&mut self, w: u64) {
        if self.watts.len() == HISTORY {
            self.watts.pop_front();
        }
        self.watts.push_back(w);
    }

    fn note_metric_event(&self, ev: &Event) {
        if let Some(m) = &self.metrics {
            m.on_event(ev);
        }
    }
}

/// Run the dashboard until the user quits (`q`/`Esc`/`Ctrl-C`) or a shutdown signal arrives.
pub fn run_tui(
    bus: u32,
    addr: u16,
    interval: Duration,
    cfg: &Config,
    auto: bool,
    metrics: &Option<Arc<Metrics>>,
    shutdown: &AtomicBool,
) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("the tui needs an interactive terminal — use `monitor` (or `log`) for non-interactive output");
    }
    let card = if auto { bus_pci_id(bus) } else { None };
    let mut app = App {
        bus,
        addr,
        label: bus_pci_id(bus).unwrap_or_else(|| format!("i2c-{bus}")),
        card,
        reading: None,
        live: false,
        active: Vec::new(),
        watts: VecDeque::with_capacity(HISTORY),
        status: "starting…".into(),
        misses: 0,
        samples: 0,
        metrics: metrics.clone(),
    };
    let mut lifecycle = Lifecycle::new(cfg.alerts);

    let mut terminal = ratatui::init();
    let res = (|| -> Result<()> {
        // sample immediately, then on each interval; poll keys at a finer cadence so the UI
        // stays responsive even with a long --interval
        let poll_step = interval.min(Duration::from_millis(200));
        let mut next_sample = Instant::now();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            if Instant::now() >= next_sample {
                sample(&mut app, &mut lifecycle, cfg);
                next_sample = Instant::now() + interval;
            }
            terminal.draw(|f| draw(f, &app))?;
            if event::poll(poll_step)? {
                if let TermEvent::Key(k) = event::read()? {
                    let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (k.code == KeyCode::Char('c')
                            && k.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        return Ok(());
                    }
                }
            }
        }
    })();
    ratatui::restore();
    res
}

fn sample(app: &mut App, lifecycle: &mut Lifecycle, cfg: &Config) {
    let mut conditions: Vec<(Condition, String)> = Vec::new();
    match read_reading(app.bus, app.addr) {
        Ok(r) if r.plausible() => {
            app.misses = 0;
            app.live = true;
            let alerts = evaluate(&r, &cfg.thresholds);
            conditions.extend(alerts.iter().map(|a| (condition_of(a), a.to_string())));
            app.push_watts(r.total_watts() as u64);
            app.status = "ok".into();
            app.reading = Some(r);
            if let Some(m) = &app.metrics {
                m.on_good_sample(&r);
            }
        }
        Ok(_) => {
            app.misses += 1;
            app.live = false;
            conditions.push((Condition::TelemetryLost, "implausible reading".into()));
            app.status =
                "implausible reading (chip answered; wrong device or GPU resetting?)".into();
            if let Some(m) = &app.metrics {
                m.on_implausible_sample();
            }
        }
        Err(e) => {
            app.misses += 1;
            app.live = false;
            conditions.push((Condition::TelemetryLost, format!("read failed: {e:#}")));
            app.status = format!("read failed: {e:#}");
            if let Some(m) = &app.metrics {
                m.on_read_error();
            }
        }
    }
    for ev in lifecycle.observe(Instant::now(), &conditions) {
        app.note_metric_event(&ev);
        match ev {
            Event::Raised { condition, .. } => {
                if !app.active.contains(&condition) {
                    app.active.push(condition);
                }
            }
            Event::Resolved { condition, .. } => app.active.retain(|c| *c != condition),
            Event::Repeated { .. } => {}
        }
    }
    app.samples += 1;

    // same card-pinned re-detection as the headless loop — never for a pinned --bus
    if let Some(pci) = &app.card {
        if app.misses >= REDETECT_AFTER {
            app.misses = 0;
            if let Some(b2) = redetect_card(app.addr, pci) {
                app.bus = b2;
                app.label = pci.clone();
            }
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let [header, bars, stats, spark, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(
        Line::from(format!(
            " astral-watch — i2c-{} @ {:#04x} — {} ",
            app.bus, app.addr, app.label
        ))
        .bold()
        .on_dark_gray(),
        header,
    );

    // per-pin current bars — the at-a-glance balance check; dimmed when the data is stale
    let bars_data: Vec<Bar> = (0..PIN_COUNT)
        .map(|i| {
            let amps = app.reading.map(|r| r.pins[i].amps).unwrap_or(0.0);
            let centi = (amps * 100.0) as u64;
            let color = if !app.live {
                Color::DarkGray
            } else if amps > OVERLOAD_A {
                Color::Red
            } else if amps == 0.0 {
                Color::DarkGray
            } else {
                Color::Green
            };
            Bar::default()
                .value(centi.min(AMPS_FULL_SCALE_CENTI))
                .text_value(format!("{amps:.2}A"))
                .label(Line::from(format!("p{}", i + 1)))
                .style(Style::default().fg(color))
        })
        .collect();
    f.render_widget(
        BarChart::default()
            .block(Block::bordered().title(" per-pin current (red >9.2A) "))
            .data(BarGroup::default().bars(&bars_data))
            .bar_width(7)
            .bar_gap(2)
            .max(AMPS_FULL_SCALE_CENTI),
        bars,
    );

    f.render_widget(
        Paragraph::new(stat_lines(app)).block(Block::bordered().title(" status ")),
        stats,
    );

    let spark_data: Vec<u64> = app.watts.iter().copied().collect();
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().title(" total watts "))
            .data(&spark_data)
            .style(Style::default().fg(Color::Cyan)),
        spark,
    );

    f.render_widget(Line::from(" q/Esc quit ").italic().on_dark_gray(), footer);
}

fn stat_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match &app.reading {
        Some(r) => {
            let bal = r
                .balance()
                .map(|b| format!("{b:.2}"))
                .unwrap_or_else(|| "—".into());
            let prefix = if app.live { "" } else { "STALE — " };
            let line = format!(
                "{prefix}total {:.1} A   ~{:.0} W   balance hi/lo {bal} (alarm >{IMBALANCE_RATIO})   samples {}",
                r.total_amps(),
                r.total_watts(),
                app.samples,
            );
            lines.push(if app.live {
                Line::from(line)
            } else {
                Line::from(line).fg(Color::Yellow)
            });
        }
        None => lines.push(Line::from(app.status.clone()).fg(Color::Yellow)),
    }
    // surface the current trouble while showing the last-known totals above
    if !app.live && app.reading.is_some() {
        lines.push(Line::from(app.status.clone()).fg(Color::Yellow));
    }
    if app.active.is_empty() {
        lines.push(Line::from("alerts: none").fg(Color::Green));
    } else {
        let names: Vec<&str> = app.active.iter().map(|c| c.label()).collect();
        lines.push(
            Line::from(format!("ALERTS: {}", names.join(", ")))
                .fg(Color::Red)
                .bold(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Pin;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app_with(amps: [f64; 6]) -> App {
        App {
            bus: 7,
            addr: 0x2b,
            card: None,
            label: "0000:0b:00.0".into(),
            reading: Some(Reading {
                pins: amps.map(|a| Pin {
                    volts: 12.0,
                    amps: a,
                }),
            }),
            live: true,
            active: Vec::new(),
            watts: VecDeque::from(vec![100, 200, 300, 250]),
            status: "ok".into(),
            misses: 0,
            samples: 5,
            metrics: None,
        }
    }

    /// Render the dashboard to an in-memory backend (no TTY) and flatten the cells to text.
    fn screen(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_header_pins_and_totals() {
        let s = screen(&app_with([8.2, 8.6, 8.3, 8.4, 8.5, 8.8]), 80, 24);
        assert!(s.contains("astral-watch"), "{s}");
        assert!(s.contains("0000:0b:00.0"), "card id in header");
        assert!(s.contains("p1") && s.contains("p6"), "per-pin bar labels");
        assert!(s.contains("total") && s.contains("balance"), "stats line");
        assert!(s.contains("alerts: none"), "healthy = no alerts");
    }

    #[test]
    fn renders_active_overload_alert() {
        let mut app = app_with([9.5, 8.0, 8.0, 8.0, 8.0, 8.0]);
        app.active = vec![Condition::Overload];
        let s = screen(&app, 80, 24);
        assert!(s.contains("ALERTS") && s.contains("OVERLOAD"), "{s}");
    }

    #[test]
    fn marks_data_stale_during_an_outage() {
        let mut app = app_with([8.0; 6]);
        app.live = false; // last read failed; totals are last-known
        let s = screen(&app, 80, 24);
        assert!(
            s.contains("STALE"),
            "stale totals must be flagged, not shown as healthy"
        );
    }

    #[test]
    fn tiny_or_degenerate_terminal_does_not_panic() {
        // the layout has two Min() rows; ratatui must shrink, not panic, on a small area
        let app = app_with([8.0; 6]);
        for (w, h) in [(20, 6), (1, 1), (200, 80)] {
            let _ = screen(&app, w, h);
        }
    }
}
