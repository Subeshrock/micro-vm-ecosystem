.PHONY: all agent daemon cli test install_deps install bundle_deps deb rpm

all: daemon cli agent bundle_deps

install_deps:
	@echo "Installing required Rust targets..."
	rustup target add x86_64-unknown-linux-musl

agent: install_deps
	@echo "Building static vyoma-agent-vm (musl)..."
	cargo build --release --target x86_64-unknown-linux-musl --bin vyoma-agent-vm
	mkdir -p bin
	cp target/x86_64-unknown-linux-musl/release/vyoma-agent-vm bin/vyoma-agent-vm

daemon:
	@echo "Building vyomad..."
	cargo build --release --bin vyomad

cli:
	@echo "Building vyoma CLI..."
	cargo build --release --bin vyoma

test:
	cargo test

install:
	@echo "Running installation script (requires sudo)..."
	sudo ./scripts/install.sh

bundle_deps:
	@echo "Downloading runtime dependencies for packaging..."
	mkdir -p bin
	if [ ! -f bin/cloud-hypervisor ]; then wget -q -O bin/cloud-hypervisor https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v41.0/cloud-hypervisor && chmod +x bin/cloud-hypervisor; fi
	if [ ! -f bin/vmlinux ]; then wget -q -O bin/vmlinux https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20260508/bzImage-x86_64 && chmod 644 bin/vmlinux; fi

deb: all
	@echo "Building Debian package..."
	cargo deb -p vyomad --no-build

rpm: all
	@echo "Building RPM package..."
	cd crates/vyomad && cargo generate-rpm
