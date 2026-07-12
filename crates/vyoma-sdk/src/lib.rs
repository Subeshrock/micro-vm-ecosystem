use anyhow::{Context, Result};
use vyoma_proto::v1::{
    CreateVmRequest, CreateVmResponse, ExecOutput, ExecRequest, ListVmsResponse,
    LogLine, LogRequest, MigrateRequest, MigrationProgress, RestoreRequest,
    SnapshotInfo, SnapshotRequest, VmIdRequest, VmInfo, VmStatusResponse,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkConfig {
    pub address: String,
    pub port: u16,
}

impl SdkConfig {
    pub fn new(address: impl Into<String>, port: u16) -> Self {
        Self {
            address: address.into(),
            port,
        }
    }

    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.address, self.port)
    }
}

pub struct VyomaClient {
    config: SdkConfig,
}

impl VyomaClient {
    pub fn new(config: SdkConfig) -> Self {
        Self { config }
    }

    pub fn connect(address: impl Into<String>) -> Self {
        Self::new(SdkConfig::new(address, 50051))
    }

    async fn send_request<R: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        request: &R,
    ) -> Result<T> {
        let stream = TcpStream::connect(&self.config.endpoint())
            .await
            .context("Failed to connect to agent")?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let request_json = serde_json::to_string(request)?;
        let request_line = serde_json::to_string(&RequestLine {
            method,
            payload: &request_json,
        })?;

        writer
            .write_all(request_line.as_bytes())
            .await
            .context("Failed to send request")?;
        writer.flush().await?;

        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .await
            .context("Failed to read response")?;

        let response: Response<T> =
            serde_json::from_str(&response_line).context("Failed to parse response")?;

        response.into_result()
    }

    pub async fn create_vm(&self, request: CreateVmRequest) -> Result<CreateVmResponse> {
        self.send_request("create_vm", &request).await
    }

    pub async fn start_vm(&self, vm_id: &str) -> Result<VmStatusResponse> {
        self.send_request("start_vm", &VmIdRequest { vm_id: vm_id.to_string() })
            .await
    }

    pub async fn stop_vm(&self, vm_id: &str) -> Result<VmStatusResponse> {
        self.send_request("stop_vm", &VmIdRequest { vm_id: vm_id.to_string() })
            .await
    }

    pub async fn delete_vm(&self, vm_id: &str) -> Result<VmStatusResponse> {
        self.send_request("delete_vm", &VmIdRequest { vm_id: vm_id.to_string() })
            .await
    }

    pub async fn get_vm_status(&self, vm_id: &str) -> Result<VmStatusResponse> {
        self.send_request("get_vm_status", &VmIdRequest { vm_id: vm_id.to_string() })
            .await
    }

    pub async fn list_vms(&self) -> Result<ListVmsResponse> {
        self.send_request("list_vms", &vyoma_proto::v1::ListVmsRequest {})
            .await
    }

    pub async fn exec(&self, vm_id: &str, command: &[&str]) -> Result<ExecOutput> {
        let request = ExecRequest {
            vm_id: vm_id.to_string(),
            command: command.iter().map(|s| s.to_string()).collect(),
        };
        self.send_request("exec", &request).await
    }

    pub async fn get_logs(&self, vm_id: &str, tail: Option<i32>) -> Result<Vec<LogLine>> {
        let request = LogRequest {
            vm_id: vm_id.to_string(),
            follow: false,
            tail: tail.unwrap_or(100),
        };
        self.send_request("get_logs", &request).await
    }

    pub async fn create_snapshot(&self, vm_id: &str, name: &str) -> Result<SnapshotInfo> {
        let request = SnapshotRequest {
            vm_id: vm_id.to_string(),
            name: name.to_string(),
        };
        self.send_request("create_snapshot", &request).await
    }

    pub async fn restore_snapshot(&self, vm_id: &str, snapshot_id: &str) -> Result<VmStatusResponse> {
        let request = RestoreRequest {
            vm_id: vm_id.to_string(),
            snapshot_id: snapshot_id.to_string(),
        };
        self.send_request("restore_snapshot", &request).await
    }

    pub async fn migrate(&self, vm_id: &str, dest: &str, bandwidth_mbps: u32) -> Result<MigrationProgress> {
        let request = MigrateRequest {
            vm_id: vm_id.to_string(),
            dest_address: dest.to_string(),
            bandwidth_mbps,
        };
        self.send_request("migrate", &request).await
    }
}

#[derive(Serialize)]
struct RequestLine<'a> {
    method: &'a str,
    payload: &'a str,
}

#[derive(Deserialize)]
struct Response<T> {
    success: bool,
    data: Option<T>,
    error: Option<String>,
}

impl<T> Response<T> {
    fn into_result(self) -> Result<T> {
        match (self.success, self.data, self.error) {
            (true, Some(data), _) => Ok(data),
            (false, _, Some(err)) => Err(anyhow::anyhow!("{}", err)),
            _ => Err(anyhow::anyhow!("Unknown error")),
        }
    }
}

pub mod mock {
    use anyhow::Result;
    use vyoma_proto::v1::{
        CreateVmRequest, CreateVmResponse, ExecOutput, ListVmsResponse, LogLine,
        MigrationProgress, SnapshotInfo, VmInfo, VmStatusResponse,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    pub struct MockVyomaClient {
        vms: Mutex<HashMap<String, VmInfo>>,
    }

    impl MockVyomaClient {
        pub fn new() -> Self {
            Self {
                vms: Mutex::new(HashMap::new()),
            }
        }

        pub fn with_vms(vms: Vec<VmInfo>) -> Self {
            let map: HashMap<String, VmInfo> = vms.into_iter().map(|v| (v.id.clone(), v)).collect();
            Self {
                vms: Mutex::new(map),
            }
        }

        pub fn create_vm(&self, _request: CreateVmRequest) -> Result<CreateVmResponse> {
            Ok(CreateVmResponse {
                vm_id: "vm-mock-123".to_string(),
            })
        }

        pub fn start_vm(&self, vm_id: &str) -> Result<VmStatusResponse> {
            let mut vms = self.vms.lock().unwrap();
            if let Some(vm) = vms.get_mut(vm_id) {
                vm.status = "Running".to_string();
            }
            Ok(VmStatusResponse {
                vm_id: vm_id.to_string(),
                status: "Running".to_string(),
            })
        }

        pub fn stop_vm(&self, vm_id: &str) -> Result<VmStatusResponse> {
            let mut vms = self.vms.lock().unwrap();
            if let Some(vm) = vms.get_mut(vm_id) {
                vm.status = "Stopped".to_string();
            }
            Ok(VmStatusResponse {
                vm_id: vm_id.to_string(),
                status: "Stopped".to_string(),
            })
        }

        pub fn delete_vm(&self, vm_id: &str) -> Result<VmStatusResponse> {
            let mut vms = self.vms.lock().unwrap();
            vms.remove(vm_id);
            Ok(VmStatusResponse {
                vm_id: vm_id.to_string(),
                status: "Deleted".to_string(),
            })
        }

        pub fn get_vm_status(&self, vm_id: &str) -> Result<VmStatusResponse> {
            let vms = self.vms.lock().unwrap();
            let status = vms
                .get(vm_id)
                .map(|v| v.status.clone())
                .unwrap_or_else(|| "NotFound".to_string());
            Ok(VmStatusResponse {
                vm_id: vm_id.to_string(),
                status,
            })
        }

        pub fn list_vms(&self) -> Result<ListVmsResponse> {
            let vms = self.vms.lock().unwrap();
            Ok(ListVmsResponse {
                vms: vms.values().cloned().collect(),
            })
        }

        pub fn exec(&self, _vm_id: &[u8], command: &[String]) -> Result<ExecOutput> {
            let cmd_str = command.join(" ");
            let (stdout, exit_code) = if cmd_str.contains("ls") {
                (b"file1.txt\nfile2.txt\n".to_vec(), 0)
            } else if cmd_str.contains("pwd") {
                (b"/home\n".to_vec(), 0)
            } else if cmd_str.contains("echo") {
                (b"hello\n".to_vec(), 0)
            } else {
                (b"".to_vec(), 127)
            };
            Ok(ExecOutput {
                stdout,
                stderr: b"".to_vec(),
                exit_code,
            })
        }

        pub fn get_logs(&self, vm_id: &str, _tail: i32) -> Result<Vec<LogLine>> {
            Ok(vec![
                LogLine {
                    line: format!("[{}] VM started", vm_id),
                    timestamp: 1234567890,
                },
                LogLine {
                    line: "[system] Kernel initialized".to_string(),
                    timestamp: 1234567891,
                },
            ])
        }

        pub fn create_snapshot(&self, vm_id: &str, name: &str) -> Result<SnapshotInfo> {
            Ok(SnapshotInfo {
                snapshot_id: format!("snap-{}-{}", vm_id, name),
                name: name.to_string(),
                created_at: 1234567890,
                size_bytes: 1024000,
            })
        }

        pub fn restore_snapshot(&self, vm_id: &str, _snapshot_id: &str) -> Result<VmStatusResponse> {
            Ok(VmStatusResponse {
                vm_id: vm_id.to_string(),
                status: "Running".to_string(),
            })
        }

        pub fn migrate(&self, vm_id: &str, _dest: &str, _bandwidth: u32) -> Result<MigrationProgress> {
            Ok(MigrationProgress {
                round: 1,
                pages_transferred: 0,
                total_pages: 65536,
                bytes_transferred: 0,
                completed: true,
                error: String::new(),
            })
        }
    }

    impl Default for MockVyomaClient {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vyoma_proto::v1::{PortMapping, VolumeMapping};

    #[test]
    fn test_sdk_config() {
        let config = SdkConfig::new("localhost", 9000);
        assert_eq!(config.endpoint(), "localhost:9000");
    }

    #[test]
    fn test_client_connect() {
        let client = VyomaClient::connect("localhost");
        assert_eq!(client.config.endpoint(), "localhost:50051");
    }

     #[test]
     fn test_mock_client_create_vm() {
         let client = mock::MockVyomaClient::new();
         let request = CreateVmRequest {
             image: "ubuntu:latest".to_string(),
             vcpus: 2,
             memory_mb: 2048,
             name: "test-vm".to_string(),
             ports: vec![],
             volumes: vec![],
             networks: vec![],
         };
        let response = client.create_vm(request).unwrap();
        assert_eq!(response.vm_id, "vm-mock-123");
    }

    #[test]
    fn test_mock_client_list_vms() {
        let vms = vec![VmInfo {
            id: "vm-1".to_string(),
            image: "nginx:latest".to_string(),
            status: "Running".to_string(),
            ip: "192.168.1.10".to_string(),
            vcpus: 2,
            memory_mb: 2048,
            ports: vec![],
            created_at: 1234567890,
        }];
        let client = mock::MockVyomaClient::with_vms(vms);
        let response = client.list_vms().unwrap();
        assert_eq!(response.vms.len(), 1);
        assert_eq!(response.vms[0].id, "vm-1");
    }

    #[test]
    fn test_mock_client_start_vm() {
        let vms = vec![VmInfo {
            id: "vm-1".to_string(),
            image: "nginx:latest".to_string(),
            status: "Stopped".to_string(),
            ip: "192.168.1.10".to_string(),
            vcpus: 2,
            memory_mb: 2048,
            ports: vec![],
            created_at: 1234567890,
        }];
        let client = mock::MockVyomaClient::with_vms(vms);
        let response = client.start_vm("vm-1").unwrap();
        assert_eq!(response.status, "Running");
    }

    #[test]
    fn test_mock_client_stop_vm() {
        let vms = vec![VmInfo {
            id: "vm-1".to_string(),
            image: "nginx:latest".to_string(),
            status: "Running".to_string(),
            ip: "192.168.1.10".to_string(),
            vcpus: 2,
            memory_mb: 2048,
            ports: vec![],
            created_at: 1234567890,
        }];
        let client = mock::MockVyomaClient::with_vms(vms);
        let response = client.stop_vm("vm-1").unwrap();
        assert_eq!(response.status, "Stopped");
    }

    #[test]
    fn test_mock_client_exec_ls() {
        let client = mock::MockVyomaClient::new();
        let output = client.exec(b"vm-1", &["ls".to_string(), "-la".to_string()]).unwrap();
        assert_eq!(output.exit_code, 0);
    }

    #[test]
    fn test_mock_client_exec_pwd() {
        let client = mock::MockVyomaClient::new();
        let output = client.exec(b"vm-1", &["pwd".to_string()]).unwrap();
        assert_eq!(std::str::from_utf8(&output.stdout).unwrap().trim(), "/home");
    }

    #[test]
    fn test_mock_client_logs() {
        let client = mock::MockVyomaClient::new();
        let logs = client.get_logs("vm-1", 100).unwrap();
        assert!(!logs.is_empty());
    }

    #[test]
    fn test_mock_client_create_snapshot() {
        let client = mock::MockVyomaClient::new();
        let snap = client.create_snapshot("vm-1", "backup-1").unwrap();
        assert_eq!(snap.name, "backup-1");
    }

    #[test]
    fn test_mock_client_migrate() {
        let client = mock::MockVyomaClient::new();
        let result = client.migrate("vm-1", "192.168.1.20", 1000).unwrap();
        assert!(result.completed);
    }
}
