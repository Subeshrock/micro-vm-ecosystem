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
ARCH="amd64"
PKG_NAME="vyoma"
WORK_DIR="target/debian/${PKG_NAME}_${VERSION}_${ARCH}"

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

echo "Building Vyoma v${VERSION} for Debian..."

# 1. Build Binaries
"$CARGO" build --release --bin vyomad --bin vyoma --bin vyoma-agent-vm

# 2. Prepare Directory Structure
mkdir -p "${WORK_DIR}/usr/bin"
mkdir -p "${WORK_DIR}/usr/lib/vyoma"
mkdir -p "${WORK_DIR}/etc/systemd/system"
mkdir -p "${WORK_DIR}/DEBIAN"

# 3. Fetch & Copy Dependencies (Cloud Hypervisor, Virtiofsd, CNI plugins)
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

# Fetch CNI plugins with checksum verification
CNI_VERSION="v1.5.1"
CNI_DIR="${WORK_DIR}/usr/lib/vyoma/cni"
echo "Fetching CNI plugins..."
mkdir -p "${CNI_DIR}/bin"
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

tar -xzf cni-plugins.tgz -C "${CNI_DIR}/bin/"

# Fetch virtiofsd with fallback URLs (no checksum - sources unreliable)
VIRTIOFSD_VERSION="1.11.1"
VIRTIOFSD_DIR="${WORK_DIR}/usr/lib/vyoma"

echo "Fetching virtiofsd..."

# Build UI (optional - requires Node.js)
echo "Building UI..."
if command -v npm &> /dev/null; then
    cd ui
    npm install
    npm run build
    cd ..
    mkdir -p "${WORK_DIR}/usr/lib/vyoma"
    cp -r ui/dist "${WORK_DIR}/usr/lib/vyoma/ui"
    echo "UI bundled successfully"
else
    echo "Skipping UI build (npm not found)"
fi
if [ ! -f "virtiofsd" ]; then
    # Primary: GitLab releases
    PRIMARY_URL="https://gitlab.com/virtio-fs/virtiofsd/-/releases/${VIRTIOFSD_VERSION}/downloads/virtiofsd-v${VIRTIOFSD_VERSION}-x86_64-musl.zip"
    if wget -q -O virtiofsd.zip "$PRIMARY_URL" 2>/dev/null; then
        unzip -q -o virtiofsd.zip
        mv virtiofsd virtiofsd_bin 2>/dev/null || true
        chmod +x virtiofsd_bin 2>/dev/null || true
    fi
fi

# Fallback 1: Try QEMU GitHub raw
if [ ! -f "virtiofsd_bin" ]; then
    FALLBACK_URL1="https://github.com/qemu/qemu/raw/main/contrib/virtiofsd/virtiofsd-x86_64"
    if wget -q -O virtiofsd "$FALLBACK_URL1" 2>/dev/null; then
        mv virtiofsd virtiofsd_bin
        chmod +x virtiofsd_bin
    fi
fi

# Fallback 2: Try another mirror
if [ ! -f "virtiofsd_bin" ]; then
    FALLBACK_URL2="https://raw.githubusercontent.com/qemu/qemu/master/contrib/virtiofsd/virtiofsd-x86_64"
    if wget -q -O virtiofsd "$FALLBACK_URL2" 2>/dev/null; then
        mv virtiofsd virtiofsd_bin
        chmod +x virtiofsd_bin
    fi
fi

# Check if we got virtiofsd
if [ -f "virtiofsd_bin" ]; then
    echo "virtiofsd fetched successfully"
    cp virtiofsd_bin "${VIRTIOFSD_DIR}/virtiofsd"
    chmod +x "${VIRTIOFSD_DIR}/virtiofsd"
else
    echo "Warning: virtiofsd not available - volume mounts may not work"
fi

# 4. Copy Assets
cp target/release/vyomad "${WORK_DIR}/usr/bin/"
cp target/release/vyoma "${WORK_DIR}/usr/bin/"
cp target/release/vyoma-agent-vm "${WORK_DIR}/usr/bin/"
cp cloud-hypervisor "${WORK_DIR}/usr/bin/cloud-hypervisor"
cp packaging/systemd/vyomad.service "${WORK_DIR}/etc/systemd/system/"
cp packaging/systemd/vyoma-net.service "${WORK_DIR}/etc/systemd/system/"

# Copy kernel and cloud-hypervisor to data directory
mkdir -p "${WORK_DIR}/var/lib/vyoma/bin"
KERNEL_URL="https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20260508/bzImage-x86_64"
KERNEL_DEST="${WORK_DIR}/var/lib/vyoma/bin/vmlinux"
if [ ! -f "kernel.bzimage" ]; then
    echo "Downloading Cloud Hypervisor kernel..."
    wget -q -O kernel.bzimage "$KERNEL_URL" || curl -L -o kernel.bzimage "$KERNEL_URL"
fi
cp kernel.bzimage "$KERNEL_DEST"
chmod 644 "$KERNEL_DEST"
echo "Kernel binary bundled"

# Also place Cloud Hypervisor where the daemon expects it
cp cloud-hypervisor "${WORK_DIR}/var/lib/vyoma/bin/cloud-hypervisor"
chmod 755 "${WORK_DIR}/var/lib/vyoma/bin/cloud-hypervisor"
echo "Cloud Hypervisor bundled to /var/lib/vyoma/bin"

# Bundle virtiofsd (from system or project bin)
if [ -f "bin/virtiofsd" ] && [ ! -L "bin/virtiofsd" ]; then
    cp bin/virtiofsd "${WORK_DIR}/usr/lib/vyoma/virtiofsd"
    chmod +x "${WORK_DIR}/usr/lib/vyoma/virtiofsd"
    echo "virtiofsd bundled from project bin"
elif [ -f "/usr/libexec/virtiofsd" ]; then
    cp /usr/libexec/virtiofsd "${WORK_DIR}/usr/lib/vyoma/virtiofsd"
    chmod +x "${WORK_DIR}/usr/lib/vyoma/virtiofsd"
    echo "virtiofsd bundled from system"
elif [ -f "/usr/bin/virtiofsd" ]; then
    cp /usr/bin/virtiofsd "${WORK_DIR}/usr/lib/vyoma/virtiofsd"
    chmod +x "${WORK_DIR}/usr/lib/vyoma/virtiofsd"
    echo "virtiofsd bundled from /usr/bin"
else
    echo "Warning: virtiofsd not found - volume mounts may not work"
fi

# 5. Create Control File
cat <<EOF > "${WORK_DIR}/DEBIAN/control"
Package: ${PKG_NAME}
Version: ${VERSION}
Section: admin
Priority: optional
Architecture: ${ARCH}
Depends: libc6, openssl, ca-certificates, iptables, kmod
Maintainer: Subeshrock <subesh.rock.3@gmail.com>
Description: Vyoma - MicroVM Ecosystem
 Docker-like experience for Cloud Hypervisor MicroVMs.
 Includes Daemon, CLI, Web UI, and virtiofsd for volume mounts.
EOF

# 6. Create Post-Install Script
# $2 = previous version (empty if fresh install)
cat <<'POSTINST' > "${WORK_DIR}/DEBIAN/postinst"
#!/bin/bash
set -e

# Create vyoma user (for socket ownership) - idempotent
if ! id vyoma >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin --comment "Vyoma MicroVM Daemon" vyoma 2>/dev/null || true
fi

# Add installing user to vyoma, kvm and disk groups
if [ -n "$SUDO_USER" ]; then
    usermod -aG vyoma "$SUDO_USER" 2>/dev/null || true
    usermod -aG kvm "$SUDO_USER" 2>/dev/null || true
    usermod -aG disk "$SUDO_USER" 2>/dev/null || true
    echo "Added $SUDO_USER to vyoma, kvm and disk groups. Log out and back in to use CLI."
fi

# Add vyoma daemon user to kvm and disk groups (for /dev/kvm and /dev/mapper/control access)
if getent group kvm > /dev/null 2>&1; then
    usermod -aG kvm vyoma 2>/dev/null || true
fi
if getent group disk > /dev/null 2>&1; then
    usermod -aG disk vyoma 2>/dev/null || true
fi

# Fix /dev/mapper/control permissions (for device mapper/DM)
chmod 0660 /dev/mapper/control 2>/dev/null || true
chown root:disk /dev/mapper/control 2>/dev/null || true

# Fix /dev/kvm permissions
chmod 0660 /dev/kvm 2>/dev/null || true
chown root:kvm /dev/kvm 2>/dev/null || true

# Create runtime directory - owned by root:vyoma, group writable (0775)
mkdir -p /run/vyoma
chown root:vyoma /run/vyoma 2>/dev/null || true
chmod 0775 /run/vyoma 2>/dev/null || true

# Setup CNI plugins directory (symlink from system data dir to package location)
mkdir -p /var/lib/vyoma/.vyoma/cni
rm -rf /var/lib/vyoma/.vyoma/cni/bin 2>/dev/null || true
ln -sf /usr/lib/vyoma/cni/bin /var/lib/vyoma/.vyoma/cni/bin

# Setup Vyoma network bridge (for VM networking)
if ! ip link show vyoma0 >/dev/null 2>&1; then
    ip link add vyoma0 type bridge
    ip addr add 172.16.0.1/24 dev vyoma0
    ip link set vyoma0 up
fi

# Enable service (idempotent - won't fail if already enabled)
systemctl daemon-reload 2>/dev/null || true
systemctl enable vyomad.service 2>/dev/null || true

# Only start on fresh install ($2 = previous version, empty if first install)
# On upgrade, use try-restart to restart only if already running
if [ -z "$2" ]; then
    # Fresh install - start the service
    systemctl start vyomad.service 2>/dev/null || true
else
    # Upgrade - restart only if already running (won't disrupt if stopped)
    systemctl try-restart vyomad.service 2>/dev/null || true
fi

echo "Vyoma v${VERSION} installed successfully!"
echo "Open http://localhost:8080 for the dashboard"
echo "Run 'vyoma run nginx:latest' to start your first VM"
POSTINST
chmod 755 "${WORK_DIR}/DEBIAN/postinst"

# 7. Create Post-Remove Script
cat <<'POSTRM' > "${WORK_DIR}/DEBIAN/postrm"
#!/bin/bash
set -e
systemctl daemon-reload 2>/dev/null || true
if [ "$1" = "purge" ]; then
    userdel vyoma 2>/dev/null || true
    rm -rf /var/lib/vyoma /run/vyoma 2>/dev/null || true
    ip link del vyoma0 2>/dev/null || true
fi
POSTRM
chmod 755 "${WORK_DIR}/DEBIAN/postrm"

# 8. Build Package
mkdir -p dist
dpkg-deb --build "${WORK_DIR}" "dist/${PKG_NAME}_${VERSION}_${ARCH}.deb"

# 9. Cleanup
rm -f cloud-hypervisor cloud-hypervisor.tgz virtiofsd.zip virtiofsd virtiofsd_bin cni-plugins.tgz
rm -rf release-v1.7.0-x86_64
rm -rf "${WORK_DIR}"

echo "Package created at dist/${PKG_NAME}_${VERSION}_${ARCH}.deb"