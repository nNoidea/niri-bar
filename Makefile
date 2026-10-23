.PHONY: all build install uninstall test check clean boxtest

all: build

build:
	cargo build --release

install:
	cargo install --path . --locked

uninstall:
	cargo uninstall niri-bar

test:
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features --locked -- -D warnings
	cargo test --all-targets --all-features --locked
boxtest:
	./scripts/boxtest.sh

check:
	cargo check --all-targets

clean:
	cargo clean

