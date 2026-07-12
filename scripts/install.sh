#!/bin/bash
set -euo pipefail

# Color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

log_info() { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

# Require root
if [ "$EUID" -ne 0 ]; then
  log_error "This script must be run as root (use sudo)."
fi

# Constants
VYOMA_USER="vyoma"
VYOMA_GROUP="vyoma"
DATA_DIR="/var/lib/vyoma"
BIN_DIR="${DATA_DIR}/bin"
INSTALL_PREFIX="/usr/local/bin"
SRC_DIR="$(pwd)"

log_info "Starting Vyoma installation..."

# 1. Create User and Group
log_info "Configuring ${VYOMA_USER} user and groups..."
if ! getent group "${VYOMA_GROUP}" > /dev/null; then
    groupadd --system "${VYOMA_GROUP}"
    log_info "Created group ${VYOMA_GROUP}."
fi

if ! getent passwd "${VYOMA_USER}" > /dev/null; then
    useradd --system --shell /bin/false -g "${VYOMA_GROUP}" -d "${DATA_DIR}" "${VYOMA_USER}"
    log_info "Created user ${VYOMA_USER}."
fi

# Add to supplementary groups
for grp in kvm disk; do
    if getent group "$grp" > /dev/null; then
        usermod -aG "$grp" "${VYOMA_USER}"
        log_info "Added ${VYOMA_USER} to $grp group."
    else
        log_warn "Group $grp does not exist, skipping."
    fi
done

# 2. Setup Data Directory
log_info "Configuring data directories at ${DATA_DIR}..."
mkdir -p "${BIN_DIR}"

# Copy dependencies
log_info "Copying dependencies to ${BIN_DIR}..."
if [ -f "${SRC_DIR}/bin/cloud-hypervisor" ]; then
    if [ ! "${SRC_DIR}/bin/cloud-hypervisor" -ef "${BIN_DIR}/cloud-hypervisor" ]; then
        cp -fL "${SRC_DIR}/bin/cloud-hypervisor" "${BIN_DIR}/"
    fi
else
    log_warn "cloud-hypervisor not found in ${SRC_DIR}/bin/. Downloading..."
    wget -q -O "${BIN_DIR}/cloud-hypervisor" https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v41.0/cloud-hypervisor
fi
chmod +x "${BIN_DIR}/cloud-hypervisor"

if [ -f "${SRC_DIR}/bin/vmlinux" ]; then
    if [ ! "${SRC_DIR}/bin/vmlinux" -ef "${BIN_DIR}/vmlinux" ]; then
        cp -fL "${SRC_DIR}/bin/vmlinux" "${BIN_DIR}/"
    fi
else
    log_warn "vmlinux not found in ${SRC_DIR}/bin/. Downloading..."
    wget -q -O "${BIN_DIR}/vmlinux" https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20260508/bzImage-x86_64
fi
chmod 644 "${BIN_DIR}/vmlinux"

if [ -f "${SRC_DIR}/bin/virtiofsd" ]; then
    if [ ! "${SRC_DIR}/bin/virtiofsd" -ef "${BIN_DIR}/virtiofsd" ]; then
        cp -fL "${SRC_DIR}/bin/virtiofsd" "${BIN_DIR}/"
    fi
elif [ -f "/usr/libexec/virtiofsd" ]; then
    # Create symlink or copy if available system-wide
    if [ ! "/usr/libexec/virtiofsd" -ef "${BIN_DIR}/virtiofsd" ]; then
        cp -fL "/usr/libexec/virtiofsd" "${BIN_DIR}/"
    fi
else
    log_warn "virtiofsd not found in ${SRC_DIR}/bin/. Virtual volumes may not work unless installed system-wide."
fi
if [ -f "${BIN_DIR}/virtiofsd" ]; then
    chmod +x "${BIN_DIR}/virtiofsd"
fi

# Set permissions
chown -R "${VYOMA_USER}:${VYOMA_GROUP}" "${DATA_DIR}"
chmod 755 "${DATA_DIR}"
chmod 755 "${BIN_DIR}"

# 3. Setup vyoma0 Bridge
log_info "Configuring vyoma0 network bridge..."
if ! ip link show vyoma0 > /dev/null 2>&1; then
    ip link add name vyoma0 type bridge
    ip addr add 172.16.0.1/24 dev vyoma0 || true
    ip link set dev vyoma0 up
    log_info "Created vyoma0 bridge."
else
    log_info "vyoma0 bridge already exists."
fi

# 4. Install Vyoma Binaries
log_info "Installing Vyoma binaries to ${INSTALL_PREFIX}..."

if [ -f "${SRC_DIR}/target/release/vyomad" ]; then
    cp -f "${SRC_DIR}/target/release/vyomad" "${INSTALL_PREFIX}/"
    chmod +x "${INSTALL_PREFIX}/vyomad"
else
    log_warn "vyomad not found in target/release/. Did you run 'make all'?"
fi

if [ -f "${SRC_DIR}/target/release/vyoma" ]; then
    cp -f "${SRC_DIR}/target/release/vyoma" "${INSTALL_PREFIX}/"
    chmod +x "${INSTALL_PREFIX}/vyoma"
else
    log_warn "vyoma not found in target/release/. Did you run 'make all'?"
fi

if [ -f "${SRC_DIR}/target/x86_64-unknown-linux-musl/release/vyoma-agent-vm" ]; then
    # The agent goes to the data dir bin so vyomad can bundle it in initramfs
    cp -f "${SRC_DIR}/target/x86_64-unknown-linux-musl/release/vyoma-agent-vm" "${BIN_DIR}/"
    chown "${VYOMA_USER}:${VYOMA_GROUP}" "${BIN_DIR}/vyoma-agent-vm"
    chmod +x "${BIN_DIR}/vyoma-agent-vm"
else
    log_warn "vyoma-agent-vm not found in target/x86_64-unknown-linux-musl/release/. Did you run 'make agent'?"
fi

log_info "Vyoma installation completed successfully!"
