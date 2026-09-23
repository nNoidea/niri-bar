PREFIX ?= $(HOME)/.cargo
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
TARGET = target/release/niri-bar

.PHONY: all build install uninstall test check clean boxtest

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
	cargo fmt --all -- --check
	cargo clippy --all-targets --locked -- -D warnings
	GDK_BACKEND=memory cargo test --all-targets --locked -- --test-threads=1

boxtest:
	./scripts/boxtest.sh

check:
	cargo check --all-targets

clean:
	cargo clean

