# Vyoma: The Micro-VM Ecosystem

[![Release Badge](https://img.shields.io/github/v/release/Subeshrock/vyoma?style=flat-square)](https://github.com/Subeshrock/vyoma/releases)
[![Build Status](https://img.shields.io/github/actions/workflow/status/Subeshrock/vyoma/release.yml?style=flat-square)](https://github.com/Subeshrock/vyoma/actions)

**Vyoma** is a production-grade Micro-VM manager. It combines the blazing speed and security of **Cloud Hypervisor** with the beloved developer experience of **Docker**.

Spin up secure, isolated VMs in milliseconds using standard OCI (Docker) images. No heavy kernels, no complex QEMU flags—just code.

---

## 🚀 Key Features (v2.1.2)

*   **Docker UX**: Familiar commands: `run`, `ps`, `logs`, `exec`.
*   **Vyoma Swarm**: Built-in clustering with **Mesh Networking** (VXLAN) and deterministic IP allocation.
*   **Vyoma Compose**: Orchestrate stacks with `vyoma-compose.yml`.
*   **Web Dashboard**: Built-in visual management UI (bundled in Daemon, accessible at `http://localhost:3000`).
*   **VS Code Extension**: Manage VMs directly from VS Code.
*   **Self-Contained**: `.deb` and `.rpm` packages bundle all dependencies (Cloud Hypervisor, CNI plugins, UI).
*   **Persistence**: Volume mounts (`virtiofs`) and Port Mapping.
*   **Snapshotting**: Instant VM snapshots and State Teleportation.
*   **Secure by Default**: Daemon runs as `vyoma` user with kernel capabilities, socket permissions 0660.

---

## 📦 Installation

### Prerequisites
- Linux kernel 5.10+ with KVM, cgroups v2, and TUN/TAP enabled.
- A user with `sudo` access.

### 1. Download Package
Go to the [Releases Page](https://github.com/Subeshrock/vyoma/releases) and download the latest package for your distro.

### 2. Install

**Debian/Ubuntu:**
```bash
sudo apt install ./vyoma_2.8.0_amd64.deb
```

**Fedora/RHEL/CentOS:**
```bash
sudo rpm -i vyoma-2.8.0-1.x86_64.rpm
```

### 3. Post-Install Group Setup

After installation, your user needs to be added to the `vyoma` group to access the daemon socket:

```bash
# Log out and log back in, or run:
newgrp vyoma
```

**Why is this needed?** The CLI connects to the daemon via Unix socket at `/run/vyoma/vyoma.sock` with permissions `0660` (owned by `vyoma:vyoma`). Without being in the `vyoma` group, the socket access is denied.

**What's Installed:**
*   `/usr/bin/vyomad`: The Daemon (Run via Systemd).
*   `/usr/bin/vyoma`: The CLI Tool.
*   `/usr/bin/cloud-hypervisor`: Bundled VMM binary.
*   `/usr/lib/vyoma/cni/bin/`: CNI plugins for networking.
*   `/usr/lib/vyoma/ui/`: Web dashboard (served at `http://localhost:8080`).
*   `/etc/systemd/system/vyomad.service`: Service definition.

### 4. Verify Installation
```bash
vyoma doctor
```
This utility checks for KVM (`/dev/kvm`), tun/tap support, and daemon connectivity.

---

## ⚡ Usage Guide

For a complete reference of all 20+ commands, see [COMMANDS.md](COMMANDS.md).

### Lifecycle
*   **Run a VM**:
    ```bash
    vyoma run ubuntu:latest --vcpu 2 --memory 1024 -p 8080:80
    ```
*   **List VMs**: `vyoma ps`
*   **Logs**: `vyoma logs -f <vm_id>`
*   **Shell Access**: `vyoma exec <vm_id> /bin/bash` (Uses a secure, agent-based HTTP channel—no SSH required)
*   **Stop/Remove**: `vyoma stop <vm_id>`, `vyoma rm <vm_id>`
*   **Pause/Resume**: `vyoma pause <vm_id>`, `vyoma resume <vm_id>`

### Networking
Vyoma uses CNI for robust networking.
*   **List Networks**: `vyoma network ls`
*   **Create Network**: `vyoma network create my-net --subnet 10.10.0.0/16`

### Clustering (Vyoma Swarm)
Turn multiple machines into a single mesh.
1.  **Initialize Seed (Leader)**:
    ```bash
    vyoma swarm init
    # OR specify IP: vyoma swarm init --advertise-addr 192.168.1.10
    ```
2.  **Join Worker**:
    ```bash
    vyoma swarm join <SEED_IP>
    ```
3.  **List Nodes**: `vyoma swarm ls`

Swarm ensures unique Subnet Leases (e.g., `10.42.1.0/24` for Node 1) and routes traffic transparently over VXLAN.

### Orchestration (Vyoma Compose)
Define complex stacks using `vyoma-compose.yml`:
```yaml
version: "1.0"
services:
  web:
    image: nginx:alpine
    ports: ["80:80"]
  db:
    image: postgres:15
    cpus: 2
```
*   **Deploy**: `vyoma up -d`
*   **Teardown**: `vyoma down`

---

## 📘 Vyomafile Reference (Build)

The `Vyomafile` is used to build custom images via `vyoma build`.

```dockerfile
# Start from a base image (Docker Hub or local)
FROM alpine:3.18

# Run commands to install packages or setup environment
RUN apk add --no-cache python3 curl

# Copy files from host execution context to VM
COPY app.py /app/app.py
COPY config.json /etc/config.json
```
> **Note**: `CMD`, `ENV`, and `ENTRYPOINT` are not yet supported. VMs start with `/bin/sh` init unless overridden.

---

## 📘 Vyoma Compose Reference

Use `vyoma-compose.yml` to orchestrate multi-VM stacks.

```yaml
version: "1.0"
services:
  web:
    image: nginx:alpine
    ports: 
      - "8080:80"        # Host:VM
    volumes:
      - "./html:/var/www" # Host:VM (Requires virtiofsd)
    cpus: 1              # vCPU limit
    memory: 512          # Memory limit in MiB
    depends_on:
      - db

  db:
    build: 
      context: ./database
      vyomafile: Vyomafile
    environment:         # Environment variables (WIP)
      POSTGRES_PASSWORD: secret
```
> **Note**: Custom `networks:` are not supported in Compose v1.0. Services communicate via the default bridge using their Service Name as hostname (DNS enabled).

---

## 🔧 Architecture & Dependencies

Vyoma is designed to be "Batteries Included", but some advanced features need helpers.

**Primary Dependencies (Bundled in Package):**
*   **Cloud Hypervisor**: The VMM (Virtual Machine Monitor).
*   **CNI Plugins**: For VM networking (bridge, host-local IPAM).
*   **Web UI**: Dashboard bundled at `/usr/lib/vyoma/ui`.
*   **KVM**: Kernel-based Virtual Machine. **MUST be enabled in BIOS/OS** (`/dev/kvm`).
*   **Systemd**: Manages the `vyomad` daemon lifecycle.

**Optional Dependencies (Install Manually):**
*   **Virtiofsd**: REQUIRED for Volume Mounts (`-v`).
    *   **Ubuntu/Debian**: `sudo apt install virtiofsd`
    *   **Fedora**: `sudo dnf install virtiofsd`
    *   **Manual**: Download binary from [GitLab](https://gitlab.com/virtio-fs/virtiofsd/-/releases) and place in `$PATH` or `/usr/bin`.

**Privilege Model:**
*   **Daemon (`vyomad`)**: Runs as **`vyoma` user** with kernel capabilities (`CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, `CAP_NET_RAW`, `CAP_SETUID`, `CAP_SETGID`).
*   **Socket**: `/run/vyoma/vyoma.sock` with permissions `0660` (root:vyoma).
*   **Client (`vyoma`)**: Runs as **User** (must be in `vyoma` group).
*   **User Must Logout/Login Once**: After installation, log out and back in for group membership to take effect.

---

## 🧪 Testing & Development

For developers contributing to Vyoma, please refer to:
*   [CLI Reference](COMMANDS.md) for full CLI reference.

## 🤝 Contributing

We welcome contributions! Please feel free to open issues or PRs.

## 📄 License

Apache 2.0 License.
