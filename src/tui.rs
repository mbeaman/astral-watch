//! Live full-screen terminal dashboard (opt-in `tui` feature).
//!
//! A focused connector-health view: per-pin current as both an at-a-glance bar chart and a
//! divergence trend chart (the melt signal is one pin drifting away from the others over
//! time), with peak-hold, a balance gauge + history, a watts sparkline, a debounced alert
//! event log, and a full-width alarm banner. Multi-GPU systems get a tab per card.
//!
//! It reuses the shared read/decode/evaluate path, the alert lifecycle, the card-pinned bus
//! re-detection, and the metrics cache (so a configured exporter still serves live data) — it
//! renders to the screen instead of CSV, and doesn't send notifications (you're watching it).
//!
//! Keys: `q`/`Esc` quit · `space` pause · `r` reset peaks · `+`/`-` sample rate ·
//! `Tab`/`shift-Tab` switch card · `?` help. `ratatui::init`/`restore` install a panic hook
//! that restores the terminal; the loop bails cleanly when stdout isn't a TTY.

use crate::alert::{evaluate, IMBALANCE_RATIO, MIN_LOAD_A, OVERLOAD_A};
use crate::cards::gpu_at;
use crate::config::Config;
use crate::decode::{Reading, PIN_COUNT};
use crate::i2c::{bus_pci_id, nvidia_buses, read_reading, redetect_card, REDETECT_AFTER};
use crate::lifecycle::{condition_of, Condition, Event, Lifecycle};
use crate::metrics::Metrics;
use anyhow::{bail, Result};
use chrono::Local;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Bar, BarChart, BarGroup, Block, Chart, Clear, Dataset, GraphType, LineGauge, List,
    ListItem, Paragraph, Sparkline, Wrap,
};
use ratatui::Frame;
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Samples kept for the trend chart / sparklines.
const HISTORY: usize = 300;
/// Alert-log lines retained.
const LOG_CAP: usize = 200;
/// Bar/axis headroom above the overload threshold (amps).
const AMPS_CEILING: f64 = 10.0;
/// Interval clamp for the +/- keys.
const MIN_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INTERVAL: Duration = Duration::from_secs(5);

/// Distinct per-pin colors for the trend chart (6 pins).
const PIN_COLORS: [Color; PIN_COUNT] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightRed,
];

/// One monitorable card (a GPU answering with plausible telemetry).
#[derive(Clone)]
struct CardTab {
    bus: u32,
    pci: String,
    model: String,
}

impl CardTab {
    fn title(&self) -> String {
        format!("{} ({})", self.model, self.pci)
    }
}

struct LogLine {
    ts: String,
    text: String,
    color: Color,
}

struct App {
    addr: u16,
    cards: Vec<CardTab>,
    selected: usize,
    /// Re-detect only when the bus was auto-detected (never overrides a pinned `--bus`).
    auto: bool,
    metrics: Option<Arc<Metrics>>,

    reading: Option<Reading>,
    live: bool,
    status: String,
    active: Vec<Condition>,

    // history (cleared on a card switch)
    pin_hist: [VecDeque<f64>; PIN_COUNT],
    pin_peak: [f64; PIN_COUNT],
    balance_hist: VecDeque<u64>, // centi-units for the sparkline
    watts_hist: VecDeque<u64>,
    peak_watts: f64,
    peak_balance: f64,

    log: VecDeque<LogLine>,

    // interaction
    paused: bool,
    show_help: bool,
    interval: Duration,

    misses: u32,
    samples: u64,
}

impl App {
    fn current_bus(&self) -> u32 {
        self.cards[self.selected].bus
    }

    fn card_pci(&self) -> Option<String> {
        // re-detect target: only when auto-detected
        self.auto.then(|| self.cards[self.selected].pci.clone())
    }

    fn clear_history(&mut self) {
        for h in &mut self.pin_hist {
            h.clear();
        }
        self.pin_peak = [0.0; PIN_COUNT];
        self.balance_hist.clear();
        self.watts_hist.clear();
        self.peak_watts = 0.0;
        self.peak_balance = 0.0;
        self.reading = None;
        self.live = false;
    }

    fn reset_peaks(&mut self) {
        self.pin_peak = [0.0; PIN_COUNT];
        self.peak_watts = 0.0;
        self.peak_balance = 0.0;
        self.push_log("peaks reset", Color::Gray);
    }

    fn push_log(&mut self, text: impl Into<String>, color: Color) {
        if self.log.len() == LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(LogLine {
            ts: Local::now().format("%H:%M:%S").to_string(),
            text: text.into(),
            color,
        });
    }

    fn push_hist(&mut self, r: &Reading) {
        for i in 0..PIN_COUNT {
            let a = r.pins[i].amps;
            let h = &mut self.pin_hist[i];
            if h.len() == HISTORY {
                h.pop_front();
            }
            h.push_back(a);
            if a > self.pin_peak[i] {
                self.pin_peak[i] = a;
            }
        }
        let w = r.total_watts();
        if self.watts_hist.len() == HISTORY {
            self.watts_hist.pop_front();
        }
        self.watts_hist.push_back(w as u64);
        if w > self.peak_watts {
            self.peak_watts = w;
        }
        if let Some(b) = r.balance() {
            if self.balance_hist.len() == HISTORY {
                self.balance_hist.pop_front();
            }
            self.balance_hist.push_back((b * 100.0) as u64);
            if b > self.peak_balance {
                self.peak_balance = b;
            }
        }
    }

    /// Returns true to quit.
    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        // quit works from anywhere, including with the help overlay open
        if matches!(code, KeyCode::Char('q'))
            || (matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL))
        {
            return true;
        }
        if self.show_help {
            self.show_help = false; // any other key dismisses help
            return false;
        }
        match code {
            KeyCode::Esc => return true,
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                self.push_log(if self.paused { "paused" } else { "resumed" }, Color::Gray);
            }
            KeyCode::Char('r') => self.reset_peaks(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.interval = (self.interval / 2).max(MIN_INTERVAL);
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                self.interval = (self.interval * 2).min(MAX_INTERVAL);
            }
            KeyCode::Tab | KeyCode::Right => self.switch_card(1),
            KeyCode::BackTab | KeyCode::Left => self.switch_card(-1),
            _ => {}
        }
        false
    }

    fn switch_card(&mut self, dir: isize) {
        if self.cards.len() < 2 {
            return;
        }
        let n = self.cards.len() as isize;
        self.selected = (((self.selected as isize + dir) % n + n) % n) as usize;
        self.clear_history();
        self.misses = 0;
        let t = self.cards[self.selected].title();
        self.push_log(format!("viewing {t}"), Color::Gray);
    }
}

/// Discover monitorable cards: every NVIDIA bus that answers with plausible telemetry, one
/// per physical card (deduped by PCI id). A pinned `--bus` yields exactly that bus.
fn discover_cards(addr: u16, auto: bool, bus: u32) -> Vec<CardTab> {
    let mk = |bus: u32| {
        let pci = bus_pci_id(bus).unwrap_or_else(|| format!("i2c-{bus}"));
        let model = gpu_at(&pci)
            .and_then(|g| g.model())
            .unwrap_or("unknown SKU")
            .to_string();
        CardTab { bus, pci, model }
    };
    if !auto {
        return vec![mk(bus)];
    }
    let mut out: Vec<CardTab> = Vec::new();
    for b in nvidia_buses() {
        let plausible = read_reading(b, addr)
            .map(|r| r.plausible())
            .unwrap_or(false);
        if plausible {
            let pci = bus_pci_id(b).unwrap_or_else(|| format!("i2c-{b}"));
            if !out.iter().any(|c| c.pci == pci) {
                out.push(mk(b));
            }
        }
    }
    if out.is_empty() {
        out.push(mk(bus)); // fall back to the acquired bus
    }
    out
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
    let cards = discover_cards(addr, auto, bus);
    let mut app = App {
        addr,
        cards,
        selected: 0,
        auto,
        metrics: metrics.clone(),
        reading: None,
        live: false,
        status: "starting…".into(),
        active: Vec::new(),
        pin_hist: std::array::from_fn(|_| VecDeque::with_capacity(HISTORY)),
        pin_peak: [0.0; PIN_COUNT],
        balance_hist: VecDeque::with_capacity(HISTORY),
        watts_hist: VecDeque::with_capacity(HISTORY),
        peak_watts: 0.0,
        peak_balance: 0.0,
        log: VecDeque::with_capacity(LOG_CAP),
        paused: false,
        show_help: false,
        interval,
        misses: 0,
        samples: 0,
    };
    app.push_log("dashboard started", Color::Gray);
    let mut lifecycle = Lifecycle::new(cfg.alerts);

    let mut terminal = ratatui::init();
    let res = (|| -> Result<()> {
        let mut next_sample = Instant::now();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            if !app.paused && Instant::now() >= next_sample {
                sample(&mut app, &mut lifecycle, cfg);
                next_sample = Instant::now() + app.interval;
            }
            terminal.draw(|f| draw(f, &app))?;
            let poll = app.interval.min(Duration::from_millis(150));
            if event::poll(poll)? {
                if let TermEvent::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Release && app.on_key(k.code, k.modifiers) {
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
    let bus = app.current_bus();
    let mut conditions: Vec<(Condition, String)> = Vec::new();
    match read_reading(bus, app.addr) {
        Ok(r) if r.plausible() => {
            app.misses = 0;
            app.live = true;
            let alerts = evaluate(&r, &cfg.thresholds);
            conditions.extend(alerts.iter().map(|a| (condition_of(a), a.to_string())));
            app.push_hist(&r);
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
            app.status = "implausible reading (wrong device or GPU resetting?)".into();
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
        if let Some(m) = &app.metrics {
            m.on_event(&ev);
        }
        match &ev {
            Event::Raised { condition, .. } => {
                if !app.active.contains(condition) {
                    app.active.push(*condition);
                }
                app.push_log(ev.to_string(), Color::Red);
            }
            Event::Resolved { condition, .. } => {
                app.active.retain(|c| c != condition);
                app.push_log(ev.to_string(), Color::Green);
            }
            Event::Repeated { .. } => app.push_log(ev.to_string(), Color::Yellow),
        }
    }
    app.samples += 1;

    if let Some(pci) = app.card_pci() {
        if app.misses >= REDETECT_AFTER {
            app.misses = 0;
            if let Some(b2) = redetect_card(app.addr, &pci) {
                app.cards[app.selected].bus = b2;
            }
        }
    }
}

// ─────────────────────────── rendering ───────────────────────────

fn draw(f: &mut Frame, app: &App) {
    let alarm = !app.active.is_empty();
    let mut rows = vec![Constraint::Length(if app.cards.len() > 1 { 2 } else { 1 })]; // title (+tabs)
    if alarm {
        rows.push(Constraint::Length(1)); // alarm banner
    }
    rows.push(Constraint::Min(8)); // body
    rows.push(Constraint::Length(1)); // status/key bar
    let areas = Layout::vertical(rows).split(f.area());
    let mut idx = 0;
    draw_title(f, areas[idx], app);
    idx += 1;
    if alarm {
        draw_alarm(f, areas[idx], app);
        idx += 1;
    }
    draw_body(f, areas[idx], app);
    idx += 1;
    draw_keybar(f, areas[idx], app);

    if app.show_help {
        draw_help(f, f.area());
    }
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let card = &app.cards[app.selected];
    let mut spans = vec![
        " astral-watch ".bold().bg(Color::Cyan).fg(Color::Black),
        Span::raw(" "),
        Span::styled(
            format!("i2c-{} @ {:#04x}", card.bus, app.addr),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("  "),
        Span::styled(card.model.clone(), Style::default().fg(Color::White)),
    ];
    if app.paused {
        spans.push("  ⏸ PAUSED".bold().fg(Color::Yellow));
    }
    spans.push(Span::styled(
        format!("   {:.0}ms", app.interval.as_secs_f64() * 1000.0),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Line::from(spans), area);

    if app.cards.len() > 1 {
        let row = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        let mut tabs: Vec<Span> = Vec::new();
        for (i, c) in app.cards.iter().enumerate() {
            let label = format!(" {} ", c.model);
            tabs.push(if i == app.selected {
                Span::styled(
                    label,
                    Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
                )
            } else {
                Span::styled(label, Style::default().fg(Color::Gray))
            });
            tabs.push(Span::raw(" "));
        }
        f.render_widget(Line::from(tabs), row);
    }
}

fn draw_alarm(f: &mut Frame, area: Rect, app: &App) {
    let names: Vec<&str> = app.active.iter().map(|c| c.label()).collect();
    let text = format!("  ⚠  ALERT: {}  ⚠", names.join("  +  "));
    f.render_widget(
        Paragraph::new(text).alignment(Alignment::Center).style(
            Style::default()
                .bg(Color::Red)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
        ),
        area,
    );
}

fn draw_body(f: &mut Frame, area: Rect, app: &App) {
    // responsive: wide => bars+trend on the left, stats/sparks/log on the right;
    // narrow => single column; tiny => just the bars
    if area.height < 8 {
        draw_bars(f, area, app);
        return;
    }
    if area.width >= 110 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(area);
        let [bars, trend] =
            Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(left);
        draw_bars(f, bars, app);
        draw_trend(f, trend, app);
        let [stats, balance, watts, log] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .areas(right);
        draw_stats(f, stats, app);
        draw_balance(f, balance, app);
        draw_watts(f, watts, app);
        draw_log(f, log, app);
    } else {
        let [bars, stats, balance, log] = Layout::vertical([
            Constraint::Min(7),
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .areas(area);
        draw_bars(f, bars, app);
        draw_stats(f, stats, app);
        draw_balance(f, balance, app);
        draw_log(f, log, app);
    }
}

fn pin_color(amps: f64, live: bool) -> Color {
    if !live {
        Color::DarkGray
    } else if amps > OVERLOAD_A {
        Color::Red
    } else if amps > OVERLOAD_A * 0.85 {
        Color::Yellow
    } else if amps == 0.0 {
        Color::DarkGray
    } else {
        Color::Green
    }
}

fn draw_bars(f: &mut Frame, area: Rect, app: &App) {
    let ceil_centi = (AMPS_CEILING * 100.0) as u64;
    let bars: Vec<Bar> = (0..PIN_COUNT)
        .map(|i| {
            let amps = app.reading.map(|r| r.pins[i].amps).unwrap_or(0.0);
            let pk = app.pin_peak[i];
            Bar::default()
                .value(((amps * 100.0) as u64).min(ceil_centi))
                .text_value(format!("{amps:.2} ▲{pk:.2}"))
                .label(Line::from(format!("p{}", i + 1)))
                .style(Style::default().fg(pin_color(amps, app.live)))
        })
        .collect();
    let title = format!(" per-pin current — overload {OVERLOAD_A}A (▲ = session peak) ");
    f.render_widget(
        BarChart::default()
            .block(Block::bordered().title(title))
            .data(BarGroup::default().bars(&bars))
            .bar_width(9)
            .bar_gap(2)
            .max(ceil_centi),
        area,
    );
}

fn draw_trend(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" per-pin trend — divergence precedes a melt ");
    // build per-pin (x,y) series from history
    let series: Vec<Vec<(f64, f64)>> = (0..PIN_COUNT)
        .map(|i| {
            app.pin_hist[i]
                .iter()
                .enumerate()
                .map(|(x, &y)| (x as f64, y))
                .collect()
        })
        .collect();
    let len = app.pin_hist[0].len().max(1) as f64;
    let ymax = app
        .pin_hist
        .iter()
        .flat_map(|h| h.iter().copied())
        .fold(OVERLOAD_A, f64::max)
        .max(1.0);
    let datasets: Vec<Dataset> = (0..PIN_COUNT)
        .map(|i| {
            Dataset::default()
                .name(format!("p{}", i + 1))
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(PIN_COLORS[i]))
                .data(&series[i])
        })
        .collect();
    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(Axis::default().bounds([0.0, len.max(1.0)]))
        .y_axis(Axis::default().bounds([0.0, ymax * 1.05]).labels([
            "0".to_string(),
            format!("{:.0}", ymax / 2.0),
            format!("{ymax:.0}A"),
        ]));
    f.render_widget(chart, area);
}

fn draw_stats(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" totals ");
    let lines = if let Some(r) = &app.reading {
        let vmin = r.pins.iter().map(|p| p.volts).fold(f64::INFINITY, f64::min);
        let vmax = r.pins.iter().map(|p| p.volts).fold(0.0, f64::max);
        let stale = if app.live { "" } else { "STALE " };
        vec![
            Line::from(vec![
                Span::raw(format!("{stale}{:.1} A", r.total_amps())).fg(if app.live {
                    Color::White
                } else {
                    Color::Yellow
                }),
                Span::raw(format!("   ~{:.0} W", r.total_watts())).fg(Color::White),
                Span::raw(format!("   peak {:.0} W", app.peak_watts)).fg(Color::DarkGray),
            ]),
            Line::from(format!(
                "pins {vmin:.2}–{vmax:.2} V   ·   samples {}",
                app.samples
            ))
            .fg(Color::Gray),
        ]
    } else {
        vec![Line::from(app.status.clone()).fg(Color::Yellow)]
    };
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_balance(f: &mut Frame, area: Rect, app: &App) {
    let bal = app.reading.as_ref().and_then(|r| r.balance());
    let ratio = bal
        .map(|b| ((b - 1.0) / (IMBALANCE_RATIO - 1.0)).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let (color, label) = match bal {
        Some(b) if b > IMBALANCE_RATIO => (Color::Red, format!("{b:.2}× IMBALANCED")),
        Some(b) if b > 1.0 + (IMBALANCE_RATIO - 1.0) * 0.66 => (Color::Yellow, format!("{b:.2}×")),
        Some(b) => (Color::Green, format!("{b:.2}×")),
        None => (Color::DarkGray, "—".into()),
    };
    let title = format!(
        " balance hi/lo (alarm >{IMBALANCE_RATIO}×, peak {:.2}) ",
        app.peak_balance
    );
    f.render_widget(
        LineGauge::default()
            .block(Block::bordered().title(title))
            .filled_style(Style::default().fg(color))
            .label(label)
            .ratio(if app.live { ratio } else { 0.0 }),
        area,
    );
}

fn draw_watts(f: &mut Frame, area: Rect, app: &App) {
    let data: Vec<u64> = app.watts_hist.iter().copied().collect();
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().title(" total watts "))
            .data(&data)
            .style(Style::default().fg(Color::Cyan)),
        area,
    );
}

fn draw_log(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" alert log ");
    let rows = area.height.saturating_sub(2) as usize;
    let items: Vec<ListItem> = app
        .log
        .iter()
        .rev()
        .take(rows)
        .rev()
        .map(|l| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", l.ts), Style::default().fg(Color::DarkGray)),
                Span::styled(l.text.clone(), Style::default().fg(l.color)),
            ]))
        })
        .collect();
    f.render_widget(List::new(items).block(block), area);
}

fn draw_keybar(f: &mut Frame, area: Rect, app: &App) {
    let key = |k: &str, d: &str| {
        vec![
            Span::styled(
                format!(" {k} "),
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
            Span::styled(format!(" {d}  "), Style::default().fg(Color::Gray)),
        ]
    };
    let mut spans = Vec::new();
    spans.extend(key("q", "quit"));
    spans.extend(key("space", if app.paused { "resume" } else { "pause" }));
    spans.extend(key("r", "reset peaks"));
    spans.extend(key("+/-", "rate"));
    if app.cards.len() > 1 {
        spans.extend(key("tab", "card"));
    }
    spans.extend(key("?", "help"));
    f.render_widget(Line::from(spans), area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 56.min(area.width.saturating_sub(4));
    let h = 16.min(area.height.saturating_sub(2));
    let [v] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(w)])
        .flex(Flex::Center)
        .areas(v);
    f.render_widget(Clear, popup);
    let lines = vec![
        Line::from("astral-watch — live dashboard".bold()),
        Line::from(""),
        Line::from("  q / Esc      quit"),
        Line::from("  space        pause / resume sampling"),
        Line::from("  r            reset session peaks"),
        Line::from("  + / -        faster / slower sample rate"),
        Line::from("  Tab / ← →    switch GPU (multi-card)"),
        Line::from("  ?            toggle this help"),
        Line::from(""),
        Line::from(vec![
            Span::raw("  thresholds:  overload "),
            Span::styled(format!("{OVERLOAD_A}A"), Style::default().fg(Color::Red)),
            Span::raw(format!("/pin · imbalance {IMBALANCE_RATIO}×")),
        ]),
        Line::from(
            format!("               (alarms gate above {MIN_LOAD_A}A total)").fg(Color::Gray),
        ),
        Line::from(""),
        Line::from("  divergence in the trend chart is the".fg(Color::Gray)),
        Line::from("  early melt signal — watch for a pin".fg(Color::Gray)),
        Line::from("  drifting away from the pack.".fg(Color::Gray)),
    ];
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" help ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Pin;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn card() -> CardTab {
        CardTab {
            bus: 7,
            pci: "0000:0b:00.0".into(),
            model: "ROG Astral RTX 5090".into(),
        }
    }

    fn app(amps: [f64; 6]) -> App {
        let mut a = App {
            addr: 0x2b,
            cards: vec![card()],
            selected: 0,
            auto: true,
            metrics: None,
            reading: Some(Reading {
                pins: amps.map(|x| Pin {
                    volts: 11.97,
                    amps: x,
                }),
            }),
            live: true,
            status: "ok".into(),
            active: Vec::new(),
            pin_hist: std::array::from_fn(|_| VecDeque::new()),
            pin_peak: [0.0; PIN_COUNT],
            balance_hist: VecDeque::new(),
            watts_hist: VecDeque::new(),
            peak_watts: 0.0,
            peak_balance: 0.0,
            log: VecDeque::new(),
            paused: false,
            show_help: false,
            interval: Duration::from_secs(1),
            misses: 0,
            samples: 3,
        };
        if let Some(r) = a.reading {
            a.push_hist(&r);
        }
        a
    }

    fn screen(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        t.backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_core_panels_wide() {
        let s = screen(&app([8.2, 8.6, 8.3, 8.4, 8.5, 8.8]), 130, 40);
        assert!(s.contains("astral-watch"));
        assert!(s.contains("per-pin") && s.contains("trend"));
        assert!(s.contains("balance") && s.contains("watts") && s.contains("totals"));
        assert!(s.contains("alert log"));
        assert!(s.contains("quit") && s.contains("pause")); // keybar
    }

    #[test]
    fn alarm_banner_shows_active_condition() {
        let mut a = app([9.5, 8.0, 8.0, 8.0, 8.0, 8.0]);
        a.active = vec![Condition::Overload];
        let s = screen(&a, 130, 40);
        assert!(s.contains("ALERT") && s.contains("OVERLOAD"));
    }

    #[test]
    fn paused_and_help_render() {
        let mut a = app([8.0; 6]);
        a.paused = true;
        assert!(screen(&a, 130, 40).contains("PAUSED"));
        a.show_help = true;
        let s = screen(&a, 130, 40);
        assert!(s.contains("help") && s.contains("switch GPU"));
    }

    #[test]
    fn multi_card_tabs_render() {
        let mut a = app([8.0; 6]);
        a.cards.push(CardTab {
            bus: 12,
            pci: "0000:17:00.0".into(),
            model: "ROG Astral RTX 5080".into(),
        });
        let s = screen(&a, 130, 40);
        assert!(
            s.contains("5090") && s.contains("5080"),
            "both card tabs: {s}"
        );
        assert!(s.contains("card")); // keybar gains the card hint
    }

    #[test]
    fn keys_drive_state() {
        let mut a = app([8.0; 6]);
        assert!(!a.paused);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(a.paused);
        a.on_key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(a.show_help);
        a.on_key(KeyCode::Char('x'), KeyModifiers::NONE); // any key dismisses help
        assert!(!a.show_help);
        assert!(a.on_key(KeyCode::Char('q'), KeyModifiers::NONE)); // quit
        assert!(a.on_key(KeyCode::Char('c'), KeyModifiers::CONTROL)); // ctrl-c quit
    }

    #[test]
    fn quit_works_even_with_help_open() {
        let mut a = app([8.0; 6]);
        a.show_help = true;
        assert!(a.on_key(KeyCode::Char('q'), KeyModifiers::NONE));
    }

    #[test]
    fn rate_keys_clamp() {
        let mut a = app([8.0; 6]);
        for _ in 0..10 {
            a.on_key(KeyCode::Char('+'), KeyModifiers::NONE);
        }
        assert!(a.interval >= MIN_INTERVAL);
        for _ in 0..10 {
            a.on_key(KeyCode::Char('-'), KeyModifiers::NONE);
        }
        assert!(a.interval <= MAX_INTERVAL);
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let a = app([8.0; 6]);
        for (w, h) in [(130, 40), (80, 24), (40, 10), (20, 6), (1, 1)] {
            let _ = screen(&a, w, h);
        }
    }

    #[test]
    fn empty_history_does_not_panic() {
        // startup state: no reading yet, empty history feeding the trend chart / sparklines
        let mut a = app([8.0; 6]);
        a.clear_history();
        for (w, h) in [(130, 40), (80, 24), (1, 1)] {
            let _ = screen(&a, w, h);
        }
    }
}
