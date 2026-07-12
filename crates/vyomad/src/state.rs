use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex as TokioMutex, RwLock};
use tokio::task::JoinHandle;
use serde::{Deserialize, Serialize};

use vyoma_core::api::{PortMapping, VolumeMount};
use vyoma_core::cgroups::CgroupManager;
use vyoma_core::fs::VirtioFsManager;
use vyoma_core::vmm::VmmManager;
use vyoma_core::vtpm::VtpmManager;
use vyoma_core::policy::PolicyManager;

use crate::swarm::{SwarmRaft, NetworkIntegration};

pub mod wal;
pub mod recovery;

#[derive(Clone)]
pub struct AppState {
    pub vms: Arc<TokioMutex<HashMap<String, Arc<TokioMutex<VmInstance>>>>>,
    pub cgroups: Arc<CgroupManager>,
    pub cni_manager: Arc<vyoma_core::cni::CniManager>,
    pub events_tx: broadcast::Sender<String>,
    pub wal: Arc<wal::Wal>,
    pub data_dir: String,
    pub swarm_raft: Arc<tokio::sync::Mutex<SwarmRaft>>,
    pub network_integration: Arc<tokio::sync::Mutex<Option<NetworkIntegration>>>,
    pub timemachine: Arc<tokio::sync::RwLock<crate::timemachine::TimeMachine>>,
    pub policy_manager: Arc<std::sync::Mutex<PolicyManager>>,
    pub api_token: Option<String>,
}

impl AppState {
    pub fn new_test() -> Self {
        let (events_tx, _) = broadcast::channel(100);
        Self {
            vms: Arc::new(TokioMutex::new(HashMap::new())),
            cgroups: Arc::new(CgroupManager::new()),
            cni_manager: Arc::new(vyoma_core::cni::CniManager::new(
                std::path::PathBuf::from("/tmp/test-cni-plugins"),
                std::path::PathBuf::from("/tmp/test-cni-config"),
            )),
            events_tx,
            wal: Arc::new(wal::Wal::new_test()),
            data_dir: "/tmp/test".to_string(),
            swarm_raft: Arc::new(tokio::sync::Mutex::new(SwarmRaft::new(1))),
            network_integration: Arc::new(tokio::sync::Mutex::new(None)),
            timemachine: Arc::new(tokio::sync::RwLock::new(crate::timemachine::TimeMachine::new_test())),
            policy_manager: Arc::new(std::sync::Mutex::new(PolicyManager::new())),
            api_token: None,
        }
    }

    pub fn with_vm_service(self: Arc<Self>) -> AppState {
        AppState {
            vms: self.vms.clone(),
            cgroups: self.cgroups.clone(),
            cni_manager: self.cni_manager.clone(),
            events_tx: self.events_tx.clone(),
            wal: self.wal.clone(),
            data_dir: self.data_dir.clone(),
            swarm_raft: self.swarm_raft.clone(),
            network_integration: self.network_integration.clone(),
            timemachine: self.timemachine.clone(),
            policy_manager: self.policy_manager.clone(),
            api_token: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VmStatus {
    PendingAttestation,
    Running,
    AttestationFailed { reason: String },
    Error { reason: String },
}

impl Default for VmStatus {
    fn default() -> Self {
        VmStatus::PendingAttestation
    }
}

impl std::fmt::Display for VmStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmStatus::PendingAttestation => write!(f, "PendingAttestation"),
            VmStatus::Running => write!(f, "Running"),
            VmStatus::AttestationFailed { reason } => write!(f, "AttestationFailed: {}", reason),
            VmStatus::Error { reason } => write!(f, "Error: {}", reason),
        }
    }
}

pub struct VmInstance {
    pub vmm: VmmManager,
    pub id: String,
    pub status: VmStatus,
    pub attestation_status: Option<String>,
    pub ch_socket_path: String,
    pub tap_name: String,
    pub dm_name: String,
    pub loop_devices: Vec<String>,
    pub cow_file_path: String,
    pub ip_address: String,
    pub proxy_tasks: Vec<JoinHandle<()>>,

    pub fs_managers: Vec<VirtioFsManager>,
    pub vtpm_manager: Option<VtpmManager>,
    pub cgroup_path: Option<String>,
    pub netns_path: Option<String>,

    pub config_ports: Vec<PortMapping>,
    pub config_volumes: Vec<VolumeMount>,
    pub hostname: Option<String>,
    pub labels: HashMap<String, String>,
    pub networks: Vec<String>,

    pub base_image_path: String,
    pub vcpu: u32,
    pub mem_size_mib: u32,

    pub attestation_task: Option<tokio::task::JoinHandle<()>>,
    pub last_cpu_sample: Option<(u64, std::time::Instant)>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VmState {
    pub id: String,
    pub tap_name: String,
    pub dm_name: String,
    pub loop_devices: Vec<String>,
    pub cow_file_path: String,
    pub ip_address: String,
    pub cgroup_path: Option<String>,
    pub netns_path: Option<String>,
    pub ports: Vec<PortMapping>,
    pub volumes: Vec<VolumeMount>,
    pub hostname: Option<String>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default)]
    pub base_image_path: String,
    #[serde(default)]
    pub vcpu: u32,
    #[serde(default)]
    pub mem_size_mib: u32,
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default)]
    pub status: VmStatus,
}

use tracing::{info, error};
use std::process::Command;
use vyoma_core::network::NetworkManager;
use vyoma_core::storage::StorageManager;

impl VmInstance {
    pub fn save_state(&self, data_dir: &str) -> anyhow::Result<()> {
        let state = VmState {
            id: self.id.clone(),
            tap_name: self.tap_name.clone(),
            dm_name: self.dm_name.clone(),
            loop_devices: self.loop_devices.clone(),
            cow_file_path: self.cow_file_path.clone(),
            ip_address: self.ip_address.clone(),
            cgroup_path: self.cgroup_path.clone(),
            netns_path: self.netns_path.clone(),
            ports: self.config_ports.clone(),
            volumes: self.config_volumes.clone(),
            hostname: self.hostname.clone(),
            labels: self.labels.clone(),
            base_image_path: self.base_image_path.clone(),
            vcpu: self.vcpu,
            mem_size_mib: self.mem_size_mib,
            networks: self.networks.clone(),
            status: self.status.clone(),
        };

        let vm_dir = std::path::PathBuf::from(data_dir).join(".vyoma").join("vms").join(&self.id);
        if !vm_dir.exists() {
            std::fs::create_dir_all(&vm_dir)?;
        }
        let state_path = vm_dir.join("state.json");
        let f = std::fs::File::create(state_path)?;
        serde_json::to_writer_pretty(f, &state)?;
        Ok(())
    }

    pub async fn cleanup(&mut self, cni_manager: &vyoma_core::cni::CniManager) {
        info!("Cleaning up VM resources for {}", self.id);

        // 1. Kill VMM
        if let Err(e) = self.vmm.kill() {
            error!("Failed to kill VMM: {}", e);
        }

        // 2. Remove Network Interface / CNI (handle multiple networks)
        if let Some(netns) = &self.netns_path {
            // Use del_multiple for cleanup with all attached networks
            if !self.networks.is_empty() {
                if let Err(e) = cni_manager.del_multiple(&self.networks, &self.id, netns) {
                    error!("CNI DEL failed for multiple networks: {}", e);
                }
            } else {
                // Fallback to single interface cleanup
                if let Err(e) = cni_manager.del(None, &self.id, netns, "eth0") {
                    error!("CNI DEL failed: {}", e);
                }
            }
            let netns_name = format!("vm-{}", self.id);
            let _ = Command::new("ip")
                .args(&["netns", "delete", &netns_name])
                .output();
        }

        if !self.tap_name.is_empty() {
            if let Err(e) = NetworkManager::remove_interface(&self.tap_name).await {
                error!("Failed to remove TAP {}: {}", self.tap_name, e);
            }
        }

        // 3. Remove DM Device
        if let Err(e) = (|| -> Result<(), vyoma_storage::StorageError> {
            let dm_manager = vyoma_storage::DmManager::new()?;
            dm_manager.remove_snapshot(&self.dm_name)
        })() {
            error!("Failed to remove DM {}: {}", self.dm_name, e);
        }

        // 4. Detach Loop Devices
        for dev in &self.loop_devices {
            if let Err(e) = StorageManager::detach_loop_device(dev) {
                error!("Failed to detach loop {}: {}", dev, e);
            }
        }

        // 5. Remove COW file
        if std::path::Path::new(&self.cow_file_path).exists() {
            let _ = std::fs::remove_file(&self.cow_file_path);
        }

        // 6. Abort Proxy Tasks
        for task in &self.proxy_tasks {
            task.abort();
        }

        // 7. Remove Cgroup
        if let Some(_path) = &self.cgroup_path {
            let cm = CgroupManager::new();
            if let Err(e) = cm.remove_vm_cgroup(&self.id) {
                error!("Failed to remove cgroup for {}: {}", self.id, e);
            }
        }

        // 8. Kill VirtioFs Managers (ADR-025)
        for fs_mgr in &mut self.fs_managers {
            if let Err(e) = fs_mgr.kill() {
                error!("Failed to kill virtiofsd: {}", e);
            }
        }

        // 8b. Abort attestation task before stopping vTPM to prevent use-after-free
        if let Some(ref handle) = self.attestation_task {
            handle.abort();
            info!("Aborted attestation task for VM {}", self.id);
        }

        // 9. Stop vTPM if present
        if let Some(ref mut vtpm) = self.vtpm_manager {
            if let Err(e) = vtpm.stop() {
                error!("Failed to stop vTPM: {}", e);
            }
        }

        // 9. Remove VM Directory
        if let Some(home) = dirs::home_dir() {
            let vm_dir = home.join(".vyoma").join("vms").join(&self.id);
            if vm_dir.exists() {
                if let Err(e) = std::fs::remove_dir_all(&vm_dir) {
                    error!("Failed to remove VM directory {}: {}", vm_dir.display(), e);
                } else {
                    info!("Removed VM directory: {}", vm_dir.display());
                }
            }
        }
    }
}
