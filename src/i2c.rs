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
use i2cdev::linux::{LinuxI2CDevice, LinuxI2CError};
use std::fs;
use std::io::ErrorKind;

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

fn device_path(bus: u32) -> String {
    format!("/dev/i2c-{bus}")
}

/// Open the i2c device, returning the typed error so callers can classify it.
fn open(bus: u32, addr: u16) -> std::result::Result<LinuxI2CDevice, LinuxI2CError> {
    LinuxI2CDevice::new(device_path(bus), addr)
}

/// Whether opening/talking to the bus was denied — the process lacks i2c access (not in the
/// `i2c` group, and not root). The denial almost always surfaces at `open()` as an
/// `io::Error`; the `Errno` arm covers a denial raised later by an ioctl.
fn is_permission_denied(err: &LinuxI2CError) -> bool {
    match err {
        LinuxI2CError::Io(e) => e.kind() == ErrorKind::PermissionDenied,
        LinuxI2CError::Errno(n) => *n == 13 || *n == 1, // EACCES, EPERM
    }
}

/// Read the 24-byte telemetry block from `addr` on `bus`, register by register.
pub fn read_raw(bus: u32, addr: u16) -> Result<[u8; RAW_LEN]> {
    let path = device_path(bus);
    let mut dev = open(bus, addr).with_context(|| format!("opening {path} @ {addr:#04x}"))?;
    read_raw_from(&mut dev).with_context(|| format!("reading {path} @ {addr:#04x}"))
}

/// Read and decode one snapshot.
pub fn read_reading(bus: u32, addr: u16) -> Result<Reading> {
    Ok(decode(&read_raw(bus, addr)?))
}

/// What scanning the NVIDIA buses for the telemetry chip turned up. Distinguishing these lets
/// the caller give an actionable message instead of always blaming an idle GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detect {
    /// A bus answered with plausible telemetry.
    Found(u32),
    /// No NVIDIA i2c buses exist — the `i2c-dev` module probably isn't loaded.
    NoBuses,
    /// Buses exist but opening them was denied: the process lacks i2c access.
    PermissionDenied,
    /// Buses exist and were readable, but none returned plausible telemetry — the GPU is
    /// genuinely idle, the address is wrong, or the SKU is unsupported.
    NoTelemetry,
}

enum Probe {
    Plausible,
    PermissionDenied,
    Other,
}

fn probe_bus(bus: u32, addr: u16) -> Probe {
    let mut dev = match open(bus, addr) {
        Ok(d) => d,
        Err(e) if is_permission_denied(&e) => return Probe::PermissionDenied,
        Err(_) => return Probe::Other,
    };
    match read_raw_from(&mut dev) {
        Ok(raw) if decode(&raw).plausible() => Probe::Plausible,
        _ => Probe::Other,
    }
}

/// Scan the NVIDIA buses for the chip, reporting *why* if none answered so a "deeply idle"
/// message is never shown when the real problem is missing i2c permissions.
pub fn detect_bus(addr: u16) -> Detect {
    let buses = nvidia_buses();
    if buses.is_empty() {
        return Detect::NoBuses;
    }
    // a denied open is the most actionable cause; all NVIDIA i2c nodes share permissions,
    // so in practice it's all-or-nothing, but prefer it over NoTelemetry if mixed
    let mut denied = false;
    for b in buses {
        match probe_bus(b, addr) {
            Probe::Plausible => return Detect::Found(b),
            Probe::PermissionDenied => denied = true,
            Probe::Other => {}
        }
    }
    if denied {
        Detect::PermissionDenied
    } else {
        Detect::NoTelemetry
    }
}

/// PCI address (e.g. `0000:0b:00.0`) of the GPU an i2c bus belongs to, from its resolved
/// sysfs path. Every i2c adapter of one card shares this, so it's a stable per-card identity
/// that survives the kernel renumbering the adapters after a GPU reset.
pub fn bus_pci_id(bus: u32) -> Option<String> {
    let real = fs::canonicalize(format!("/sys/class/i2c-dev/i2c-{bus}")).ok()?;
    pci_id_from_path(&real.to_string_lossy())
}

/// Deepest PCI BDF component of a sysfs path — the GPU function itself, not a parent bridge.
fn pci_id_from_path(path: &str) -> Option<String> {
    path.split('/')
        .rev()
        .find(|c| is_pci_bdf(c))
        .map(str::to_string)
}

/// Normalize a PCI id for cross-source matching. The sysfs form (`0000:0b:00.0`) and the NVML /
/// nvidia-smi form (`00000000:0B:00.0`) differ only in domain width and hex case, so canonicalize
/// to a fixed 8-hex lowercase domain plus the rest. This keeps the two forms equal while still
/// distinguishing GPUs that share a `bus:device.function` across different PCI domains (so the
/// safety daemon never caps a same-BDF sibling in another domain).
pub fn norm_pci(s: &str) -> String {
    let lower = s.trim().to_ascii_lowercase();
    match lower.split_once(':') {
        Some((dom, rest)) => match u32::from_str_radix(dom, 16) {
            Ok(d) => format!("{d:08x}:{rest}"),
            Err(_) => lower, // unexpected form — compare it whole rather than guess
        },
        None => lower,
    }
}

/// Matches a PCI `domain:bus:device.function` slot, e.g. `0000:0b:00.0` (rejects `pci0000:00`).
fn is_pci_bdf(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && s.char_indices()
            .all(|(i, c)| matches!(i, 4 | 7 | 10) || c.is_ascii_hexdigit())
}

/// Consecutive unusable samples on an auto-detected bus before re-running detection — covers
/// the GPU resetting and the kernel re-enumerating the i2c bus under a new number.
pub const REDETECT_AFTER: u32 = 10;

/// Re-detect for a known card: the first plausible bus whose GPU PCI id matches `want_pci`.
/// Unlike [`detect_bus`], this never migrates to a *different* GPU after a renumber, so on a
/// multi-Astral box a crashed card is never silently swapped for a healthy sibling.
pub fn redetect_card(addr: u16, want_pci: &str) -> Option<u32> {
    nvidia_buses().into_iter().find(|&b| {
        bus_pci_id(b).as_deref() == Some(want_pci) && matches!(probe_bus(b, addr), Probe::Plausible)
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

    #[test]
    fn pci_id_extracted_from_sysfs_path() {
        // real shape: /sys/class/i2c-dev/i2c-0 resolves through the GPU's PCI function
        assert_eq!(
            pci_id_from_path(
                "/sys/devices/pci0000:00/0000:00:03.1/0000:0b:00.0/i2c-0/i2c-dev/i2c-0"
            ),
            Some("0000:0b:00.0".to_string()),
            "must pick the deepest BDF (the GPU), not the bridge or the pci0000:00 root"
        );
        assert_eq!(
            pci_id_from_path("/sys/devices/platform/whatever/i2c-9"),
            None
        );
        assert!(is_pci_bdf("0000:0b:00.0"));
        assert!(!is_pci_bdf("pci0000:00"));
        assert!(!is_pci_bdf("0000:0b:00.")); // wrong length
        assert!(!is_pci_bdf("zzzz:0b:00.0")); // non-hex
    }

    #[test]
    fn norm_pci_reconciles_domain_width_but_keeps_domains_distinct() {
        // sysfs (4-hex) and NVML/nvidia-smi (8-hex, upper) forms of the same card match
        assert_eq!(norm_pci("0000:0b:00.0"), norm_pci("00000000:0B:00.0"));
        // but a same-bus:device.function card in a *different* PCI domain must NOT collide
        assert_ne!(norm_pci("0000:0b:00.0"), norm_pci("0001:0b:00.0"));
        assert_eq!(norm_pci("0000:0b:00.0"), "00000000:0b:00.0");
    }

    #[test]
    fn permission_denied_is_classified_distinctly() {
        // the open-time denial (io::Error) — the real path when not in the i2c group
        let io_denied = LinuxI2CError::Io(ErrorKind::PermissionDenied.into());
        assert!(is_permission_denied(&io_denied));
        // and an ioctl-time denial via errno (EACCES / EPERM)
        assert!(is_permission_denied(&LinuxI2CError::Errno(13)));
        assert!(is_permission_denied(&LinuxI2CError::Errno(1)));

        // a genuinely idle/absent chip is NOT a permission problem
        assert!(!is_permission_denied(&LinuxI2CError::Io(
            ErrorKind::NotFound.into()
        )));
        assert!(!is_permission_denied(&LinuxI2CError::Io(
            ErrorKind::TimedOut.into()
        )));
        assert!(!is_permission_denied(&LinuxI2CError::Errno(6))); // ENXIO
    }
}
