# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **GPU identity correlation** — the startup banner now lists every NVIDIA GPU (VGA *and*
  3D-controller class, so a second card isn't missed) and, after detection, names the card
  actually backing the monitored i2c bus (`# monitoring <pci> (<model>)`) instead of guessing
  the first VGA. On a multi-GPU box it notes that only one card is watched.
- **Graceful shutdown** — on SIGTERM (`systemctl stop`) or SIGINT (Ctrl-C) the watchdog
  flushes queued notifications (best-effort, within a short deadline) instead of dropping a
  final raise/resolve, and exits promptly even while waiting for a GPU.
- **Bus re-detection** — after sustained read failures on an auto-detected bus (e.g. a GPU
  reset renumbered the i2c adapters), the same scoped probe re-attaches. It is pinned to the
  originating card's PCI id, so on a multi-Astral box the watchdog never silently migrates to
  a different card (which would falsely resolve the crashed card's alert).

### Changed
- CI `actions/checkout` bumped to v5 (off the deprecated Node 20 runtime).

## [0.3.1] - 2026-06-15

### Fixed
- Bus autodetection no longer blames an idle GPU for a permission error. Opening `/dev/i2c-*`
  without i2c access (not in the `i2c` group, not root) now reports `permission denied` with
  how to fix it, instead of the misleading "GPU deeply idle? run under load" — which showed
  even with the card at full load. `autodetect_bus` became `detect_bus -> Detect` so the cause
  (no buses / permission denied / no telemetry) is distinguished and surfaced.

### Changed
- README `Setup` section rewritten to define the access model (i2c group / sudo) and the GPU
  load requirement up front, with quick-try, service-install, and run-without-sudo paths.

## [0.3.0] - 2026-06-11

### Added
- **Prometheus exporter** — opt-in via the `[export]` config section (any mode) or the new
  `export` subcommand. Scrapes render a cached snapshot and never touch the i2c bus.
  Metrics: per-pin volts/amps, totals, balance ratio (`+Inf` when a pin reads ~0 A under
  load — the series never vanishes at maximal imbalance), debounced alert gauges and raise
  counters, staleness, sample/failure counters, build info.
- **Grafana dashboard** (`docs/grafana-dashboard.json`) — per-pin current with the 9.2 A
  threshold, balance ratio, total power, alert state timeline, and watchdog stats that show
  red `NO SIGNAL` (not stale green) when the process dies.
- **Distribution** — tag-triggered release workflow publishing prebuilt Linux binaries
  (gnu + static musl) with checksums; an AUR `PKGBUILD` ([`packaging/aur/`](packaging/aur/));
  a packager-facing `make install-files` target (no cargo invocation, vendor paths under
  `/usr/lib`); CI jobs for MSRV, crates.io packaging, staged-install + systemd/udev
  validation, and `cargo audit`.

### Changed
- `monitor`/`log` internals unified into one sampling loop with display/CSV/metrics sinks.
- The exporter binds before GPU detection, so scrapes see `up 0` while waiting for a card.
- A bind failure disables the exporter with a warning in monitor/log modes (the watchdog
  keeps sampling); only the `export` subcommand fails fast.
- MSRV raised to **1.85** (clap 4.6 / ureq 3 / toml 0.9).

## [0.2.0] - 2026-06-11

### Added
- **Alert lifecycle** — majority-window confirmation (3 of the last 5 samples by default),
  consecutive-clean resolution, periodic re-notification. Telemetry-loss samples freeze the
  physical conditions: no-data can neither confirm an alert nor fake an all-clear.
- **Notification delivery** — ntfy push, generic JSON webhooks, desktop `notify-send`; one
  worker thread and bounded raise-first queue per transport, three attempts per message.
- **Config file** — `/etc/astral-watch.toml` < `~/.config/astral-watch/config.toml` <
  `--config`; thresholds, alert windows, transports. Unknown keys are rejected; loosened
  thresholds warn at startup. `make install` places a commented example if absent.
- Startup now waits for a readable GPU bus (raising TELEMETRY LOST through the notifier)
  instead of exiting into a silent systemd restart loop.

### Changed
- stderr carries debounced lifecycle events instead of per-sample alert lines; the per-sample
  record stays in the CSV, and is mirrored to stderr while the CSV is unwritable.
- systemd unit allows `AF_INET`/`AF_INET6` so configured notifications can leave the box.

## [0.1.1] - 2026-06-11

### Fixed
- CSV alerts field is RFC-4180-quoted and alert text comma-free — multi-pin alerts no longer
  corrupt the forensic record.
- i2c read errors are no longer conflated with implausible readings (`GPU_UNREACHABLE` vs
  `IMPLAUSIBLE_READING`), and a failing CSV write degrades instead of killing the watchdog.
- Telemetry words are read hi→lo→hi with re-reads while the value moves, eliminating torn
  multi-amp phantom spikes; `docs/SAFETY.md` documents the ~36 read-only transactions.
- `--interval` validation (negative panicked, zero busy-looped), `--keep 0` semantics,
  negative `--max-mb`, `0X` hex prefixes; header is written to any empty CSV.
- Packaging: `ExecStartPre` modprobe runs privileged instead of silently failing under
  `NoNewPrivileges`; sysusers.d/modules-load.d snippets replace echoed manual steps;
  `make install` honors `PREFIX` in the unit and runs the system steps on live installs;
  staged (`DESTDIR`) installs place the unit in `/usr/lib/systemd/system`; uninstall stops
  the service.

## [0.1.0] - 2026-06-10

Initial release: per-pin 12V-2x6 telemetry (ITE IT8915FN over `/dev/i2c-*`), live display,
auto-rotating CSV logging with falloff capture, overload/disconnect/imbalance alerts,
hardened systemd service + udev rule, read-only safety design (`docs/SAFETY.md`).

[Unreleased]: https://github.com/mbeaman/astral-watch/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/mbeaman/astral-watch/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/mbeaman/astral-watch/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/mbeaman/astral-watch/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/mbeaman/astral-watch/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/mbeaman/astral-watch/releases/tag/v0.1.0
