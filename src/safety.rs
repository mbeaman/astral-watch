//! Opt-in NVML auto power-cap safety daemon (`--features safety`).
//!
//! astral-watch's **only** GPU-state mutation, and only when explicitly armed. On a confirmed,
//! sustained connector overload (or a disconnected pin under load) it reduces the GPU's power
//! limit via NVML, lowering aggregate — and therefore per-pin — current to slow a connector
//! melt. It is harm-*minimization*, not a cure: a board-level cap cannot rebalance a single
//! high-resistance pin, only reduce how much current that pin carries (see `docs/SAFETY.md`).
//!
//! Design (the safety-critical invariants, decided with an adversarial hardware-safety review):
//!   - **Off** unless built with `--features safety`, run as the `safety` subcommand, AND
//!     `[safety] enabled = true`. The default build and service stay read-only.
//!   - **Latched.** One decisive cap, held until the daemon exits with no cap engaged, an
//!     operator runs `restore-power-limit`, or the machine reboots (the NVML limit is volatile).
//!     Auto-recovery is unsafe: the cap *causes* the overload reading to clear, so releasing on
//!     "clear" would flap the limit and falsely report all-clear on a still-damaged connector.
//!   - **Never-raise.** It only ever *lowers* the limit (`min(target, the limit in effect)`); if
//!     it can't reduce further it does not write and loudly reports the lever is exhausted
//!     (likely a true hardware fault needing physical inspection).
//!   - **Fail-safe.** On exit while a cap is engaged it *holds* the cap (a confirmed overload
//!     occurred — never slam full current back onto a suspect connector); state is persisted to
//!     tmpfs `/run` so a crash + restart adopts the live cap instead of ratcheting down. A
//!     SIGKILL or reboot leaves the card at most *under*-powered, which can never melt a pin.
//!   - **Right GPU, confirmed.** The card is matched to the monitored i2c bus by PCI id — never
//!     NVML index 0 — and the limit is read back after setting to confirm it took.
//!
//! It runs its own read-only i2c sampling loop and its own alert [`Lifecycle`], independent of
//! the unprivileged monitor, so protection never depends on the monitor's liveness.

use crate::alert::evaluate;
use crate::config::Config;
use crate::decode::Reading;
use crate::i2c::{bus_pci_id, norm_pci, read_reading, redetect_card, REDETECT_AFTER};
use crate::lifecycle::{condition_of, Condition, Event, Lifecycle};
use crate::notify::{Dispatcher, Message, Priority};
use anyhow::{bail, Context, Result};
use chrono::Local;
use nvml_wrapper::{Device, Nvml};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// tmpfs directory for the cap state file (cleared on reboot — same lifetime as the volatile
/// NVML limit, so a surviving state file always means a still-live cap).
const STATE_DIR: &str = "/run/astral-watch-safety";
const STATE_FILE: &str = "/run/astral-watch-safety/cap-state.json";

/// A pin physically can't carry this many amps before failing — a reading above it is a torn
/// i2c read, not a real overload. A separate, stricter gate than [`Reading::plausible`] (which
/// only checks voltage) because the daemon *acts* on the value, with no human in the loop.
const MAX_PIN_AMPS: f64 = 20.0;
const MAX_TOTAL_AMPS: f64 = 100.0;

/// Tolerance (mW) when comparing the in-effect limit to what we set — NVML may round, and
/// another power-limit manager changing it by more than this means "someone else took over".
const LIMIT_TOL_MW: u32 = 2_000;

/// What an attempted cap resolves to. Pure, so it is exhaustively testable without a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapDecision {
    /// Write this limit (mW); guaranteed strictly below the current limit and within [min,max].
    Apply(u32),
    /// The lever is exhausted — the safe target is not below the limit already in effect.
    Exhausted,
}

/// Resolve the cap: a fraction of the stock limit, clamped to NVML's constraints, and — the
/// critical invariant — never above the limit already in effect, so a fault can never *raise*
/// power (e.g. on an already-undervolted card).
pub fn compute_cap_mw(
    default_mw: u32,
    current_mw: u32,
    min_mw: u32,
    max_mw: u32,
    fraction: f64,
) -> CapDecision {
    let target = (f64::from(default_mw) * fraction).round() as u32;
    let target = target.clamp(min_mw, max_mw);
    if target < current_mw {
        CapDecision::Apply(target)
    } else {
        CapDecision::Exhausted
    }
}

/// Stricter, action-gating plausibility: every pin and the total within physical bounds. A
/// torn read that slips past the voltage-only [`Reading::plausible`] must not drive a cap.
fn current_plausible(r: &Reading) -> bool {
    r.pins
        .iter()
        .all(|p| p.amps.is_finite() && (0.0..=MAX_PIN_AMPS).contains(&p.amps))
        && r.total_amps() <= MAX_TOTAL_AMPS
}

fn is_trigger(c: Condition, cfg: &crate::config::SafetyConfig) -> bool {
    match c {
        Condition::Overload => true,
        Condition::Disconnected => cfg.trigger_disconnect,
        Condition::Imbalance => cfg.trigger_imbalance,
        Condition::TelemetryLost => false,
    }
}

fn within(a: u32, b: u32, tol: u32) -> bool {
    a.abs_diff(b) <= tol
}

/// Persisted across a crash + same-boot restart so the daemon adopts a live cap (and keeps the
/// *true* original to restore) instead of reading the already-lowered limit as "original".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CapState {
    pci: String,
    original_mw: u32,
    capped_mw: u32,
    ts: String,
}

fn write_state_at(path: &Path, st: &CapState) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // write-then-rename so a crash mid-write can't leave a half-written state file
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(st)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_state_at(path: &Path) -> Option<CapState> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_state(st: &CapState) -> Result<()> {
    write_state_at(Path::new(STATE_FILE), st)
}
fn read_state() -> Option<CapState> {
    read_state_at(Path::new(STATE_FILE))
}
fn clear_state() {
    let _ = fs::remove_file(STATE_FILE);
}

// ───────────────────────────── NVML I/O ─────────────────────────────

/// Find the NVML device for the monitored card by PCI id — enumerate and match on the
/// device's own bus id (normalized), never falling back to index 0 (NVML enumeration order is
/// not PCI order, so index 0 could be a healthy sibling on a multi-GPU box).
fn find_device<'a>(nvml: &'a Nvml, want_pci: &str) -> Result<Device<'a>> {
    let want = norm_pci(want_pci);
    let count = nvml.device_count().context("NVML device_count")?;
    for i in 0..count {
        let dev = nvml
            .device_by_index(i)
            .with_context(|| format!("NVML device_by_index({i})"))?;
        if let Ok(info) = dev.pci_info() {
            if norm_pci(&info.bus_id) == want {
                return Ok(dev);
            }
        }
    }
    bail!("no NVML device matches the monitored card {want_pci}; refusing to act (never caps device 0)")
}

/// (default, min, max, current) power limit in milliwatts.
fn read_limits(nvml: &Nvml, pci: &str) -> Result<(u32, u32, u32, u32)> {
    let dev = find_device(nvml, pci)?;
    let default_mw = dev
        .power_management_limit_default()
        .context("NVML default power limit")?;
    let cons = dev
        .power_management_limit_constraints()
        .context("NVML power-limit constraints")?;
    let current_mw = dev
        .power_management_limit()
        .context("NVML current power limit")?;
    Ok((default_mw, cons.min_limit, cons.max_limit, current_mw))
}

fn read_current(nvml: &Nvml, pci: &str) -> Result<u32> {
    find_device(nvml, pci)?
        .power_management_limit()
        .context("NVML current power limit")
}

/// Set the limit and read it back (the *management* limit, not `enforced_power_limit` which is
/// the instantaneous thermally-clamped value and would falsely look like the set "didn't take").
fn set_and_confirm(nvml: &Nvml, pci: &str, target_mw: u32) -> Result<u32> {
    let mut dev = find_device(nvml, pci)?;
    dev.set_power_management_limit(target_mw)
        .context("NVML set_power_management_limit (needs root)")?;
    dev.power_management_limit()
        .context("NVML power limit read-back")
}

enum RestoreOutcome {
    Restored,
    /// The in-effect limit is no longer the value we set — another tool took over; left as-is.
    LeftAlone,
}

fn restore_to_original(
    nvml: &Nvml,
    pci: &str,
    original_mw: u32,
    capped_mw: u32,
) -> Result<RestoreOutcome> {
    let mut dev = find_device(nvml, pci)?;
    let cur = dev.power_management_limit().context("NVML current limit")?;
    if within(cur, capped_mw, LIMIT_TOL_MW) {
        dev.set_power_management_limit(original_mw)
            .context("NVML restore power limit")?;
        Ok(RestoreOutcome::Restored)
    } else {
        Ok(RestoreOutcome::LeftAlone)
    }
}

// ───────────────────────────── the daemon ─────────────────────────────

/// Owns the NVML handle and the engaged-cap state; on drop it HOLDS an engaged cap (never
/// restores on exit — a confirmed overload occurred) and leaves the state file for
/// `restore-power-limit` or a same-boot restart to adopt.
struct CapGuard {
    nvml: Nvml,
    pci: String,
    original_mw: u32,
    capped_mw: u32,
    engaged: bool,
}

impl Drop for CapGuard {
    fn drop(&mut self) {
        if !self.engaged {
            return;
        }
        eprintln!(
            "# safety: EXITING WITH POWER CAP STILL ENGAGED on {} (now {}W). A sustained \
             overload was detected and the cap is LATCHED — inspect the connector, then run \
             `astral-watch restore-power-limit` or reboot to restore {}W.",
            self.pci,
            self.capped_mw / 1000,
            self.original_mw / 1000,
        );
    }
}

/// The fixed inputs to the cap computation, read once from NVML at startup.
#[derive(Debug, Clone, Copy)]
struct CapParams {
    default_mw: u32,
    min_mw: u32,
    max_mw: u32,
    fraction: f64,
}

impl CapParams {
    fn decide(&self, current_mw: u32) -> CapDecision {
        compute_cap_mw(
            self.default_mw,
            current_mw,
            self.min_mw,
            self.max_mw,
            self.fraction,
        )
    }
}

/// Try to engage a cap for a confirmed trigger condition. No-op for an already-engaged cap.
fn try_engage(
    guard: &mut CapGuard,
    dispatcher: &Dispatcher,
    params: &CapParams,
    ts: &str,
    cond: Condition,
) {
    let cur = match read_current(&guard.nvml, &guard.pci) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("# safety: cannot read the current power limit to cap: {e:#}");
            dispatcher.publish(audit(
                "cap failed",
                format!(
                    "{} on {} but reading the power limit failed: {e:#}",
                    cond.label(),
                    guard.pci
                ),
                ts,
            ));
            return;
        }
    };
    match params.decide(cur) {
        CapDecision::Apply(target) => match set_and_confirm(&guard.nvml, &guard.pci, target) {
            Ok(readback) if within(readback, target, LIMIT_TOL_MW) => {
                guard.original_mw = cur;
                guard.capped_mw = readback;
                guard.engaged = true;
                if let Err(e) = write_state(&CapState {
                    pci: guard.pci.clone(),
                    original_mw: cur,
                    capped_mw: readback,
                    ts: ts.to_string(),
                }) {
                    eprintln!("# safety: warning: could not persist cap state: {e:#} (crash recovery degraded)");
                }
                eprintln!(
                    "# safety: CAPPED {} {}W -> {}W ({})",
                    guard.pci,
                    cur / 1000,
                    readback / 1000,
                    cond.label()
                );
                dispatcher.publish(audit(
                    "power capped",
                    format!(
                        "{} on {} — capped {}W -> {}W to cut connector current. The fault is NOT \
                         cleared; inspect the connector. Restore: `astral-watch restore-power-limit`",
                        cond.label(),
                        guard.pci,
                        cur / 1000,
                        readback / 1000
                    ),
                    ts,
                ));
            }
            Ok(readback) => {
                eprintln!(
                    "# safety: cap DID NOT STICK on {} (asked {}W, limit now {}W) — NOT protected",
                    guard.pci,
                    target / 1000,
                    readback / 1000
                );
                dispatcher.publish(audit(
                    "cap failed",
                    format!(
                        "{} on {} but the power limit did not change (asked {}W, still {}W) — the \
                         GPU is NOT protected; physical inspection required",
                        cond.label(),
                        guard.pci,
                        target / 1000,
                        readback / 1000
                    ),
                    ts,
                ));
            }
            Err(e) => {
                eprintln!("# safety: set_power_management_limit failed: {e:#}");
                dispatcher.publish(audit(
                    "cap failed",
                    format!(
                        "{} on {} but the NVML power-limit write failed: {e:#}",
                        cond.label(),
                        guard.pci
                    ),
                    ts,
                ));
            }
        },
        CapDecision::Exhausted => {
            eprintln!(
                "# safety: cannot reduce power further on {} (limit {}W already at/below the safe \
                 target) — likely a hardware fault; inspect the connector",
                guard.pci,
                cur / 1000
            );
            dispatcher.publish(audit(
                "cannot reduce power",
                format!(
                    "{} on {} but the power limit ({}W) is already at/below the safe target — the \
                     cap lever is exhausted; this is likely a true hardware fault needing physical \
                     intervention",
                    cond.label(),
                    guard.pci,
                    cur / 1000
                ),
                ts,
            ));
        }
    }
}

/// A loud, urgent audit notification for a cap action (its own channel — the daemon never
/// emits a lifecycle "resolved", which on a capped fault would be a false all-clear).
fn audit(what: &str, body: String, ts: &str) -> Message {
    Message {
        kind: "raised",
        condition: "power_safety",
        title: format!("astral-watch SAFETY: {what}"),
        body,
        priority: Priority::Urgent,
        ts: ts.to_string(),
    }
}

fn nap(dur: Duration, shutdown: &AtomicBool) {
    let step = Duration::from_millis(200);
    let mut left = dur;
    while !left.is_zero() && !shutdown.load(Ordering::Relaxed) {
        let n = left.min(step);
        sleep(n);
        left -= n;
    }
}

/// Run the safety daemon until shutdown. Refuses to start (loud) rather than become a silent
/// no-op when it can't actually protect the GPU.
#[allow(clippy::too_many_arguments)]
pub fn run_safety(
    mut bus: u32,
    addr: u16,
    interval: Duration,
    cfg: &Config,
    dispatcher: &Dispatcher,
    auto: bool,
    shutdown: &AtomicBool,
) -> Result<()> {
    eprintln!("# ============================================================");
    eprintln!("# astral-watch SAFETY DAEMON");
    eprintln!("# This is the ONLY mode that mutates GPU state: on a sustained");
    eprintln!("# connector overload it REDUCES the GPU power limit via NVML.");
    eprintln!("# ============================================================");

    let scfg = &cfg.safety;
    if !scfg.enabled {
        bail!("[safety] enabled = false — refusing to run the actuating daemon. Set `[safety] enabled = true` in the config to arm it.");
    }

    let pci = bus_pci_id(bus).context(
        "cannot resolve the PCI id of the monitored i2c bus — refusing to cap a GPU I can't identify",
    )?;
    let nvml = Nvml::init().context(
        "NVML init failed (libnvidia-ml missing, or a driver/userspace version mismatch). The safety daemon needs NVML to act — not starting.",
    )?;
    let (default_mw, min_mw, max_mw, current_mw) = read_limits(&nvml, &pci)?;
    // refuse implausible constraints rather than compute a nonsense cap
    if !(min_mw < max_mw
        && (min_mw..=max_mw).contains(&default_mw)
        && (50_000..=2_000_000).contains(&default_mw))
    {
        bail!(
            "NVML power-limit constraints look implausible for {pci} (default {}W, range {}-{}W) — refusing to act",
            default_mw / 1000,
            min_mw / 1000,
            max_mw / 1000
        );
    }
    let params = CapParams {
        default_mw,
        min_mw,
        max_mw,
        fraction: scfg.target_fraction,
    };
    let preview = match params.decide(current_mw) {
        CapDecision::Apply(t) => format!("{}W", t / 1000),
        CapDecision::Exhausted => "no headroom (already at/below target)".into(),
    };
    eprintln!(
        "# safety: {pci}  limit {}W  default {}W  range {}-{}W  -> on overload cap to {preview}  (latched; trigger disconnect={} imbalance={})",
        current_mw / 1000,
        default_mw / 1000,
        min_mw / 1000,
        max_mw / 1000,
        scfg.trigger_disconnect,
        scfg.trigger_imbalance,
    );

    let mut guard = CapGuard {
        nvml,
        pci: pci.clone(),
        original_mw: current_mw,
        capped_mw: 0,
        engaged: false,
    };

    // Adopt a same-boot cap left by a crash/restart: keep the cap engaged with the TRUE
    // original, rather than restoring (which would re-expose the connector) or treating the
    // already-lowered limit as the original (which would ratchet down).
    if let Some(st) = read_state() {
        if norm_pci(&st.pci) == norm_pci(&pci) {
            guard.original_mw = st.original_mw;
            guard.capped_mw = st.capped_mw;
            guard.engaged = true;
            eprintln!(
                "# safety: adopted an existing cap from {STATE_FILE} (orig {}W, cap {}W) — latched; restore with `astral-watch restore-power-limit`",
                st.original_mw / 1000,
                st.capped_mw / 1000
            );
        } else {
            clear_state(); // stale entry for a different card
        }
    }

    let mut lifecycle = Lifecycle::new(cfg.alerts);
    let mut misses = 0u32;
    let card = if auto { Some(pci.clone()) } else { None };

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(()); // guard drops -> holds an engaged cap
        }
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut conditions: Vec<(Condition, String)> = Vec::new();
        match read_reading(bus, addr) {
            Ok(r) if r.plausible() && current_plausible(&r) => {
                misses = 0;
                let alerts = evaluate(&r, &cfg.thresholds);
                conditions.extend(alerts.iter().map(|a| (condition_of(a), a.to_string())));
            }
            Ok(_) => {
                misses += 1;
                conditions.push((
                    Condition::TelemetryLost,
                    "implausible reading (failed sanity checks)".into(),
                ));
            }
            Err(e) => {
                misses += 1;
                conditions.push((Condition::TelemetryLost, format!("read failed: {e:#}")));
            }
        }

        for ev in lifecycle.observe(Instant::now(), &conditions) {
            // The cap is latched: act only on a fresh raise of a trigger condition. We never
            // publish Repeated/Resolved — a "resolved" while capped would be a false all-clear,
            // because the cap is what pulled current down.
            if let Event::Raised { condition, .. } = ev {
                if is_trigger(condition, scfg) && !guard.engaged {
                    try_engage(&mut guard, dispatcher, &params, &ts, condition);
                }
            }
        }

        // re-detect the bus only on an auto-detected, card-pinned basis, like the monitor
        if auto && misses >= REDETECT_AFTER {
            misses = 0;
            if let Some(want) = card.as_deref() {
                if let Some(b2) = redetect_card(addr, want) {
                    bus = b2;
                }
            }
        }

        nap(interval, shutdown);
    }
    // Unreachable: the loop only returns via the shutdown branch above. The dispatcher and
    // guard drop here (guard holds an engaged cap).
}

/// `restore-power-limit` subcommand: undo a cap recorded in the state file. Safe to run any
/// time; a no-op if no astral-watch cap is recorded.
pub fn run_restore() -> Result<()> {
    let Some(st) = read_state() else {
        eprintln!(
            "no astral-watch power cap is recorded ({STATE_FILE} absent) — nothing to restore. A reboot clears any volatile cap."
        );
        return Ok(());
    };
    let nvml = Nvml::init().context("NVML init failed — cannot restore the power limit")?;
    match restore_to_original(&nvml, &st.pci, st.original_mw, st.capped_mw)? {
        RestoreOutcome::Restored => eprintln!(
            "restored {} power limit to {}W",
            st.pci,
            st.original_mw / 1000
        ),
        RestoreOutcome::LeftAlone => eprintln!(
            "the power limit on {} is no longer the value astral-watch set ({}W) — another tool changed it; leaving it as-is",
            st.pci,
            st.capped_mw / 1000
        ),
    }
    clear_state();
    Ok(())
}

/// Where the state file lives (for docs/tests).
pub fn state_dir() -> PathBuf {
    PathBuf::from(STATE_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SafetyConfig;
    use crate::decode::{Pin, Reading, PIN_COUNT};

    fn reading(amps: [f64; 6]) -> Reading {
        Reading {
            pins: amps.map(|a| Pin {
                volts: 11.97,
                amps: a,
            }),
        }
    }

    #[test]
    fn cap_lowers_to_fraction_of_default() {
        // 575W stock, 600W max, 100W min, currently at stock; 50% -> 287.5W (rounds to 287500mW)
        let d = compute_cap_mw(575_000, 575_000, 100_000, 600_000, 0.5);
        assert_eq!(d, CapDecision::Apply(287_500));
    }

    #[test]
    fn cap_never_raises_on_an_undervolted_card() {
        // user already runs a 250W undervolt; 50% of 575W = 287W is HIGHER -> must not raise
        assert_eq!(
            compute_cap_mw(575_000, 250_000, 100_000, 600_000, 0.5),
            CapDecision::Exhausted
        );
    }

    #[test]
    fn cap_clamps_to_min_constraint() {
        // 10% of 575W = 57.5W is below the 100W floor -> clamp to 100W (still below current)
        assert_eq!(
            compute_cap_mw(575_000, 575_000, 100_000, 600_000, 0.1),
            CapDecision::Apply(100_000)
        );
    }

    #[test]
    fn cap_exhausted_when_floor_at_or_above_current() {
        // already at the NVML floor: nothing lower to set
        assert_eq!(
            compute_cap_mw(575_000, 100_000, 100_000, 600_000, 0.1),
            CapDecision::Exhausted
        );
    }

    #[test]
    fn current_plausibility_rejects_torn_reads() {
        assert!(current_plausible(&reading([8.2, 8.6, 8.3, 8.4, 8.5, 8.8])));
        // a single pin pinned at the 16-bit max (65.535A) is a torn read, not an overload
        let mut torn = [8.0; 6];
        torn[2] = 65.535;
        assert!(!current_plausible(&reading(torn)));
        // total over the connector's physical ceiling
        assert!(!current_plausible(&reading([19.0; 6])));
    }

    #[test]
    fn triggers_respect_config() {
        let mut c = SafetyConfig::default();
        assert!(is_trigger(Condition::Overload, &c));
        assert!(is_trigger(Condition::Disconnected, &c)); // default on
        assert!(!is_trigger(Condition::Imbalance, &c)); // default off
        assert!(!is_trigger(Condition::TelemetryLost, &c)); // never
        c.trigger_disconnect = false;
        c.trigger_imbalance = true;
        assert!(!is_trigger(Condition::Disconnected, &c));
        assert!(is_trigger(Condition::Imbalance, &c));
    }

    #[test]
    fn state_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("aw-safety-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("cap-state.json");
        let st = CapState {
            pci: "0000:0b:00.0".into(),
            original_mw: 575_000,
            capped_mw: 287_500,
            ts: "2026-06-17T12:00:00".into(),
        };
        write_state_at(&path, &st).unwrap();
        assert_eq!(read_state_at(&path).unwrap(), st);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn within_tolerance() {
        assert!(within(287_500, 287_000, LIMIT_TOL_MW));
        assert!(!within(287_500, 250_000, LIMIT_TOL_MW));
    }

    #[test]
    fn pin_count_is_six() {
        assert_eq!(PIN_COUNT, 6);
    }
}
