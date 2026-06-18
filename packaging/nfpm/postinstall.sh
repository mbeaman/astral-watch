#!/bin/sh
# nfpm postinstall — shared by the .rpm (arg = integer install count) and .deb
# (arg = "configure"). Every step here is idempotent, so it is run unconditionally
# on both install and upgrade; no install-vs-upgrade branching needed.
#
# Ordering matters: the i2c group must exist (sysusers) BEFORE the udev trigger
# applies GROUP="i2c" to the NVIDIA i2c nodes, and the unit dir must be re-scanned
# (daemon-reload) last. Each tool is guarded because it may be absent in a build
# chroot or minimal container.
set -e

# 1. Create the astral-watch system user + i2c group from the shipped sysusers snippet.
if command -v systemd-sysusers >/dev/null 2>&1; then
	systemd-sysusers /usr/lib/sysusers.d/astral-watch.conf || :
fi

# 2. Load the i2c-dev kernel module now (the modules-load.d snippet handles boot).
if command -v modprobe >/dev/null 2>&1; then
	modprobe i2c-dev || :
fi

# 3. Reload + re-trigger udev so 99-astral-watch.rules grants the (now-existing) i2c
#    group access to /dev/i2c-* on the NVIDIA GPU.
if command -v udevadm >/dev/null 2>&1; then
	udevadm control --reload || :
	udevadm trigger --subsystem-match=i2c-dev || :
fi

# 4. Re-scan unit files. The monitor unit is shipped DISABLED on purpose: this tool only
#    works on a specific ASUS ROG Astral GPU, and `Restart=always` would just restart-loop
#    on a generic host. (`systemctl preset` is avoided — most distros preset new units to
#    enabled, which would auto-start it.) The admin enables it explicitly, as below.
if command -v systemctl >/dev/null 2>&1; then
	systemctl daemon-reload || :
fi

echo "astral-watch installed. Enable the read-only monitor with:" >&2
echo "  sudo systemctl enable --now astral-watch" >&2

exit 0
