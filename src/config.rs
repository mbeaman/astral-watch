//! Configuration loading: `--config` flag, XDG user config, `/etc/astral-watch.toml`.

use crate::alert::{Thresholds, IMBALANCE_RATIO, MIN_LOAD_A, OVERLOAD_A};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// System-wide config path (what the shipped service reads).
pub const SYSTEM_PATH: &str = "/etc/astral-watch.toml";

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub thresholds: Thresholds,
    pub alerts: AlertPolicy,
    pub notify: NotifyConfig,
    pub export: Option<ExportConfig>,
    pub safety: SafetyConfig,
}

/// Opt-in NVML auto power-cap safety daemon (the `safety` subcommand, built with
/// `--features safety`). This struct is compiled unconditionally so a `[safety]` block parses
/// even in the default read-only build (deny_unknown_fields would otherwise reject it); only
/// the actuating code is feature-gated. See `docs/SAFETY.md`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SafetyConfig {
    /// Arm the daemon. OFF by default — this is the only mode that mutates GPU state.
    pub enabled: bool,
    /// Cap target as a fraction of the GPU's stock (default) power limit. The daemon never
    /// *raises* the limit, so the effective cap is `min(this, the limit already in effect)`.
    pub target_fraction: f64,
    /// Also cap on a disconnected pin under load (the surviving pins carry its share).
    pub trigger_disconnect: bool,
    /// Also cap on imbalance alone (off by default — capping can't change the hi/lo ratio).
    pub trigger_imbalance: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target_fraction: 0.5,
            trigger_disconnect: true,
            trigger_imbalance: false,
        }
    }
}

/// Prometheus exporter — presence of the section enables the listener in every mode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    /// Listen address, e.g. `127.0.0.1:9942` (bind non-loopback deliberately).
    pub listen: String,
}

/// When alerts raise, resolve, and re-notify. Sample-count based: a steady fault confirms
/// in `confirm_samples × --interval` (1.5 s for the shipped service at 0.5 s).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AlertPolicy {
    /// Raise once the condition is seen in this many of the last `2×confirm_samples−1`
    /// samples (majority window; see [`crate::lifecycle`]).
    pub confirm_samples: u32,
    /// Consecutive clean samples before an active alert resolves.
    pub resolve_samples: u32,
    /// Re-notify while an alert stays active, in minutes (0 = notify once).
    pub repeat_minutes: u64,
}

impl Default for AlertPolicy {
    fn default() -> Self {
        Self {
            confirm_samples: 3,
            resolve_samples: 20,
            repeat_minutes: 10,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotifyConfig {
    /// Desktop notifications via `notify-send` — only works in a desktop session,
    /// not from the system service (no session bus there).
    pub desktop: bool,
    pub ntfy: Option<NtfyConfig>,
    pub webhook: Option<WebhookConfig>,
}

/// Phone/browser push via [ntfy](https://ntfy.sh) (or self-hosted).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NtfyConfig {
    /// Server base URL.
    #[serde(default = "default_ntfy_url")]
    pub url: String,
    /// Topic name — acts as a secret on public servers, pick something unguessable.
    pub topic: String,
    /// Access token for protected topics (sent as `Authorization: Bearer`).
    #[serde(default)]
    pub token: Option<String>,
}

fn default_ntfy_url() -> String {
    "https://ntfy.sh".into()
}

/// Generic JSON webhook (POST).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub url: String,
    /// Sent as `Authorization: Bearer`.
    #[serde(default)]
    pub token: Option<String>,
}

impl NotifyConfig {
    /// Names of the enabled transports, for the startup banner (never secrets).
    pub fn enabled(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.ntfy.is_some() {
            v.push("ntfy");
        }
        if self.webhook.is_some() {
            v.push("webhook");
        }
        if self.desktop {
            v.push("desktop");
        }
        v
    }
}

/// Resolve and load: explicit path (must exist) > XDG user config > `/etc` > built-in
/// defaults. Returns the config and the file it came from (`None` = defaults).
pub fn load(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>)> {
    if let Some(p) = explicit {
        return Ok((load_file(p)?, Some(p.to_path_buf())));
    }
    for p in candidate_paths() {
        if p.exists() {
            return Ok((load_file(&p)?, Some(p)));
        }
    }
    Ok((Config::default(), None))
}

fn load_file(p: &Path) -> Result<Config> {
    let text = fs::read_to_string(p).with_context(|| format!("reading config {}", p.display()))?;
    parse(&text).with_context(|| format!("in config {}", p.display()))
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(base) = xdg {
        v.push(base.join("astral-watch").join("config.toml"));
    }
    v.push(PathBuf::from(SYSTEM_PATH));
    v
}

/// Parse and validate a config document.
pub fn parse(text: &str) -> Result<Config> {
    let cfg: Config = toml::from_str(text)?;
    cfg.validate()?;
    Ok(cfg)
}

impl Config {
    /// Hard errors: values the program cannot run with.
    fn validate(&self) -> Result<()> {
        let t = &self.thresholds;
        for (name, v) in [
            ("thresholds.overload_amps", t.overload_amps),
            ("thresholds.imbalance_ratio", t.imbalance_ratio),
            ("thresholds.min_load_amps", t.min_load_amps),
        ] {
            if !v.is_finite() || v <= 0.0 {
                bail!("{name} must be a positive number (got {v})");
            }
        }
        if self.alerts.confirm_samples == 0 {
            bail!("alerts.confirm_samples must be >= 1");
        }
        if self.alerts.resolve_samples == 0 {
            bail!("alerts.resolve_samples must be >= 1");
        }
        if let Some(n) = &self.notify.ntfy {
            if n.topic.trim().is_empty() {
                bail!("notify.ntfy.topic must not be empty");
            }
            check_url("notify.ntfy.url", &n.url)?;
        }
        if let Some(w) = &self.notify.webhook {
            check_url("notify.webhook.url", &w.url)?;
        }
        if let Some(e) = &self.export {
            if !e.listen.contains(':') {
                bail!(
                    "export.listen must be an address:port like 127.0.0.1:9942 (got {:?})",
                    e.listen
                );
            }
        }
        let f = self.safety.target_fraction;
        if !f.is_finite() || !(0.1..=1.0).contains(&f) {
            bail!("safety.target_fraction must be between 0.1 and 1.0 (got {f})");
        }
        Ok(())
    }

    /// Soft warnings worth printing at startup (loosened safety margins).
    pub fn warnings(&self) -> Vec<String> {
        let t = &self.thresholds;
        let mut w = Vec::new();
        if t.overload_amps > OVERLOAD_A {
            w.push(format!(
                "thresholds.overload_amps {} is looser than the ASUS default {OVERLOAD_A}",
                t.overload_amps
            ));
        }
        if t.imbalance_ratio > IMBALANCE_RATIO {
            w.push(format!(
                "thresholds.imbalance_ratio {} is looser than the default {IMBALANCE_RATIO}",
                t.imbalance_ratio
            ));
        }
        if t.min_load_amps > MIN_LOAD_A {
            w.push(format!(
                "thresholds.min_load_amps {} mutes imbalance/disconnect below a higher load than the default {MIN_LOAD_A}",
                t.min_load_amps
            ));
        }
        w
    }
}

fn check_url(name: &str, url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("{name} must start with http:// or https:// (got {url:?})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_all_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn full_example_parses() {
        let cfg = parse(
            r#"
            [thresholds]
            overload_amps = 9.0
            [alerts]
            confirm_samples = 5
            repeat_minutes = 0
            [notify]
            desktop = true
            [notify.ntfy]
            topic = "secret-topic"
            token = "tk_x"
            [notify.webhook]
            url = "https://example.com/hook"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.thresholds.overload_amps, 9.0);
        assert_eq!(cfg.alerts.confirm_samples, 5);
        assert_eq!(cfg.alerts.repeat_minutes, 0);
        let n = cfg.notify.ntfy.as_ref().unwrap();
        assert_eq!(n.url, "https://ntfy.sh", "server URL defaults");
        assert_eq!(n.topic, "secret-topic");
        assert_eq!(cfg.notify.enabled(), vec!["ntfy", "webhook", "desktop"]);
    }

    #[test]
    fn unknown_keys_rejected() {
        // typos must not be silently ignored in a safety tool
        assert!(parse("[thresholds]\noverlaod_amps = 9.0").is_err());
        assert!(parse("[alerting]\nconfirm_samples = 3").is_err());
    }

    #[test]
    fn unusable_values_rejected() {
        assert!(parse("[thresholds]\noverload_amps = 0.0").is_err());
        assert!(parse("[thresholds]\noverload_amps = -1.0").is_err());
        assert!(parse("[alerts]\nconfirm_samples = 0").is_err());
        assert!(parse("[notify.ntfy]\ntopic = \"\"").is_err());
        assert!(parse("[notify.webhook]\nurl = \"example.com\"").is_err());
    }

    #[test]
    fn loosened_thresholds_warn() {
        let cfg = parse("[thresholds]\noverload_amps = 11.0\nimbalance_ratio = 2.0").unwrap();
        let w = cfg.warnings();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("looser"));
        // tighter-than-default is fine
        assert!(parse("[thresholds]\noverload_amps = 8.0")
            .unwrap()
            .warnings()
            .is_empty());
    }

    #[test]
    fn shipped_example_config_matches_schema_and_defaults() {
        let example = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/astral-watch.toml"
        ));
        // commented-out settings use '#key'/'#[table]' (no space); prose comments use '# '
        let uncommented: String = example
            .lines()
            .map(|l| {
                l.strip_prefix('#')
                    .filter(|rest| rest.starts_with(|c: char| c == '[' || c.is_ascii_lowercase()))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = parse(&uncommented).expect("example config must parse once uncommented");
        // the values shown as defaults in the example must BE the defaults
        assert_eq!(cfg.thresholds, crate::alert::Thresholds::default());
        assert_eq!(cfg.alerts, AlertPolicy::default());
        assert!(cfg.notify.ntfy.is_some() && cfg.notify.webhook.is_some());
    }

    #[test]
    fn safety_section_parses_and_validates() {
        let cfg = parse("[safety]\nenabled = true\ntarget_fraction = 0.6\n").unwrap();
        assert!(cfg.safety.enabled);
        assert_eq!(cfg.safety.target_fraction, 0.6);
        // out-of-range fractions rejected (a safety value must be sane)
        assert!(parse("[safety]\ntarget_fraction = 1.5").is_err());
        assert!(parse("[safety]\ntarget_fraction = 0.0").is_err());
        // unknown keys rejected even in the safety table
        assert!(parse("[safety]\nenabledd = true").is_err());
        // the section is compiled unconditionally, so the default (read-only) build parses it
        assert_eq!(
            parse("[safety]\nenabled = false").unwrap().safety,
            SafetyConfig::default()
        );
    }

    #[test]
    fn explicit_path_must_exist() {
        assert!(load(Some(Path::new("/nonexistent/astral.toml"))).is_err());
    }

    #[test]
    fn explicit_path_loads() {
        let dir = std::env::temp_dir().join(format!("aw-cfg-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("c.toml");
        fs::write(&p, "[alerts]\nconfirm_samples = 7\n").unwrap();
        let (cfg, from) = load(Some(&p)).unwrap();
        assert_eq!(cfg.alerts.confirm_samples, 7);
        assert_eq!(from.as_deref(), Some(p.as_path()));
        let _ = fs::remove_dir_all(&dir);
    }
}
