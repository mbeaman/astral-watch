PREFIX ?= /usr/local
DESTDIR ?=
# vendor files in packages belong in /usr/lib; admin-installed ones in /etc
UNITDIR = $(if $(DESTDIR),/usr/lib/systemd/system,/etc/systemd/system)
UDEVDIR = $(if $(DESTDIR),/usr/lib/udev/rules.d,/etc/udev/rules.d)

.PHONY: build release test fmt clippy check install install-files uninstall

build:
	cargo build

release:
	cargo build --release

test:
	cargo test --all

fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets -- -D warnings

check: fmt clippy test

# sub-make keeps the build strictly before the file copies, even under `make -j`
install: release
	$(MAKE) install-files

# files only, no cargo invocation — for packagers (PKGBUILD package()) whose build step
# already produced target/release/astral-watch and who must not touch the network here
install-files:
	install -Dm755 target/release/astral-watch $(DESTDIR)$(PREFIX)/bin/astral-watch
	install -Dm644 packaging/99-astral-watch.rules $(DESTDIR)$(UDEVDIR)/99-astral-watch.rules
	install -Dm644 packaging/sysusers.d/astral-watch.conf $(DESTDIR)/usr/lib/sysusers.d/astral-watch.conf
	install -Dm644 packaging/modules-load.d/astral-watch.conf $(DESTDIR)/usr/lib/modules-load.d/astral-watch.conf
	install -d $(DESTDIR)$(UNITDIR)
	sed 's|/usr/local/bin/|$(PREFIX)/bin/|' packaging/astral-watch.service \
		> $(DESTDIR)$(UNITDIR)/astral-watch.service
	chmod 644 $(DESTDIR)$(UNITDIR)/astral-watch.service
	@if [ -n "$(DESTDIR)" ] || [ ! -e /etc/astral-watch.toml ]; then \
		install -Dm644 packaging/astral-watch.toml $(DESTDIR)/etc/astral-watch.toml; \
	else \
		echo "keeping existing /etc/astral-watch.toml"; \
	fi
	@if [ -z "$(DESTDIR)" ]; then \
		{ modprobe i2c-dev || true; } && \
		systemd-sysusers && \
		udevadm control --reload && \
		udevadm trigger && \
		systemctl daemon-reload && \
		echo && \
		echo "Installed. Enable the auto-restarting logging service with:" && \
		echo "  sudo systemctl enable --now astral-watch"; \
	else \
		echo "Staged install into $(DESTDIR) (no system steps run)."; \
	fi

uninstall:
	@if [ -z "$(DESTDIR)" ]; then \
		systemctl disable --now astral-watch 2>/dev/null || true; \
	fi
	rm -f $(DESTDIR)$(PREFIX)/bin/astral-watch
	rm -f $(DESTDIR)$(UDEVDIR)/99-astral-watch.rules
	rm -f $(DESTDIR)/usr/lib/sysusers.d/astral-watch.conf
	rm -f $(DESTDIR)/usr/lib/modules-load.d/astral-watch.conf
	rm -f $(DESTDIR)$(UNITDIR)/astral-watch.service
	@if [ -z "$(DESTDIR)" ]; then \
		systemctl daemon-reload && udevadm control --reload; \
	fi
