#!/bin/sh
# nfpm preremove — runs BEFORE files are deleted.
# On the .rpm path, $1 == 0 means a real uninstall (not an upgrade); on the .deb
# path, $1 == "remove". In both cases we stop+disable the monitor unit while its
# unit file still exists on disk. On an upgrade ($1 >= 1 / "upgrade") we leave the
# running service alone.
set -e

if [ "$1" = "0" ] || [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
	if command -v systemctl >/dev/null 2>&1; then
		systemctl --no-reload disable --now astral-watch.service || :
	fi
fi

exit 0
