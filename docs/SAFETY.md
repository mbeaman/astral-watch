# Safety: read-only by default

`astral-watch` reads a sensor chip (ITE IT8915FN) that lives on a live GPU's i2c bus. This
document explains why that access cannot change device state, and what precautions the tool takes.

The default build and the shipped service are **strictly read-only**. There is exactly one
exception — the [opt-in safety daemon](#opt-in-safety-daemon), a separately-built,
separately-enabled, off-by-default component that can reduce the GPU power limit via NVML to
slow a connector melt. It is documented in full below; nothing else ever writes GPU state.

## The access is read-only

For each telemetry byte, the tool issues an **SMBus read-byte-data** transaction:

```
S  Addr+W  [A]  Reg(0x80..)  [A]   Sr  Addr+R  [A]  Data  [NA]  P
```

It writes exactly **one byte — the register pointer** (`0x80…0x97`) — and then reads. A
*state-changing* i2c/SMBus write requires the host to transmit a **data byte after** the register
pointer (`… Reg [A] Data [A] P`). `astral-watch` never sends that trailing data byte, so even if a
register were writable, nothing is written.

Because each 16-bit value spans two transactions, the chip can update it between the high- and
low-byte reads ("tearing"), which could fabricate a current spike that never happened. Each u16
is therefore read **high → low → high**, re-reading while the high byte moves. A snapshot is
typically **36 transactions** (three per 16-bit value, more only while a value is changing) —
all of them the identical read-only pattern above.

This is identical to what `i2cget … b` does, and to the reads in `sus`, the LACT #906
proof-of-concept, and LibreHardwareMonitor (`NvAPI_I2CReadEx`). All four independently use the same
read-only register map (`0x2B` / `0x80` / 24 bytes), and two ROG Astral owners have confirmed it on
Linux with no side effects.

## Precautions taken

- **No bus scanning.** The tool only ever addresses the single known chip address (`0x2B`); it never
  runs an `i2cdetect`-style probe, which can disturb other devices on the GPU's i2c bus. Bus
  *re-detection* after a sustained read failure (e.g. the GPU reset and the kernel renumbered the
  adapters) reuses this same scoped probe — `0x2B` on NVIDIA-named adapters only — never a scan.
- **Targeted bus.** It only touches i2c adapters whose sysfs `name` identifies them as NVIDIA.
- **Plausibility gating.** A response is only trusted if it decodes to a sane rail voltage (~12 V);
  otherwise it's reported as "not the chip / unsupported SKU" rather than shown as data.
- **Least privilege.** The provided systemd unit runs as an unprivileged user in the `i2c` group,
  granted access to only the NVIDIA i2c nodes via the shipped udev rule.

## Alerting and time-to-alert

Notifications are debounced with a majority window: an alert raises once the condition is
seen in `confirm_samples` of the last `2 × confirm_samples − 1` samples (default 3-of-5).
A steady fault therefore confirms in 3 consecutive samples — **1.5 s** at the shipped
service's 0.5 s interval — and a fault oscillating at the sample rate (a pin current
hovering around the threshold) still confirms instead of being reset by every clean sample.
This is a deliberate trade: it filters isolated glitches (which would otherwise page you at
3 a.m. for nothing and teach you to mute the tool) at the cost of ~1 s of added latency on
a real fault — negligible against the minutes-to-hours timescale of a connector heating
toward failure. The per-sample CSV record is **not** debounced; every raw sample lands in
the log.

Two rules guard against false all-clears: telemetry-loss samples count as *unknown*, not
healthy — an active alert can neither confirm from nor "resolve" into a gap in the data —
and if no readable GPU bus exists at startup, the watchdog waits and raises TELEMETRY LOST
instead of exiting into a silent restart loop.

Network activity is **opt-in**: nothing listens and nothing connects out unless configured.
Outbound connections go only to the ntfy server / webhook URL you set; notification delivery
runs on separate threads and can never stall or kill the sampling loop. The Prometheus
exporter (`[export]` config or the `export` subcommand) is the one opt-in listener: it serves
a read-only, unauthenticated `GET /metrics` from a cached snapshot — a scrape never touches
the i2c bus — and defaults to loopback; bind it beyond loopback only on a network you trust.

## What the default build does *not* do

The default build and the shipped `astral-watch.service` are **strictly read-only**: no writes,
no power/clock/fan control, no NVML. The one exception is the opt-in safety daemon below — a
separate feature build and a separate, disabled-by-default unit.

## Opt-in safety daemon

Built only with `cargo build --features safety` (or `sudo make install-safety`), run as
`astral-watch safety` (its own `astral-watch-safety.service`, shipped **disabled**), and armed
only when `[safety] enabled = true`. It is astral-watch's only GPU-state mutation — and it acts
through the **NVIDIA driver (NVML)**, never via raw i2c. On a confirmed, sustained overload (or a
disconnected pin under load) it **reduces the GPU power limit**, pulling aggregate — and therefore
per-pin — current down.

Design invariants (chosen with an adversarial hardware-safety review):

- **Triple opt-in, off by default.** A cargo `safety` feature + a unit shipped disabled +
  `[safety] enabled = false`. The default install never gains NVML or the ability to write GPU
  state. (Arming `enabled = true` on a non-`safety` build is refused loudly, never a silent no-op.)
- **Privilege separation.** Setting the power limit requires root, so the safety unit runs
  privileged — but it is a *separate* unit; the always-on monitor stays unprivileged and
  read-only. The safety unit keeps every other systemd hardening directive.
- **Latched.** One decisive cap, held until you run `astral-watch restore-power-limit` or reboot
  (the NVML limit is volatile and resets on reboot/driver reload). It does **not** auto-restore
  when the overload "clears" — the cap is *what cleared it*, so auto-restore would flap the limit
  and report a false all-clear on a still-damaged connector. An engaged cap means a fault was
  detected and the connector needs **physical inspection**.
- **Never-raise.** It only ever *lowers* the limit. On an already-undervolted card where the safe
  target sits above the current limit, it does nothing and loudly reports the lever is exhausted
  (likely a true hardware fault).
- **Fail-safe by direction.** On stop or crash it leaves the cap engaged (under-powered can never
  melt a pin); a SIGKILL or reboot at worst leaves the card capped, which self-heals on reboot. A
  same-boot restart adopts the live cap (state in `/run/astral-watch-safety/`) instead of
  ratcheting the limit down. Restore is manual (`restore-power-limit`) or a reboot.
- **Right GPU.** The capped device is matched to the monitored card by PCI id (never NVML index
  0), and the limit is read back after setting to confirm it took; if it didn't, it alerts loudly
  rather than believe the GPU is protected.
- **Harm-minimization, not a cure.** A board-level cap lowers how much current the worst pin
  carries, but cannot rebalance a single high-resistance / poorly-seated pin. If the alert
  persists after a cap, the connector may still be failing — **inspect it physically**.

Enabling it (after reading this section):

```sh
sudo make install-safety                 # builds the safety-capable binary + the disabled unit
# arm it in /etc/astral-watch.toml:
#   [safety]
#   enabled = true
#   # target_fraction = 0.5              # cap to 50% of the stock power limit (default)
sudo systemctl enable --now astral-watch-safety
```

Undo a cap at any time with `sudo astral-watch restore-power-limit` (or reboot).
