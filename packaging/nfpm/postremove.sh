#!/bin/sh
# nfpm postremove — runs AFTER files are deleted.
# Reload systemd + udev so the now-removed unit/rule drop out of the live state.
# Deliberately does NOT userdel the astral-watch user or delete the i2c group:
# /var/log/astral-watch (created by the unit's LogsDirectory=) is owned by that uid,
# and the i2c group may be shared with other tooling. Leaving the sysusers account
# in place is the documented, non-orphaning behavior.
set -e

if [ "$1" = "0" ] || [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
	if command -v systemctl >/dev/null 2>&1; then
		systemctl daemon-reload || :
	fi
	if command -v udevadm >/dev/null 2>&1; then
		udevadm control --reload || :
	fi
fi

exit 0
