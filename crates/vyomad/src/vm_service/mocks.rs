//! Mock implementations for unit testing VM service stages

use std::path::PathBuf;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::types::{
    PreparedStorage, VmNetworkConfig, NetworkInfo, AgentConfig,
    ChConfig, VmRunRequest,
};
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct MockPreparedStorage {
    pub dm_device_path: String,
    pub loop_devices: Vec<String>,
    pub cow_file_path: String,
    pub dm_name: String,
}

impl From<MockPreparedStorage> for PreparedStorage {
    fn from(m: MockPreparedStorage) -> Self {
        PreparedStorage {
            dm_device_path: m.dm_device_path,
            loop_devices: m.loop_devices,
            cow_file_path: m.cow_file_path,
            dm_name: m.dm_name,
        }
    }
}

#[async_trait]
pub trait StorageProvider: Send + Sync {
    async fn prepare_storage(
        &self,
        _state: &AppState,
        base_image_path: &PathBuf,
        vm_dir: &PathBuf,
        vm_id: &str,
    ) -> anyhow::Result<PreparedStorage>;
}

pub struct MockStorageProvider {
    storage: MockPreparedStorage,
}

impl MockStorageProvider {
    pub fn new(storage: MockPreparedStorage) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl StorageProvider for MockStorageProvider {
    async fn prepare_storage(
        &self,
        _state: &AppState,
        _base_image_path: &PathBuf,
        _vm_dir: &PathBuf,
        _vm_id: &str,
    ) -> anyhow::Result<PreparedStorage> {
        Ok(self.storage.clone().into())
    }
}

#[derive(Debug, Clone)]
pub struct MockNetworkConfig {
    pub ip_address: String,
    pub primary_tap: String,
    pub gateway: String,
    pub network_infos: Vec<MockNetworkInfo>,
    pub netns_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MockNetworkInfo {
    pub ip: String,
    pub tap_name: String,
    pub gateway: Option<String>,
    pub interface_name: String,
    pub network_name: String,
}

impl From<MockNetworkConfig> for VmNetworkConfig {
    fn from(m: MockNetworkConfig) -> Self {
        VmNetworkConfig {
            ip_address: m.ip_address,
            primary_tap: m.primary_tap,
            gateway: m.gateway,
            network_infos: m.network_infos.into_iter().map(|n| NetworkInfo {
                ip: n.ip,
                tap_name: n.tap_name,
                gateway: n.gateway,
                interface_name: n.interface_name,
                network_name: n.network_name,
            }).collect(),
            netns_path: m.netns_path,
        }
    }
}

#[async_trait]
pub trait NetworkProvider: Send + Sync {
    async fn setup_network(
        &self,
        _state: &AppState,
        _vm_id: &str,
        _networks: &[String],
    ) -> anyhow::Result<VmNetworkConfig>;
}

pub struct MockNetworkProvider {
    config: MockNetworkConfig,
}

impl MockNetworkProvider {
    pub fn new(config: MockNetworkConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl NetworkProvider for MockNetworkProvider {
    async fn setup_network(
        &self,
        _state: &AppState,
        _vm_id: &str,
        _networks: &[String],
    ) -> anyhow::Result<VmNetworkConfig> {
        Ok(self.config.clone().into())
    }
}

#[async_trait]
pub trait AgentProvider: Send + Sync {
    async fn prepare_agent(
        &self,
        _state: &AppState,
        _dm_path: &str,
        _vm_dir: &PathBuf,
        _config: &vyoma_core::oci::OciImageConfig,
        _ip_address: &str,
        _gateway: &str,
        _volumes: &[vyoma_core::api::VolumeMount],
    ) -> anyhow::Result<AgentConfig>;
}

pub struct MockAgentProvider {
    pub agent_config: AgentConfig,
}

impl MockAgentProvider {
    pub fn new(agent_config: AgentConfig) -> Self {
        Self { agent_config }
    }
}

#[async_trait]
impl AgentProvider for MockAgentProvider {
    async fn prepare_agent(
        &self,
        _state: &AppState,
        _dm_path: &str,
        _vm_dir: &PathBuf,
        _config: &vyoma_core::oci::OciImageConfig,
        _ip_address: &str,
        _gateway: &str,
        _volumes: &[vyoma_core::api::VolumeMount],
    ) -> anyhow::Result<AgentConfig> {
        Ok(self.agent_config.clone())
    }
}

pub struct MockVmmManager {
    pub socket_path: String,
    pub started: bool,
}

impl MockVmmManager {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            started: false,
        }
    }

    pub fn mark_started(&mut self) {
        self.started = true;
    }
}

#[async_trait]
pub trait BootProvider: Send + Sync {
    async fn start_vm(
        &self,
        ch_config: &ChConfig,
        network_config: &VmNetworkConfig,
        request: &VmRunRequest,
        state: &AppState,
    ) -> anyhow::Result<(
        MockVmmManager,
        Vec<tokio::task::JoinHandle<()>>,
        Vec<vyoma_core::fs::VirtioFsManager>,
    )>;
}

pub struct MockBootProvider;

impl MockBootProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BootProvider for MockBootProvider {
    async fn start_vm(
        &self,
        ch_config: &ChConfig,
        _network_config: &VmNetworkConfig,
        _request: &VmRunRequest,
        _state: &AppState,
    ) -> anyhow::Result<(
        MockVmmManager,
        Vec<tokio::task::JoinHandle<()>>,
        Vec<vyoma_core::fs::VirtioFsManager>,
    )> {
        let mut vmm = MockVmmManager::new(&ch_config.socket_path);
        vmm.mark_started();
        Ok((vmm, vec![], vec![]))
    }
}

impl Default for MockBootProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_mock_storage_provider() {
        let storage = MockPreparedStorage {
            dm_device_path: "/dev/mapper/test".to_string(),
            loop_devices: vec!["/dev/loop0".to_string()],
            cow_file_path: "/tmp/test.cow".to_string(),
            dm_name: "test".to_string(),
        };
        let provider = MockStorageProvider::new(storage);
        assert!(!provider.storage.dm_name.is_empty());
    }

    #[test]
    fn test_mock_network_provider() {
        let config = MockNetworkConfig {
            ip_address: "192.168.1.100".to_string(),
            primary_tap: "tap0".to_string(),
            gateway: "192.168.1.1".to_string(),
            network_infos: vec![MockNetworkInfo {
                ip: "192.168.1.100".to_string(),
                tap_name: "tap0".to_string(),
                gateway: Some("192.168.1.1".to_string()),
                interface_name: "eth0".to_string(),
                network_name: "default".to_string(),
            }],
            netns_path: Some("/var/run/netns/test".to_string()),
        };
        let provider = MockNetworkProvider::new(config);
        assert_eq!(provider.config.ip_address, "192.168.1.100");
    }

    #[test]
    fn test_mock_boot_provider_default() {
        let provider = MockBootProvider::new();
        let provider2 = MockBootProvider::default();
        assert!(std::mem::size_of_val(&provider) >= 0);
        assert!(std::mem::size_of_val(&provider2) >= 0);
    }

    #[test]
    fn test_agent_config_from_mock() {
        let agent = AgentConfig {
            initramfs_path: Some(PathBuf::from("/tmp/initramfs")),
            cmd: vec!["/sbin/init".to_string()],
            workdir: "/".to_string(),
            envs: vec![],
        };
        let provider = MockAgentProvider::new(agent);
        assert!(provider.agent_config.initramfs_path.is_some());
    }
}