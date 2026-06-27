#!/bin/bash
set -e

# Locate cargo (works with rustup and system installs)
if command -v cargo >/dev/null 2>&1; then
    CARGO=$(command -v cargo)
elif [ -f "$HOME/.cargo/bin/cargo" ]; then
    CARGO="$HOME/.cargo/bin/cargo"
else
    echo "Error: cargo not found. Install Rust from https://rustup.rs"
    exit 1
fi

VERSION="2.8.0"
WORK_DIR="target/rpm"
SOURCES_DIR="${WORK_DIR}/SOURCES/vyoma-${VERSION}"

# Hardcoded checksums for binary verification
# Computed from official release sources
CLOUD_HYPERVISOR_CHECKSUM="8a013003ae29f59da7b2d7ac67f19eea3ea535166dac00ff5e8c2c27ac643f8a"
CNI_PLUGINS_CHECKSUM="77baa2f669980a82255ffa2f2717de823992480271ee778aa51a9c60ae89ff9b"

# Verify checksum of a downloaded file
# Usage: verify_checksum <file> <expected_sha256>
verify_checksum() {
    local file="$1"
    local expected="$2"

    if [ "$VYOMA_SKIP_CHECKSUM" = "1" ]; then
        echo "Skipping checksum verification (VYOMA_SKIP_CHECKSUM=1)"
        return 0
    fi

    if [ ! -f "$file" ]; then
        echo "Error: file $file not found for checksum verification"
        return 1
    fi

    local actual
    actual=$(sha256sum "$file" | awk '{print $1}')

    if [ "$actual" != "$expected" ]; then
        echo "CHECKSUM MISMATCH for $file"
        echo "Expected: $expected"
        echo "Actual:   $actual"
        return 1
    fi

    echo "Checksum OK: $file"
    return 0
}

# Clean up any previous build
rm -rf "${WORK_DIR}"
mkdir -p "${WORK_DIR}"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
mkdir -p "${SOURCES_DIR}"

# 1. Build Binaries
"$CARGO" build --release --bin vyomad --bin vyoma
rustup target add x86_64-unknown-linux-musl
"$CARGO" build --release --target x86_64-unknown-linux-musl --bin vyoma-agent-vm

# 2. Copy binaries to source dir
cp target/release/vyomad "${SOURCES_DIR}/"
cp target/release/vyoma "${SOURCES_DIR}/"
cp target/x86_64-unknown-linux-musl/release/vyoma-agent-vm "${SOURCES_DIR}/vyoma-agent-vm"

# 3. Fetch Dependencies with checksum verification
echo "Fetching dependencies..."

# Fetch Cloud Hypervisor with checksum verification
if [ ! -f "cloud-hypervisor" ]; then
    echo "Downloading cloud-hypervisor v41.0..."
    wget -q -O cloud-hypervisor https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v41.0/cloud-hypervisor
    chmod +x cloud-hypervisor
fi

echo "Verifying cloud-hypervisor checksum..."
if ! verify_checksum "cloud-hypervisor" "$CLOUD_HYPERVISOR_CHECKSUM"; then
    echo "ERROR: cloud-hypervisor checksum verification failed!"
    rm -f cloud-hypervisor
    exit 1
fi

cp cloud-hypervisor "${SOURCES_DIR}/"
cp packaging/systemd/vyomad.service "${SOURCES_DIR}/"
cp packaging/systemd/vyoma-net.service "${SOURCES_DIR}/"

# Fetch CNI plugins with checksum verification
CNI_VERSION="v1.5.1"
CNI_DIR="${SOURCES_DIR}/cni/bin"
echo "Fetching CNI plugins..."
mkdir -p "${CNI_DIR}"
if [ ! -f "cni-plugins.tgz" ]; then
    echo "Downloading CNI plugins ${CNI_VERSION}..."
    wget -q -O cni-plugins.tgz "https://github.com/containernetworking/plugins/releases/download/${CNI_VERSION}/cni-plugins-linux-amd64-${CNI_VERSION}.tgz"
fi

echo "Verifying CNI plugins checksum..."
if ! verify_checksum "cni-plugins.tgz" "$CNI_PLUGINS_CHECKSUM"; then
    echo "ERROR: CNI plugins checksum verification failed!"
    rm -f cni-plugins.tgz
    exit 1
fi

tar -xzf cni-plugins.tgz -C "${CNI_DIR}/"

# Copy UI (built by workflow step or locally)
echo "Bundling UI..."
UI_AVAILABLE=false
if [ -d "ui/dist" ]; then
    mkdir -p "${SOURCES_DIR}/ui/dist"
    cp -r ui/dist/* "${SOURCES_DIR}/ui/dist/"
    echo "UI bundled successfully"
    UI_AVAILABLE=true
else
    echo "Warning: UI not found - dashboard will not be available"
fi

# Copy kernel binary
mkdir -p "${SOURCES_DIR}"
KERNEL_URL="https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20260508/bzImage-x86_64"
if [ ! -f "kernel.bzimage" ]; then
    echo "Downloading Cloud Hypervisor kernel..."
    wget -q -O kernel.bzimage "$KERNEL_URL" || curl -L -o kernel.bzimage "$KERNEL_URL"
fi
mkdir -p "${SOURCES_DIR}/bin"
cp kernel.bzimage "${SOURCES_DIR}/bin/vmlinux"
chmod 644 "${SOURCES_DIR}/bin/vmlinux"
echo "Kernel binary bundled"

# Bundle virtiofsd (from system or project bin)
if [ -f "bin/virtiofsd" ] && [ ! -L "bin/virtiofsd" ]; then
    cp bin/virtiofsd "${SOURCES_DIR}/virtiofsd"
    chmod +x "${SOURCES_DIR}/virtiofsd"
    echo "virtiofsd bundled from project bin"
elif [ -f "/usr/libexec/virtiofsd" ]; then
    cp /usr/libexec/virtiofsd "${SOURCES_DIR}/virtiofsd"
    chmod +x "${SOURCES_DIR}/virtiofsd"
    echo "virtiofsd bundled from system"
elif [ -f "/usr/bin/virtiofsd" ]; then
    cp /usr/bin/virtiofsd "${SOURCES_DIR}/virtiofsd"
    chmod +x "${SOURCES_DIR}/virtiofsd"
    echo "virtiofsd bundled from /usr/bin"
else
    echo "Warning: virtiofsd not found - volume mounts may not work"
fi

# 4. Create Source Tarball
cd "${WORK_DIR}/SOURCES"
tar -czf "vyoma-${VERSION}.tar.gz" "vyoma-${VERSION}"
cd ../../..

# 5. Create SPEC File
SPEC_CONTENT="Name:       vyoma
Version:    ${VERSION}
Release:    1%{?dist}
Summary:    MicroVM Ecosystem
License:    MIT
URL:        https://github.com/Subeshrock/micro-vm-ecosystem
Source0:    vyoma-${VERSION}.tar.gz
Requires:   iptables
Requires:   kmod

%description
Vyoma is a lightweight MicroVM runtime for running containers
as lightweight virtual machines. Combines Cloud Hypervisor speed with Docker UX.
Includes CLI, Daemon, Web UI, CNI plugins, and virtiofsd for volume mounts.

%prep
%setup -q

%install
mkdir -p %{buildroot}/usr/bin
mkdir -p %{buildroot}/usr/lib/vyoma
mkdir -p %{buildroot}/etc/systemd/system
install -m 755 vyomad %{buildroot}/usr/bin/vyomad
install -m 755 vyoma %{buildroot}/usr/bin/vyoma
install -m 755 vyoma-agent-vm %{buildroot}/usr/bin/vyoma-agent-vm
install -m 755 cloud-hypervisor %{buildroot}/usr/bin/cloud-hypervisor
install -m 644 vyomad.service %{buildroot}/etc/systemd/system/vyomad.service
install -m 644 vyoma-net.service %{buildroot}/etc/systemd/system/vyoma-net.service

# Install CNI plugins
cp -r cni %{buildroot}/usr/lib/vyoma/cni

# Install kernel binary
mkdir -p %{buildroot}/var/lib/vyoma/bin
install -m 644 bin/vmlinux %{buildroot}/var/lib/vyoma/bin/vmlinux
install -m 755 cloud-hypervisor %{buildroot}/var/lib/vyoma/bin/cloud-hypervisor"


if [ "$UI_AVAILABLE" = true ]; then
    SPEC_CONTENT="$SPEC_CONTENT

# Install UI
cp -r ui %{buildroot}/usr/lib/vyoma/ui"
fi

if [ -f "${SOURCES_DIR}/virtiofsd" ]; then
    SPEC_CONTENT="$SPEC_CONTENT
install -m 755 virtiofsd %{buildroot}/usr/lib/vyoma/virtiofsd"
fi

SPEC_CONTENT="$SPEC_CONTENT

%files
/usr/bin/vyomad
/usr/bin/vyoma
/usr/bin/vyoma-agent-vm
/usr/bin/cloud-hypervisor
/etc/systemd/system/vyomad.service
/etc/systemd/system/vyoma-net.service
/usr/lib/vyoma/cni
/var/lib/vyoma/bin/vmlinux
/var/lib/vyoma/bin/cloud-hypervisor"

if [ "$UI_AVAILABLE" = true ]; then
    SPEC_CONTENT="$SPEC_CONTENT
/usr/lib/vyoma/ui"
fi

if [ -f "${SOURCES_DIR}/virtiofsd" ]; then
    SPEC_CONTENT="$SPEC_CONTENT
/usr/lib/vyoma/virtiofsd"
fi

SPEC_CONTENT="$SPEC_CONTENT

%preun
# Stop service before package removal (not on upgrade)
# \$1 = 0 means removal, \$1 = 1 means upgrade
if [ \$1 -eq 0 ]; then
    systemctl stop vyomad.service 2>/dev/null || true
    systemctl disable vyomad.service 2>/dev/null || true
fi

%post
# $1 = 1: fresh install, $1 = 2: upgrade
if ! getent passwd vyoma > /dev/null 2>&1; then
    useradd -r -s /sbin/nologin -c 'Vyoma daemon' -d /var/lib/vyoma vyoma 2>/dev/null || true
fi

# Add vyoma daemon user to kvm and disk groups (for /dev/kvm and /dev/mapper/control access)
if getent group kvm > /dev/null 2>&1; then
    usermod -aG kvm vyoma 2>/dev/null || true
fi
if getent group disk > /dev/null 2>&1; then
    usermod -aG disk vyoma 2>/dev/null || true
fi

# Add installing user to vyoma, kvm, disk groups
if [ -n "$SUDO_USER" ]; then
    usermod -aG vyoma "$SUDO_USER" 2>/dev/null || true
    usermod -aG kvm "$SUDO_USER" 2>/dev/null || true
    usermod -aG disk "$SUDO_USER" 2>/dev/null || true
    printf 'Added %s to vyoma, kvm, disk groups. Log out and back in to use CLI.\n' "$SUDO_USER"
fi

# Fix /dev/kvm permissions
chmod 0660 /dev/kvm 2>/dev/null || true
chown root:kvm /dev/kvm 2>/dev/null || true

# Fix /dev/mapper/control permissions (for device mapper/DM)
chmod 0660 /dev/mapper/control 2>/dev/null || true
chown root:disk /dev/mapper/control 2>/dev/null || true

# Create runtime directory - owned by root:vyoma, group writable (0775)
mkdir -p /run/vyoma
chown root:vyoma /run/vyoma 2>/dev/null || true
chmod 0775 /run/vyoma 2>/dev/null || true

# Setup CNI plugins directory (symlink from system data dir to package location)
mkdir -p /var/lib/vyoma/.vyoma/cni 2>/dev/null || true
rm -rf /var/lib/vyoma/.vyoma/cni/bin 2>/dev/null || true
ln -sf /usr/lib/vyoma/cni/bin /var/lib/vyoma/.vyoma/cni/bin 2>/dev/null || true

# Setup Vyoma network bridge (for VM networking)
if ! ip link show vyoma0 >/dev/null 2>&1; then
    ip link add vyoma0 type bridge
    ip addr add 172.16.0.1/24 dev vyoma0
    ip link set vyoma0 up
fi

# Reload systemd and enable service (idempotent)
systemctl daemon-reload 2>/dev/null || true
systemctl enable vyomad.service 2>/dev/null || true

# Only start on fresh install (\$1 = 1); on upgrade (\$1 = 2), try-restart if running
if [ \$1 -eq 1 ]; then
    systemctl start vyomad.service 2>/dev/null || true
else
    systemctl try-restart vyomad.service 2>/dev/null || true
fi

%postun
# \$1 = 0: package removal, \$1 = 1: upgrade
if [ \$1 -eq 0 ]; then
    # Package removal - cleanup (not on upgrade)
    userdel vyoma 2>/dev/null || true
    rm -rf /var/lib/vyoma /run/vyoma 2>/dev/null || true
    ip link del vyoma0 2>/dev/null || true
    systemctl daemon-reload 2>/dev/null || true
fi

%changelog
* Wed Apr 01 2026 Subeshrock <subesh.rock.3@gmail.com> - ${VERSION}-1
- Run daemon as root with capabilities (no sudo needed)
- Remove sudoers configuration
- Fix socket and KVM permissions
- Bundle CNI plugins for networking
- Bundle UI in package
- Fix /run/vyoma directory permissions (0775)
"

echo "$SPEC_CONTENT" > "${WORK_DIR}/SPECS/vyoma.spec"

# 6. Build RPM
rpmbuild --define "_topdir $(pwd)/${WORK_DIR}" -bb "${WORK_DIR}/SPECS/vyoma.spec"

# 7. Move to dist
mkdir -p dist
mv ${WORK_DIR}/RPMS/x86_64/*.rpm dist/

# 8. Cleanup
rm -f cloud-hypervisor cloud-hypervisor.tgz virtiofsd.zip virtiofsd virtiofsd_bin cni-plugins.tgz
rm -rf release-v1.7.0-x86_64
rm -rf "${WORK_DIR}"

echo "RPM created in dist/"