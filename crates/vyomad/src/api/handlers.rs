use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    Json,
    body::Body,
};
use tokio_stream::wrappers::BroadcastStream;
// use tokio_stream::StreamExt;
use futures::stream::Stream;
use flate2::read::GzDecoder;
use tar::Archive;
use futures::StreamExt;
use tokio_util::io::StreamReader;
use uuid;
use tempfile;
use vyoma_teleport::{VmInfo};
use vyoma_core::api::{PortMapping, VolumeMount};
use vyoma_core::fs::VirtioFsManager;
use vyoma_core::policy::PolicyStatus;
use vyoma_core::proxy::ProxyManager;
use vyoma_core::vmm::VmmManager;
use vyoma_storage::{DmManager, LoopManager, LoopDevice, DmDevice};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use reqwest::{Client, Method};
use tokio::sync::Mutex as TokioMutex;
use std::path::PathBuf;
use tracing::{error, info, warn};

use crate::swarm;
use crate::state::{AppState, VmInstance, VmState, VmStatus, wal::WalEntry};
use crate::vm_service::state as vm_service_state;
use crate::vm_service::image::ensure_image_locally_handler;

#[derive(serde::Deserialize, Serialize)]
pub struct LegacyNodeInfo {
    pub id: String,
    pub ip: String,
    pub role: String,
    pub subnet_id: u8,
    pub wireguard_public_key: Option<String>,
    pub wireguard_port: Option<u16>,
}

use vyoma_teleport::MigrationProgress as TeleportProgress;
use std::sync::OnceLock;

static MIGRATION_SESSIONS: OnceLock<StdMutex<HashMap<String, MigrationSession>>> = OnceLock::new();

fn get_migration_sessions() -> &'static StdMutex<HashMap<String, MigrationSession>> {
    MIGRATION_SESSIONS.get_or_init(|| StdMutex::new(HashMap::new()))
}

#[derive(Clone, Debug)]
pub struct MigrationSession {
    pub session_id: String,
    pub vm_id: String,
    pub source_node: String,
    pub target_node: String,
    pub status: String,
    pub progress: Option<TeleportProgress>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
}




pub async fn shutdown_signal(state: AppState) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let _ = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Signal received, starting graceful shutdown...");

    let ids: Vec<String> = {
        let map = state.vms.lock().await;
        map.keys().cloned().collect()
    };

    if !ids.is_empty() {
        info!("Cleaning up {} active VMs...", ids.len());
        for id in ids {
            let vm_arc = {
                let mut map = state.vms.lock().await;
                map.remove(&id)
            };
            if let Some(vm_mutex) = vm_arc {
                let mut vm = vm_mutex.lock().await;
                vm.cleanup(&state.cni_manager).await;

                // WAL: Log VM stop
        if let Err(e) = state.wal.append(&WalEntry::vm_stop(id.clone())) {
            error!("Failed to write WAL entry: {}", e);
        }
            }
        }
    }
    info!("Graceful shutdown complete.");
}

pub async fn health_check() -> &'static str {
    "OK"
}

#[derive(Deserialize)]
pub struct RunRequest {
    pub image: String,
    #[serde(default = "default_vcpu")]
    pub vcpu: u32,
    #[serde(default = "default_mem")]
    pub mem_size_mib: u32,
    #[serde(default)]
    pub ports: Vec<PortMapping>,
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default)]
    pub labels: HashMap<String, String>,

    #[serde(default)]
    pub base_image_path: String,
}

fn default_vcpu() -> u32 {
    1
}
fn default_mem() -> u32 {
    512
}

#[derive(Serialize)]
pub struct RunResponse {
    vm_id: String,
    status: String,
    ip_address: String,
}

use vyoma_core::layers::LayerManager;
use vyoma_core::network::NetworkManager;
use vyoma_core::oci::OciManager;
use vyoma_core::storage::StorageManager;
use vyoma_agent_protocol::{AgentRequest, AgentResponse};

#[derive(Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
}

#[derive(Serialize)]
pub struct ExecResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub async fn exec_in_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, (StatusCode, String)> {
    info!("Request to execute command in VM: {}", id);

    let vm_ip = {
        let vms = state.vms.lock().await;
        if let Some(vm_arc) = vms.get(&id) {
            let vm = vm_arc.lock().await;
            vm.ip_address.clone()
        } else {
            return Err((StatusCode::NOT_FOUND, "VM not found".to_string()));
        }
    };

    // Clean IP in case of CIDR notation
    let target_ip = vm_ip.split('/').next().unwrap_or(&vm_ip).to_string();

    let target_addr = format!("{}:9999", target_ip);
    let mut last_error = String::new();
    let mut stream_opt = None;

    for attempt in 0..10 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(&target_addr),
        )
        .await
        {
            Ok(Ok(s)) => {
                stream_opt = Some(s);
                break;
            }
            Ok(Err(e)) => {
                last_error = e.to_string();
            }
            Err(_) => {
                last_error = "Connection timeout to agent".to_string();
            }
        };
    }

    let mut stream = match stream_opt {
        Some(s) => s,
        None => return Err((StatusCode::SERVICE_UNAVAILABLE, format!("Connection failed after retries: {}", last_error))),
    };

    let req = AgentRequest::ExecCommand {
        cmd: payload.command,
        env: HashMap::new(),
        workdir: None,
    };

    let req_json = serde_json::to_vec(&req).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let len_prefix = (req_json.len() as u32).to_be_bytes();
    
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    stream.write_all(&len_prefix).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    stream.write_all(&req_json).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut resp_len_buf = [0u8; 4];
    stream.read_exact(&mut resp_len_buf).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let resp_len = u32::from_be_bytes(resp_len_buf) as usize;

    let mut resp_buf = vec![0u8; resp_len];
    stream.read_exact(&mut resp_buf).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let resp: AgentResponse = serde_json::from_slice(&resp_buf).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match resp {
        AgentResponse::ExecOutput { stdout, stderr, exit_code } => {
            Ok(Json(ExecResponse {
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                exit_code,
            }))
        }
        AgentResponse::Error { message } => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Agent error: {}", message))),
        _ => Err((StatusCode::INTERNAL_SERVER_ERROR, "Unexpected agent response".to_string())),
    }
}

pub async fn run_vm(
    State(state): State<AppState>,
    Json(payload): Json<RunRequest>,
) -> Result<Json<RunResponse>, (StatusCode, String)> {
    info!("Received request to run image: {}", payload.image);

    let request = crate::vm_service::types::VmRunRequest::from(payload);
    let result = crate::vm_service::run_vm(Arc::new(state), request).await;
    
    result.map(|r| Json(crate::api::handlers::RunResponse {
        vm_id: r.vm_id,
        status: r.status,
        ip_address: r.ip_address,
    })).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn stop_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<String, (StatusCode, String)> {
    vm_service_state::stop_vm(&state, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
pub struct CommitRequest {
    new_image_name: String,
}

pub async fn commit_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<CommitRequest>,
) -> Result<String, (StatusCode, String)> {
    info!("Request to commit VM: {} to new image: {}", id, payload.new_image_name);
    
    vm_service_state::commit_vm(&state, &id, &payload.new_image_name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn pause_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<String, (StatusCode, String)> {
    vm_service_state::pause_vm(&state, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn resume_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<String, (StatusCode, String)> {
    vm_service_state::resume_vm(&state, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn snapshot_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<String, (StatusCode, String)> {
    info!("Request to snapshot VM: {}", id);

    let result = vm_service_state::snapshot_vm(&state, &id, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(format!("Snapshot {} created for VM {} at {:?}", result.id, id, result.path))
}

#[derive(Deserialize)]
pub struct RestoreRequest {
    pub snapshot_path: String,
    pub mem_path: String,
    pub cow_path: String,
    pub original_vm_id: String,
}

#[derive(Deserialize)]
pub struct TimeTravelRequest {
    pub vm_id: String,
    pub snapshot_id: String,
}

pub async fn restore_vm(
    State(state): State<AppState>,
    Json(payload): Json<RestoreRequest>,
) -> Result<Json<RunResponse>, (StatusCode, String)> {
    info!(
        "Request to restore VM from snapshot: {}",
        payload.snapshot_path
    );

    let home = dirs::home_dir().ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No home dir".into()))?;
    let vyoma_root = home.join(".vyoma");
    let images_root = vyoma_root.join("images");
    let vms_root = vyoma_root.join("vms");

    // We still assume alpine:latest base for MVP clone
    let safe_image_name = "alpine_latest";
    let image_store_path = images_root.join(&safe_image_name);
    let base_image_file = image_store_path.join("base.ext4");

    // 2. New VM ID & Dir (Clone from Snapshot)
    let vm_id = uuid::Uuid::new_v4().to_string();
    let vm_dir = vms_root.join(&vm_id);
    std::fs::create_dir_all(&vm_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 3. New COW File
    let cow_file = vm_dir.join("diff.cow");

    info!("Restoring disk state from {:?}", payload.cow_path);
    std::fs::copy(&payload.cow_path, &cow_file).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to copy COW file: {}", e),
        )
    })?;

    // We don't create empty cow file anymore
    // StorageManager::create_cow_file(&cow_file, size_mb)...

    // 4. Setup Storage Stack
    let base_loop = StorageManager::setup_loop_device(&base_image_file).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Loop base: {}", e),
        )
    })?;
    let cow_loop = StorageManager::setup_loop_device(&cow_file).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Loop cow: {}", e),
        )
    })?;

        use vyoma_storage::DmManager;
        let dm_name = format!("vyoma-{}", vm_id);
        let dm_manager = DmManager::new().map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("DM init: {}", e))
        })?;
        let base_path = std::path::Path::new(&base_loop);
        let cow_path = std::path::Path::new(&cow_loop);
        let dm_device = dm_manager.create_snapshot(&dm_name, base_path, cow_path)
            .map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("DM create: {}", e))
            })?;
        let dm_path = dm_device.path().to_string_lossy().to_string();

    // 5. Setup Network
    // Ensure bridge exists (idempotent-ish)
    let bridge_name = "ign0";
    let bridge_cidr = "172.16.0.1/24";
    // We skip bridge setup here assuming it's up from previous run, or we should just call it safe.
    // NetworkManager::setup_bridge(bridge_name, bridge_cidr)...

    let tap_name = format!("tap{}", &vm_id[0..8]);
    NetworkManager::setup_tap(&tap_name, bridge_name).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("TAP setup: {}", e),
        )
    })?;

    let random_octet = rand::random::<u8>();
    let safe_octet = std::cmp::max(2, std::cmp::min(254, random_octet));
    let vm_ip = format!("172.16.0.{}", safe_octet);

    // 6. Cloud Hypervisor VMM
    let socket_path = vm_dir.join("ch.sock").to_string_lossy().to_string();
    let mut vmm = VmmManager::new(&socket_path);

    if let Err(e) = vmm.start_daemon(&format!("{}/bin/cloud-hypervisor", state.data_dir), None, false) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to start Cloud Hypervisor: {}", e),
        ));
    }

    // 7. Load Snapshot INSTEAD of Boot Source
    if let Err(e) = vmm
        .load_snapshot(&payload.snapshot_path, &payload.mem_path)
        .await
    {
        let _ = vmm.kill();
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Load snapshot: {}", e),
        ));
    }

    // 8. Add Drives (Must match configuration of snapped VM usually, but we are attaching NEW cow)
    // Firecracker snapshot restoration often involves re-attaching block devices.
    // The device ID "rootfs" must match.
    if let Err(e) = vmm.add_drive("rootfs", &dm_path, true).await {
        let _ = vmm.kill();
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Add drive: {}", e),
        ));
    }

    // 9. Add Network
    if let Err(e) = vmm.add_network_interface("eth0", &tap_name, None).await {
        let _ = vmm.kill();
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Add net: {}", e)));
    }

    // 10. Resume
    if let Err(e) = vmm.resume_instance().await {
        let _ = vmm.kill();
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Resume instance: {}", e),
        ));
    }

    // Success - Store State
    let proxy_tasks = Vec::new(); // No ports restored for now
    let instance = VmInstance {
        vmm,
        id: vm_id.clone(),
        status: VmStatus::Running, // Restored VMs are considered running
        ch_socket_path: socket_path.clone(),
        tap_name: tap_name.clone(),
        dm_name: dm_name.clone(),
        loop_devices: vec![base_loop, cow_loop],
        cow_file_path: cow_file.to_string_lossy().to_string(),
        ip_address: vm_ip.clone(),
        proxy_tasks,

        fs_managers: Vec::new(),
        cgroup_path: {
            let default_vcpu = 2u32;
            let default_mem_mib = 1024u32;
            match crate::vm_service::setup_cgroups(&state, &vm_id, default_vcpu, default_mem_mib) {
                Ok(path) => path,
                Err(e) => {
                    warn!("Failed to setup cgroups for restored VM: {}", e);
                    None
                }
            }
        },
        netns_path: None, // Simplified restore lacks CNI for now
        config_ports: vec![],
        config_volumes: vec![],
        hostname: None,
        labels: HashMap::new(),
        base_image_path: String::new(),
        vcpu: 1,
        mem_size_mib: 512,
        networks: vec![],
        vtpm_manager: None,
        attestation_status: None,
        attestation_task: None,
    };

    {
        let mut vms = state.vms.lock().await;
        vms.insert(vm_id.clone(), Arc::new(TokioMutex::new(instance)));

    // WAL: Log VM creation and start
    if let Err(e) = state.wal.append(&WalEntry::vm_create(vm_id.clone())) {
        error!("Failed to write WAL entry: {}", e);
    }
    if let Err(e) = state.wal.append(&WalEntry::vm_start(vm_id.clone())) {
        error!("Failed to write WAL entry: {}", e);
    }
    }

    Ok(Json(RunResponse {
        vm_id,
        status: "Restored".to_string(),
        ip_address: vm_ip,
    }))
}



pub async fn time_travel_vm(
    State(state): State<AppState>,
    Json(payload): Json<TimeTravelRequest>,
) -> Result<Json<RunResponse>, (StatusCode, String)> {
    info!(
        "Request to time-travel VM {} to snapshot: {}",
        payload.vm_id, payload.snapshot_id
    );

    let tm = state.timemachine.read().await;
    let _snapshot = tm.get_snapshot(&payload.vm_id, &payload.snapshot_id)
        .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".to_string()))?;

    let home = dirs::home_dir().ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No home dir".into()))?;
    let vms_root = home.join(".vyoma").join("vms");
    let source_vm_dir = vms_root.join(&payload.vm_id);
    let snaps_dir = source_vm_dir.join("snapshots").join(&payload.snapshot_id);

    let snapshot_path = snaps_dir.join("snapshot.snap");
    let mem_path = snaps_dir.join("memory.mem");
    let cow_path = snaps_dir.join("diff.cow");

    let restore_req = RestoreRequest {
        snapshot_path: snapshot_path.to_string_lossy().to_string(),
        mem_path: mem_path.to_string_lossy().to_string(),
        cow_path: cow_path.to_string_lossy().to_string(),
        original_vm_id: payload.vm_id.clone(),
    };

    // Forward to existing restore logic
    restore_vm(State(state.clone()), Json(restore_req)).await
}

#[derive(Serialize)]
pub struct HistoryResponse {
    pub snapshots: Vec<crate::timemachine::SnapshotEntry>,
}

pub async fn history_vm(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let tm = state.timemachine.read().await;
    let history = tm.get_snapshot_history(&id).unwrap_or_default();
    Ok(Json(HistoryResponse { snapshots: history }))
}

#[derive(Serialize)]
struct VmSummary {
    id: String,
    status: String,
    ip_address: String,
    hostname: Option<String>,
    labels: HashMap<String, String>,
    attestation_status: Option<String>,

    // For restart
    base_image_path: String,
    vcpu: u32,
    mem_size_mib: u32,
}

#[derive(Serialize)]
pub struct ListResponse {
    vms: Vec<VmSummary>,
}

#[derive(Serialize)]
pub struct PullResponse {
    status: String,
    path: String,
}

#[derive(Deserialize)]
pub struct PullRequest {
    image: String,
}

pub async fn pull_image_handler(
    Json(payload): Json<PullRequest>,
) -> Result<Json<PullResponse>, (StatusCode, String)> {
    info!("Handling Pull request for {}", payload.image);

    let path = ensure_image_locally_handler(&payload.image).await?;

    Ok(Json(PullResponse {
        status: "Image pulled successfully".to_string(),
        path: path.to_string_lossy().to_string(),
    }))
}

pub async fn list_vms(State(state): State<AppState>) -> Json<ListResponse> {
    let instances: Vec<Arc<TokioMutex<VmInstance>>> = {
        let vms_map = state.vms.lock().await;
        vms_map.values().cloned().collect()
    };

    let mut summaries = Vec::new();
    for arc_inst in instances {
        let inst = arc_inst.lock().await;
        summaries.push(VmSummary {
            id: inst.id.clone(),
            status: inst.status.to_string(),
            ip_address: inst.ip_address.clone(),
            hostname: inst.hostname.clone(),
            labels: inst.labels.clone(),
            attestation_status: inst.attestation_status.clone(),
            base_image_path: inst.base_image_path.clone(),
            vcpu: inst.vcpu,
            mem_size_mib: inst.mem_size_mib,
        });
    }

    Json(ListResponse { vms: summaries })
}

pub async fn stream_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    info!("Request to stream logs for VM: {}", id);

    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(&id).cloned()
    };

    if let Some(vm_mutex) = vm_arc {
        let vm = vm_mutex.lock().await;
        let rx = vm.vmm.subscribe_logs();

        let stream = tokio_stream::StreamExt::filter_map(
            BroadcastStream::new(rx),
            |try_msg| {
                match try_msg {
                    Ok(msg) => Some(Ok(Event::default().data(msg))),
                    Err(_) => None,
                }
            },
        );

        Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
    } else {
        Err((StatusCode::NOT_FOUND, "VM not found".to_string()))
    }
}

pub async fn inspect_vm_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<String, (StatusCode, String)> {
    // 1. Check running
    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(&id).cloned()
    };

    if let Some(vm_mutex) = vm_arc {
        let vm = vm_mutex.lock().await;
        // ... build json ...
        let (status_str, error_reason) = match &vm.status {
            VmStatus::PendingAttestation => ("pending_attestation", None),
            VmStatus::Running => ("running", None),
            VmStatus::AttestationFailed { reason } => ("attestation_failed", Some(reason.clone())),
            VmStatus::Error { reason } => ("error", Some(reason.clone())),
        };

        let val = serde_json::json!({
            "id": vm.id,
            "status": status_str,
            "error_reason": error_reason,
            "tap_name": vm.tap_name,
            "dm_name": vm.dm_name,
            "loop_devices": vm.loop_devices,
            "cow_file_path": vm.cow_file_path,
            "ip_address": vm.ip_address,
            "cgroup_path": vm.cgroup_path,
            "netns_path": vm.netns_path,
            "ports": vm.config_ports,
            "volumes": vm.config_volumes,
            "hostname": vm.hostname,
            "labels": vm.labels,
            "base_image_path": vm.base_image_path,
            "vcpu": vm.vcpu,
            "mem_size_mib": vm.mem_size_mib,
        });
        return Ok(val.to_string());
    }

    // 2. Check disk (stopped)
    let home = dirs::home_dir().ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No home dir".into()))?;
    let state_file = home
        .join(".vyoma")
        .join("vms")
        .join(&id)
        .join("state.json");
    if state_file.exists() {
        let f = std::fs::File::open(state_file)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let s: serde_json::Value = serde_json::from_reader(f)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return Ok(s.to_string());
    }

    Err((StatusCode::NOT_FOUND, "VM not found".to_string()))
}

pub async fn build_image(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    body: Body,
) -> Result<String, (StatusCode, String)> {
    info!("Received build request using VM-isolated build system");

    let measured = params.get("measured").map(|v| v == "true").unwrap_or(false);
    if measured {
        info!("Measured build requested via query parameter");
    }

    // 1. Stream body to a temp file (tar.gz)
    let temp_dir =
        tempfile::tempdir().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let tar_path = temp_dir.path().join("context.tar.gz");

    {
        // Convert Body stream to AsyncRead
        use futures::StreamExt;
        let stream = body
            .into_data_stream()
            .map(|b| b.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
        let mut reader = StreamReader::new(stream);
        let mut file = tokio::fs::File::create(&tar_path)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        tokio::io::copy(&mut reader, &mut file)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    // 2. Unpack context
    let tar_file = std::fs::File::open(&tar_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let decoder = GzDecoder::new(tar_file);
    let mut archive = Archive::new(decoder);
    let context_dir = temp_dir.path().join("context");
    std::fs::create_dir(&context_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    archive.unpack(&context_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unpack failed: {}", e),
        )
    })?;

    // 3. Parse Vyomafile
    let vyomafile_path = context_dir.join("Vyomafile");
    if !vyomafile_path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Vyomafile not found in build context".to_string(),
        ));
    }

    let vyomafile = vyoma_build::Vyomafile::parse(&vyomafile_path)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Failed to parse Vyomafile: {}", e)))?;

    // Check if Vyomafile requests measured boot
    let vyomafile_measured = vyomafile.has_measured_boot();
    if vyomafile_measured {
        info!("Measured build requested via VM_MEASURED_BOOT directive in Vyomafile");
    }

    // Measured build is enabled if either query parameter or Vyomafile directive specifies it
    let measured = measured || vyomafile_measured;

    // 4. Generate build ID and prepare work directory
    let build_id = uuid::Uuid::new_v4().to_string();
    let home = dirs::home_dir().ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No home dir".into()))?;
    let work_dir = home.join(".vyoma").join("builds");
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 5. Determine signing key path from policy config
    let signing_key_path = {
        let policy = state.policy_manager.lock().unwrap();
        policy.get_config().measured_boot.build_signing_key_path.clone()
    };

    // 6. Execute build using VM-isolated BuildRunner
    let mut build_runner = vyoma_build::BuildRunner::new(work_dir.clone())
        .with_measured(measured, signing_key_path)
        .with_cgroups(Arc::clone(&state.cgroups));
    let build_result = build_runner
        .build(&vyomafile_path, &context_dir, &build_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Build failed: {:?}", e),
            )
        })?;

    info!(
        "Build completed successfully: {} -> {}",
        build_id,
        build_result.rootfs_path.display()
    );

    // 7. Return the build ID
    Ok(build_id)
}

pub async fn start_process_monitor(state: AppState) {
    tokio::spawn(async move {
        // Check every 5 seconds
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));

        loop {
            interval.tick().await;

            let ids: Vec<String> = {
                let map = state.vms.lock().await;
                map.keys().cloned().collect()
            };

            for id in ids {
                // Get VM Arc
                let vm_arc = {
                    let map = state.vms.lock().await;
                    map.get(&id).cloned()
                };

                if let Some(vm_mutex) = vm_arc {
                    let mut vm = vm_mutex.lock().await;

                    // 1. Check Cloud Hypervisor
                    match vm.vmm.try_wait() {
                        Ok(Some(status)) => {
                            let mut msg = format!(
                                "Monitor: VM {} Cloud Hypervisor process EXITED (Reaped): {}",
                                id, status
                            );

                            // Check for OOM
                            if let Ok(count) = state.cgroups.get_oom_kill_count(&id) {
                                if count > 0 {
                                    msg.push_str(&format!(
                                        " [WARNING: OOM Kill Detected: {}]",
                                        count
                                    ));
                                }
                            }
                            error!("{}", msg);
                        }
                        Ok(None) => {
                            // Still running
                        }
                        Err(e) => {
                            error!("Monitor: VM {} wait error: {}", id, e);
                        }
                    }

                    // 2. Check VirtioFS
                    let fs_count = vm.fs_managers.len();
                    for idx in 0..fs_count {
                        // Check status with scoped mutable borrow
                        let exit_status = {
                            let fs = &mut vm.fs_managers[idx];
                            match fs.try_wait() {
                                Ok(Some(s)) => Some(s),
                                Err(e) => {
                                    error!("Monitor Check Error for FS {}: {}", id, e);
                                    None
                                }
                                Ok(None) => None,
                            }
                        };

                        if let Some(status) = exit_status {
                            warn!(
                                "Monitor: VM {} VirtioFS (vol{}) EXITED with {}. Restarting...",
                                id, idx, status
                            );

                            // Get Config (Immutable borrow OK now)
                            let host_path = vm.config_volumes.get(idx).map(|v| v.host_path.clone());

                            if let Some(path) = host_path {
                                let fs = &mut vm.fs_managers[idx]; // Re-borrow mutably
                                if let Err(e) = fs.start(&path) {
                                    error!(
                                        "Monitor: FAILED to auto-restart VirtioFS for VM {}: {}",
                                        id, e
                                    );
                                } else {
                                    info!("Monitor: RESTARTED VirtioFS for VM {} (vol{})", id, idx);
                                }
                            } else {
                                error!(
                                    "Monitor: Cannot restart VirtioFS (vol{}), config not found.",
                                    idx
                                );
                            }
                        }
                    }
                }
            }
        }
    });
}

#[derive(Deserialize)]
pub struct CreateNetworkRequest {
    name: String,
    subnet: String,
    #[serde(default = "default_driver")]
    driver: String,
}

fn default_driver() -> String {
    "bridge".to_string()
}

pub async fn list_networks_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    state
        .cni_manager
        .list_networks()
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn create_network_handler(
    State(state): State<AppState>,
    Json(payload): Json<CreateNetworkRequest>,
) -> Result<String, (StatusCode, String)> {
    match payload.driver.as_str() {
        "bridge" => state
            .cni_manager
            .create_network(&payload.name, &payload.subnet)
            .map(|_| format!("Network {} created", payload.name))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        "overlay" => state
            .cni_manager
            .create_overlay_network(&payload.name, &payload.subnet)
            .map(|_| format!("Overlay Network {} created", payload.name))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("Unknown driver: {}", payload.driver),
        )),
    }
}

pub async fn delete_network_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<String, (StatusCode, String)> {
    state
        .cni_manager
        .delete_network(&name)
        .map(|_| format!("Network {} deleted", name))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
pub struct JoinRequest {
    pub node_id: u64,
    pub addr: String,
    pub public_key: String,
    pub wireguard_key: Option<String>,
    pub wireguard_port: Option<u16>,
}

#[derive(Serialize)]
pub struct InitResponse {
    pub node_id: u64,
    pub subnet_id: u8,
    pub wireguard_port: Option<u16>,
    pub wireguard_key: Option<String>,
}

#[derive(Serialize)]
pub struct JoinResponse {
    pub node_id: u64,
    pub subnet_id: u8,
    pub peers: Vec<swarm::NodeInfo>,
}

fn get_outbound_ip() -> String {
    match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => match s.connect("8.8.8.8:80") {
            Ok(_) => s.local_addr().map(|a| a.ip().to_string()).unwrap_or("127.0.0.1".to_string()),
            Err(_) => "127.0.0.1".to_string(),
        },
        Err(_) => "127.0.0.1".to_string(),
    }
}

pub async fn swarm_init_handler(State(state): State<AppState>) -> Result<Json<InitResponse>, (StatusCode, String)> {
    let node_id = 1u64;
    let addr = format!("{}:7946", get_outbound_ip());
    
    let mut swarm_raft = state.swarm_raft.lock().await;
    swarm_raft.bootstrap(addr, format!("init_key_{}", node_id), None, None)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("SwarmRaft Error: {}", e)))?;
    
    let subnet_id = ((node_id % 254) + 1) as u8;
    
    info!("Swarm initialized - Node ID: {}, Subnet: 10.42.{}.0/24", node_id, subnet_id);
    
    Ok(Json(InitResponse {
        node_id,
        subnet_id,
        wireguard_port: None,
        wireguard_key: None,
    }))
}

pub async fn swarm_join_handler(
    State(state): State<AppState>,
    Json(payload): Json<JoinRequest>,
) -> Result<Json<JoinResponse>, (StatusCode, String)> {
    let client = reqwest::Client::new();
    
    let leader_addr = payload.addr.clone();
    let join_url = format!("http://{}/swarm/join-propose", leader_addr);
    
    let req = serde_json::json!({
        "node_id": payload.node_id,
        "addr": payload.addr,
        "public_key": payload.public_key,
        "wireguard_key": payload.wireguard_key,
        "wireguard_port": payload.wireguard_port,
    });
    
    let response = client.post(&join_url)
        .json(&req)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to contact leader: {}", e)))?;
    
    if !response.status().is_success() {
        return Err((StatusCode::BAD_REQUEST, format!("Join rejected: {}", response.text().await.unwrap_or_default())));
    }
    
    let subnet_id = ((payload.node_id % 254) + 1) as u8;
    
    {
        let mut swarm_raft = state.swarm_raft.lock().await;
        if !swarm_raft.is_initialized() {
            let addr = format!("{}:7946", get_outbound_ip());
            swarm_raft.bootstrap(addr, format!("init_key_{}", payload.node_id), None, None)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        }
        swarm_raft.add_node(
            payload.node_id,
            payload.addr,
            payload.public_key,
            payload.wireguard_key.clone(),
            payload.wireguard_port,
        ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to add node locally: {}", e)))?;
    }
    
    let peers: Vec<swarm::NodeInfo> = {
        let swarm_raft = state.swarm_raft.lock().await;
        swarm_raft.get_nodes().iter().map(|n| (*n).clone()).collect()
    };
    
    info!("Node {} joined swarm - Subnet: 10.42.{}.0/24, Peers: {}", payload.node_id, subnet_id, peers.len());
    
    Ok(Json(JoinResponse {
        node_id: payload.node_id,
        subnet_id,
        peers,
    }))
}

#[derive(Serialize)]
pub struct SwarmNodeInfo {
    pub id: u64,
    pub addr: String,
    pub public_key: String,
    pub wireguard_key: Option<String>,
    pub wireguard_port: Option<u16>,
    pub subnet_id: Option<u8>,
    pub is_leader: bool,
}

pub async fn swarm_nodes_handler(State(state): State<AppState>) -> Json<Vec<SwarmNodeInfo>> {
    let swarm_raft = state.swarm_raft.lock().await;
    
    let nodes: Vec<SwarmNodeInfo> = swarm_raft.get_nodes().iter().map(|n| {
        SwarmNodeInfo {
            id: n.id,
            addr: n.addr.clone(),
            public_key: n.public_key.clone(),
            wireguard_key: n.wireguard_key.clone(),
            wireguard_port: n.wireguard_port,
            subnet_id: n.subnet_id,
            is_leader: n.is_leader,
        }
    }).collect();
    
    Json(nodes)
}

#[derive(Serialize)]
pub struct RegisterResponse {
    pub node_id: u64,
    pub subnet_id: u8,
    pub peers: Vec<SwarmNodeInfo>,
}

pub async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events_tx.subscribe();
    let stream = BroadcastStream::new(rx);

    Sse::new(tokio_stream::StreamExt::filter_map(stream, |msg| {
            match msg {
                Ok(data) => Some(Ok(Event::default().data(data))),
                Err(_) => Some(Ok(Event::default().comment("missed message"))),
            }
        })
    )
    .keep_alive(axum::response::sse::KeepAlive::default())
}

// --- Extended API Handlers for Web UI ---

pub async fn list_images_handler() -> Json<Vec<String>> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let images_root = home.join(".vyoma").join("images");
    let mut images = Vec::new();
    if let Ok(entries) = std::fs::read_dir(images_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                 if let Ok(name) = entry.file_name().into_string() {
                     images.push(name);
                 }
            }
        }
    }
    Json(images)
}



#[derive(Serialize)]
pub struct VolumeInfo {
    name: String,
    path: String,
}

pub async fn list_volumes_handler() -> Json<Vec<VolumeInfo>> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let vol_root = home.join(".vyoma").join("volumes");
    let mut vols = Vec::new();
    if let Ok(entries) = std::fs::read_dir(vol_root) {
        for entry in entries.flatten() {
             if let Ok(name) = entry.file_name().into_string() {
                  vols.push(VolumeInfo { name: name, path: entry.path().to_string_lossy().to_string() });
             }
        }
    }
    Json(vols)
}

#[derive(Deserialize)]
pub struct TeleportRequest {
    pub vm_id: String,
    pub target_node_ip: String,
    pub bandwidth_mbps: Option<u32>,
}

#[derive(Serialize)]
pub struct TeleportResponse {
    pub session_id: String,
    pub status: String,
    pub message: String,
}

#[derive(Serialize, Deserialize)]
pub struct AdoptTeleportRequest {
    pub vm_id: String,
}

pub async fn adopt_teleported_vm(
    State(state): State<AppState>,
    Json(payload): Json<AdoptTeleportRequest>,
) -> Result<Json<TeleportResponse>, (StatusCode, String)> {
    info!("Handling POST /adopt-teleported-vm for VM {}", payload.vm_id);

    let home = dirs::home_dir().ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No home dir".into()))?;
    let vm_dir = home.join(".vyoma").join("vms").join(&payload.vm_id);

    if !vm_dir.exists() {
        return Err((StatusCode::NOT_FOUND, format!("VM directory not found for {}", payload.vm_id)));
    }

    let ch_socket = vm_dir.join("ch.sock").to_string_lossy().to_string();

    // Query the VM info from the now-running Cloud Hypervisor
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .unix_socket(ch_socket.as_str())
        .build()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to build socket client: {}", e)))?;

    let response = client
        .request(Method::GET, "http://localhost/api/v1/vm.info")
        .send()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to query vm.info: {}", e)))?;

    let vm_info: VmInfo = response
        .json()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse vm.info: {}", e)))?;

    // Ensure VM is running
    if vm_info.state != "Running" {
        // Try to resume it
        client
            .request(Method::PUT, "http://localhost/api/v1/vm.resume")
            .send()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to resume VM: {}", e)))?;
    }

    // Register the adopted VM in the daemon state
    let vmm = VmmManager::new(&ch_socket);
    let instance = Arc::new(TokioMutex::new(VmInstance {
        vmm,
        id: payload.vm_id.clone(),
        status: VmStatus::Running, // Adopted VMs are considered running
        attestation_status: None,
        ch_socket_path: ch_socket,
        tap_name: String::new(),
        dm_name: String::new(),
        loop_devices: Vec::new(),
        cow_file_path: String::new(),
        ip_address: String::new(),
        proxy_tasks: Vec::new(),
        fs_managers: Vec::new(),
        cgroup_path: None,
        netns_path: None,
        config_ports: Vec::new(),
        config_volumes: Vec::new(),
        hostname: None,
        labels: HashMap::new(),
        networks: Vec::new(),
        base_image_path: String::new(),
        vcpu: 1,
        mem_size_mib: 512,
        vtpm_manager: None,
        attestation_task: None,
    }));

    {
        let mut vms = state.vms.lock().await;
        vms.insert(payload.vm_id.clone(), instance);
    }

    info!("Adopted VM {} into local state", payload.vm_id);

    Ok(Json(TeleportResponse {
        session_id: payload.vm_id.clone(),
        status: "adopted".to_string(),
        message: format!("VM {} adopted successfully", payload.vm_id),
    }))
}

pub async fn teleport_handler(
    State(state): State<AppState>,
    Json(payload): Json<TeleportRequest>,
) -> Result<Json<TeleportResponse>, (StatusCode, String)> {
    info!("Handling POST /teleport for VM {} to {}", payload.vm_id, payload.target_node_ip);

    let session_id = uuid::Uuid::new_v4().to_string();
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let migration_session = MigrationSession {
        session_id: session_id.clone(),
        vm_id: payload.vm_id.clone(),
        source_node: "local".to_string(),
        target_node: payload.target_node_ip.clone(),
        status: "initiating".to_string(),
        progress: None,
        started_at,
        completed_at: None,
    };

    {
        let sessions = get_migration_sessions();
        let mut sessions = sessions.lock().unwrap();
        sessions.insert(session_id.clone(), migration_session.clone());
    }

    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(&payload.vm_id).cloned()
    };

    if let Some(vm_mutex) = vm_arc {
        let (ch_socket, mem_size_mib_for_spawn) = {
            let vm = vm_mutex.lock().await;
            (vm.ch_socket_path.clone(), vm.mem_size_mib)
        };
        let target_url = payload.target_node_ip.clone();
        let vms_for_cleanup = Arc::clone(&state.vms);

        // Get local WireGuard IP for trusted source restriction
        let local_wg_ip = state.network_integration
            .lock()
            .await
            .as_ref()
            .and_then(|ni| ni.get_local_wireguard_ip());
        
        let trusted_ips = local_wg_ip.map(|ip| vec![ip.to_string()]);

        let http_client = reqwest::Client::new();
        let target_api = format!("http://{}:3000/receive-teleport", target_url);

        info!("Telling target node at {} to prepare for reception (trusted IPs: {:?})...", target_api, trusted_ips);

        let res = http_client.post(&target_api)
            .json(&ReceiveTeleportRequest {
                vm_id: payload.vm_id.clone(),
                trusted_source_ips: trusted_ips,
            })
            .send()
            .await;

        if let Err(e) = res {
            {
                let sessions = get_migration_sessions();
                let mut sessions = sessions.lock().unwrap();
                if let Some(s) = sessions.get_mut(&session_id) {
                    s.status = "failed".to_string();
                }
            }
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Target node unreachable: {}", e)));
        } else if let Ok(resp) = res {
            if !resp.status().is_success() {
                {
                    let sessions = get_migration_sessions();
                    let mut sessions = sessions.lock().unwrap();
                    if let Some(s) = sessions.get_mut(&session_id) {
                        s.status = "failed".to_string();
                    }
                }
                return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Target node refused: {:?}", resp.text().await)));
            }
        }

        let bandwidth = payload.bandwidth_mbps;
        let session_id_clone = session_id.clone();

        let progress_callback = Box::new(move |progress: TeleportProgress| {
            let sessions = get_migration_sessions();
            if let Ok(mut sessions) = sessions.lock() {
                if let Some(session) = sessions.get_mut(&session_id_clone) {
                    session.status = progress.status.clone();
                    session.progress = Some(progress);
                }
            }
        });

        let ch_socket_for_spawn = ch_socket.clone();
        let session_id_for_spawn = session_id.clone();
        let target_url_for_spawn = target_url.clone();
        let adopt_url_for_spawn = format!("http://{}:3000/adopt-teleported-vm", target_url_for_spawn);
        let vm_id_for_cleanup = payload.vm_id.clone();
        let vms_for_cleanup = vms_for_cleanup.clone();

        tokio::spawn(async move {
            let teleporter = vyoma_teleport::sender::Teleporter::new(
                payload.vm_id.clone(),
                target_url_for_spawn,
                mem_size_mib_for_spawn as u64,
            );

            let result = teleporter.teleport_vm_with_config(
                &ch_socket_for_spawn,
                bandwidth,
                Some(progress_callback),
            ).await;

            if result.is_ok() {
                 // Stop the source VM after successful migration
                 {
                     let vm_arc_opt = {
                         let vms = vms_for_cleanup.lock().await;
                         vms.get(&vm_id_for_cleanup).cloned()
                     };

                    if let Some(vm_arc) = vm_arc_opt {
                        let mut vm = vm_arc.lock().await;
                        if let Err(e) = vm.vmm.pause_instance().await {
                            error!("Failed to pause source VM {}: {}", vm_id_for_cleanup, e);
                        }
                        info!("Source VM {} paused after successful live migration", vm_id_for_cleanup);
                    }
                }

                 // Remove from registry
                 if let Some(_vm_arc) = vms_for_cleanup.lock().await.remove(&vm_id_for_cleanup) {
                     info!("Source VM {} removed from registry after migration", vm_id_for_cleanup);
                 }

                // Notify target node to adopt the VM
                let adopt_resp = reqwest::Client::new()
                    .post(&adopt_url_for_spawn)
                    .json(&AdoptTeleportRequest { vm_id: vm_id_for_cleanup.clone() })
                    .send()
                    .await;

                match adopt_resp {
                    Ok(r) if r.status().is_success() => {
                        info!("Target node adopted VM {}", vm_id_for_cleanup);
                    }
                    Ok(r) => {
                        warn!("Target adoption returned non-success: {}", r.status());
                    }
                    Err(e) => {
                        warn!("Failed to notify target to adopt VM {}: {}", vm_id_for_cleanup, e);
                    }
                }

                let sessions = get_migration_sessions();
                if let Ok(mut sessions) = sessions.lock() {
                    if let Some(session) = sessions.get_mut(&session_id_for_spawn) {
                        session.status = "completed".to_string();
                        session.completed_at = Some(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0)
                        );
                    }
                }
            } else {
                 // Resume source VM on migration failure
                 {
                     let vm_arc_opt = {
                         let vms = vms_for_cleanup.lock().await;
                         vms.get(&vm_id_for_cleanup).cloned()
                     };

                    if let Some(vm_arc) = vm_arc_opt {
                        let mut vm = vm_arc.lock().await;
                        if let Err(e) = vm.vmm.resume_instance().await {
                            error!("Failed to resume source VM {}: {}", vm_id_for_cleanup, e);
                        } else {
                            info!("Source VM {} resumed after migration failure", vm_id_for_cleanup);
                        }
                    }
                }

                let sessions = get_migration_sessions();
                if let Ok(mut sessions) = sessions.lock() {
                    if let Some(session) = sessions.get_mut(&session_id_for_spawn) {
                        session.status = "failed".to_string();
                    }
                }
            }

            match result {
                Ok(_) => info!("Teleportation of VM {} succeeded!", payload.vm_id),
                Err(e) => error!("Teleportation of VM {} failed: {}", payload.vm_id, e),
            }
        });

        {
            let sessions = get_migration_sessions();
            let mut sessions = sessions.lock().unwrap();
            if let Some(s) = sessions.get_mut(&session_id) {
                s.status = "in_progress".to_string();
            }
        }

        Ok(Json(TeleportResponse {
            session_id,
            status: "in_progress".to_string(),
            message: "Live migration initiated".to_string(),
        }))
    } else {
        {
            let sessions = get_migration_sessions();
            let mut sessions = sessions.lock().unwrap();
            if let Some(s) = sessions.get_mut(&session_id) {
                s.status = "failed".to_string();
            }
        }
        Err((StatusCode::NOT_FOUND, "VM not found".to_string()))
    }
}

#[derive(Serialize, Deserialize)]
pub struct ReceiveTeleportRequest {
    pub vm_id: String,
    #[serde(default)]
    pub trusted_source_ips: Option<Vec<String>>,
}

pub async fn receive_teleport_handler(
    State(state): State<AppState>,
    Json(payload): Json<ReceiveTeleportRequest>,
) -> Result<String, (StatusCode, String)> {
    info!("Handling POST /receive-teleport for VM {}", payload.vm_id);
    
    // Warn if no trusted source IPs provided (insecure mode)
    if payload.trusted_source_ips.is_none() {
        warn!("Receiving teleport for VM {} WITHOUT trusted source IPs; migration port is open to any host!", payload.vm_id);
    } else {
        info!("Receiving teleport with trusted source IPs: {:?}", payload.trusted_source_ips);
    }
    
    // Create directories
    let home = dirs::home_dir().ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No home dir".into()))?;
    let vm_dir = home.join(".vyoma").join("vms").join(&payload.vm_id);
    std::fs::create_dir_all(&vm_dir).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    
    let ch_socket = vm_dir.join("ch.sock").to_string_lossy().to_string();
    let mut vmm = VmmManager::new(&ch_socket);

    // Start Cloud Hypervisor
    if let Err(e) = vmm.start_daemon(&format!("{}/bin/cloud-hypervisor", state.data_dir), None, false) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to start Cloud Hypervisor: {}", e),
        ));
    }

    // Call Receiver with trusted source IPs
    let receiver = vyoma_teleport::receiver::TeleportReceiver::new(PathBuf::new(), PathBuf::new(), "0.0.0.0".to_string());
    if let Err(e) = receiver.start_receiving_with_config(&ch_socket, payload.trusted_source_ips.clone()).await {
        let _ = vmm.kill();
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to set receive-migration mode: {}", e)));
    }

    // We leave it running. Sender will connect to port 9000 and it will resume automatically.
    // The VM state should probably be adopted by the daemon after migration completes.
    // In a complete implementation, we would wait for it and insert into `state.vms`.
    
    Ok(format!("Cloud Hypervisor ready to receive VM {} on TCP port 9000 (trusted sources: {:?})", payload.vm_id, payload.trusted_source_ips))
}

#[derive(Serialize)]
pub struct TeleportStatusResponse {
    pub session_id: String,
    pub vm_id: String,
    pub source_node: String,
    pub target_node: String,
    pub status: String,
    pub progress: Option<TeleportProgress>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
}

pub async fn teleport_status_handler(
    State(_state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<TeleportStatusResponse>, (StatusCode, String)> {
    let sessions = get_migration_sessions();
    let sessions = sessions.lock().unwrap();

    if let Some(session) = sessions.get(&session_id) {
        Ok(Json(TeleportStatusResponse {
            session_id: session.session_id.clone(),
            vm_id: session.vm_id.clone(),
            source_node: session.source_node.clone(),
            target_node: session.target_node.clone(),
            status: session.status.clone(),
            progress: session.progress.clone(),
            started_at: session.started_at,
            completed_at: session.completed_at,
        }))
    } else {
        Err((StatusCode::NOT_FOUND, "Session not found".to_string()))
    }
}

#[derive(Deserialize)]
pub struct PolicyRequest {
    pub policy: String,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct PolicyResponse {
    pub policy: String,
    pub enabled: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestResponse {
    pub vm_id: String,
    pub verified: bool,
    pub pcr_results: Vec<PcrResult>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PcrResult {
    pub pcr_index: u32,
    pub expected: String,
    pub actual: String,
    pub verified: bool,
}

pub async fn attest_vm_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AttestResponse>, (StatusCode, String)> {
    info!("Attestation request for VM {}", id);

    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(&id).cloned()
    };

    let vm_arc = match vm_arc {
        Some(arc) => arc,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("VM {} not found", id),
            ));
        }
    };

    let tpm_socket = {
        let vm = vm_arc.lock().await;
        vm.vtpm_manager.as_ref().map(|vtpm| vtpm.socket_path().to_string())
    };

    let tpm_socket = match tpm_socket {
        Some(s) => s,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("VM {} has no vTPM - measured boot not enabled", id),
            ));
        }
    };

    // Build attestation config from policy (same as boot-time does)
    let config = crate::vm_service::policy::build_attestation_config(&Arc::new(state.clone()));
    let image_name = resolve_image_name_from_path(&vm_arc.lock().await.base_image_path);

    // Call the shared verification function
    let result = crate::vm_service::policy::verify_attestation_from_manifest(
        &image_name,
        &tpm_socket,
        &config,
    ).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Map policy PcrResult to handler PcrResult
    let pcr_results = result.pcr_results.into_iter().map(|pcr| PcrResult {
        pcr_index: pcr.pcr_index,
        expected: pcr.expected,
        actual: pcr.actual,
        verified: pcr.verified,
    }).collect();

    let verified = result.verified;

    info!(
        "Attestation for VM {}: {}",
        id,
        if verified { "VERIFIED" } else { "FAILED" }
    );

    // Update the VM instance's attestation status
    {
        let mut vm = vm_arc.lock().await;
        vm.attestation_status = Some(if verified {
            "VERIFIED".to_string()
        } else {
            "FAILED".to_string()
        });
    }

    Ok(Json(AttestResponse {
        vm_id: id,
        verified,
        pcr_results,
        error: result.error,
    }))
}

fn resolve_image_name_from_path(base_image_path: &str) -> String {
    use std::path::Path;
    if let Some(path) = Path::new(base_image_path).parent() {
        if let Some(name) = path.file_name() {
            return name.to_string_lossy().to_string();
        }
    }
    base_image_path.to_string()
}

pub async fn set_policy_handler(
    State(state): State<AppState>,
    Json(payload): Json<PolicyRequest>,
) -> Result<Json<PolicyResponse>, (StatusCode, String)> {
    info!("Setting policy: {} to {}", payload.policy, payload.enabled);

    match payload.policy.as_str() {
        "require-measured-boot" => {
            let mut policy = state.policy_manager.lock().unwrap();
            policy.set_require_measured_boot(payload.enabled);
            let message = if payload.enabled {
                "Measured boot policy enabled. VMs will require TPM attestation."
            } else {
                "Measured boot policy disabled."
            };
            Ok(Json(PolicyResponse {
                policy: payload.policy,
                enabled: payload.enabled,
                message: message.to_string(),
            }))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("Unknown policy: {}", payload.policy),
        )),
    }
}

pub async fn get_policy_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<PolicyResponse>>, (StatusCode, String)> {
    let policy = state.policy_manager.lock().unwrap();
    let config = policy.get_config();
    let status = PolicyStatus::from_config(config);

    let responses: Vec<PolicyResponse> = status.into_iter().map(|s| PolicyResponse {
        policy: s.policy_name,
        enabled: s.enabled,
        message: if s.enforced { "enforced" } else { "optional" }.to_string(),
    }).collect();

    Ok(Json(responses))
}
