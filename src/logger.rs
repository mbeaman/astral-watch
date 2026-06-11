//! CSV logging with self-contained size-based rotation.

use crate::alert::Alert;
use crate::decode::Reading;
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Appends per-sample rows to a CSV, rotating to `FILE.1 .. FILE.keep` once it passes `max_bytes`.
pub struct CsvLogger {
    path: PathBuf,
    file: File,
    max_bytes: u64,
    keep: u32,
    rows: u64,
}

fn header() -> String {
    let mut h = String::from("timestamp");
    for i in 1..=6 {
        let _ = write!(h, ",p{i}_V,p{i}_A");
    }
    h.push_str(",total_A,total_W,balance,alerts\n");
    h
}

impl CsvLogger {
    pub fn open(path: impl AsRef<Path>, max_bytes: u64, keep: u32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let is_new = !path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening log {}", path.display()))?;
        if is_new {
            file.write_all(header().as_bytes())?;
        }
        Ok(Self {
            path,
            file,
            max_bytes,
            keep,
            rows: 0,
        })
    }

    /// Append one decoded sample plus any alerts.
    pub fn log(&mut self, ts: &str, r: &Reading, alerts: &[Alert]) -> Result<()> {
        let mut row = String::from(ts);
        for p in &r.pins {
            let _ = write!(row, ",{:.3},{:.3}", p.volts, p.amps);
        }
        let bal = r.balance().map(|b| format!("{b:.3}")).unwrap_or_default();
        let al = alerts
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(";");
        let _ = writeln!(
            row,
            ",{:.3},{:.0},{},{}",
            r.total_amps(),
            r.total_watts(),
            bal,
            al
        );
        self.write(&row)
    }

    /// Record that the chip was unreachable (e.g. the GPU fell off the bus).
    pub fn log_unreachable(&mut self, ts: &str, msg: &str) -> Result<()> {
        // 17 columns: timestamp, then empty through `balance`, msg in `alerts`.
        let row = format!("{ts}{}{msg}\n", ",".repeat(16));
        self.write(&row)
    }

    fn write(&mut self, row: &str) -> Result<()> {
        self.file.write_all(row.as_bytes())?;
        self.file.flush()?;
        self.rows += 1;
        if self.max_bytes > 0 && self.rows % 20 == 0 {
            let size = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
            if size > self.max_bytes {
                self.rotate()?;
            }
        }
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        let suffix = |i: u32| PathBuf::from(format!("{}.{i}", self.path.display()));
        let _ = fs::remove_file(suffix(self.keep));
        for i in (1..self.keep).rev() {
            if suffix(i).exists() {
                let _ = fs::rename(suffix(i), suffix(i + 1));
            }
        }
        let _ = fs::rename(&self.path, suffix(1));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("reopening log {}", self.path.display()))?;
        file.write_all(header().as_bytes())?;
        self.file = file;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{Pin, Reading};

    fn sample() -> Reading {
        Reading {
            pins: [Pin {
                volts: 12.0,
                amps: 8.0,
            }; 6],
        }
    }

    #[test]
    fn writes_header_and_rotates() {
        let dir = std::env::temp_dir().join(format!("astral-watch-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("t.csv");
        let _ = fs::remove_file(&path);
        // tiny cap so it rotates quickly
        let mut log = CsvLogger::open(&path, 200, 3).unwrap();
        for _ in 0..200 {
            log.log("2026-01-01T00:00:00", &sample(), &[]).unwrap();
        }
        assert!(path.exists(), "live log exists");
        assert!(dir.join("t.csv.1").exists(), "a rotation happened");
        let head = fs::read_to_string(&path).unwrap();
        assert!(
            head.starts_with("timestamp,"),
            "rotated file keeps a header"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
