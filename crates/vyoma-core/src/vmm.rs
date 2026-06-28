use anyhow::{anyhow, Result};
use serde::{Serialize, Deserialize};
use std::path::Path;
use std::process::{Command, Child, Stdio};
use std::time::Duration;
use std::fmt;
use std::thread;
use std::io::{BufRead, BufReader};
use tracing::{info, error};
use tokio::sync::broadcast;
use reqwest::{Client, Method};

use crate::ch_types::{
    VmConfig, CpusConfig, MemoryConfig, PayloadConfig, DiskConfig, NetConfig, FsConfig,
    ConsoleConfig, VmSnapshotConfig, VmRestoreConfig, SendMigrationData, ReceiveMigrationData
};

impl fmt::Debug for VmmManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VmmManager")
         .field("socket_path", &self.socket_path)
         .field("process", &if self.process.is_some() { "Some(Child)" } else { "None" })
         .finish()
    }
}

/// Manages a Cloud Hypervisor process and its API interaction.
pub struct VmmManager {
    socket_path: String,
    process: Option<Child>,
    log_sender: broadcast::Sender<String>,
    config: VmConfig,
    client: Client,
}

impl VmmManager {
    pub fn new(socket_path: &str) -> Self {
        let (tx, _) = broadcast::channel(100);
        
        let client = Client::builder()
            .unix_socket(socket_path)
            .build()
            .unwrap_or_else(|_| Client::new()); // Fallback, but unix_socket should work
            
        Self {
            socket_path: socket_path.to_string(),
            process: None,
            log_sender: tx,
            config: VmConfig::default(),
            client,
        }
    }
    
    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(|p| p.id())
    }

    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if let Some(child) = self.process.as_mut() {
            child.try_wait().map_err(|e| anyhow!("Failed to wait on child: {}", e))
        } else {
            Ok(None)
        }
    }

    pub fn start_daemon(&mut self, binary_path: &str, netns: Option<&str>, rootless: bool) -> Result<()> {
        info!("Starting Cloud Hypervisor at {} (Socket: {}, NetNS: {:?}, Rootless: {})", binary_path, self.socket_path, netns, rootless);
        
        if Path::new(&self.socket_path).exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        let mut child = if rootless {
             Command::new("unshare")
                .arg("-r")
                .arg("-n")
                .arg(binary_path)
                .arg("--api-socket")
                .arg(&self.socket_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| anyhow!("Failed to spawn cloud-hypervisor with unshare: {}", e))?
        } else if let Some(ns) = netns {
             Command::new("ip")
                .arg("netns")
                .arg("exec")
                .arg(ns)
                .arg(binary_path)
                .arg("--api-socket")
                .arg(&self.socket_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| anyhow!("Failed to spawn cloud-hypervisor in netns: {}", e))?
        } else {
             Command::new(binary_path)
                .arg("--api-socket")
                .arg(&self.socket_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| anyhow!("Failed to spawn cloud-hypervisor: {}", e))?
        };

        let stdout = child.stdout.take().ok_or(anyhow!("Failed to capture stdout"))?;
        let stderr = child.stderr.take().ok_or(anyhow!("Failed to capture stderr"))?;
        let tx_out = self.log_sender.clone();
        let tx_err = self.log_sender.clone();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                if let Ok(l) = line {
                    tracing::info!("[CH STDOUT] {}", l);
                    let _ = tx_out.send(format!("[STDOUT] {}", l));
                }
            }
        });

        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    tracing::error!("[CH STDERR] {}", l);
                    let _ = tx_err.send(format!("[STDERR] {}", l));
                }
            }
        });

        self.process = Some(child);
        self.wait_for_socket(Duration::from_secs(5))?;
        
        Ok(())
    }

    fn wait_for_socket(&self, timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if Path::new(&self.socket_path).exists() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(anyhow!("Timed out waiting for Cloud Hypervisor socket"))
    }

    pub async fn check_alive(&self) -> bool {
        self.api_request::<()>("/api/v1/vmm.ping", Method::GET, None).await.is_ok()
    }

    async fn api_request<T: Serialize>(&self, endpoint: &str, method: Method, body: Option<&T>) -> Result<()> {
        let url = format!("http://localhost{}", endpoint);
        info!("VMM api_request: socket_path={}, endpoint={}, method={}", self.socket_path, endpoint, method);
        
        if !Path::new(&self.socket_path).exists() {
            return Err(anyhow!("Cloud Hypervisor socket not found at {} - it may have crashed", self.socket_path));
        }

        let mut req = self.client.request(method.clone(), &url);
        
        if let Some(b) = body {
            req = req.json(b);
        }

        let response = req.send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let err_text = response.text().await.unwrap_or_default();
            error!("API {} failed. status: {}, body: {}", endpoint, status, err_text);
            return Err(anyhow!("API request {} failed with {}: {}", endpoint, status, err_text));
        }

        Ok(())
    }

    pub async fn set_boot_source(&mut self, kernel_path: &str, boot_args: &str, initramfs: Option<&str>) -> Result<()> {
        self.config.payload = Some(PayloadConfig {
            kernel: Some(kernel_path.to_string()),
            cmdline: Some(boot_args.to_string()),
            initramfs: initramfs.map(|s| s.to_string()),
        });
        Ok(())
    }

    pub async fn add_drive(&mut self, drive_id: &str, host_path: &str, is_root: bool) -> Result<()> {
        let disk = DiskConfig {
            path: host_path.to_string(),
            readonly: if is_root { Some(true) } else { None }, // Usually rootfs is readonly if there's a COW layer
            direct: None,
            vhost_user: None,
        };
        
        if let Some(disks) = &mut self.config.disks {
            disks.push(disk);
        } else {
            self.config.disks = Some(vec![disk]);
        }
        Ok(())
    }
    
    pub async fn set_machine_config(&mut self, vcpu_count: u32, mem_size_mib: u32) -> Result<()> {
        self.config.cpus = Some(CpusConfig {
            boot_vcpus: vcpu_count,
            max_vcpus: Some(vcpu_count),
        });
        
        self.config.memory = Some(MemoryConfig {
            size: (mem_size_mib as u64) * 1024 * 1024,
            shared: Some(true), // often required for virtiofs or vhost-user
            hugepages: None,
        });
        Ok(())
    }

    pub async fn add_network_interface(&mut self, iface_id: &str, host_dev_name: &str, guest_mac: Option<&str>) -> Result<()> {
        let net = NetConfig {
            tap: Some(host_dev_name.to_string()),
            mac: guest_mac.map(|s| s.to_string()),
            ip: None,
            mask: None,
            vhost_user: None,
        };
        
        if let Some(nets) = &mut self.config.net {
            nets.push(net);
        } else {
            self.config.net = Some(vec![net]);
        }
        Ok(())
    }

    pub async fn add_file_system(&mut self, fs_id: &str, socket_path: &str, tag: &str) -> Result<()> {
        let fs = FsConfig {
            tag: tag.to_string(),
            socket: socket_path.to_string(),
            num_queues: Some(1),
            queue_size: Some(1024),
        };
        
        if let Some(fss) = &mut self.config.fs {
            fss.push(fs);
        } else {
            self.config.fs = Some(vec![fs]);
        }
        Ok(())
    }

    pub async fn add_vsock(&mut self, cid: u32, socket_path: &str) -> Result<()> {
        let vsock = crate::ch_types::VsockConfig {
            cid,
            socket: socket_path.to_string(),
        };
        self.config.vsock = Some(vsock);
        Ok(())
    }

    pub async fn enable_serial(&mut self, file_path: &str) -> Result<()> {
        let serial = crate::ch_types::ConsoleConfig {
            file: Some(file_path.to_string()),
            mode: "File".to_string(),
        };
        self.config.serial = Some(serial);
        Ok(())
    }

    pub async fn set_firmware(&mut self, firmware_path: &str, secure_boot: bool, uefi_vars: Option<&str>) -> Result<()> {
        let firmware = crate::ch_types::FirmwareConfig {
            firmware_path: firmware_path.to_string(),
            secure_boot,
            uefi_vars: uefi_vars.map(|s| s.to_string()),
        };
        self.config.firmware = Some(firmware);
        info!("Configured firmware: {} (secure_boot: {})", firmware_path, secure_boot);
        Ok(())
    }

    pub async fn set_tpm(&mut self, socket_path: &str) -> Result<()> {
        let tpm = crate::ch_types::TpmConfig {
            socket_path: socket_path.to_string(),
            tpm_version: "2.0".to_string(),
        };
        self.config.tpm = Some(tpm);
        info!("Configured TPM at {}", socket_path);
        Ok(())
    }

    pub async fn start_instance(&self) -> Result<()> {
        // Step 1: Create VM with config
        info!("Sending VmConfig to Cloud Hypervisor: {:?}", self.config);
        self.api_request("/api/v1/vm.create", Method::PUT, Some(&self.config)).await?;
        
        // Step 2: Boot VM
        self.api_request::<()>("/api/v1/vm.boot", Method::PUT, None).await?;
        Ok(())
    }

    pub async fn pause_instance(&self) -> Result<()> {
        self.api_request::<()>("/api/v1/vm.pause", Method::PUT, None).await
    }

    pub async fn resume_instance(&self) -> Result<()> {
        self.api_request::<()>("/api/v1/vm.resume", Method::PUT, None).await
    }

    pub async fn create_snapshot(&self, snapshot_path: &str, mem_file_path: &str) -> Result<()> {
        let config = VmSnapshotConfig {
            destination_url: format!("file://{}", snapshot_path),
        };
        self.api_request("/api/v1/vm.snapshot", Method::PUT, Some(&config)).await
    }

    pub async fn load_snapshot(&self, snapshot_path: &str, mem_file_path: &str) -> Result<()> {
        let config = VmRestoreConfig {
            source_url: format!("file://{}", snapshot_path),
        };
        self.api_request("/api/v1/vm.restore", Method::PUT, Some(&config)).await
    }

    pub async fn send_migration(&self, target_url: &str, bandwidth_mbps: Option<u32>) -> Result<()> {
        let config = SendMigrationData {
            destination_url: target_url.to_string(),
            local: None,
            bandwidth: bandwidth_mbps,
        };
        self.api_request("/api/v1/vm.send-migration", Method::PUT, Some(&config)).await
    }

    pub async fn receive_migration(&self, receiver_url: &str) -> Result<()> {
        let config = ReceiveMigrationData {
            receiver_url: receiver_url.to_string(),
        };
        self.api_request("/api/v1/vm.receive-migration", Method::PUT, Some(&config)).await
    }

    pub async fn enable_sev_snp(&mut self, policy: Option<&str>, guest_key_root: Option<&str>) -> Result<()> {
        let sev_snp = crate::ch_types::SevSnpConfig {
            enabled: true,
            policy: policy.map(|s| s.to_string()),
            certificate_path: None,
            guest_key_root_hash: guest_key_root.map(|s| s.to_string()),
            host_data: None,
        };
        self.config.sev_snp = Some(sev_snp);
        info!("Enabled SEV-SNP with policy: {:?}", policy);
        Ok(())
    }

    pub async fn enable_tdx(&mut self, measurement_uuid: Option<&str>) -> Result<()> {
        let tdx = crate::ch_types::TdxConfig {
            enabled: true,
            measurement_uuid: measurement_uuid.map(|s| s.to_string()),
        };
        self.config.tdx = Some(tdx);
        info!("Enabled TDX with UUID: {:?}", measurement_uuid);
        Ok(())
    }

    pub fn is_sev_snp_enabled(&self) -> bool {
        self.config.sev_snp.as_ref().map(|c| c.enabled).unwrap_or(false)
    }

    pub fn is_tdx_enabled(&self) -> bool {
        self.config.tdx.as_ref().map(|c| c.enabled).unwrap_or(false)
    }

    pub fn kill(&mut self) -> Result<()> {
        if let Some(mut child) = self.process.take() {
            info!("Killing Cloud Hypervisor process");
            let _ = child.kill();
            let _ = child.wait();
        }
        if Path::new(&self.socket_path).exists() {
           let _ = std::fs::remove_file(&self.socket_path);
        }
        Ok(())
    }
    
    pub fn subscribe_logs(&self) -> broadcast::Receiver<String> {
        self.log_sender.subscribe()
    }
}

impl Drop for VmmManager {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}
