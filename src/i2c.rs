//! i2c bus discovery and reading the IT8915FN over `/dev/i2c-*`.
//!
//! The telemetry block is read **byte-by-byte** (SMBus read-byte-data per register). A single
//! block read returns garbage on some SKUs; per-register reads are the method confirmed to work.
//! This access is **read-only**: it only ever writes the register pointer, never a data byte.

use crate::decode::{decode, Reading, RAW_LEN};
use anyhow::{Context, Result};
use i2cdev::core::I2CDevice;
use i2cdev::linux::LinuxI2CDevice;
use std::fs;

/// i2c slave address of the ASUS power-telemetry MCU.
pub const CHIP_ADDR: u16 = 0x2b;
/// First telemetry register (`0x80..=0x97`).
pub const REG_BASE: u8 = 0x80;

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

/// Read the 24-byte telemetry block from `addr` on `bus`, register by register.
pub fn read_raw(bus: u32, addr: u16) -> Result<[u8; RAW_LEN]> {
    let path = format!("/dev/i2c-{bus}");
    let mut dev = LinuxI2CDevice::new(&path, addr)
        .with_context(|| format!("opening {path} @ {addr:#04x}"))?;
    let mut buf = [0u8; RAW_LEN];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = dev
            .smbus_read_byte_data(REG_BASE + i as u8)
            .with_context(|| format!("read reg {:#04x} on {path}", REG_BASE + i as u8))?;
    }
    Ok(buf)
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
