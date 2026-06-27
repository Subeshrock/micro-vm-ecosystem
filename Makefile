.PHONY: all agent daemon cli test install_deps

all: daemon cli agent

install_deps:
	@echo "Installing required Rust targets..."
	rustup target add x86_64-unknown-linux-musl

agent: install_deps
	@echo "Building static vyoma-agent-vm (musl)..."
	cargo build --release --target x86_64-unknown-linux-musl --bin vyoma-agent-vm

daemon:
	@echo "Building vyomad..."
	cargo build --release --bin vyomad

cli:
	@echo "Building vyoma CLI..."
	cargo build --release --bin vyoma

test:
	cargo test
