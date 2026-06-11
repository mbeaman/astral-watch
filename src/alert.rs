//! Per-pin alert evaluation — the connector-melt early-warning.

use crate::decode::Reading;
use serde::Deserialize;
use std::fmt;

/// ASUS Power Detector+ per-pin overload threshold (amps).
pub const OVERLOAD_A: f64 = 9.2;
/// hi/lo current ratio above which the load is considered imbalanced.
pub const IMBALANCE_RATIO: f64 = 1.5;
/// Imbalance / disconnect are only meaningful above this total load (amps); below it the
/// per-pin currents are tiny and the ratio is just noise.
pub const MIN_LOAD_A: f64 = 5.0;

/// Alert thresholds; the defaults match ASUS Power Detector+, overridable via the
/// `[thresholds]` config section.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Thresholds {
    /// Per-pin overload (amps).
    pub overload_amps: f64,
    /// hi/lo per-pin current ratio considered imbalanced.
    pub imbalance_ratio: f64,
    /// Total load (amps) below which imbalance/disconnect are noise.
    pub min_load_amps: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            overload_amps: OVERLOAD_A,
            imbalance_ratio: IMBALANCE_RATIO,
            min_load_amps: MIN_LOAD_A,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Alert {
    /// One or more pins exceeded the overload threshold.
    Overload { pins: Vec<usize>, limit: f64 },
    /// One or more pins read ~0 A while the card is under load (lost contact).
    Disconnected(Vec<usize>),
    /// hi/lo current ratio exceeded the imbalance threshold.
    Imbalance(f64),
}

impl fmt::Display for Alert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Alert::Overload { pins, limit } => {
                write!(f, "OVERLOAD pins {} >{limit}A", join_pins(pins))
            }
            Alert::Disconnected(p) => {
                write!(f, "DISCONNECTED? pins {} ~0A under load", join_pins(p))
            }
            Alert::Imbalance(r) => write!(f, "IMBALANCE hi/lo={r:.2}"),
        }
    }
}

/// Pin list as `1+2+5` — kept free of commas so alert text can sit in a CSV field.
fn join_pins(pins: &[usize]) -> String {
    pins.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("+")
}

/// Evaluate a reading into zero or more alerts.
pub fn evaluate(r: &Reading, t: &Thresholds) -> Vec<Alert> {
    let amps: Vec<f64> = r.pins.iter().map(|p| p.amps).collect();
    let mut out = Vec::new();

    let over: Vec<usize> = amps
        .iter()
        .enumerate()
        .filter(|(_, &a)| a > t.overload_amps)
        .map(|(i, _)| i + 1)
        .collect();
    if !over.is_empty() {
        out.push(Alert::Overload {
            pins: over,
            limit: t.overload_amps,
        });
    }

    if amps.iter().sum::<f64>() > t.min_load_amps {
        let dead: Vec<usize> = amps
            .iter()
            .enumerate()
            .filter(|(_, &a)| a < 0.1)
            .map(|(i, _)| i + 1)
            .collect();
        if !dead.is_empty() {
            out.push(Alert::Disconnected(dead));
        }
        if let Some(bal) = r.balance() {
            if bal > t.imbalance_ratio {
                out.push(Alert::Imbalance(bal));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Pin;

    fn reading(amps: [f64; 6]) -> Reading {
        Reading {
            pins: amps.map(|a| Pin {
                volts: 12.0,
                amps: a,
            }),
        }
    }

    fn eval(amps: [f64; 6]) -> Vec<Alert> {
        evaluate(&reading(amps), &Thresholds::default())
    }

    #[test]
    fn healthy_balanced_has_no_alerts() {
        assert!(eval([8.1, 8.3, 8.2, 8.4, 8.5, 8.6]).is_empty());
    }

    #[test]
    fn overload_flagged() {
        assert!(eval([9.5, 8.0, 8.0, 8.0, 8.0, 8.0])
            .iter()
            .any(|a| matches!(a, Alert::Overload { .. })));
    }

    #[test]
    fn disconnect_flagged_under_load() {
        assert!(eval([0.0, 9.0, 9.0, 9.0, 9.0, 9.0])
            .iter()
            .any(|a| matches!(a, Alert::Disconnected(_))));
    }

    #[test]
    fn imbalance_flagged() {
        assert!(eval([12.0, 5.0, 8.0, 8.0, 8.0, 8.0])
            .iter()
            .any(|a| matches!(a, Alert::Imbalance(_))));
    }

    #[test]
    fn idle_noise_not_flagged() {
        // big ratio but tiny absolute load -> ignored
        assert!(eval([0.4, 0.6, 0.5, 0.5, 0.6, 0.7]).is_empty());
    }

    #[test]
    fn custom_thresholds_honored() {
        let strict = Thresholds {
            overload_amps: 8.0,
            ..Thresholds::default()
        };
        let alerts = evaluate(&reading([8.5, 7.0, 7.0, 7.0, 7.0, 7.0]), &strict);
        match &alerts[..] {
            [Alert::Overload { pins, limit }] => {
                assert_eq!(pins, &vec![1]);
                assert_eq!(*limit, 8.0);
                // the configured limit shows up in the alert text, not the default
                assert!(alerts[0].to_string().contains(">8A"));
            }
            other => panic!("expected one overload, got {other:?}"),
        }
    }

    #[test]
    fn multi_pin_alert_text_has_no_commas() {
        // alert text lands in a CSV field; commas would break column alignment
        let alerts = eval([9.5, 9.6, 0.0, 8.0, 8.0, 8.0]);
        assert!(alerts.len() >= 2, "expected overload + disconnect");
        for a in &alerts {
            let s = a.to_string();
            assert!(!s.contains(','), "comma in alert text: {s}");
        }
        assert!(alerts.iter().any(|a| a.to_string().contains("pins 1+2")));
    }
}
