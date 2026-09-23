PREFIX ?= $(HOME)/.cargo
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
TARGET = target/release/niri-bar

.PHONY: all build install uninstall test check clean

all: build

build:
	cargo build --release

install: test build
	install -d "$(DESTDIR)$(BINDIR)"
	install -m 755 "$(TARGET)" "$(DESTDIR)$(BINDIR)/niri-bar"
	install -d "$(DESTDIR)$(DATADIR)/niri-bar"
	install -m 644 resources/config.default.toml "$(DESTDIR)$(DATADIR)/niri-bar/config.default.toml" || true
	install -m 644 resources/style.default.css "$(DESTDIR)$(DATADIR)/niri-bar/style.default.css" || true

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/niri-bar"

test:
	GDK_BACKEND=memory cargo test --all-targets -- --test-threads=1

check:
	cargo check --all-targets

clean:
	cargo clean
