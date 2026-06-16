# astral-watch

Per-pin **12V-2x6 / 12VHPWR** power monitoring and connector-melt early-warning for
**ASUS ROG Astral** GPUs on **Linux**.

ASUS ROG Astral cards carry an **ITE IT8915FN** microcontroller that measures voltage and
current on each of the six 12V pins of the power connector — the data behind ASUS's
"Power Detector+" feature. That tool is **Windows-only**. `astral-watch` reads the same chip
over `/dev/i2c-*` on Linux, shows it live, logs it, and **alerts on the imbalance that
precedes a melted connector**.

> ⚠️ Uneven per-pin current is the signature of a poorly-seated 12V-2x6 connector and the
> mechanism behind the RTX 40/50-series melting failures. Catching it early is the point.

## Features

- **Live per-pin display** — voltage, current, total, and balance ratio, refreshing in place.
- **CSV logging with rotation** — continuous, append-only, self-rotating; survives reboots as a service.
- **Alerts** — per-pin overload (`>9.2 A`), disconnect (`~0 A` under load), and imbalance (`hi/lo > 1.5×`).
- **Alerts that reach you** — phone/browser push via [ntfy](https://ntfy.sh), generic JSON
  webhooks, and desktop notifications. A debounced raise/resolve lifecycle sends *one* alert
  per incident (plus periodic reminders), not thousands of per-sample messages.
- **Prometheus + Grafana** — a built-in exporter (`GET /metrics`) and a ready-made
  [dashboard](docs/grafana-dashboard.json); scrapes read a cached snapshot and never touch
  the i2c bus.
- **Falloff capture** — writes a `GPU_UNREACHABLE` row the instant the GPU drops off the bus,
  so the per-pin state *right before* a crash is preserved.
- **Read-only & safe** — only ever writes the i2c register pointer, never a data byte (see [Safety](#safety)).
- Small, dependency-light single binary.

## Supported cards

Per-pin telemetry is an ASUS **ROG Astral / Matrix** feature (TUF/Prime don't have the chip).
`astral-watch` works on any Astral that answers with plausible telemetry; the table below is just
for naming. Don't see yours? [Add it](CONTRIBUTING.md).

| Subsystem (`1043:____`) | Model |
|---|---|
| `89ED` | ROG Astral RTX 5090 O32G Gaming |
| `89EA`, `89E3`, `89DE` | ROG Astral RTX 5090 |
| `8A61` | ROG Astral RTX 5090 LC |
| `8A2E` | ROG Astral RTX 5090 (variant) |
| `89EC` | ROG Astral RTX 5080 |

## Setup

Two things gate access, and the tool tells you which is missing if you skip one:

- **i2c bus access.** Readings come from `/dev/i2c-*`, which is root-owned and group-readable
  (the shipped udev rule puts the NVIDIA adapters in the **`i2c` group**, mode `0660`). So
  `astral-watch` must run **as root (`sudo`)** or **as a user in the `i2c` group** — otherwise
  it reports `permission denied opening /dev/i2c-*`.
- **A GPU under load.** The chip only answers with plausible telemetry while the card is
  drawing power; on a deeply idle GPU autodetect finds nothing. Run a game or benchmark.

You also need the `i2c-dev` kernel module (`sudo modprobe i2c-dev`; the service loads it for
you). Building from source needs Rust 1.85+.

### Quick try

Grab a prebuilt binary (gnu + static musl) from the
[releases page](https://github.com/mbeaman/astral-watch/releases), or build from source:

```sh
git clone https://github.com/mbeaman/astral-watch
cd astral-watch
cargo build --release
sudo modprobe i2c-dev
sudo ./target/release/astral-watch          # live per-pin view, under load
```

A bare binary (a prebuilt download, or `cargo install astral-watch`) is *only* the binary —
`sudo` runs it, but the udev rule, the unprivileged service, and the `i2c` group come from the
full install below.

### Install the service (recommended)

The way to run it continuously: an auto-restarting logger that survives reboots and runs
**unprivileged** — as a dedicated `astral-watch` user in the `i2c` group, granted bus access by
the shipped udev rule.

```sh
sudo make install     # binary -> /usr/local/bin; udev rule; systemd unit; sysusers +
                      # modules-load snippets. Creates the i2c group and service user,
                      # loads i2c-dev, reloads udev/systemd.
sudo systemctl enable --now astral-watch
```

It logs to `/var/log/astral-watch/gpu-pins.csv` and reads `/etc/astral-watch.toml` (a commented
example is installed there — see [Configuration](#configuration)). On Arch, use the
[AUR `PKGBUILD`](packaging/aur/) instead of `make install`.

### Run it yourself without sudo

`make install` creates the `i2c` group; add your user to it and re-login:

```sh
sudo usermod -aG i2c "$USER"     # then log out and back in
astral-watch                     # no sudo needed; the service keeps logging in the background
```

## Usage

```sh
astral-watch                       # live per-pin display (default)
astral-watch log gpu-pins.csv      # log to CSV (auto-rotates at 50 MB, keeps 5 backups)
astral-watch log --interval 0.25   # faster sampling to catch transients
astral-watch export                # serve Prometheus metrics on 127.0.0.1:9942
astral-watch --bus 0 --addr 0x2b   # pin the bus/address manually
```

For Prometheus alongside CSV logging (e.g. the systemd service), add to the config instead:

```toml
[export]
listen = "127.0.0.1:9942"
```

then import [`docs/grafana-dashboard.json`](docs/grafana-dashboard.json) into Grafana. If you
alert through Prometheus, pair the metrics with an `absent(astral_watch_up)` (or
`up{job="..."} == 0`) rule — a dead exporter can't report its own death.

CSV columns: `timestamp, p1_V, p1_A, … p6_V, p6_A, total_A, total_W, balance, alerts`.

## Configuration

Optional TOML config: `/etc/astral-watch.toml` (the service reads this; `make install` puts a
fully commented example there), overridden by `~/.config/astral-watch/config.toml`, overridden
by `--config PATH`. Everything has safe defaults; the part most people want:

```toml
[notify.ntfy]
topic = "your-unguessable-topic"   # then subscribe in the ntfy app — that's it
```

Thresholds, the alert confirm/resolve windows, re-notification cadence, webhooks, and desktop
notifications are covered in the example file ([`packaging/astral-watch.toml`](packaging/astral-watch.toml)).
An alert raises once the condition is seen in 3 of the last 5 samples (1.5 s for a steady
fault as the shipped service samples at 0.5 s — and an oscillating one still confirms) and
resolves after 20 consecutive clean samples; both are configurable. No-data samples never
count as healthy: telemetry loss can't fake an all-clear.

## How it works

The IT8915FN sits at i2c address `0x2B` on the GPU's own NVIDIA i2c adapter. Register `0x80`
exposes a 24-byte block: six pins × `(u16 mV, u16 mA)` big-endian, in reverse pin order. It is
read **byte-by-byte** (a single block read returns garbage on some SKUs).

## Safety

This tool reads a chip on a live GPU's i2c bus. The access is **read-only**: a state-changing
i2c write requires a data byte *after* the register pointer — `astral-watch` only ever writes the
register pointer (`0x80…`) and then reads. It targets a single known address and never bus-scans.
See [`docs/SAFETY.md`](docs/SAFETY.md).

## Roadmap

- **0.1:** read + decode + alerts + CSV/rotation + service. *(MVP)* ✓
- **0.1.1:** hardening — CSV integrity, tear-resistant reads, install-path fixes. ✓
- **0.2:** alert lifecycle (raise/resolve/repeat) + ntfy/webhook/desktop delivery + config file. ✓
- **0.3 (here):** Prometheus exporter + Grafana dashboard, unified sampler/sink loop;
  release infrastructure — prebuilt binaries, AUR `PKGBUILD`, crates.io packaging.
- **next:** a TUI.
- **later:** opt-in **safety daemon** (auto power-cap via NVML on sustained overload),
  high-rate event-capture ring buffer, multi-GPU identity correlation.

## Credits

Built on reverse-engineering by the community:
[`sus`](https://github.com/jan-provaznik/sus),
[LACT issue #906](https://github.com/ilya-zlobintsev/LACT/issues/906),
[LibreHardwareMonitor #2168](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/pull/2168),
and [`astral-power-monitoring`](https://github.com/Timic3/astral-power-monitoring).

## License & disclaimer

MIT — see [LICENSE](LICENSE). Not affiliated with or endorsed by ASUS or NVIDIA. Use at your own
risk; the authors are not liable for any hardware damage.
