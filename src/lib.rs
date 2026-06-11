//! astral-watch — per-pin 12V-2x6 power telemetry for ASUS ROG Astral GPUs on Linux.
//!
//! The card carries an ITE IT8915FN microcontroller on the GPU's i2c bus that reports
//! per-pin voltage and current for all six 12V pins of the 12V-2x6 (12VHPWR) connector.
//! This crate reads it over `/dev/i2c-*` (read-only), decodes it, and raises alerts on
//! overload / disconnect / imbalance — the connector-melt early-warning that ASUS's
//! Windows-only "Power Detector+" provides.

pub mod alert;
pub mod cards;
pub mod config;
pub mod decode;
pub mod exporter;
pub mod i2c;
pub mod lifecycle;
pub mod logger;
pub mod metrics;
pub mod notify;
