.PHONY: all build install uninstall test check clean boxtest

all: build

build:
	cargo build --release

install:
	cargo install --path . --locked
	-pkill -x niri-bar 2>/dev/null || true
	@sleep 0.15
	-niri msg action spawn -- niri-bar 2>/dev/null || true

uninstall:
	cargo uninstall niri-bar

test:
	cargo fmt --all -- --check
	cargo check --all-targets
	cargo clippy --all-targets --all-features --locked -- -D warnings
	cargo test --all-targets --all-features --locked
boxtest:
	./scripts/boxtest.sh

check:
	cargo check --all-targets

clean:
	cargo clean

