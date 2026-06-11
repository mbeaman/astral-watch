# Safety: why this is a read-only operation

`astral-watch` reads a sensor chip (ITE IT8915FN) that lives on a live GPU's i2c bus. This
document explains why that access cannot change device state, and what precautions the tool takes.

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
  runs an `i2cdetect`-style probe, which can disturb other devices on the GPU's i2c bus.
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

Network activity is **opt-in and outbound-only**: nothing listens, and connections are made
only to the ntfy server / webhook URL you configure. Notification delivery runs on a separate
thread and can never stall or kill the sampling loop.

## What it does *not* do

No writes, no power/clock/fan control, no NVML actions. A future opt-in safety daemon may cap GPU
power on sustained overload — but that will act through the **NVIDIA driver (NVML)**, never via raw
i2c writes.
