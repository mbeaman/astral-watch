//! Decoding the ITE IT8915FN per-pin telemetry block.
//!
//! Register `0x80` exposes a 24-byte block: six pins, each `(u16 mV voltage, u16 mA current)`
//! big-endian, stored in **reverse** pin order (offset 0 = highest pin number). Verified
//! against real ASUS ROG Astral RTX 5090 hardware and corroborated by LibreHardwareMonitor
//! PR #2168 and the LACT #906 proof-of-concept.

/// Number of 12V power pins on the 12V-2x6 connector.
pub const PIN_COUNT: usize = 6;
/// Length of the raw telemetry block (six pins × four bytes).
pub const RAW_LEN: usize = PIN_COUNT * 4;

/// A single 12V pin's measured voltage and current.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pin {
    pub volts: f64,
    pub amps: f64,
}

/// One decoded snapshot of all six pins, in physical pin order (pin 1 first).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub pins: [Pin; PIN_COUNT],
}

impl Reading {
    /// Sum of per-pin current (amps).
    pub fn total_amps(&self) -> f64 {
        self.pins.iter().map(|p| p.amps).sum()
    }

    /// Sum of per-pin power (watts).
    pub fn total_watts(&self) -> f64 {
        self.pins.iter().map(|p| p.volts * p.amps).sum()
    }

    /// Highest per-pin voltage seen (used as a plausibility signal).
    pub fn max_volts(&self) -> f64 {
        self.pins.iter().map(|p| p.volts).fold(0.0, f64::max)
    }

    /// Current imbalance as the highest/lowest pin ratio, or `None` when the lowest pin is
    /// essentially zero (the ratio is meaningless / undefined there).
    pub fn balance(&self) -> Option<f64> {
        let lo = self
            .pins
            .iter()
            .map(|p| p.amps)
            .fold(f64::INFINITY, f64::min);
        let hi = self.pins.iter().map(|p| p.amps).fold(0.0, f64::max);
        (lo > 0.05).then_some(hi / lo)
    }

    /// A real reading carries the ~12V rail voltage on its pins; anything outside a sane band
    /// means we're not actually looking at the IT8915FN (wrong device / GPU gone / garbage).
    pub fn plausible(&self) -> bool {
        (5.0..=20.0).contains(&self.max_volts())
    }
}

/// Decode a 24-byte telemetry block into per-pin readings (physical pin order).
pub fn decode(raw: &[u8; RAW_LEN]) -> Reading {
    let mut pins = [Pin {
        volts: 0.0,
        amps: 0.0,
    }; PIN_COUNT];
    for (i, pin) in pins.iter_mut().enumerate() {
        let mv = u16::from_be_bytes([raw[i * 4], raw[i * 4 + 1]]);
        let ma = u16::from_be_bytes([raw[i * 4 + 2], raw[i * 4 + 3]]);
        *pin = Pin {
            volts: f64::from(mv) / 1000.0,
            amps: f64::from(ma) / 1000.0,
        };
    }
    pins.reverse(); // stored high-pin-first; present as pin 1..6
    Reading { pins }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real capture from an ASUS ROG Astral RTX 5090 (subsystem 1043:8a2e) at ~607 W.
    const SAMPLE: [u8; RAW_LEN] = [
        0x2e, 0x98, 0x21, 0xd4, 0x2e, 0x90, 0x21, 0xd4, 0x2e, 0x90, 0x20, 0x80, 0x2e, 0xa0, 0x20,
        0x58, 0x2e, 0xa0, 0x21, 0x5c, 0x2e, 0xa0, 0x1f, 0xe0,
    ];

    #[test]
    fn decodes_known_sample() {
        let r = decode(&SAMPLE);
        // pin 1 = reversed-last group (0x2ea0 mV, 0x1fe0 mA)
        assert!((r.pins[0].volts - 11.936).abs() < 1e-6, "{:?}", r.pins[0]);
        assert!((r.pins[0].amps - 8.160).abs() < 1e-6, "{:?}", r.pins[0]);
        assert!((r.pins[5].amps - 8.660).abs() < 1e-6, "{:?}", r.pins[5]);
        assert!((r.total_amps() - 50.62).abs() < 0.01);
        assert!((r.total_watts() - 604.0).abs() < 2.0);
        assert!(r.plausible());
        let bal = r.balance().expect("balanced sample has a ratio");
        assert!((1.0..1.1).contains(&bal), "balance {bal}");
    }

    #[test]
    fn all_zero_is_implausible() {
        assert!(!decode(&[0u8; RAW_LEN]).plausible());
    }
}
