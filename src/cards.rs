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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_class_matches_vga_and_3d_only() {
        assert!(is_gpu_class("0x030000")); // VGA controller
        assert!(is_gpu_class("0x030200")); // 3D controller (secondary GPU)
        assert!(is_gpu_class("0x030000\n")); // trailing newline from sysfs
        assert!(!is_gpu_class("0x010802")); // NVMe
        assert!(!is_gpu_class("0x040300")); // audio (the GPU's HDMI audio function)
    }

    #[test]
    fn model_lookup() {
        assert_eq!(model_for(0x8a2e), Some("ROG Astral RTX 5090 (variant)"));
        assert_eq!(model_for(0x0000), None);
        let g = GpuInfo {
            pci: "0000:0b:00.0".into(),
            subsystem_vendor: ASUS_VENDOR,
            subsystem_device: 0x89ed,
        };
        assert!(g.is_asus());
        assert_eq!(g.model(), Some("ROG Astral RTX 5090 O32G Gaming"));
    }
}

fn read_hex(path: &Path) -> Option<u16> {
    let s = fs::read_to_string(path).ok()?;
    u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

/// An NVIDIA GPU as seen in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    /// PCI address, e.g. `0000:0b:00.0`.
    pub pci: String,
    pub subsystem_vendor: u16,
    pub subsystem_device: u16,
}

impl GpuInfo {
    /// Model name if this card's subsystem id is in the DB.
    pub fn model(&self) -> Option<&'static str> {
        model_for(self.subsystem_device)
    }
    /// Whether the board partner is ASUS (per-pin telemetry is an ASUS Astral/Matrix feature).
    pub fn is_asus(&self) -> bool {
        self.subsystem_vendor == ASUS_VENDOR
    }
}

/// True for an NVIDIA display (`0x0300`) or 3D (`0x0302`) controller class — secondary GPUs
/// commonly enumerate as 3D controllers, so matching only VGA would miss a second Astral.
fn is_gpu_class(class: &str) -> bool {
    let c = class.trim();
    c.starts_with("0x0300") || c.starts_with("0x0302")
}

fn read_gpu(dir: &Path) -> Option<GpuInfo> {
    if read_hex(&dir.join("vendor")) != Some(NVIDIA_VENDOR) {
        return None;
    }
    if !is_gpu_class(&fs::read_to_string(dir.join("class")).ok()?) {
        return None;
    }
    Some(GpuInfo {
        pci: dir.file_name()?.to_string_lossy().into_owned(),
        subsystem_vendor: read_hex(&dir.join("subsystem_vendor")).unwrap_or(0),
        subsystem_device: read_hex(&dir.join("subsystem_device")).unwrap_or(0),
    })
}

/// All NVIDIA display/3D GPUs present, sorted by PCI address.
pub fn nvidia_gpus() -> Vec<GpuInfo> {
    let mut v: Vec<GpuInfo> = match fs::read_dir("/sys/bus/pci/devices") {
        Ok(rd) => rd.flatten().filter_map(|e| read_gpu(&e.path())).collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by(|a, b| a.pci.cmp(&b.pci));
    v
}

/// The NVIDIA GPU at a specific PCI address — used to name the card actually backing the
/// monitored i2c bus, rather than guessing the first VGA in the system.
pub fn gpu_at(pci: &str) -> Option<GpuInfo> {
    read_gpu(&Path::new("/sys/bus/pci/devices").join(pci))
}
