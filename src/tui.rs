//! Live full-screen terminal dashboard (opt-in `tui` feature).
//!
//! A focused connector-health view: per-pin current as both an at-a-glance bar chart (with a
//! 9.2 A limit line and session peak caps) and a divergence trend chart (the melt signal is
//! one pin drifting away from the others over time), plus a balance gauge, a watts sparkline,
//! and a scrollable debounced alert event log. Multi-GPU systems get a tab per card; any
//! panel can be zoomed full-screen.
//!
//! It reuses the shared read/decode/evaluate path, the alert lifecycle, the card-pinned bus
//! re-detection, and the metrics cache (so a configured exporter still serves live data) — it
//! renders to the screen instead of CSV, and doesn't send notifications (you're watching it).
//!
//! Styling is terminal-theme-respecting: emphasis uses reverse-video rather than hardcoded
//! backgrounds, and `NO_COLOR` disables color entirely, so it reads on light and dark.
//!
//! Keys: `q`/`Ctrl-C` quit · `space` pause · `r` reset peaks · `+`/`-` rate · `Tab` card ·
//! `1`-`5` zoom a panel (`0`/`Esc` back) · `↑`/`↓`/wheel scroll the log · `?` help.

use crate::alert::{evaluate, IMBALANCE_RATIO, MIN_LOAD_A, OVERLOAD_A};
use crate::cards::gpu_at;
use crate::config::Config;
use crate::decode::{Reading, PIN_COUNT};
use crate::i2c::{bus_pci_id, norm_pci, nvidia_buses, read_reading, redetect_card, REDETECT_AFTER};
use crate::lifecycle::{condition_of, Condition, Event, Lifecycle};
use crate::metrics::Metrics;
use anyhow::{bail, Result};
use chrono::Local;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Chart, Clear, Dataset, Gauge, GraphType, List, ListItem, Padding,
    Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline, Wrap,
};
use ratatui::Frame;
use std::collections::VecDeque;
use std::fs;
use std::io::IsTerminal;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
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

/// The zoomable panels, in `1`..`5` key order.
const PANELS: [&str; 5] = ["bars", "trend", "balance", "watts", "log"];

/// Color resolver: collapses to the terminal default when `NO_COLOR` is set.
#[derive(Clone, Copy)]
struct Theme {
    color: bool,
}

impl Theme {
    fn c(&self, c: Color) -> Color {
        if self.color {
            c
        } else {
            Color::Reset
        }
    }
    /// A style for emphasis that adapts to light/dark via reverse-video.
    fn badge(&self, c: Color) -> Style {
        if self.color {
            Style::default()
                .fg(c)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }
}

/// Best-effort GPU stats from `nvidia-smi` — the i2c chip can't see util/temp/board power.
#[derive(Clone, Default)]
struct GpuStat {
    pci: String, // normalized "bb:dd.f"
    util: Option<u8>,
    draw_w: Option<f64>,
    limit_w: Option<f64>,
    temp_c: Option<i32>,
    fan: Option<u8>,
}

fn parse_field<T: std::str::FromStr>(s: &str) -> Option<T> {
    let s = s.trim();
    if s.is_empty() || s.starts_with('[') {
        None // [N/A], [Not Supported]
    } else {
        s.parse().ok()
    }
}

fn parse_gpu_csv(out: &str) -> Vec<GpuStat> {
    out.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() < 6 {
                return None;
            }
            Some(GpuStat {
                pci: norm_pci(f[0]),
                util: parse_field(f[1]),
                draw_w: parse_field(f[2]),
                limit_w: parse_field(f[3]),
                temp_c: parse_field(f[4]),
                fan: parse_field(f[5]),
            })
        })
        .collect()
}

const NVSMI_QUERY: &str =
    "--query-gpu=pci.bus_id,utilization.gpu,power.draw,power.limit,temperature.gpu,fan.speed";

fn query_nvidia_smi() -> Option<Vec<GpuStat>> {
    let out = Command::new("nvidia-smi")
        .args([NVSMI_QUERY, "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_gpu_csv(&String::from_utf8_lossy(&out.stdout)))
}

/// Spawn a background poller refreshing GPU stats every ~1.5s. Returns `None` (and starts no
/// thread) when nvidia-smi isn't available — the header then shows connector + sysfs only.
fn spawn_gpu_poller(stop: Arc<AtomicBool>) -> Option<Arc<Mutex<Vec<GpuStat>>>> {
    let initial = query_nvidia_smi()?; // probe; absent/erroring nvidia-smi -> no poller
    let shared = Arc::new(Mutex::new(initial));
    let worker = Arc::clone(&shared);
    let _ = thread::Builder::new()
        .name("gpu-poll".into())
        .spawn(move || loop {
            for _ in 0..10 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(150));
            }
            if let Some(stats) = query_nvidia_smi() {
                *worker.lock().unwrap() = stats;
            }
        });
    Some(shared)
}

/// PCIe generation from a `current_link_speed` string like "16.0 GT/s PCIe".
fn gen_from_speed(s: &str) -> Option<u8> {
    let gts: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some(match gts {
        g if g >= 64.0 => 6,
        g if g >= 32.0 => 5,
        g if g >= 16.0 => 4,
        g if g >= 8.0 => 3,
        g if g >= 5.0 => 2,
        _ => 1,
    })
}

/// Current PCIe link as `("Gen4×16", at_max)` from sysfs; `at_max` is false when down-trained.
fn pcie_link(pci: &str) -> Option<(String, bool)> {
    let base = format!("/sys/bus/pci/devices/{pci}");
    let cur_s = fs::read_to_string(format!("{base}/current_link_speed")).ok()?;
    let cur_w: u16 = fs::read_to_string(format!("{base}/current_link_width"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let cur_gen = gen_from_speed(&cur_s)?;
    let at_max = match (
        fs::read_to_string(format!("{base}/max_link_speed")),
        fs::read_to_string(format!("{base}/max_link_width")),
    ) {
        (Ok(ms), Ok(mw)) => {
            gen_from_speed(&ms) == Some(cur_gen) && mw.trim().parse::<u16>().ok() == Some(cur_w)
        }
        _ => true,
    };
    Some((format!("Gen{cur_gen}×{cur_w}"), at_max))
}

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
    auto: bool,
    metrics: Option<Arc<Metrics>>,
    theme: Theme,
    /// Background nvidia-smi snapshot (one row per GPU); `None` if nvidia-smi is absent.
    gpu: Option<Arc<Mutex<Vec<GpuStat>>>>,

    reading: Option<Reading>,
    live: bool,
    link: Option<(String, bool)>, // current PCIe link, and whether it's at the card's max
    status: String,
    active: Vec<Condition>,

    pin_hist: [VecDeque<f64>; PIN_COUNT],
    pin_peak: [f64; PIN_COUNT],
    watts_hist: VecDeque<u64>,
    peak_watts: f64,
    peak_balance: f64,

    log: VecDeque<LogLine>,
    log_scroll: usize, // lines scrolled back from the newest (0 = tailing)

    paused: bool,
    show_help: bool,
    focus: Option<usize>, // zoomed panel index into PANELS
    interval: Duration,

    misses: u32,
    samples: u64,
}

impl App {
    fn current_bus(&self) -> u32 {
        self.cards[self.selected].bus
    }

    fn card_pci(&self) -> Option<String> {
        self.auto.then(|| self.cards[self.selected].pci.clone())
    }

    /// The latest nvidia-smi row for the card currently being viewed, if any.
    fn gpu_stat(&self) -> Option<GpuStat> {
        let want = norm_pci(&self.cards[self.selected].pci);
        let lock = self.gpu.as_ref()?.lock().ok()?;
        lock.iter().find(|g| g.pci == want).cloned()
    }

    fn refresh_link(&mut self) {
        self.link = pcie_link(&self.cards[self.selected].pci);
    }

    fn clear_history(&mut self) {
        for h in &mut self.pin_hist {
            h.clear();
        }
        self.pin_peak = [0.0; PIN_COUNT];
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
        if self.log_scroll > 0 {
            // keep the viewport anchored while scrolled back, but bounded
            self.log_scroll = (self.log_scroll + 1).min(self.log.len().saturating_sub(1));
        }
    }

    fn scroll_log(&mut self, delta: isize) {
        let max = self.log.len().saturating_sub(1);
        let next = self.log_scroll as isize + delta;
        self.log_scroll = next.clamp(0, max as isize) as usize;
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
            if b > self.peak_balance {
                self.peak_balance = b;
            }
        }
    }

    /// Returns true to quit.
    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        // quit works from anywhere
        if matches!(code, KeyCode::Char('q'))
            || (matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL))
        {
            return true;
        }
        if self.show_help {
            self.show_help = false; // any other key closes help
            return false;
        }
        match code {
            KeyCode::Esc => {
                if self.focus.is_some() {
                    self.focus = None; // Esc first leaves a zoomed panel
                } else {
                    return true;
                }
            }
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
            KeyCode::Char('0') => self.focus = None,
            KeyCode::Char(d @ '1'..='5') => {
                self.focus = Some((d as u8 - b'1') as usize);
            }
            KeyCode::Tab | KeyCode::Right => self.switch_card(1),
            KeyCode::BackTab | KeyCode::Left => self.switch_card(-1),
            KeyCode::Up => self.scroll_log(1),
            KeyCode::Down => self.scroll_log(-1),
            KeyCode::PageUp => self.scroll_log(10),
            KeyCode::PageDown => self.scroll_log(-10),
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
        self.refresh_link();
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
        out.push(mk(bus));
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
    // best-effort GPU stats from nvidia-smi (util/power/temp/fan); None if it isn't installed
    let gpu_stop = Arc::new(AtomicBool::new(false));
    let gpu = spawn_gpu_poller(Arc::clone(&gpu_stop));
    let mut app = App {
        addr,
        cards,
        selected: 0,
        auto,
        metrics: metrics.clone(),
        theme: Theme {
            color: std::env::var_os("NO_COLOR").is_none(),
        },
        gpu,
        reading: None,
        live: false,
        link: None,
        status: "starting…".into(),
        active: Vec::new(),
        pin_hist: std::array::from_fn(|_| VecDeque::with_capacity(HISTORY)),
        pin_peak: [0.0; PIN_COUNT],
        watts_hist: VecDeque::with_capacity(HISTORY),
        peak_watts: 0.0,
        peak_balance: 0.0,
        log: VecDeque::with_capacity(LOG_CAP),
        log_scroll: 0,
        paused: false,
        show_help: false,
        focus: None,
        interval,
        misses: 0,
        samples: 0,
    };
    app.push_log("dashboard started", Color::Gray);
    app.refresh_link();
    let mut lifecycle = Lifecycle::new(cfg.alerts);

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    // ratatui's panic hook restores the screen but not mouse mode — chain ours ahead of it
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        prev_hook(info);
    }));
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
                match event::read()? {
                    // on_key applies its side effect and returns true to quit
                    TermEvent::Key(k)
                        if k.kind != KeyEventKind::Release && app.on_key(k.code, k.modifiers) =>
                    {
                        return Ok(())
                    }
                    TermEvent::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => app.scroll_log(1),
                        MouseEventKind::ScrollDown => app.scroll_log(-1),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    })();
    gpu_stop.store(true, Ordering::Relaxed); // stop the nvidia-smi poller
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn sample(app: &mut App, lifecycle: &mut Lifecycle, cfg: &Config) {
    let bus = app.current_bus();
    app.refresh_link(); // cheap sysfs read; reflects live up/down-train
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

fn set_cell(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if let Some(c) = buf.cell_mut((x, y)) {
        c.set_symbol(ch.encode_utf8(&mut [0u8; 4]));
        c.set_style(style);
    }
}

/// Write `text` centered in the column slot `[x0, x0+slot)` at row `y`.
fn put_centered(buf: &mut Buffer, x0: u16, slot: u16, y: u16, text: &str, style: Style) {
    let tw = text.chars().count() as u16;
    let off = slot.saturating_sub(tw) / 2;
    for (i, ch) in text.chars().enumerate() {
        let x = x0 + off + i as u16;
        if x >= x0 + slot {
            break;
        }
        set_cell(buf, x, y, ch, style);
    }
}

fn panel(theme: &Theme, title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.c(Color::DarkGray)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(theme.c(Color::Gray)),
        ))
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let compact = area.height < 16; // collapse the device panel to one line on short terminals
    let multi = app.cards.len() > 1;
    let alarm = !app.active.is_empty();
    let mut cons = Vec::new();
    if multi {
        cons.push(Constraint::Length(1)); // tabs
    }
    cons.push(Constraint::Length(if compact { 1 } else { 5 })); // header
    if alarm {
        cons.push(Constraint::Length(1)); // alarm banner
    }
    cons.push(Constraint::Min(6)); // body
    cons.push(Constraint::Length(1)); // keybar
    let a = Layout::vertical(cons).split(area);
    let mut i = 0;
    if multi {
        draw_tabs(f, a[i], app);
        i += 1;
    }
    if compact {
        draw_title_compact(f, a[i], app);
    } else {
        draw_device(f, a[i], app);
    }
    i += 1;
    if alarm {
        draw_alarm(f, a[i], app);
        i += 1;
    }
    match app.focus {
        Some(p) => draw_panel(f, a[i], app, p),
        None => draw_body(f, a[i], app),
    }
    i += 1;
    draw_keybar(f, a[i], app);

    if app.show_help {
        draw_help(f, area, app);
    }
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let mut tabs: Vec<Span> = Vec::new();
    for (i, c) in app.cards.iter().enumerate() {
        let label = format!(" {} ", c.model);
        tabs.push(if i == app.selected {
            Span::styled(label, t.badge(Color::Cyan))
        } else {
            Span::styled(label, Style::default().fg(t.c(Color::Gray)))
        });
        tabs.push(Span::raw(" "));
    }
    f.render_widget(Line::from(tabs), area);
}

fn draw_title_compact(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let card = &app.cards[app.selected];
    let mut spans = vec![
        Span::styled(" astral-watch ", t.badge(Color::Cyan)),
        Span::raw(" "),
        Span::styled(card.model.clone(), Style::default().fg(t.c(Color::White))),
    ];
    if let Some((link, at_max)) = &app.link {
        let c = if *at_max { Color::Green } else { Color::Yellow };
        spans.push(Span::styled(
            format!("  PCIe {link}"),
            Style::default().fg(t.c(c)),
        ));
    }
    if app.paused {
        spans.push(Span::styled("  ⏸ PAUSED", t.badge(Color::Yellow)));
    }
    f.render_widget(Line::from(spans), area);
}

fn util_color(_u: u8) -> Color {
    Color::Cyan // utilization is load, not a fault — neutral accent
}
fn pwr_color(frac: f64) -> Color {
    if frac > 0.97 {
        Color::Red
    } else if frac > 0.85 {
        Color::Yellow
    } else {
        Color::Green
    }
}
fn temp_color(c: i32) -> Color {
    if c >= 85 {
        Color::Red
    } else if c >= 75 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// An inline text gauge: `LABEL ████░░ value`.
fn bar_spans(
    t: &Theme,
    label: &str,
    frac: f64,
    color: Color,
    width: usize,
    value: &str,
) -> Vec<Span<'static>> {
    let filled = ((frac.clamp(0.0, 1.0)) * width as f64).round() as usize;
    let filled = filled.min(width);
    vec![
        Span::styled(format!("{label} "), Style::default().fg(t.c(Color::Gray))),
        Span::styled("█".repeat(filled), Style::default().fg(t.c(color))),
        Span::styled(
            "░".repeat(width - filled),
            Style::default().fg(t.c(Color::DarkGray)),
        ),
        Span::styled(format!(" {value}"), Style::default().fg(t.c(Color::White))),
    ]
}

/// The nvtop-style "device" header: identity + PCIe link, GPU/power/temp/fan, connector summary.
fn draw_device(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let block = panel(t, " device ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let rows = Layout::vertical([Constraint::Length(1); 3]).split(inner);
    let card = &app.cards[app.selected];

    // line 1 — identity + PCIe link (yellow when down-trained)
    let mut l0 = vec![
        Span::styled(" astral-watch ", t.badge(Color::Cyan)),
        Span::raw("  "),
        Span::styled(
            card.model.clone(),
            Style::default()
                .fg(t.c(Color::White))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", card.pci),
            Style::default().fg(t.c(Color::DarkGray)),
        ),
    ];
    if let Some((link, at_max)) = &app.link {
        let c = if *at_max { Color::Green } else { Color::Yellow };
        l0.push(Span::styled(
            format!("  PCIe {link}"),
            Style::default().fg(t.c(c)),
        ));
        if !*at_max {
            l0.push(Span::styled(" ↓", Style::default().fg(t.c(Color::Yellow))));
        }
    }
    l0.push(Span::styled(
        format!("  i2c-{} @ {:#04x}", card.bus, app.addr),
        Style::default().fg(t.c(Color::DarkGray)),
    ));
    if let Some(p) = app.focus {
        l0.push(Span::styled(
            format!("  [{}]", PANELS[p]),
            Style::default().fg(t.c(Color::Cyan)),
        ));
    }
    if app.paused {
        l0.push(Span::styled("  ⏸ PAUSED", t.badge(Color::Yellow)));
    }
    f.render_widget(Line::from(l0), rows[0]);

    // line 2 — GPU load / power / temp / fan (nvidia-smi), or a note when unavailable
    let l1 = if let Some(g) = app.gpu_stat() {
        let mut s: Vec<Span> = Vec::new();
        if let Some(u) = g.util {
            s.extend(bar_spans(
                t,
                "GPU",
                u as f64 / 100.0,
                util_color(u),
                10,
                &format!("{u}%"),
            ));
            s.push(Span::raw("   "));
        }
        match (g.draw_w, g.limit_w) {
            (Some(d), Some(l)) if l > 0.0 => {
                s.extend(bar_spans(
                    t,
                    "PWR",
                    d / l,
                    pwr_color(d / l),
                    10,
                    &format!("{d:.0}/{l:.0}W"),
                ));
                s.push(Span::raw("   "));
            }
            (Some(d), _) => s.push(Span::styled(
                format!("PWR {d:.0}W   "),
                Style::default().fg(t.c(Color::White)),
            )),
            _ => {}
        }
        if let Some(c) = g.temp_c {
            s.push(Span::styled(
                format!("{c}°C  "),
                Style::default().fg(t.c(temp_color(c))),
            ));
        }
        if let Some(fan) = g.fan {
            s.push(Span::styled(
                format!("fan {fan}%"),
                Style::default().fg(t.c(Color::Gray)),
            ));
        }
        Line::from(s)
    } else {
        Line::from(Span::styled(
            "GPU stats: nvidia-smi not available (connector data only)",
            Style::default().fg(t.c(Color::DarkGray)),
        ))
    };
    f.render_widget(l1, rows[1]);

    // line 3 — connector telemetry summary
    let l2 = if let Some(r) = &app.reading {
        let bal = r
            .balance()
            .map(|b| format!("{b:.2}×"))
            .unwrap_or_else(|| "—".into());
        let vmin = r.pins.iter().map(|p| p.volts).fold(f64::INFINITY, f64::min);
        let vmax = r.pins.iter().map(|p| p.volts).fold(0.0, f64::max);
        let prefix = if app.live { "" } else { "STALE · " };
        Line::from(Span::styled(
            format!(
                "{prefix}connector  {:.1} A · {:.0} W · balance {bal} · pins {vmin:.2}–{vmax:.2} V",
                r.total_amps(),
                r.total_watts(),
            ),
            Style::default().fg(t.c(if app.live { Color::Gray } else { Color::Yellow })),
        ))
    } else {
        Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(t.c(Color::Yellow)),
        ))
    };
    f.render_widget(l2, rows[2]);
}

fn draw_alarm(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let names: Vec<&str> = app.active.iter().map(|c| c.label()).collect();
    let text = format!("  ⚠  ALERT: {}  ⚠", names.join("  +  "));
    let style = if t.color {
        Style::default()
            .bg(Color::Red)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK)
    } else {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD | Modifier::SLOW_BLINK)
    };
    f.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(style),
        area,
    );
}

fn draw_body(f: &mut Frame, area: Rect, app: &App) {
    if area.height < 6 {
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
            Constraint::Length(4),
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
            Constraint::Length(4),
            Constraint::Min(3),
        ])
        .areas(area);
        draw_bars(f, bars, app);
        draw_stats(f, stats, app);
        draw_balance(f, balance, app);
        draw_log(f, log, app);
    }
}

/// One zoomed panel filling the body.
fn draw_panel(f: &mut Frame, area: Rect, app: &App, p: usize) {
    match p {
        0 => draw_bars(f, area, app),
        1 => draw_trend(f, area, app),
        2 => draw_balance(f, area, app),
        3 => draw_watts(f, area, app),
        _ => draw_log(f, area, app),
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

/// Custom vertical bars with a 9.2 A limit line and per-pin session peak caps.
fn draw_bars(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let block = panel(&t, " per-pin current — limit 9.2A · ▔ peak ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < PIN_COUNT as u16 || inner.height < 3 {
        return;
    }
    let slot = inner.width / PIN_COUNT as u16;
    let bw = slot.saturating_sub(1).max(1);
    let label_y = inner.bottom() - 1;
    let value_y = inner.bottom() - 2;
    let bar_bottom = value_y - 1; // last bar row
    let bar_h = bar_bottom.saturating_sub(inner.y) + 1; // rows for the fill
    let cells_for = |amps: f64| -> u16 {
        let c = (amps / AMPS_CEILING).clamp(0.0, 1.0) * bar_h as f64;
        (c.round() as u16).min(bar_h)
    };
    let buf = f.buffer_mut();
    for i in 0..PIN_COUNT {
        let amps = app.reading.map(|r| r.pins[i].amps).unwrap_or(0.0);
        let color = Style::default().fg(t.c(pin_color(amps, app.live)));
        let x0 = inner.x + slot * i as u16;
        let fill = cells_for(amps);
        for r in 0..fill {
            for col in 0..bw {
                set_cell(buf, x0 + col, bar_bottom - r, '█', color);
            }
        }
        // peak cap
        let peak = app.pin_peak[i];
        if peak > 0.0 {
            let pr = cells_for(peak).saturating_sub(1);
            let pstyle = Style::default()
                .fg(t.c(Color::White))
                .add_modifier(Modifier::DIM);
            for col in 0..bw {
                set_cell(buf, x0 + col, bar_bottom - pr, '▔', pstyle);
            }
        }
        put_centered(buf, x0, slot, value_y, &format!("{amps:.1}"), color);
        put_centered(
            buf,
            x0,
            slot,
            label_y,
            &format!("p{}", i + 1),
            Style::default().fg(t.c(Color::Gray)),
        );
    }
    // the 9.2 A limit line, drawn across all slots
    let lr = cells_for(OVERLOAD_A).saturating_sub(1);
    let ly = bar_bottom - lr;
    let lstyle = Style::default()
        .fg(t.c(Color::Red))
        .add_modifier(Modifier::DIM);
    for x in inner.x..inner.x + slot * PIN_COUNT as u16 {
        // only on empty cells, so the line doesn't punch holes through bar fill
        let empty = buf
            .cell(ratatui::layout::Position { x, y: ly })
            .map(|c| c.symbol() == " ")
            .unwrap_or(false);
        if empty {
            set_cell(buf, x, ly, '┄', lstyle);
        }
    }
}

fn draw_trend(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let block = panel(t, " per-pin trend — divergence precedes a melt ");
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
                .style(Style::default().fg(t.c(PIN_COLORS[i])))
                .data(&series[i])
        })
        .collect();
    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(Axis::default().bounds([0.0, len.max(1.0)]))
        .y_axis(
            Axis::default()
                .bounds([0.0, ymax * 1.05])
                .labels(["0".to_string(), format!("{ymax:.0}A")]),
        );
    f.render_widget(chart, area);
}

fn draw_stats(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let block = panel(t, " totals ");
    let lines = if let Some(r) = &app.reading {
        let vmin = r.pins.iter().map(|p| p.volts).fold(f64::INFINITY, f64::min);
        let vmax = r.pins.iter().map(|p| p.volts).fold(0.0, f64::max);
        let stale = if app.live { "" } else { "STALE " };
        vec![
            Line::from(vec![
                Span::styled(
                    format!("{stale}{:.1} A", r.total_amps()),
                    Style::default().fg(t.c(if app.live {
                        Color::White
                    } else {
                        Color::Yellow
                    })),
                ),
                Span::styled(
                    format!("   ~{:.0} W", r.total_watts()),
                    Style::default().fg(t.c(Color::White)),
                ),
                Span::styled(
                    format!("   peak {:.0} W", app.peak_watts),
                    Style::default().fg(t.c(Color::DarkGray)),
                ),
            ]),
            Line::from(Span::styled(
                format!("pins {vmin:.2}–{vmax:.2} V   ·   samples {}", app.samples),
                Style::default().fg(t.c(Color::Gray)),
            )),
        ]
    } else {
        vec![Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(t.c(Color::Yellow)),
        ))]
    };
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_balance(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let bal = app.reading.as_ref().and_then(|r| r.balance());
    let ratio = bal
        .map(|b| ((b - 1.0) / (IMBALANCE_RATIO - 1.0)).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    // normal / warn / alarm zones, colored green / yellow / red
    let (color, status) = match bal {
        Some(b) if b > IMBALANCE_RATIO => (Color::Red, "ALARM"),
        Some(b) if b > 1.0 + (IMBALANCE_RATIO - 1.0) * 0.66 => (Color::Yellow, "WARN"),
        Some(_) => (Color::Green, "NORMAL"),
        None => (Color::DarkGray, "idle"),
    };
    let label = match bal {
        Some(b) => format!("{b:.2}×  {status}"),
        None => "—  idle".into(),
    };
    let title = format!(
        " balance hi/lo · alarm >{IMBALANCE_RATIO}× · peak {:.2}× ",
        app.peak_balance
    );
    f.render_widget(
        Gauge::default()
            .block(panel(t, &title))
            .gauge_style(Style::default().fg(t.c(color)))
            .ratio(if app.live { ratio } else { 0.0 })
            .label(Span::styled(
                label,
                Style::default().add_modifier(Modifier::BOLD),
            )),
        area,
    );
}

fn draw_watts(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let data: Vec<u64> = app.watts_hist.iter().copied().collect();
    f.render_widget(
        Sparkline::default()
            .block(panel(t, " total watts "))
            .data(&data)
            .style(Style::default().fg(t.c(Color::Cyan))),
        area,
    );
}

fn draw_log(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let block = panel(t, " alert log ");
    let inner = block.inner(area);
    let rows = inner.height as usize;
    let total = app.log.len();
    // newest at the bottom; scroll back at most `total - rows` so the window never shrinks
    // below a full page or blanks out
    let max_scroll = total.saturating_sub(rows);
    let s = app.log_scroll.min(max_scroll);
    let end = total - s;
    let start = end.saturating_sub(rows);
    let items: Vec<ListItem> = app
        .log
        .iter()
        .skip(start)
        .take(end - start)
        .map(|l| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", l.ts),
                    Style::default().fg(t.c(Color::DarkGray)),
                ),
                Span::styled(l.text.clone(), Style::default().fg(t.c(l.color))),
            ]))
        })
        .collect();
    f.render_widget(List::new(items).block(block), area);
    if total > rows {
        let mut sb = ScrollbarState::new(total.saturating_sub(rows)).position(start);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area,
            &mut sb,
        );
    }
}

fn draw_keybar(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let key = move |k: &str, d: &str| {
        vec![
            Span::styled(format!(" {k} "), t.badge(Color::Gray)),
            Span::styled(format!(" {d}  "), Style::default().fg(t.c(Color::Gray))),
        ]
    };
    let mut spans = Vec::new();
    spans.extend(key("q", "quit"));
    spans.extend(key("space", if app.paused { "resume" } else { "pause" }));
    spans.extend(key("1-5", "zoom"));
    spans.extend(key("↑↓", "log"));
    spans.extend(key("+/-", "rate"));
    if app.cards.len() > 1 {
        spans.extend(key("tab", "card"));
    }
    spans.extend(key("?", "help"));
    f.render_widget(Line::from(spans), area);
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let w = 58.min(area.width.saturating_sub(4));
    let h = 18.min(area.height.saturating_sub(2));
    let [v] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(w)])
        .flex(Flex::Center)
        .areas(v);
    f.render_widget(Clear, popup);
    let dim = Style::default().fg(t.c(Color::Gray));
    let lines = vec![
        Line::from(Span::styled(
            "astral-watch — live dashboard",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  q / Ctrl-C   quit (from anywhere)"),
        Line::from("  space        pause / resume sampling"),
        Line::from("  r            reset session peaks"),
        Line::from("  + / -        faster / slower sample rate"),
        Line::from("  1 - 5        zoom a panel · 0 / Esc back"),
        Line::from("  ↑ / ↓ wheel  scroll the alert log"),
        Line::from("  Tab / ← →    switch GPU (multi-card)"),
        Line::from("  ? / Esc      toggle / close this help"),
        Line::from(""),
        Line::from(vec![
            Span::raw("  thresholds:  overload "),
            Span::styled(
                format!("{OVERLOAD_A}A"),
                Style::default().fg(t.c(Color::Red)),
            ),
            Span::raw(format!("/pin · imbalance {IMBALANCE_RATIO}×")),
        ]),
        Line::from(Span::styled(
            format!("               (alarms gate above {MIN_LOAD_A}A total)"),
            dim,
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  watch the trend chart for a pin drifting",
            dim,
        )),
        Line::from(Span::styled("  away from the pack — the melt tell.", dim)),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .block(panel(t, " help ").border_style(Style::default().fg(t.c(Color::Cyan)))),
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
            theme: Theme { color: true },
            gpu: None,
            reading: Some(Reading {
                pins: amps.map(|x| Pin {
                    volts: 11.97,
                    amps: x,
                }),
            }),
            live: true,
            link: Some(("Gen4×16".into(), false)),
            status: "ok".into(),
            active: Vec::new(),
            pin_hist: std::array::from_fn(|_| VecDeque::new()),
            pin_peak: [0.0; PIN_COUNT],
            watts_hist: VecDeque::new(),
            peak_watts: 0.0,
            peak_balance: 0.0,
            log: VecDeque::new(),
            log_scroll: 0,
            paused: false,
            show_help: false,
            focus: None,
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
        assert!(s.contains("quit") && s.contains("zoom"));
    }

    #[test]
    fn bars_show_limit_line_and_values() {
        let s = screen(&app([8.2, 8.6, 8.3, 8.4, 8.5, 8.8]), 130, 40);
        assert!(s.contains('┄'), "9.2A limit line drawn");
        assert!(s.contains('█'), "bar fill drawn");
        assert!(s.contains("p1") && s.contains("p6"), "pin labels");
    }

    #[test]
    fn device_header_shows_link_and_connector() {
        let s = screen(&app([8.2, 8.6, 8.3, 8.4, 8.5, 8.8]), 130, 40);
        assert!(s.contains("device"), "device panel");
        assert!(s.contains("ROG Astral RTX 5090"), "model");
        assert!(s.contains("PCIe Gen4×16"), "pcie link from sysfs");
        assert!(
            s.contains("connector") && s.contains("balance"),
            "telemetry summary"
        );
        assert!(
            s.contains("nvidia-smi not available"),
            "gpu line degrades when no stats"
        );
    }

    #[test]
    fn device_header_shows_gpu_stats_when_present() {
        let mut a = app([8.0; 6]);
        a.gpu = Some(Arc::new(Mutex::new(vec![GpuStat {
            pci: "00000000:0b:00.0".into(), // normalized to match card 0000:0b:00.0
            util: Some(100),
            draw_w: Some(320.0),
            limit_w: Some(600.0),
            temp_c: Some(69),
            fan: Some(62),
        }])));
        let s = screen(&a, 130, 40);
        assert!(s.contains("GPU") && s.contains("100%"), "util");
        assert!(
            s.contains("PWR") && s.contains("320/600W"),
            "power draw/limit"
        );
        assert!(s.contains("69°C") && s.contains("fan 62%"), "temp + fan");
    }

    #[test]
    fn balance_gauge_status_words() {
        // normal
        assert!(screen(&app([8.0; 6]), 130, 40).contains("NORMAL"));
        // alarm (one pin way high -> ratio > 1.5)
        let s = screen(&app([12.0, 6.0, 8.0, 8.0, 8.0, 8.0]), 130, 40);
        assert!(s.contains("ALARM"), "imbalanced -> ALARM: {s}");
    }

    #[test]
    fn parse_gpu_csv_handles_na() {
        let v = parse_gpu_csv("00000000:0B:00.0, 100, 320.36, 600.00, 69, [N/A]\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].pci, "00000000:0b:00.0");
        assert_eq!(v[0].util, Some(100));
        assert_eq!(v[0].limit_w, Some(600.0));
        assert_eq!(v[0].fan, None); // [N/A] -> None, not a parse panic
    }

    #[test]
    fn gen_from_speed_maps() {
        assert_eq!(gen_from_speed("16.0 GT/s PCIe"), Some(4));
        assert_eq!(gen_from_speed("32.0 GT/s PCIe"), Some(5));
        assert_eq!(gen_from_speed("2.5 GT/s PCIe"), Some(1));
    }

    #[test]
    fn alarm_banner_shows_active_condition() {
        let mut a = app([9.5, 8.0, 8.0, 8.0, 8.0, 8.0]);
        a.active = vec![Condition::Overload];
        let s = screen(&a, 130, 40);
        assert!(s.contains("ALERT") && s.contains("OVERLOAD"));
    }

    #[test]
    fn focus_zooms_a_single_panel() {
        let mut a = app([8.0; 6]);
        a.focus = Some(1); // trend
        let s = screen(&a, 130, 40);
        assert!(s.contains("trend") && s.contains("[trend]"));
        assert!(!s.contains("alert log"), "other panels hidden when zoomed");
    }

    #[test]
    fn paused_and_help_render() {
        let mut a = app([8.0; 6]);
        a.paused = true;
        assert!(screen(&a, 130, 40).contains("PAUSED"));
        a.show_help = true;
        let s = screen(&a, 130, 40);
        assert!(s.contains("help") && s.contains("switch GPU") && s.contains("zoom a panel"));
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
        assert!(s.contains("5090") && s.contains("5080"));
        assert!(s.contains("card"));
    }

    #[test]
    fn no_color_theme_renders() {
        let mut a = app([8.0; 6]);
        a.theme = Theme { color: false };
        let s = screen(&a, 130, 40);
        assert!(s.contains("astral-watch") && s.contains('█'));
    }

    #[test]
    fn keys_drive_state() {
        let mut a = app([8.0; 6]);
        a.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(a.paused);
        a.on_key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(a.focus, Some(1));
        a.on_key(KeyCode::Esc, KeyModifiers::NONE); // Esc leaves zoom first (doesn't quit)
        assert_eq!(a.focus, None);
        a.on_key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(a.show_help);
        a.on_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(!a.show_help);
        assert!(a.on_key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(a.on_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    #[test]
    fn quit_works_even_with_help_open() {
        let mut a = app([8.0; 6]);
        a.show_help = true;
        assert!(a.on_key(KeyCode::Char('q'), KeyModifiers::NONE));
    }

    #[test]
    fn log_scroll_clamps() {
        let mut a = app([8.0; 6]);
        for i in 0..50 {
            a.push_log(format!("line {i}"), Color::Gray);
        }
        a.scroll_log(-5); // can't scroll below 0
        assert_eq!(a.log_scroll, 0);
        a.scroll_log(1000); // clamps to len-1
        assert_eq!(a.log_scroll, a.log.len() - 1);
    }

    #[test]
    fn scrolled_log_keeps_a_full_window() {
        let mut a = app([8.0; 6]);
        for i in 0..60 {
            a.push_log(format!("evt{i}"), Color::Gray);
        }
        a.log_scroll = 10_000; // mash scroll-up far past the top
        let s = screen(&a, 130, 40);
        // must not shrink to one line, and must reach the oldest entries
        assert!(
            s.matches("evt").count() >= 5,
            "scrolled log should still fill the pane"
        );
        assert!(s.contains("evt0"), "oldest line visible at the top");
    }

    #[test]
    fn streaming_past_cap_while_scrolled_does_not_blank() {
        let mut a = app([8.0; 6]);
        for i in 0..20 {
            a.push_log(format!("evt{i}"), Color::Gray);
        }
        a.scroll_log(6);
        for i in 20..300 {
            a.push_log(format!("evt{i}"), Color::Gray); // exceeds LOG_CAP
        }
        let s = screen(&a, 130, 40);
        assert!(
            s.matches("evt").count() >= 5,
            "log must not blank when streaming past cap"
        );
    }

    #[test]
    fn rate_keys_clamp() {
        let mut a = app([8.0; 6]);
        for _ in 0..12 {
            a.on_key(KeyCode::Char('+'), KeyModifiers::NONE);
        }
        assert!(a.interval >= MIN_INTERVAL);
        for _ in 0..12 {
            a.on_key(KeyCode::Char('-'), KeyModifiers::NONE);
        }
        assert!(a.interval <= MAX_INTERVAL);
    }

    #[test]
    fn extreme_sizes_do_not_panic() {
        let mut a = app([9.9, 0.0, 8.0, 8.0, 8.0, 8.0]);
        a.active = vec![Condition::Overload];
        for f in [None, Some(0), Some(1), Some(4)] {
            a.focus = f;
            for (w, h) in [
                (130, 40),
                (80, 24),
                (40, 10),
                (12, 5),
                (6, 4),
                (3, 3),
                (1, 1),
                (200, 80),
            ] {
                let _ = screen(&a, w, h);
            }
        }
    }

    #[test]
    fn empty_history_does_not_panic() {
        let mut a = app([8.0; 6]);
        a.clear_history();
        for (w, h) in [(130, 40), (80, 24), (1, 1)] {
            let _ = screen(&a, w, h);
        }
    }
}
