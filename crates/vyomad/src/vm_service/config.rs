use anyhow::{Context, Result};
use std::path::PathBuf;

use super::types::{ChConfig, VmNetworkConfig, VmRunRequest, AgentConfig};
use crate::state::AppState;

pub fn build_ch_config(
    state: &AppState,
    vm_id: &str,
    cid: &u32,
    vm_dir: &PathBuf,
    rootfs_path: &str,
    network_config: &VmNetworkConfig,
    agent_config: &AgentConfig,
    kernel_path: &PathBuf,
) -> ChConfig {
    let socket_path = vm_dir.join("ch.sock").to_string_lossy().to_string();
    let ch_path = {
        let bundled = format!("{}/bin/cloud-hypervisor", state.data_dir);
        if std::path::Path::new(&bundled).exists() {
            bundled
        } else {
            "/usr/bin/cloud-hypervisor".to_string()
        }
    };
    let vsock_path = vm_dir.join("vsock.sock");

    let boot_args = format!(
        "console=ttyS0 reboot=k panic=1 root=/dev/vda rw ip={}::{}:255.255.255.0:{}:eth0:off:{} init=/sbin/vyoma-init",
        network_config.ip_address,
        network_config.gateway,
        vm_id,
        network_config.gateway
    );

    let initramfs_path = agent_config.initramfs_path.as_ref()
        .map(|p| p.to_string_lossy().to_string());

    ChConfig {
        kernel_path: kernel_path.to_string_lossy().to_string(),
        ch_path,
        socket_path,
        boot_args,
        rootfs_path: rootfs_path.to_string(),
        vsock_cid: *cid,
        vsock_path,
        initramfs_path,
        tpm_socket_path: None,
    }
}

pub fn validate_ch_config(config: &ChConfig) -> Result<()> {
    if !std::path::Path::new(&config.kernel_path).exists() {
        anyhow::bail!("Kernel binary not found at {}", config.kernel_path);
    }
    if !std::path::Path::new(&config.ch_path).exists() {
        anyhow::bail!("Cloud Hypervisor binary not found at {}", config.ch_path);
    }
    if let Some(ref initramfs) = config.initramfs_path {
        if !std::path::Path::new(initramfs).exists() {
            anyhow::bail!("Initramfs not found at {}", initramfs);
        }
    }
    Ok(())
}
