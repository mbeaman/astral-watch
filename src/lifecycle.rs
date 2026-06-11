//! Alert lifecycle: per-sample conditions → debounced raised/repeated/resolved events.
//!
//! [`crate::alert::evaluate`] is instantaneous. Notifying on every sample would send
//! thousands of messages per hour during a sustained imbalance (alert fatigue mutes the one
//! that matters), and a single glitched sample could page someone for nothing. This state
//! machine debounces with a majority window: a condition raises once it is seen in
//! `confirm_samples` of the last `2 × confirm_samples − 1` samples — so a steady fault
//! confirms in `confirm_samples` consecutive samples (time-to-alert = confirm_samples ×
//! interval), and a fault oscillating at the sample rate still confirms instead of being
//! reset by every clean sample. An active alert resolves only after `resolve_samples`
//! consecutive clean samples, and re-notifies every `repeat_minutes` while active.
//!
//! Telemetry-loss samples are *unknown*, not healthy: while [`Condition::TelemetryLost`]
//! is present, the physical conditions' windows and resolve counters are frozen, so an
//! overload can neither confirm from nor "resolve" into a gap in the data — the tool must
//! never send an all-clear it didn't measure.

use crate::alert::Alert;
use crate::config::AlertPolicy;
use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

/// The sustained conditions an alert lifecycle tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    Overload,
    Disconnected,
    Imbalance,
    /// No usable telemetry — i2c read failed or the data failed sanity checks.
    TelemetryLost,
}

/// Every condition, in `Condition::idx` order.
pub const CONDITIONS: [Condition; 4] = [
    Condition::Overload,
    Condition::Disconnected,
    Condition::Imbalance,
    Condition::TelemetryLost,
];

impl Condition {
    /// Stable lowercase id (webhook payloads).
    pub fn id(self) -> &'static str {
        match self {
            Condition::Overload => "overload",
            Condition::Disconnected => "disconnected",
            Condition::Imbalance => "imbalance",
            Condition::TelemetryLost => "telemetry_lost",
        }
    }

    /// Display form for titles and stderr.
    pub fn label(self) -> &'static str {
        match self {
            Condition::Overload => "OVERLOAD",
            Condition::Disconnected => "DISCONNECT",
            Condition::Imbalance => "IMBALANCE",
            Condition::TelemetryLost => "TELEMETRY LOST",
        }
    }

    /// Position in [`CONDITIONS`].
    pub(crate) fn idx(self) -> usize {
        match self {
            Condition::Overload => 0,
            Condition::Disconnected => 1,
            Condition::Imbalance => 2,
            Condition::TelemetryLost => 3,
        }
    }
}

/// Which lifecycle condition an instantaneous alert feeds.
pub fn condition_of(a: &Alert) -> Condition {
    match a {
        Alert::Overload { .. } => Condition::Overload,
        Alert::Disconnected(_) => Condition::Disconnected,
        Alert::Imbalance(_) => Condition::Imbalance,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Raised {
        condition: Condition,
        detail: String,
    },
    Repeated {
        condition: Condition,
        detail: String,
        active_for: Duration,
    },
    Resolved {
        condition: Condition,
        active_for: Duration,
    },
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Raised { detail, .. } => write!(f, "ALERT RAISED: {detail}"),
            Event::Repeated {
                detail, active_for, ..
            } => write!(f, "ALERT ACTIVE {}: {detail}", fmt_duration(*active_for)),
            Event::Resolved {
                condition,
                active_for,
            } => write!(
                f,
                "ALERT RESOLVED: {} clear after {}",
                condition.label(),
                fmt_duration(*active_for)
            ),
        }
    }
}

/// Compact human duration: `42s`, `5m02s`, `1h07m`.
pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

#[derive(Debug)]
enum State {
    /// Not raised; sliding window over the most recent known samples.
    Watching { window: VecDeque<bool> },
    Active {
        since: Instant,
        last_notified: Instant,
        clean: u32,
        detail: String,
    },
}

impl State {
    fn watching() -> Self {
        State::Watching {
            window: VecDeque::new(),
        }
    }
}

/// Per-condition debouncing state machine. Feed it every sample via [`Lifecycle::observe`].
pub struct Lifecycle {
    policy: AlertPolicy,
    states: [State; 4],
}

impl Lifecycle {
    pub fn new(policy: AlertPolicy) -> Self {
        Self {
            policy,
            states: [
                State::watching(),
                State::watching(),
                State::watching(),
                State::watching(),
            ],
        }
    }

    /// Feed one sample's instantaneous conditions; returns the events to announce.
    /// `now` is injected for testability.
    pub fn observe(&mut self, now: Instant, present: &[(Condition, String)]) -> Vec<Event> {
        let telemetry_lost = present.iter().any(|(c, _)| *c == Condition::TelemetryLost);
        let confirm = self.policy.confirm_samples as usize;
        let window_len = 2 * confirm - 1;
        let mut events = Vec::new();
        for cond in CONDITIONS {
            let detail = present.iter().find(|(c, _)| *c == cond).map(|(_, d)| d);
            // A telemetry-lost sample says nothing about the physical conditions:
            // freeze their state rather than letting no-data count as health.
            if telemetry_lost && cond != Condition::TelemetryLost && detail.is_none() {
                continue;
            }
            let state = &mut self.states[cond.idx()];
            match state {
                State::Watching { window } => {
                    window.push_back(detail.is_some());
                    while window.len() > window_len {
                        window.pop_front();
                    }
                    if let Some(d) = detail {
                        if window.iter().filter(|&&seen| seen).count() >= confirm {
                            events.push(Event::Raised {
                                condition: cond,
                                detail: d.clone(),
                            });
                            *state = State::Active {
                                since: now,
                                last_notified: now,
                                clean: 0,
                                detail: d.clone(),
                            };
                        }
                    }
                }
                State::Active {
                    since,
                    last_notified,
                    clean,
                    detail: current,
                } => match detail {
                    Some(d) => {
                        *clean = 0;
                        d.clone_into(current);
                        let repeat_secs = self.policy.repeat_minutes.saturating_mul(60);
                        if repeat_secs > 0
                            && now.duration_since(*last_notified)
                                >= Duration::from_secs(repeat_secs)
                        {
                            events.push(Event::Repeated {
                                condition: cond,
                                detail: d.clone(),
                                active_for: now.duration_since(*since),
                            });
                            *last_notified = now;
                        }
                    }
                    None => {
                        *clean += 1;
                        if *clean >= self.policy.resolve_samples {
                            events.push(Event::Resolved {
                                condition: cond,
                                active_for: now.duration_since(*since),
                            });
                            *state = State::watching();
                        }
                    }
                },
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(confirm: u32, resolve: u32, repeat_min: u64) -> AlertPolicy {
        AlertPolicy {
            confirm_samples: confirm,
            resolve_samples: resolve,
            repeat_minutes: repeat_min,
        }
    }

    fn overload() -> (Condition, String) {
        (Condition::Overload, "OVERLOAD pins 1+2 >9.2A".into())
    }

    fn lost() -> (Condition, String) {
        (Condition::TelemetryLost, "read failed".into())
    }

    #[test]
    fn raises_after_confirm_consecutive_samples() {
        let mut lc = Lifecycle::new(policy(3, 5, 0));
        let t = Instant::now();
        assert!(lc.observe(t, &[overload()]).is_empty());
        assert!(lc.observe(t, &[overload()]).is_empty());
        let ev = lc.observe(t, &[overload()]);
        assert!(
            matches!(
                &ev[..],
                [Event::Raised {
                    condition: Condition::Overload,
                    ..
                }]
            ),
            "{ev:?}"
        );
        // already active: no further raise
        assert!(lc.observe(t, &[overload()]).is_empty());
    }

    #[test]
    fn duty_cycled_fault_still_confirms() {
        // overload present 2 of every 3 samples: a real sustained hazard (the connector
        // heats on average current) that strict consecutive-counting would never confirm
        let mut lc = Lifecycle::new(policy(3, 5, 0));
        let t = Instant::now();
        let mut raised = false;
        for i in 0..9 {
            let sample = if i % 3 == 2 { vec![] } else { vec![overload()] };
            if lc
                .observe(t, &sample)
                .iter()
                .any(|e| matches!(e, Event::Raised { .. }))
            {
                raised = true;
                break;
            }
        }
        assert!(raised, "majority window must confirm a 2/3-duty fault");
    }

    #[test]
    fn isolated_glitches_do_not_raise() {
        let mut lc = Lifecycle::new(policy(3, 5, 0));
        let t = Instant::now();
        // single-sample glitches separated by clean stretches: 1 of 5 in the window
        for _ in 0..10 {
            assert!(
                lc.observe(t, &[overload()]).is_empty(),
                "isolated glitch raised"
            );
            for _ in 0..6 {
                assert!(lc.observe(t, &[]).is_empty());
            }
        }
    }

    #[test]
    fn confirm_one_raises_immediately() {
        let mut lc = Lifecycle::new(policy(1, 1, 0));
        let ev = lc.observe(Instant::now(), &[overload()]);
        assert!(matches!(&ev[..], [Event::Raised { .. }]));
    }

    #[test]
    fn resolves_after_clean_samples_with_duration() {
        let mut lc = Lifecycle::new(policy(1, 2, 0));
        let t0 = Instant::now();
        lc.observe(t0, &[overload()]);
        let t1 = t0 + Duration::from_secs(90);
        assert!(
            lc.observe(t1, &[]).is_empty(),
            "one clean sample is not resolution"
        );
        let ev = lc.observe(t1, &[]);
        match &ev[..] {
            [Event::Resolved {
                condition: Condition::Overload,
                active_for,
            }] => assert_eq!(*active_for, Duration::from_secs(90)),
            other => panic!("expected resolve, got {other:?}"),
        }
        // and it can raise again afterwards
        assert!(!lc.observe(t1, &[overload()]).is_empty());
    }

    #[test]
    fn telemetry_loss_does_not_resolve_an_active_alert() {
        // the GPU falling off the bus is a plausible *consequence* of connector damage —
        // an unreachable stretch must never produce "OVERLOAD resolved"
        let mut lc = Lifecycle::new(policy(1, 3, 0));
        let t = Instant::now();
        lc.observe(t, &[overload()]);
        for _ in 0..50 {
            let ev = lc.observe(t, &[lost()]);
            assert!(
                !ev.iter().any(|e| matches!(
                    e,
                    Event::Resolved {
                        condition: Condition::Overload,
                        ..
                    }
                )),
                "no-data samples resolved an overload: {ev:?}"
            );
        }
        // telemetry returns, genuinely clean: now it may resolve
        lc.observe(t, &[]);
        lc.observe(t, &[]);
        let ev = lc.observe(t, &[]);
        assert!(
            ev.iter().any(|e| matches!(
                e,
                Event::Resolved {
                    condition: Condition::Overload,
                    ..
                }
            )),
            "{ev:?}"
        );
    }

    #[test]
    fn flaky_bus_starves_neither_overload_nor_telemetry_alerts() {
        // alternate reads fail while a genuine overload persists: both the overload and
        // the telemetry trouble must confirm — this exact pattern produced zero events
        // under strict consecutive-counting with clean-on-unknown semantics
        let mut lc = Lifecycle::new(policy(3, 20, 0));
        let t = Instant::now();
        let mut raised = Vec::new();
        for i in 0..20 {
            let sample = if i % 2 == 0 {
                vec![overload()]
            } else {
                vec![lost()]
            };
            for ev in lc.observe(t, &sample) {
                if let Event::Raised { condition, .. } = ev {
                    raised.push(condition);
                }
            }
        }
        assert!(
            raised.contains(&Condition::Overload),
            "overload starved: {raised:?}"
        );
        assert!(
            raised.contains(&Condition::TelemetryLost),
            "telemetry-lost starved: {raised:?}"
        );
    }

    #[test]
    fn active_alert_repeats_on_interval_with_latest_detail() {
        let mut lc = Lifecycle::new(policy(1, 1, 10));
        let t0 = Instant::now();
        lc.observe(t0, &[overload()]);
        // 9 minutes in: too early
        assert!(lc
            .observe(t0 + Duration::from_secs(9 * 60), &[overload()])
            .is_empty());
        let worse = (Condition::Overload, "OVERLOAD pins 1+2+3 >9.2A".to_string());
        let ev = lc.observe(t0 + Duration::from_secs(10 * 60), &[worse]);
        match &ev[..] {
            [Event::Repeated {
                detail, active_for, ..
            }] => {
                assert!(detail.contains("1+2+3"), "repeat carries latest detail");
                assert_eq!(*active_for, Duration::from_secs(600));
            }
            other => panic!("expected repeat, got {other:?}"),
        }
        // interval restarts after each repeat
        assert!(lc
            .observe(t0 + Duration::from_secs(19 * 60), &[overload()])
            .is_empty());
    }

    #[test]
    fn repeat_zero_means_notify_once() {
        let mut lc = Lifecycle::new(policy(1, 1, 0));
        let t0 = Instant::now();
        lc.observe(t0, &[overload()]);
        assert!(lc
            .observe(t0 + Duration::from_secs(86_400), &[overload()])
            .is_empty());
    }

    #[test]
    fn huge_repeat_minutes_does_not_overflow() {
        let mut lc = Lifecycle::new(policy(1, 1, u64::MAX));
        let t0 = Instant::now();
        lc.observe(t0, &[overload()]);
        assert!(lc
            .observe(t0 + Duration::from_secs(86_400), &[overload()])
            .is_empty());
    }

    #[test]
    fn conditions_are_independent() {
        let mut lc = Lifecycle::new(policy(2, 1, 0));
        let t = Instant::now();
        lc.observe(t, &[overload(), lost()]);
        let ev = lc.observe(t, &[overload(), lost()]);
        assert_eq!(ev.len(), 2, "{ev:?}");
        // telemetry recovers and stays clean while the overload persists
        let ev = lc.observe(t, &[overload()]);
        assert!(
            matches!(
                &ev[..],
                [Event::Resolved {
                    condition: Condition::TelemetryLost,
                    ..
                }]
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn event_display_is_readable() {
        let raised = Event::Raised {
            condition: Condition::Overload,
            detail: "OVERLOAD pins 1+2 >9.2A".into(),
        };
        assert_eq!(raised.to_string(), "ALERT RAISED: OVERLOAD pins 1+2 >9.2A");
        let resolved = Event::Resolved {
            condition: Condition::Imbalance,
            active_for: Duration::from_secs(3722),
        };
        assert_eq!(
            resolved.to_string(),
            "ALERT RESOLVED: IMBALANCE clear after 1h02m"
        );
    }

    #[test]
    fn duration_formats() {
        assert_eq!(fmt_duration(Duration::from_secs(42)), "42s");
        assert_eq!(fmt_duration(Duration::from_secs(302)), "5m02s");
        assert_eq!(fmt_duration(Duration::from_secs(4020)), "1h07m");
    }
}
