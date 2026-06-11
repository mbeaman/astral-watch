//! Prometheus metrics: a mutex-cached snapshot the sampling loop feeds and a scrape renders.
//!
//! The sampler is the only thing that touches the i2c bus — a scrape reads this cache, so
//! Prometheus polling can never add bus traffic, no matter how aggressive the scrape
//! interval. Alert gauges/counters track the debounced lifecycle, not raw per-sample
//! evaluations; the staleness gauge lets dashboards distinguish "healthy" from "no data".

use crate::alert::MIN_LOAD_A;
use crate::decode::{Reading, PIN_COUNT};
use crate::lifecycle::{Event, CONDITIONS};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Default)]
struct Inner {
    /// Last plausible reading and when it arrived.
    reading: Option<(Reading, Instant)>,
    /// Whether the most recent sample produced a usable reading.
    last_sample_ok: bool,
    samples_total: u64,
    read_failures_total: u64,
    implausible_total: u64,
    /// Lifecycle-active flags and raise counters, indexed like [`CONDITIONS`].
    active: [bool; 4],
    raised_total: [u64; 4],
}

/// Shared metrics state; clone the [`std::sync::Arc`] into the exporter thread.
#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// A sample decoded to a plausible reading.
    pub fn on_good_sample(&self, r: &Reading) {
        let mut g = self.inner.lock().unwrap();
        g.reading = Some((*r, Instant::now()));
        g.last_sample_ok = true;
        g.samples_total += 1;
    }

    /// The chip answered but the data failed sanity checks.
    pub fn on_implausible_sample(&self) {
        let mut g = self.inner.lock().unwrap();
        g.last_sample_ok = false;
        g.samples_total += 1;
        g.implausible_total += 1;
    }

    /// The i2c read itself failed.
    pub fn on_read_error(&self) {
        let mut g = self.inner.lock().unwrap();
        g.last_sample_ok = false;
        g.samples_total += 1;
        g.read_failures_total += 1;
    }

    /// Track debounced alert state from a lifecycle event.
    pub fn on_event(&self, ev: &Event) {
        let mut g = self.inner.lock().unwrap();
        match ev {
            Event::Raised { condition, .. } => {
                g.active[condition.idx()] = true;
                g.raised_total[condition.idx()] += 1;
            }
            Event::Resolved { condition, .. } => g.active[condition.idx()] = false,
            Event::Repeated { .. } => {}
        }
    }

    /// Render the Prometheus text exposition (format 0.0.4).
    pub fn render(&self) -> String {
        let g = self.inner.lock().unwrap();
        let mut out = String::with_capacity(2048);
        let mut metric = |name: &str, help: &str, kind: &str, body: &dyn Fn(&mut String)| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
            body(&mut out);
        };

        if let Some((r, _)) = &g.reading {
            metric(
                "astral_watch_pin_volts",
                "Per-pin voltage of the 12V-2x6 connector (last reading).",
                "gauge",
                &|o| {
                    for i in 0..PIN_COUNT {
                        let _ = writeln!(
                            o,
                            "astral_watch_pin_volts{{pin=\"{}\"}} {}",
                            i + 1,
                            r.pins[i].volts
                        );
                    }
                },
            );
            metric(
                "astral_watch_pin_amps",
                "Per-pin current of the 12V-2x6 connector (last reading).",
                "gauge",
                &|o| {
                    for i in 0..PIN_COUNT {
                        let _ = writeln!(
                            o,
                            "astral_watch_pin_amps{{pin=\"{}\"}} {}",
                            i + 1,
                            r.pins[i].amps
                        );
                    }
                },
            );
            metric(
                "astral_watch_total_amps",
                "Sum of per-pin current.",
                "gauge",
                &|o| {
                    let _ = writeln!(o, "astral_watch_total_amps {}", r.total_amps());
                },
            );
            metric(
                "astral_watch_total_watts",
                "Sum of per-pin power.",
                "gauge",
                &|o| {
                    let _ = writeln!(o, "astral_watch_total_watts {}", r.total_watts());
                },
            );
            // a pin at ~0 A under real load is MAXIMAL imbalance, not "no data": the
            // series must never vanish at the most dangerous moment, or a raw-value
            // alert rule (balance_ratio > 1.5) would silently resolve mid-incident
            let balance = match r.balance() {
                Some(b) => Some(b.to_string()),
                None if r.total_amps() > MIN_LOAD_A => Some("+Inf".to_string()),
                None => None,
            };
            if let Some(b) = balance {
                metric(
                    "astral_watch_balance_ratio",
                    "Highest/lowest per-pin current ratio (+Inf when a pin reads ~0 A under load; absent when idle).",
                    "gauge",
                    &|o| {
                        let _ = writeln!(o, "astral_watch_balance_ratio {b}");
                    },
                );
            }
        }
        if let Some((_, at)) = &g.reading {
            let age = at.elapsed().as_secs_f64();
            metric(
                "astral_watch_last_reading_age_seconds",
                "Seconds since the last plausible reading.",
                "gauge",
                &|o| {
                    let _ = writeln!(o, "astral_watch_last_reading_age_seconds {age:.3}");
                },
            );
        }
        metric(
            "astral_watch_up",
            "1 when the most recent sample produced a plausible reading.",
            "gauge",
            &|o| {
                let _ = writeln!(o, "astral_watch_up {}", u8::from(g.last_sample_ok));
            },
        );
        metric(
            "astral_watch_alert_active",
            "1 while the debounced alert lifecycle holds this condition raised.",
            "gauge",
            &|o| {
                for c in CONDITIONS {
                    let _ = writeln!(
                        o,
                        "astral_watch_alert_active{{condition=\"{}\"}} {}",
                        c.id(),
                        u8::from(g.active[c.idx()])
                    );
                }
            },
        );
        metric(
            "astral_watch_alerts_raised_total",
            "Debounced alerts raised since start, by condition.",
            "counter",
            &|o| {
                for c in CONDITIONS {
                    let _ = writeln!(
                        o,
                        "astral_watch_alerts_raised_total{{condition=\"{}\"}} {}",
                        c.id(),
                        g.raised_total[c.idx()]
                    );
                }
            },
        );
        metric(
            "astral_watch_samples_total",
            "Samples attempted since start.",
            "counter",
            &|o| {
                let _ = writeln!(o, "astral_watch_samples_total {}", g.samples_total);
            },
        );
        metric(
            "astral_watch_read_failures_total",
            "Samples whose i2c read failed.",
            "counter",
            &|o| {
                let _ = writeln!(
                    o,
                    "astral_watch_read_failures_total {}",
                    g.read_failures_total
                );
            },
        );
        metric(
            "astral_watch_implausible_readings_total",
            "Samples that decoded to implausible data.",
            "counter",
            &|o| {
                let _ = writeln!(
                    o,
                    "astral_watch_implausible_readings_total {}",
                    g.implausible_total
                );
            },
        );
        metric(
            "astral_watch_build_info",
            "Build metadata.",
            "gauge",
            &|o| {
                let _ = writeln!(
                    o,
                    "astral_watch_build_info{{version=\"{}\"}} 1",
                    env!("CARGO_PKG_VERSION")
                );
            },
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Pin;
    use crate::lifecycle::Condition;
    use std::time::Duration;

    fn reading(amps: f64) -> Reading {
        Reading {
            pins: [Pin { volts: 12.0, amps }; PIN_COUNT],
        }
    }

    #[test]
    fn renders_reading_gauges() {
        let m = Metrics::new();
        m.on_good_sample(&reading(8.0));
        let text = m.render();
        assert!(
            text.contains("astral_watch_pin_amps{pin=\"1\"} 8"),
            "{text}"
        );
        assert!(
            text.contains("astral_watch_pin_amps{pin=\"6\"} 8"),
            "{text}"
        );
        assert!(text.contains("astral_watch_total_amps 48"), "{text}");
        assert!(text.contains("astral_watch_total_watts 576"), "{text}");
        assert!(text.contains("astral_watch_balance_ratio 1"), "{text}");
        assert!(text.contains("astral_watch_up 1"), "{text}");
        assert!(
            text.contains("astral_watch_last_reading_age_seconds"),
            "{text}"
        );
        assert!(text.contains("astral_watch_samples_total 1"), "{text}");
    }

    #[test]
    fn no_reading_yet_still_renders_up_and_counters() {
        let m = Metrics::new();
        let text = m.render();
        assert!(!text.contains("astral_watch_pin_volts"), "{text}");
        assert!(!text.contains("astral_watch_balance_ratio"), "{text}");
        assert!(text.contains("astral_watch_up 0"), "{text}");
        assert!(text.contains("astral_watch_samples_total 0"), "{text}");
        assert!(text.contains("astral_watch_build_info"), "{text}");
    }

    #[test]
    fn failures_flip_up_but_keep_last_reading() {
        let m = Metrics::new();
        m.on_good_sample(&reading(8.0));
        m.on_read_error();
        m.on_implausible_sample();
        let text = m.render();
        assert!(text.contains("astral_watch_up 0"), "{text}");
        // the last good reading stays visible; staleness says how old it is
        assert!(
            text.contains("astral_watch_pin_amps{pin=\"1\"} 8"),
            "{text}"
        );
        assert!(text.contains("astral_watch_samples_total 3"), "{text}");
        assert!(
            text.contains("astral_watch_read_failures_total 1"),
            "{text}"
        );
        assert!(
            text.contains("astral_watch_implausible_readings_total 1"),
            "{text}"
        );
    }

    #[test]
    fn dead_pin_under_load_renders_infinite_balance_not_absence() {
        let m = Metrics::new();
        let mut r = reading(9.0);
        r.pins[0].amps = 0.0; // dead pin, 45 A total — the pre-melt scenario
        m.on_good_sample(&r);
        let text = m.render();
        assert!(
            text.contains("astral_watch_balance_ratio +Inf"),
            "balance must not vanish at maximal imbalance: {text}"
        );

        // genuinely idle: absence is correct
        let m = Metrics::new();
        m.on_good_sample(&reading(0.02));
        assert!(!m.render().contains("astral_watch_balance_ratio"));
    }

    #[test]
    fn lifecycle_events_drive_alert_gauges_and_counters() {
        let m = Metrics::new();
        m.on_event(&Event::Raised {
            condition: Condition::Overload,
            detail: "x".into(),
        });
        let text = m.render();
        assert!(
            text.contains("astral_watch_alert_active{condition=\"overload\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("astral_watch_alert_active{condition=\"imbalance\"} 0"),
            "{text}"
        );
        assert!(
            text.contains("astral_watch_alerts_raised_total{condition=\"overload\"} 1"),
            "{text}"
        );

        // repeats don't re-count; resolve clears the gauge but not the counter
        m.on_event(&Event::Repeated {
            condition: Condition::Overload,
            detail: "x".into(),
            active_for: Duration::from_secs(600),
        });
        m.on_event(&Event::Resolved {
            condition: Condition::Overload,
            active_for: Duration::from_secs(900),
        });
        let text = m.render();
        assert!(
            text.contains("astral_watch_alert_active{condition=\"overload\"} 0"),
            "{text}"
        );
        assert!(
            text.contains("astral_watch_alerts_raised_total{condition=\"overload\"} 1"),
            "{text}"
        );
    }
}
