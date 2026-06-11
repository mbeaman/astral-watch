PREFIX ?= /usr/local
DESTDIR ?=

.PHONY: build release test fmt clippy check install uninstall

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

install: release
	install -Dm755 target/release/astral-watch $(DESTDIR)$(PREFIX)/bin/astral-watch
	install -Dm644 packaging/99-astral-watch.rules $(DESTDIR)/etc/udev/rules.d/99-astral-watch.rules
	install -Dm644 packaging/astral-watch.service $(DESTDIR)/etc/systemd/system/astral-watch.service
	@echo
	@echo "Installed. To enable the auto-restarting logging service:"
	@echo "  sudo groupadd -f i2c"
	@echo "  sudo useradd -r -s /usr/sbin/nologin -g i2c astral-watch 2>/dev/null || true"
	@echo "  sudo udevadm control --reload && sudo udevadm trigger"
	@echo "  sudo systemctl daemon-reload && sudo systemctl enable --now astral-watch"

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/astral-watch
	rm -f $(DESTDIR)/etc/udev/rules.d/99-astral-watch.rules
	rm -f $(DESTDIR)/etc/systemd/system/astral-watch.service
