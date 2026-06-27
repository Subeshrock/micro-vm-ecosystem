use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{info, error};

use vyoma_proto::v1::vm_service_server::VmService;
use vyoma_proto::v1::{
    CreateVmRequest, CreateVmResponse, ExecOutput, ExecRequest, ListVmsRequest, ListVmsResponse,
    LogLine, LogRequest, MigrateRequest, MigrationProgress, PortMapping as ProtoPortMapping,
    RestoreRequest, SnapshotInfo, SnapshotRequest, VmIdRequest, VmInfo as ProtoVmInfo,
    VmStatusResponse, VolumeMapping as ProtoVolumeMapping,
};

use crate::state::AppState;
use crate::vm_service;
use crate::vm_service::types::VmRunRequest;
use vyoma_core::api::{PortMapping, VolumeMount};

pub struct GrpcVmService {
    state: Arc<AppState>,
}

impl GrpcVmService {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl VmService for GrpcVmService {
    async fn create_vm(
        &self,
        request: Request<CreateVmRequest>,
    ) -> Result<Response<CreateVmResponse>, Status> {
        let req = request.into_inner();
        info!("gRPC: Request to create VM: {} (image: {})", req.name, req.image);

        // Convert protobuf types to internal types with validation
        let ports: Vec<PortMapping> = req.ports.into_iter().map(|p| {
            let host_port = u16::try_from(p.host)
                .map_err(|_| Status::invalid_argument(format!("host port {} out of range", p.host)))?;
            let vm_port = u16::try_from(p.vm)
                .map_err(|_| Status::invalid_argument(format!("vm port {} out of range", p.vm)))?;
            Ok(PortMapping { host_port, vm_port })
        }).collect::<Result<Vec<_>, Status>>()?;

        let volumes: Vec<VolumeMount> = req.volumes.into_iter().map(|v| VolumeMount {
            host_path: v.host_path,
            vm_path: v.vm_path,
            read_only: false, // default to read-write for CRI
        }).collect();

        // Build labels - include name as a label
        let mut labels = std::collections::HashMap::new();
        if !req.name.is_empty() {
            labels.insert("name".to_string(), req.name.clone());
            labels.insert("vyoma.service".to_string(), req.name.clone());
        }

        let vm_request = VmRunRequest {
            image: req.image.clone(),
            vcpu: req.vcpus as u32,
            mem_size_mib: req.memory_mb as u32,
            ports,
            volumes,
            hostname: if req.name.is_empty() { None } else { Some(req.name.clone()) },
            networks: req.networks.clone(),
            labels,
            base_image_path: String::new(), // will be resolved during creation
        };

        let state = Arc::clone(&self.state);
        match vm_service::run_vm(state, vm_request).await {
            Ok(result) => {
                info!("gRPC: VM created successfully: {}", result.vm_id);
                Ok(Response::new(CreateVmResponse {
                    vm_id: result.vm_id,
                }))
            }
            Err(e) => {
                error!("gRPC: Failed to create VM: {}", e);
                Err(Status::internal(format!("Failed to create VM: {}", e)))
            }
        }
    }

    async fn start_vm(
        &self,
        request: Request<VmIdRequest>,
    ) -> Result<Response<VmStatusResponse>, Status> {
        let req = request.into_inner();
        let vm_id = req.vm_id;
        info!("gRPC: Request to start VM: {}", vm_id);

        // Check if VM exists - since run_vm already boots, just verify it exists
        let exists = {
            let vms = self.state.vms.lock().await;
            vms.contains_key(&vm_id)
        };

        if exists {
            Ok(Response::new(VmStatusResponse {
                vm_id,
                status: "Running".to_string(),
            }))
        } else {
            Err(Status::not_found("VM not found"))
        }
    }

    async fn stop_vm(
        &self,
        request: Request<VmIdRequest>,
    ) -> Result<Response<VmStatusResponse>, Status> {
        let req = request.into_inner();
        let id = req.vm_id;
        info!("gRPC: Request to stop VM: {}", id);

        let vm_arc = {
            let mut vms = self.state.vms.lock().await;
            vms.remove(&id)
        };

        if let Some(vm_mutex) = vm_arc {
            let mut vm = vm_mutex.lock().await;
            vm.cleanup(&self.state.cni_manager).await;

            if let Err(e) = self.state.wal.append(&crate::state::wal::WalEntry::vm_stop(id.clone())) {
                error!("Failed to write WAL entry: {}", e);
            }
            
            let _ = self.state.events_tx.send(serde_json::json!({
                "type": "vm_stop",
                "id": id
            }).to_string());
            
            Ok(Response::new(VmStatusResponse {
                vm_id: id,
                status: "stopped".to_string()
            }))
        } else {
            Err(Status::not_found("VM not found"))
        }
    }

    async fn delete_vm(
        &self,
        request: Request<VmIdRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let vm_id = req.vm_id;
        info!("gRPC: Request to delete VM: {}", vm_id);

        // Use the vm_service::state::stop_vm function
        match crate::vm_service::state::stop_vm(&self.state, &vm_id).await {
            Ok(_) => {
                info!("gRPC: VM deleted successfully: {}", vm_id);
                Ok(Response::new(()))
            }
            Err(e) => {
                error!("gRPC: Failed to delete VM: {}", e);
                Err(Status::internal(format!("Failed to delete VM: {}", e)))
            }
        }
    }

    async fn list_vms(
        &self,
        _request: Request<ListVmsRequest>,
    ) -> Result<Response<ListVmsResponse>, Status> {
        let instances: Vec<std::sync::Arc<tokio::sync::Mutex<crate::state::VmInstance>>> = {
            let vms_map = self.state.vms.lock().await;
            vms_map.values().cloned().collect()
        };

        let mut vms = Vec::new();
        for arc_inst in instances {
            let inst = arc_inst.lock().await;
            vms.push(ProtoVmInfo {
                id: inst.id.clone(),
                image: inst.base_image_path.clone(),
                status: "running".to_string(),
                ip: inst.ip_address.clone(),
                vcpus: inst.vcpu,
                memory_mb: inst.mem_size_mib as u64,
                ports: vec![],
                created_at: 0,
            });
        }

        Ok(Response::new(ListVmsResponse { vms }))
    }

    async fn get_vm(
        &self,
        request: Request<VmIdRequest>,
    ) -> Result<Response<ProtoVmInfo>, Status> {
        let req = request.into_inner();
        let vm_arc = {
            let vms = self.state.vms.lock().await;
            vms.get(&req.vm_id).cloned()
        };

        if let Some(vm_mutex) = vm_arc {
            let inst = vm_mutex.lock().await;
            let info = ProtoVmInfo {
                id: inst.id.clone(),
                image: inst.base_image_path.clone(),
                status: "running".to_string(),
                ip: inst.ip_address.clone(),
                vcpus: inst.vcpu,
                memory_mb: inst.mem_size_mib as u64,
                ports: vec![],
                created_at: 0,
            };
            Ok(Response::new(info))
        } else {
            Err(Status::not_found("VM not found"))
        }
    }

    type ExecCommandStream = tonic::codegen::BoxStream<ExecOutput>;

    async fn exec_command(
        &self,
        request: Request<ExecRequest>,
    ) -> Result<Response<Self::ExecCommandStream>, Status> {
        Err(Status::unimplemented("Not implemented"))
    }

    type StreamLogsStream = tonic::codegen::BoxStream<LogLine>;

    async fn stream_logs(
        &self,
        request: Request<LogRequest>,
    ) -> Result<Response<Self::StreamLogsStream>, Status> {
        use tokio_stream::wrappers::BroadcastStream;
        use tokio_stream::StreamExt;
        
        let req = request.into_inner();
        let vm_id = req.vm_id;

        let vm_arc = {
            let vms = self.state.vms.lock().await;
            vms.get(&vm_id).cloned()
        };
        
        let vm_arc = vm_arc.ok_or_else(|| Status::not_found("VM not found"))?;

        let rx = {
            let vm = vm_arc.lock().await;
            vm.vmm.subscribe_logs()
        };

        let stream = BroadcastStream::new(rx).filter_map(|result| {
            match result {
                Ok(msg) => Some(Ok(LogLine {
                    line: msg,
                    timestamp: chrono::Utc::now().timestamp(),
                })),
                Err(_) => None,
            }
        });

        Ok(Response::new(Box::pin(stream) as Self::StreamLogsStream))
    }

    async fn create_snapshot(
        &self,
        request: Request<SnapshotRequest>,
    ) -> Result<Response<SnapshotInfo>, Status> {
        Err(Status::unimplemented("Not implemented"))
    }

    async fn restore_snapshot(
        &self,
        request: Request<RestoreRequest>,
    ) -> Result<Response<ProtoVmInfo>, Status> {
        Err(Status::unimplemented("Not implemented"))
    }

    type MigrateVmStream = tonic::codegen::BoxStream<MigrationProgress>;

    async fn migrate_vm(
        &self,
        request: Request<MigrateRequest>,
    ) -> Result<Response<Self::MigrateVmStream>, Status> {
        Err(Status::unimplemented("Not implemented"))
    }
}


