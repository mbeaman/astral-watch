//! CSV logging with self-contained size-based rotation.

use crate::alert::Alert;
use crate::decode::Reading;
use anyhow::{Context, Result};
use std::borrow::Cow;
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

/// RFC-4180-quote a field when it contains a comma, quote, or line break; pass through otherwise.
fn csv_field(s: &str) -> Cow<'_, str> {
    if s.contains([',', '"', '\n', '\r']) {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else {
        Cow::Borrowed(s)
    }
}

impl CsvLogger {
    pub fn open(path: impl AsRef<Path>, max_bytes: u64, keep: u32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening log {}", path.display()))?;
        // header on any empty file — also covers a pre-created file (touch/truncate) and a
        // previous instance killed between create and header write
        if file.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
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
            csv_field(&al)
        );
        self.write(&row)
    }

    /// Record a sample with no usable reading (chip unreachable or data implausible).
    pub fn log_unreachable(&mut self, ts: &str, msg: &str) -> Result<()> {
        // 17 columns: timestamp, then empty through `balance`, msg in `alerts`.
        let row = format!("{ts}{}{}\n", ",".repeat(16), csv_field(msg));
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
        let warn = |what: &str, e: std::io::Error| {
            eprintln!("# log rotation: {what} failed: {e} (continuing)");
        };
        if self.keep == 0 {
            // keep=0: no backups — drop the full log and start fresh
            if let Err(e) = fs::remove_file(&self.path) {
                warn("removing full log", e);
            }
        } else {
            let _ = fs::remove_file(suffix(self.keep)); // oldest backup; may not exist
            for i in (1..self.keep).rev() {
                if suffix(i).exists() {
                    if let Err(e) = fs::rename(suffix(i), suffix(i + 1)) {
                        warn("shifting backup", e);
                    }
                }
            }
            if let Err(e) = fs::rename(&self.path, suffix(1)) {
                warn("renaming full log", e);
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("reopening log {}", self.path.display()))?;
        // only on a fresh file — if the rename above failed we're still appending to the
        // old data and must not inject a header mid-file
        if file.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
            file.write_all(header().as_bytes())?;
        }
        self.file = file;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{evaluate, Thresholds};
    use crate::decode::{Pin, Reading};

    fn sample() -> Reading {
        Reading {
            pins: [Pin {
                volts: 12.0,
                amps: 8.0,
            }; 6],
        }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astral-watch-test-{tag}-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// Split one CSV row into fields, honoring RFC-4180 quoting.
    fn fields(row: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut chars = row.trim_end_matches('\n').chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => quoted = !quoted,
                ',' if !quoted => out.push(std::mem::take(&mut cur)),
                c => cur.push(c),
            }
        }
        out.push(cur);
        out
    }

    #[test]
    fn writes_header_and_rotates() {
        let dir = tmp_dir("rotate");
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

    #[test]
    fn header_added_to_preexisting_empty_file() {
        let dir = tmp_dir("empty");
        let path = dir.join("t.csv");
        fs::write(&path, b"").unwrap(); // e.g. admin pre-created it with touch+chown
        let mut log = CsvLogger::open(&path, 0, 0).unwrap();
        log.log("2026-01-01T00:00:00", &sample(), &[]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("timestamp,"), "missing header: {text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keep_zero_rotates_without_backups() {
        let dir = tmp_dir("keep0");
        let path = dir.join("t.csv");
        let _ = fs::remove_file(&path);
        let mut log = CsvLogger::open(&path, 200, 0).unwrap();
        for _ in 0..200 {
            log.log("2026-01-01T00:00:00", &sample(), &[]).unwrap();
        }
        assert!(path.exists(), "live log exists");
        assert!(
            !dir.join("t.csv.1").exists(),
            "keep=0 must not leave a backup"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_pin_alert_row_stays_17_columns() {
        let dir = tmp_dir("quote");
        let path = dir.join("t.csv");
        let _ = fs::remove_file(&path);
        let mut log = CsvLogger::open(&path, 0, 0).unwrap();

        // two simultaneous alerts (overload on 2 pins + disconnect) — the worst alert text
        let mut r = sample();
        r.pins[0].amps = 9.5;
        r.pins[1].amps = 9.6;
        r.pins[2].amps = 0.0;
        let alerts = evaluate(&r, &Thresholds::default());
        assert!(alerts.len() >= 2);
        log.log("2026-01-01T00:00:00", &r, &alerts).unwrap();

        // an unreachable message containing commas must also stay one field
        log.log_unreachable(
            "2026-01-01T00:00:01",
            "GPU_UNREACHABLE (read failed: foo, bar)",
        )
        .unwrap();

        let text = fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            assert_eq!(fields(line).len(), 17, "bad column count in: {line}");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
