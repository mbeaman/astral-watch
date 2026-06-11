//! i2c bus discovery and reading the IT8915FN over `/dev/i2c-*`.
//!
//! The telemetry block is read **byte-by-byte** (SMBus read-byte-data per register). A single
//! block read returns garbage on some SKUs; per-register reads are the method confirmed to work.
//! This access is **read-only**: it only ever writes the register pointer, never a data byte.
//!
//! Because each 16-bit value spans two transactions, the MCU can update it between the high and
//! low byte ("tearing"), fabricating a current spike that never happened. Each u16 is therefore
//! read hi → lo → hi and re-read while the high byte moves (see `read_u16_consistent`).

use crate::decode::{decode, Reading, RAW_LEN};
use anyhow::{Context, Result};
use i2cdev::core::I2CDevice;
use i2cdev::linux::LinuxI2CDevice;
use std::fs;

/// i2c slave address of the ASUS power-telemetry MCU.
pub const CHIP_ADDR: u16 = 0x2b;
/// Canonical textual form of [`CHIP_ADDR`], used as the CLI default (kept in sync by a test).
pub const CHIP_ADDR_STR: &str = "0x2b";
/// First telemetry register (`0x80..=0x97`).
pub const REG_BASE: u8 = 0x80;

/// One SMBus read-byte-data transaction — the seam that makes the read logic testable.
pub trait RegReader {
    fn read_reg(&mut self, reg: u8) -> Result<u8>;
}

impl RegReader for LinuxI2CDevice {
    fn read_reg(&mut self, reg: u8) -> Result<u8> {
        self.smbus_read_byte_data(reg)
            .with_context(|| format!("read reg {reg:#04x}"))
    }
}

/// Logical i2c bus numbers that belong to an NVIDIA GPU.
pub fn nvidia_buses() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/i2c-dev") else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(name) = fs::read_to_string(e.path().join("name")) else {
            continue;
        };
        if !name.contains("NVIDIA") {
            continue;
        }
        if let Some(n) = e
            .file_name()
            .to_str()
            .and_then(|s| s.strip_prefix("i2c-"))
            .and_then(|s| s.parse::<u32>().ok())
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    out
}

/// Read one big-endian u16 (`reg` = high byte, `reg+1` = low) with tear suppression: read the
/// high byte again after the low byte; if it moved, the MCU updated the value mid-read and the
/// torn pair is discarded. This eliminates high-byte tears (the multi-amp phantom spikes); a
/// value oscillating fast enough to return to the same high byte between reads, or still moving
/// after `retries` re-reads, can yield a composite — but one bounded by a single low-byte wrap
/// (±256 mV/mA), noise relative to the alert thresholds.
fn read_u16_consistent(dev: &mut impl RegReader, reg: u8, retries: u32) -> Result<[u8; 2]> {
    let mut hi = dev.read_reg(reg)?;
    let mut lo = dev.read_reg(reg + 1)?;
    for _ in 0..retries {
        let hi2 = dev.read_reg(reg)?;
        if hi2 == hi {
            return Ok([hi, lo]);
        }
        hi = hi2;
        lo = dev.read_reg(reg + 1)?;
    }
    Ok([hi, lo])
}

/// Read the 24-byte telemetry block through any [`RegReader`], tear-checked per u16.
pub fn read_raw_from(dev: &mut impl RegReader) -> Result<[u8; RAW_LEN]> {
    let mut buf = [0u8; RAW_LEN];
    for i in (0..RAW_LEN).step_by(2) {
        let [hi, lo] = read_u16_consistent(dev, REG_BASE + i as u8, 3)?;
        buf[i] = hi;
        buf[i + 1] = lo;
    }
    Ok(buf)
}

/// Read the 24-byte telemetry block from `addr` on `bus`, register by register.
pub fn read_raw(bus: u32, addr: u16) -> Result<[u8; RAW_LEN]> {
    let path = format!("/dev/i2c-{bus}");
    let mut dev = LinuxI2CDevice::new(&path, addr)
        .with_context(|| format!("opening {path} @ {addr:#04x}"))?;
    read_raw_from(&mut dev).with_context(|| format!("reading {path} @ {addr:#04x}"))
}

/// Read and decode one snapshot.
pub fn read_reading(bus: u32, addr: u16) -> Result<Reading> {
    Ok(decode(&read_raw(bus, addr)?))
}

/// First NVIDIA bus that returns plausible telemetry at `addr`, if any.
pub fn autodetect_bus(addr: u16) -> Option<u32> {
    nvidia_buses().into_iter().find(|&b| {
        read_reading(b, addr)
            .map(|r| r.plausible())
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock chip: serves `regs`, switching to `regs_after` once `switch_after` reads have
    /// happened (simulates the MCU updating values mid-read).
    struct MockChip {
        regs: [u8; RAW_LEN],
        regs_after: [u8; RAW_LEN],
        switch_after: u32,
        reads: u32,
    }

    impl RegReader for MockChip {
        fn read_reg(&mut self, reg: u8) -> Result<u8> {
            let i = (reg - REG_BASE) as usize;
            let v = if self.reads < self.switch_after {
                self.regs[i]
            } else {
                self.regs_after[i]
            };
            self.reads += 1;
            Ok(v)
        }
    }

    #[test]
    fn stable_chip_reads_through() {
        let regs: [u8; RAW_LEN] = std::array::from_fn(|i| i as u8);
        let mut chip = MockChip {
            regs,
            regs_after: regs,
            switch_after: u32::MAX,
            reads: 0,
        };
        assert_eq!(read_raw_from(&mut chip).unwrap(), regs);
        // hi, lo, hi-confirm per u16: exactly 3 reads per pair when stable
        assert_eq!(chip.reads, (RAW_LEN as u32 / 2) * 3);
    }

    #[test]
    fn torn_u16_is_rejected() {
        // value at reg 0x80/0x81 is 0x20FF, becomes 0x2100 after the first (hi) read —
        // a naive hi,lo read would return the torn 0x2000
        let mut regs = [0u8; RAW_LEN];
        regs[0] = 0x20;
        regs[1] = 0xFF;
        let mut after = regs;
        after[0] = 0x21;
        after[1] = 0x00;
        let mut chip = MockChip {
            regs,
            regs_after: after,
            switch_after: 1,
            reads: 0,
        };
        let buf = read_raw_from(&mut chip).unwrap();
        assert_eq!(
            [buf[0], buf[1]],
            [0x21, 0x00],
            "must re-read, never return the torn pair"
        );
    }

    #[test]
    fn chip_addr_str_matches_const() {
        let parsed = u16::from_str_radix(CHIP_ADDR_STR.trim_start_matches("0x"), 16).unwrap();
        assert_eq!(parsed, CHIP_ADDR);
    }
}
