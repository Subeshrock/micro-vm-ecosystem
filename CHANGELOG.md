# Changelog

## [0.6.0] - Upcoming (The Polish Update)
- **Lifecycle**: `vyoma restart` (Stop/Run replacement), `vyoma exec` (Agent-based HTTP channel replacing SSH, using statically compiled musl agent).
- **Monitoring**: Real-time metrics endpoint and `vyoma stats` CLI command to view live CPU and memory usage of running VMs.
- **Kernel Upgrade**: Replaced the bundled VM kernel with a custom-built Linux v6.1.100 kernel featuring `CONFIG_SQUASHFS=y` and `CONFIG_OVERLAY_FS=y` to ensure container root filesystems are correctly mounted with a writable overlay layer.
- **Logging**: `vyoma logs <service>` support.
- **Inspect**: API for inspecting VM details.

## [0.5.0] - 2026-01-23 (The Scale Edition)
- **Scaling**: `vyoma scale web=3`.
- **Metadata**: Labels support, robust `vyoma down`/`ps`.
- **Load Balancing**: Round-Robin DNS.

## [0.4.0] - 2026-01-20 (The Composer Edition)
- **Vyoma Compose**: `vyoma up`/`vyoma down` with `vyoma-compose.yml`.
- **Private Registry**: Support for `~/.docker/config.json` auth.
- **Service Discovery**: Internal DNS name resolution for Stack services.

## [0.3.0] - 2026-01-23

### Major Feature: True Rootless Mode
- **Security**: Complete implementation of Rootless Runtime (`vyoma run` without sudo).
- **Networking**: Integrated `slirp4netns` for unprivileged user-mode networking.
- **Build**: Implemented `debugfs`-based image population (replacing mount), allowing `vyoma build` and `vyoma pull` to run without root.

### Major Feature: Network Management CLI
- **Management**: Added `vyoma network create`, `vyoma network ls`, and `vyoma network rm` commands.
- **Flexibility**: Users can now create isolated CNI bridge networks with custom subnets.
- **API**: Added Daemon endpoints (`/networks`) for managing CNI configurations dynamically.

## [0.2.0-rc1] - 2025-12-27

### Feature: Daemon Robustness
- **State Persistence**: VMs now persist their state across daemon restarts. `vyomad` automatically adopts running VMs on startup.
- **Graceful Shutdown**: `vyomad` handles SIGINT/SIGTERM to cleanly stop VMs and detach resources (Loop/DM).
- **Zombie Reaping**: Implemented background process monitor to reap zombie Cloud Hypervisor/VirtioFS processes.
- **Auto-Recovery**: Daemon automatically restarts `virtiofsd` processes if they crash.
- **OOM Monitoring**: Daemon logs Kernel OOM Kill events for monitored VMs.

### Feature: Networking Maturity
- **Internal DNS**: `vyomad` now runs an embedded DNS server on the gateway IP (172.16.0.1:53).
- **Service Discovery**: VMs can resolve each other by name (e.g., `my-vm.vyoma`).
- **CNI Integration**: Full integration with CNI plugins (`ptp`/`bridge`) for network setup (Hybrid mode).

### Feature: Storage
- **Copy Instruction**: `vyoma build` now supports `COPY` instruction to inject files into the image.
- **Snapshot/Restore**: Initial implementation of VM Snapshotting (Pause/Snapshot/Resume).

### Documentation
- Added `TROUBLESHOOTING.md`.
- Added `ADR.md` entries for Robustness and DNS.

### Changes
- Updated Kernel Boot Args to configure Hostname and DNS.
