use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tracing::{info, error};
use vyoma_core::fs::VirtioFsManager;
use vyoma_core::proxy::ProxyManager;
use vyoma_core::vmm::VmmManager;
use vyoma_core::vtpm::VtpmManager;

use super::types::{ChConfig, VmNetworkConfig, VmRunRequest};
use crate::state::AppState;

/// Determines whether TPM is needed based on policy and request.
fn needs_tpm(state: &AppState, request: &VmRunRequest) -> bool {
    let policy = state.policy_manager.lock().unwrap();
    policy.must_verify_on_boot()
}

pub async fn start_vm(
    state: &AppState,
    ch_config: &ChConfig,
    network_config: &VmNetworkConfig,
    request: &VmRunRequest,
) -> Result<(VmmManager, Vec<JoinHandle<()>>, Vec<VirtioFsManager>, Option<VtpmManager>)> {
    let use_tpm = needs_tpm(state, request);
    let mut vtpm_manager: Option<VtpmManager> = None;

    let mut vmm = VmmManager::new(&ch_config.socket_path);

    if !std::path::Path::new(&ch_config.kernel_path).exists() {
        anyhow::bail!("Kernel binary not found at {}", ch_config.kernel_path);
    }
    if !std::path::Path::new(&ch_config.ch_path).exists() {
        anyhow::bail!("Cloud Hypervisor binary not found at {}", ch_config.ch_path);
    }

    // Start vTPM if needed for measured boot attestation
    if use_tpm {
        info!("Starting vTPM for measured boot attestation");
        let vm_id_str = request.labels.get("vm_id").cloned()
            .unwrap_or_else(|| ch_config.socket_path.split('/').last().unwrap_or("unknown").to_string());
        let base_dir = std::path::Path::new(&ch_config.socket_path).parent()
            .unwrap_or(std::path::Path::new("/tmp"));
        match VtpmManager::new(vm_id_str.as_str(), base_dir) {
            Ok(mut vtpm) => {
                if let Err(e) = vtpm.start() {
                    error!("Failed to start vTPM: {}, continuing without TPM", e);
                } else {
                    info!("vTPM started at {}", vtpm.socket_path());
                    vtpm_manager = Some(vtpm);
                }
            }
            Err(e) => {
                error!("Failed to create vTPM manager: {}, continuing without TPM", e);
            }
        }
    }

    info!("Spawning Cloud Hypervisor in Privileged Mode...");

    vmm.start_daemon(&ch_config.ch_path, None, false)
        .context("Failed to start Cloud Hypervisor")?;

    vmm.set_boot_source(&ch_config.kernel_path, &ch_config.boot_args, ch_config.initramfs_path.as_deref()).await
        .context("Boot source")?;

    vmm.add_drive("rootfs", &ch_config.rootfs_path, true).await
        .context("Add drive")?;

    vmm.add_network_interface("eth0", &network_config.primary_tap, None).await
        .context("Add net (primary)")?;

    for (idx, network_info) in network_config.network_infos.iter().enumerate().skip(1) {
        let ifname = format!("eth{}", idx);
        if let Err(e) = vmm.add_network_interface(&ifname, &network_info.tap_name, None).await {
            error!("Failed to add network interface {}: {}", ifname, e);
        } else {
            info!("Added network interface {} with TAP {}", ifname, network_info.tap_name);
        }
    }

    let vsock_path = ch_config.vsock_path.to_string_lossy().to_string();
    vmm.add_vsock(ch_config.vsock_cid, &vsock_path).await
        .context("Add vsock")?;

    let mut fs_managers = Vec::new();
    for (idx, vol) in request.volumes.iter().enumerate() {
        let tag = format!("vol{}", idx);
        let socket_path = std::path::PathBuf::from(&ch_config.socket_path)
            .parent().unwrap()
            .join(format!("fs_{}.sock", idx));

        let mut fs_mgr = VirtioFsManager::new(&tag, socket_path.to_string_lossy().as_ref());

        if let Err(e) = fs_mgr.start(&vol.host_path) {
            let _ = vmm.kill();
            anyhow::bail!("Failed to start virtiofsd for {}: {}", vol.host_path, e);
        }

        vmm.add_file_system(&tag, socket_path.to_string_lossy().as_ref(), &tag).await
            .context("Add fs")?;

        fs_managers.push(fs_mgr);
    }

    // Configure TPM if vTPM was started
    if let Some(ref vtpm) = vtpm_manager {
        vmm.set_tpm(&vtpm.socket_path()).await
            .context("Failed to configure TPM")?;
        info!("Configured vTPM at {}", vtpm.socket_path());
    }

    vmm.set_machine_config(request.vcpu, request.mem_size_mib).await
        .context("Machine config")?;

    let serial_path = std::path::PathBuf::from(&ch_config.socket_path)
        .parent().unwrap()
        .join("serial.log");
    vmm.enable_serial(&serial_path.to_string_lossy()).await?;

    vmm.start_instance().await
        .context("Start instance")?;

    let mut proxy_tasks = Vec::new();
    for mapping in &request.ports {
        let handle = ProxyManager::start_proxy(
            mapping.host_port,
            network_config.ip_address.clone(),
            mapping.vm_port,
        );
        proxy_tasks.push(handle);
    }

    Ok((vmm, proxy_tasks, fs_managers, vtpm_manager))
}