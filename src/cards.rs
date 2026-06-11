//! Known ASUS ROG Astral GPUs with per-pin telemetry.
//!
//! Subsystem device IDs are community-sourced (LibreHardwareMonitor PR #2168, LACT #906, and
//! confirmed owners). The reader also works on **unlisted** SKUs as long as the chip answers
//! with plausible telemetry — the DB is just for naming. Add yours via a PR (see CONTRIBUTING.md).

use std::fs;
use std::path::Path;

/// ASUS PCI subsystem vendor id.
pub const ASUS_VENDOR: u16 = 0x1043;
/// NVIDIA PCI vendor id.
pub const NVIDIA_VENDOR: u16 = 0x10de;

pub struct Card {
    pub subsystem: u16,
    pub model: &'static str,
}

pub const CARDS: &[Card] = &[
    Card {
        subsystem: 0x89ed,
        model: "ROG Astral RTX 5090 O32G Gaming",
    },
    Card {
        subsystem: 0x89ea,
        model: "ROG Astral RTX 5090",
    },
    Card {
        subsystem: 0x89e3,
        model: "ROG Astral RTX 5090",
    },
    Card {
        subsystem: 0x89de,
        model: "ROG Astral RTX 5090",
    },
    Card {
        subsystem: 0x8a61,
        model: "ROG Astral RTX 5090 LC",
    },
    Card {
        subsystem: 0x8a2e,
        model: "ROG Astral RTX 5090 (variant)",
    },
    Card {
        subsystem: 0x89ec,
        model: "ROG Astral RTX 5080",
    },
];

/// Model name for a subsystem device id, if known.
pub fn model_for(subsystem: u16) -> Option<&'static str> {
    CARDS
        .iter()
        .find(|c| c.subsystem == subsystem)
        .map(|c| c.model)
}

fn read_hex(path: &Path) -> Option<u16> {
    let s = fs::read_to_string(path).ok()?;
    u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

/// Detected GPU: `(pci_address, subsystem_vendor, subsystem_device)` of the first NVIDIA VGA.
pub fn detect_gpu() -> Option<(String, u16, u16)> {
    for e in fs::read_dir("/sys/bus/pci/devices").ok()?.flatten() {
        let dir = e.path();
        if read_hex(&dir.join("vendor")) != Some(NVIDIA_VENDOR) {
            continue;
        }
        // class 0x0300xx = VGA / display controller
        match fs::read_to_string(dir.join("class")) {
            Ok(c) if c.trim().starts_with("0x0300") => {}
            _ => continue,
        }
        let sv = read_hex(&dir.join("subsystem_vendor")).unwrap_or(0);
        let sd = read_hex(&dir.join("subsystem_device")).unwrap_or(0);
        return Some((e.file_name().to_string_lossy().into_owned(), sv, sd));
    }
    None
}
