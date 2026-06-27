use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::Colorize;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use futures::stream::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use tar::Archive; // Removed Builder since we will use tar::Builder inline
use tracing::{error, info, warn};

use vyoma_compose::VyomaCompose;
use vyoma_core::api::PortMapping;
use vyoma_core::api::VolumeMount;
use vyoma_image::signing::TrustPolicy;
use vyoma_image::SignedManifest;

/// Resolve socket path with fallback to user-writable location
fn resolve_socket_path(default_path: &str) -> String {
    // Try default path first
    if Path::new(default_path).exists() {
        return default_path.to_string();
    }
    
    // Fallback to user-specific runtime directory
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let user_socket = format!("{}/vyoma.sock", xdg);
        if Path::new(&user_socket).exists() {
            return user_socket;
        }
    }
    
    // Fallback to /tmp (for development)
    let tmp_socket = format!("{}/vyoma.sock", std::env::temp_dir().display());
    if Path::new(&tmp_socket).exists() {
        return tmp_socket;
    }
    
    // Return default path anyway - let it fail with clear error
    default_path.to_string()
}

#[derive(Parser)]
#[command(name = "vyoma")]
#[command(about = "Vyoma: Docker for Micro-VMs", long_about = None)]
struct Cli {
    /// Socket path to daemon (Unix Socket)
    #[arg(short, long, global = true, default_value = "/run/vyoma/vyoma.sock")]
    socket_path: String,

    /// HTTP port of daemon (for HTTP requests through Unix Socket)
    #[arg(short = 'p', long, global = true, default_value_t = 8080)]
    http_port: u16,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Pull an image from Docker Hub
    Pull {
        /// Image to pull (e.g. ubuntu:latest)
        image: String,
    },
    /// Run a new VM
    Run {
        /// Image to run (e.g. ubuntu:latest)
        image: String,

        /// vCPUs (default: 1)
        #[arg(long, default_value = "1")]
        vcpu: u32,

        /// Memory in MiB (default: 512)
        #[arg(long, default_value = "512")]
        memory: u32,

        /// Port mappings (e.g. -p 8080:80)
        #[arg(short, long)]
        ports: Vec<String>,

        /// Volume mounts (e.g. -v /home/user/app:/app)
        #[arg(short, long)]
        volumes: Vec<String>,

        /// Hostname for the VM
        #[arg(long)]
        hostname: Option<String>,

        /// Labels (key=value)
        #[arg(short = 'l', long)]
        labels: Vec<String>,
    },
    /// Stop a VM
    Stop {
        /// VM ID
        id: String,
    },
    /// Pause a VM
    Pause {
        /// VM ID
        id: String,
    },
    /// Resume a VM
    Resume {
        /// VM ID
        id: String,
    },
    /// Commit an active VM into a new image
    Commit {
        /// VM ID or Name
        id: String,
        /// New image name (e.g., custom-img:latest)
        new_image_name: String,
    },
     /// List active VMs
     Ps,
      /// Show snapshot history of a VM
      History {
          /// VM ID
          id: String,
      },
      /// Create a snapshot of a VM
      Snapshot {
          /// VM ID
          id: String,
      },
     /// Time travel a VM to a specific snapshot
     TimeTravel {
         /// VM ID
         id: String,
         /// Snapshot ID to travel to
         #[arg(long)]
         to: String,
     },
    /// Save a VM snapshot to a file (Export/Teleportation)
    Save {
        /// VM ID to save
        id: String,
        /// Output file path (e.g. my-vm.tar.gz)
        output: String,
    },
    /// Load a VM from a file (Import/Teleportation)
    Load {
        /// Input file path (e.g. my-vm.tar.gz)
        input: String,
    },
    /// Live Teleport a VM to another Swarm node
    Teleport {
        /// VM ID to teleport
        id: String,
        /// Target Node IP (must expose gRPC on 7071)
        target: String,
        /// Bandwidth limit in Mbps (optional)
        #[arg(long)]
        bandwidth: Option<u32>,
        /// Show live progress bar
        #[arg(long, default_value = "true")]
        progress: bool,
    },
    /// Stream logs from a VM
    Logs {
        /// VM ID
        id: String,
        /// Follow log output
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Build a new image from an Vyomafile
    Build {
        /// Path to build context (directory containing Vyomafile)
        #[arg(default_value = ".")]
        path: String,

        /// Perform measured build: launch ephemeral VM to pre-compute PCR values
        #[arg(long)]
        measured: bool,
    },
    /// Check system dependencies and environment health
    Doctor,
    /// Manage networks
    Network {
        #[command(subcommand)]
        command: NetworkCommands,
    },
    /// Create and start resources from a compose file
    Up {
        /// Path to compose file (default: vyoma-compose.yml)
        #[arg(short, long, default_value = "vyoma-compose.yml")]
        file: String,

        /// Detached mode: Run containers in the background
        #[arg(short, long)]
        detach: bool,
    },
    /// Stop and remove resources
    Down {
        /// Path to compose file (default: vyoma-compose.yml)
        #[arg(short, long, default_value = "vyoma-compose.yml")]
        file: String,
    },
    /// Execute a command in a VM (via SSH)
    Exec {
        /// VM ID or Name
        id: String,
        /// Command to run
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },
    /// Restart a VM (Replaces it)
    Restart {
        /// VM ID or Name
        id: String,
    },
    /// Scale services (e.g. web=3)
    Scale {
        /// Scaling arguments (service=count)
        replicas: Vec<String>,

        /// Path to compose file (default: vyoma-compose.yml)
        #[arg(short, long, default_value = "vyoma-compose.yml")]
        file: String,
    },
    /// Manage Swarm/Cluster
    Swarm {
        #[command(subcommand)]
        command: SwarmCommands,
    },
    /// Attest a VM using TPM quote verification
    Attest {
        /// VM ID to attest
        id: String,

        /// Path to signed VMIF manifest (auto-discovered if omitted)
        #[arg(long)]
        manifest: Option<String>,

        /// Trust policy keys directory for manifest signature verification
        #[arg(long)]
        trust_keys: Option<String>,

        /// Skip manifest signature verification (use with caution)
        #[arg(long)]
        no_verify_signature: bool,

        /// Optional nonce to prevent replay attacks
        #[arg(long)]
        nonce: Option<String>,

        /// Output format: human (default) or json
        #[arg(long, default_value = "human")]
        format: String,
    },
    /// Show real-time stats for a VM
    Stats {
        /// VM ID or Name
        id: String,
        /// Refresh interval in seconds (default: 1)
        #[arg(short, long, default_value = "1")]
        interval: u64,
        /// Run once and exit (no live updates)
        #[arg(long)]
        no_stream: bool,
    },
}

#[derive(Subcommand)]
enum SwarmCommands {
    /// Initialize a new swarm
    Init,
    /// Join an existing swarm
    Join {
        /// IP address of a node in the swarm
        ip: String,
    },
    /// List nodes in the swarm
    Ls,
}

#[derive(Subcommand)]
enum NetworkCommands {
    /// List available networks
    Ls,
    /// Create a new bridge network
    Create {
        /// Network name
        name: String,
        /// Subnet CIDR (e.g. 10.244.0.0/16)
        #[arg(long)]
        subnet: String,
        /// Network Driver (bridge, overlay)
        #[arg(long, default_value = "bridge")]
        driver: String,
    },
    /// Remove a network
    Rm {
        /// Network name
        name: String,
    },
}

#[derive(Serialize)]
struct RunRequest {
    image: String,
    vcpu: u32,
    mem_size_mib: u32,
    ports: Vec<PortMapping>,
    volumes: Vec<VolumeMount>,
    hostname: Option<String>,
    labels: HashMap<String, String>,
}

#[derive(Serialize)]
struct RestoreRequest {
    snapshot_path: String,
    mem_path: String,
    cow_path: String,
    original_vm_id: String,
}

#[derive(Serialize)]
struct TimeTravelRequest {
    vm_id: String,
    snapshot_id: String,
}

#[derive(Deserialize, Debug)]
struct RunResponse {
    vm_id: String,
    status: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    
    // Resolve socket path with fallback to user-writable location
    let socket_path = resolve_socket_path(&cli.socket_path);
    info!("Using socket path: {}", socket_path);
    
    // Connect via Unix Socket
    let client = Client::builder()
        .unix_socket(socket_path.as_str())
        .build()?;
    
    // Daemon URL for HTTP requests (goes through Unix Socket proxy)
    let daemon_url = format!("http://localhost:{}", cli.http_port);

    match cli.command {
        Commands::Pull { image } => {
            info!("Requesting to pull image: {}", image);
            let resp = client
                .post(format!("{}/pull", daemon_url))
                .json(&serde_json::json!({ "image": image }))
                .send()
                .await;
            handle_simple_response(resp, &daemon_url).await?;
        }
        Commands::Run {
            image,
            vcpu,
            memory,
            ports,
            volumes,
            hostname,
            labels,
        } => {
            info!("Requesting to run image: {}", image);

            let mut port_mappings = Vec::new();
            for p in ports {
                let parts: Vec<&str> = p.split(':').collect();
                if parts.len() != 2 {
                    error!("Invalid port format: {}. Use host:vm (e.g., 8080:80)", p);
                    return Ok(());
                }

                let host_port = parts[0]
                    .parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("Invalid host port: {}", parts[0]))?;
                let vm_port = parts[1]
                    .parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("Invalid vm port: {}", parts[1]))?;

                port_mappings.push(PortMapping { host_port, vm_port });
            }

            let mut volume_mounts = Vec::new();
            for v in volumes {
                let parts: Vec<&str> = v.split(':').collect();
                if parts.len() != 2 {
                    error!(
                        "Invalid volume format: {}. Use host:vm (e.g., /foo:/bar)",
                        v
                    );
                    return Ok(());
                }
                volume_mounts.push(VolumeMount {
                    host_path: parts[0].to_string(),
                    vm_path: parts[1].to_string(),
                    read_only: false, // Default RW for now
                });
            }

            let mut label_map = HashMap::new();
            for l in labels {
                if let Some((k, v)) = l.split_once('=') {
                    label_map.insert(k.to_string(), v.to_string());
                } else {
                    label_map.insert(l.clone(), "".to_string());
                }
            }

            let payload = RunRequest {
                image,
                vcpu,
                mem_size_mib: memory,
                ports: port_mappings,
                volumes: volume_mounts,
                hostname,
                labels: label_map,
            };

            let resp = client
                .post(format!("{}/run", daemon_url))
                .json(&payload)
                .send()
                .await;

            handle_response(resp, &daemon_url).await?;
        }
        Commands::Commit { id, new_image_name } => {
            info!("Requesting to commit VM {} to image {}", id, new_image_name);
            let payload = serde_json::json!({
                "new_image_name": new_image_name
            });
            let resp = client
                .post(format!("{}/commit/{}", daemon_url, id))
                .json(&payload)
                .send()
                .await;
            handle_simple_response(resp, &daemon_url).await?;
        }
        Commands::Stop { id } => {
            info!("Requesting to stop VM: {}", id);
            let resp = client
                .post(format!("{}/stop/{}", daemon_url, id))
                .send()
                .await;
            handle_simple_response(resp, &daemon_url).await?;
        }
        Commands::Pause { id } => {
            info!("Requesting to pause VM: {}", id);
            let resp = client
                .post(format!("{}/pause/{}", daemon_url, id))
                .send()
                .await;
            handle_simple_response(resp, &daemon_url).await?;
        }
        Commands::Resume { id } => {
            info!("Requesting to resume VM: {}", id);
            let resp = client
                .post(format!("{}/resume/{}", daemon_url, id))
                .send()
                .await;
            handle_simple_response(resp, &daemon_url).await?;
        }
        Commands::Exec { id, cmd } => {
            // 1. Resolve ID (if Name provided)
            let mut target_id = id.clone();
            let resp = client.get(format!("{}/ps", daemon_url)).send().await;
            if let Ok(r) = resp {
                if let Ok(list) = r.json::<ListResponse>().await {
                    for vm in list.vms {
                        if vm.id == id
                            || vm.hostname.as_deref() == Some(id.as_str())
                            || vm.labels.get("vyoma.service").map(|s| s.as_str())
                                == Some(id.as_str())
                        {
                            target_id = vm.id;
                            break;
                        }
                    }
                }
            }

            let payload = serde_json::json!({
                "command": cmd
            });

            info!("Executing command in VM {}...", target_id);
            let resp = client
                .post(format!("{}/exec/{}", daemon_url, target_id))
                .json(&payload)
                .send()
                .await;

            match resp {
                Ok(response) => {
                    if response.status().is_success() {
                        #[derive(Deserialize)]
                        struct ExecResp {
                            stdout: String,
                            stderr: String,
                            exit_code: i32,
                        }

                        if let Ok(exec_result) = response.json::<ExecResp>().await {
                            if !exec_result.stdout.is_empty() {
                                print!("{}", exec_result.stdout);
                            }
                            if !exec_result.stderr.is_empty() {
                                eprint!("{}", exec_result.stderr);
                            }
                            std::process::exit(exec_result.exit_code);
                        } else {
                            error!("Failed to parse execution response");
                            std::process::exit(1);
                        }
                    } else {
                        let error_msg = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                        error!("Execution failed: {}", error_msg);
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    error!("Failed to connect to daemon at {}: {}", daemon_url, e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Restart { id } => {
            // 1. Resolve ID (if Name provided)
            let mut target_id = id.clone();
            let resp = client.get(format!("{}/ps", daemon_url)).send().await;
            if let Ok(r) = resp {
                if let Ok(list) = r.json::<ListResponse>().await {
                    for vm in list.vms {
                        if vm.id == id
                            || vm.hostname.as_deref() == Some(id.as_str())
                            || vm.labels.get("vyoma.service").map(|s| s.as_str())
                                == Some(id.as_str())
                        {
                            target_id = vm.id;
                            break;
                        }
                    }
                }
            }

            // 2. Inspect (to get config)
            info!("Restarting VM {} (Fetching config...)", target_id);
            let resp = client
                .get(format!("{}/vms/{}", daemon_url, target_id))
                .send()
                .await;

            match resp {
                Ok(r) => {
                    if !r.status().is_success() {
                        error!("Failed to inspect VM {}: {}", target_id, r.status());
                        return Ok(());
                    }
                    let vm_state: VmState = r.json().await?;

                    // 3. Stop
                    info!("Stopping VM {}...", target_id);
                    let _ = client
                        .post(format!("{}/stop/{}", daemon_url, target_id))
                        .send()
                        .await;

                    // 4. Run (New)
                    info!("Starting replacement VM...");

                    let payload = RunRequest {
                        image: vm_state.base_image_path, // Pass resolved path
                        vcpu: vm_state.vcpu,
                        mem_size_mib: vm_state.mem_size_mib,
                        ports: vm_state.ports,
                        volumes: vm_state.volumes,
                        hostname: vm_state.hostname,
                        labels: vm_state.labels,
                    };

                    let resp = client
                        .post(format!("{}/run", daemon_url))
                        .json(&payload)
                        .send()
                        .await;
                    handle_simple_response(resp, &daemon_url).await?;
                }
                Err(e) => error!("Failed to connect to daemon: {}", e),
            }
        }
        Commands::Ps => {
            let resp = client.get(format!("{}/ps", daemon_url)).send().await;

            match resp {
                Ok(response) => {
                    if response.status().is_success() {
                        let body: ListResponse = response.json().await?;
                        println!(
                            "{:<36} {:<15} {:<15} {:<15} {:<30}",
                            "VM ID", "IP ADDRESS", "HOSTNAME", "ATTESTATION", "LABELS"
                        );
                        for vm in body.vms {
                            let labels_str = vm
                                .labels
                                .iter()
                                .map(|(k, v)| {
                                    if v.is_empty() {
                                        k.clone()
                                    } else {
                                        format!("{}={}", k, v)
                                    }
                                })
                                 .collect::<Vec<_>>()
                                 .join(", ");
                            let hostname_str = vm.hostname.unwrap_or_else(|| "-".to_string());
                            let attest_str = vm.attestation_status.unwrap_or_else(|| "-".to_string());

                            println!(
                                "{:<36} {:<15} {:<15} {:<15} {:<30}",
                                vm.id, vm.ip_address, hostname_str, attest_str, labels_str
                            );
                        }
                    } else {
                        error!("Daemon returned error: {}", response.status());
                    }
                }
                Err(e) => {
                    error!("Failed to connect to daemon at {}: {}", daemon_url, e);
                }
            }
        }
        Commands::Attest { id, .. } => {
            info!("Requesting attestation for VM: {}", id);
            let resp = client
                .post(format!("{}/attest/{}", daemon_url, id))
                .send()
                .await;

            match resp {
                Ok(response) => {
                    if response.status().is_success() {
                        let attest_resp: serde_json::Value = response.json().await?;
                        let verified = attest_resp.get("verified")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let vm_id = attest_resp.get("vm_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&id);

                        if verified {
                            println!("VM {} attestation: {}", vm_id, "VERIFIED".green());
                        } else {
                            println!("VM {} attestation: {}", vm_id, "FAILED".red());
                        }

                        if let Some(pcr_results) = attest_resp.get("pcr_results").and_then(|v| v.as_array()) {
                            println!("\nPCR Results:");
                            println!("{:<10} {:<64} {:<64} {:<12}",
                                "PCR Index", "Expected", "Actual", "Verified");
                            for pcr in pcr_results {
                                let pcr_index = pcr.get("pcr_index").and_then(|v| v.as_u64()).unwrap_or(0);
                                let expected = pcr.get("expected").and_then(|v| v.as_str()).unwrap_or("");
                                let actual = pcr.get("actual").and_then(|v| v.as_str()).unwrap_or("");
                                let verified = pcr.get("verified").and_then(|v| v.as_bool()).unwrap_or(false);

                                let status = if verified { "✓".green() } else { "✗".red() };
                                println!(
                                    "{:<10} {:<64} {:<64} {:<12}",
                                    pcr_index, expected, actual, status
                                );
                            }
                        }

                        if let Some(error) = attest_resp.get("error").and_then(|v| v.as_str()) {
                            println!("\nError: {}", error.red());
                        }
                    } else {
                        let error_msg = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                        error!("Attestation failed: {}", error_msg);
                    }
                }
                Err(e) => {
                    error!("Failed to connect to daemon at {}: {}", daemon_url, e);
                }
            }
        }
        Commands::Snapshot { id } => {
            info!("Requesting to snapshot VM: {}", id);
            let resp = client
                .post(format!("{}/snapshot/{}", daemon_url, id))
                .send()
                .await;
            handle_simple_response(resp, &daemon_url).await?;
        }
        Commands::History { id } => {
            let resp = client.get(format!("{}/history/{}", daemon_url, id)).send().await;
            match resp {
                Ok(response) => {
                    if response.status().is_success() {
                        let json: serde_json::Value = response.json().await?;
                        println!("{}", serde_json::to_string_pretty(&json)?);
                    } else {
                        error!("Daemon returned error: {}", response.status());
                    }
                }
                Err(e) => error!("Failed to connect: {}", e),
            }
        }
        Commands::TimeTravel { id, to } => {
            info!("Requesting to time-travel VM {} to snapshot: {}", id, to);
            let payload = TimeTravelRequest {
                vm_id: id,
                snapshot_id: to,
            };

            let resp = client
                .post(format!("{}/time-travel", daemon_url))
                .json(&payload)
                .send()
                .await;

            handle_response(resp, &daemon_url).await?;
        }
        Commands::Save { id, output } => {
            info!("Saving VM {} to {}", id, output);
            export_vm(&id, &output)?;
        }
        Commands::Load { input } => {
            info!("Loading VM from {}", input);
            import_vm(&input, &daemon_url).await?;
        }
        Commands::Teleport { id, target, bandwidth, progress } => {
            info!("Initiating Live Teleportation for VM {} to target {}", id, target);

            let target_daemon = format!("http://{}:3000", target);
            let status_check = client.get(format!("{}/teleport/status/test", target_daemon)).send().await;
            
            let use_live_migration = status_check.map(|r| r.status().as_u16() != 404).unwrap_or(false);

            if !use_live_migration {
                warn!("Target node {} doesn't support live migration, falling back to snapshot/stream mode", target);
                // Start receiver on the target node
                let receiver_payload = serde_json::json!({
                    "vm_id": id
                });
                let recv_resp = client
                    .post(format!("{}/receive-teleport", target_daemon))
                    .json(&receiver_payload)
                    .send()
                    .await;

                if let Err(e) = recv_resp {
                    error!("Failed to start receiver on target {}: {}", target, e);
                    return Ok(());
                }
                let recv_resp = recv_resp.unwrap();
                if !recv_resp.status().is_success() {
                    error!("Target refused to start receiver: {}", recv_resp.text().await.unwrap_or_default());
                    return Ok(());
                }
                info!("Receiver started on target: {}", recv_resp.text().await.unwrap_or_default());

                // Send migration from source
                let payload = serde_json::json!({
                    "vm_id": id,
                    "target_node_ip": target
                });
                let resp = client
                    .post(format!("{}/teleport", daemon_url))
                    .json(&payload)
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    error!("Teleport (fallback) failed: {}", resp.text().await.unwrap_or_default());
                    return Ok(());
                }
                println!("{}", resp.text().await.unwrap_or_default());
                return Ok(());
            }

            let payload = serde_json::json!({
                "vm_id": id,
                "target_node_ip": target,
                "bandwidth_mbps": bandwidth
            });
            let resp = client
                .post(format!("{}/teleport", daemon_url))
                .json(&payload)
                .send()
                .await?;

            if !resp.status().is_success() {
                error!("Teleport failed: {}", resp.text().await.unwrap_or_default());
                return Ok(());
            }

            let text = resp.text().await.unwrap_or_default();
            println!("{}", text);
            
            if progress {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(session_id) = data.get("session_id").and_then(|v| v.as_str()) {
                        show_migration_progress(&client, &daemon_url, session_id).await;
                    }
                }
            }
        }
        Commands::Logs { id, follow: _ } => {
            // 1. Resolve ID (if Name provided)
            let mut target_id = id.clone();
            let resp = client.get(format!("{}/ps", daemon_url)).send().await;
            if let Ok(r) = resp {
                if let Ok(list) = r.json::<ListResponse>().await {
                    for vm in list.vms {
                        if vm.id == id
                            || vm.hostname.as_deref() == Some(id.as_str())
                            || vm.labels.get("vyoma.service").map(|s| s.as_str())
                                == Some(id.as_str())
                        {
                            target_id = vm.id;
                            break;
                        }
                    }
                }
            }

            let resp = client
                .get(format!("{}/logs/{}", daemon_url, target_id))
                .send()
                .await?;

            if !resp.status().is_success() {
                error!("Failed to get logs: {}", resp.status());
                return Ok(());
            }

            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();

            while let Some(item) = stream.next().await {
                let bytes = item?;
                let chunk = String::from_utf8_lossy(&bytes);
                buffer.push_str(&chunk);

                while let Some(idx) = buffer.find('\n') {
                    let line = buffer[..idx].to_string();
                    buffer = buffer[idx + 1..].to_string();

                    if line.starts_with("data: ") {
                        println!("{}", &line[6..]);
                    }
                }
            }
        }
        Commands::Build { path, measured } => {
            info!("Building image from context: {} (measured={})", path, measured);
            build_image_with_client(&path, &client, &daemon_url, measured).await?;
        }
        Commands::Doctor => {
            run_doctor().await?;
        }
        Commands::Network { command } => match command {
            NetworkCommands::Ls => {
                let resp = client.get(format!("{}/networks", daemon_url)).send().await;
                match resp {
                    Ok(response) => {
                        if response.status().is_success() {
                            let networks: Vec<String> = response.json().await?;
                            println!("NETWORKS:");
                            for net in networks {
                                println!("{}", net);
                            }
                        } else {
                            error!("Daemon returned error: {}", response.status());
                        }
                    }
                    Err(e) => error!("Failed to list networks: {}", e),
                }
            }
            NetworkCommands::Create {
                name,
                subnet,
                driver,
            } => {
                let payload = serde_json::json!({
                    "name": name,
                    "subnet": subnet,
                    "driver": driver
                });
                let resp = client
                    .post(format!("{}/networks", daemon_url))
                    .json(&payload)
                    .send()
                    .await;
                handle_simple_response(resp, &daemon_url).await?;
            }
            NetworkCommands::Rm { name } => {
                let resp = client
                    .delete(format!("{}/networks/{}", daemon_url, name))
                    .send()
                    .await;
                handle_simple_response(resp, &daemon_url).await?;
            }
        },
        Commands::Swarm { command } => match command {
            SwarmCommands::Init => {
                info!("Initializing swarm...");
                let resp = client
                    .post(format!("{}/swarm/init", daemon_url))
                    .send()
                    .await;
                handle_simple_response(resp, &daemon_url).await?;
            }
            SwarmCommands::Join { ip } => {
                info!("Joining swarm at {}...", ip);
                let payload = serde_json::json!({ "seed_ip": ip });
                let resp = client
                    .post(format!("{}/swarm/join", daemon_url))
                    .json(&payload)
                    .send()
                    .await;
                handle_simple_response(resp, &daemon_url).await?;
            }
            SwarmCommands::Ls => {
                let resp = client
                    .get(format!("{}/swarm/nodes", daemon_url))
                    .send()
                    .await;
                // Just print raw JSON for MVP
                handle_simple_response(resp, &daemon_url).await?;
            }
        },
        Commands::Up { file, detach } => {
            info!("Processing compose file: {}", file);
            match VyomaCompose::from_file(&file) {
                Ok(compose) => {
                    println!("Vyoma Compose v{}", compose.version);

                    let stack_name = std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "default".to_string());

                    let service_order = match compose.start_order() {
                        Ok(o) => o,
                        Err(e) => {
                            error!("Dependency resolution failed: {}", e);
                            return Ok(());
                        }
                    };

                    println!("Services found: {}", service_order.len());

                    // Provision Networks
                    let mut created_networks = HashMap::new();
                    for (net_name, net_config) in &compose.networks {
                        let full_net_name = format!("{}_{}", stack_name, net_name);
                        
                        let req = serde_json::json!({
                            "name": full_net_name,
                            "subnet": net_config.ipam.config.first().and_then(|c| c.subnet.clone()).unwrap_or_else(|| "".to_string()),
                            "driver": net_config.driver.clone().unwrap_or_else(|| "bridge".to_string())
                        });

                        let resp = client.post(format!("{}/networks", daemon_url))
                            .json(&req)
                            .send()
                            .await;
                            
                        if let Ok(r) = resp {
                            if r.status().is_success() {
                                info!("Network {} provisioned", full_net_name);
                            } else {
                                warn!("Failed to provision network {}: {}", full_net_name, r.text().await.unwrap_or_default());
                            }
                        }
                        created_networks.insert(net_name.clone(), full_net_name);
                    }

                    // Pre-check running services via Daemon
                    let mut service_ids = HashMap::new();
                    let resp = client.get(format!("{}/ps", daemon_url)).send().await;
                    if let Ok(r) = resp {
                        if let Ok(list) = r.json::<ListResponse>().await {
                            for vm in list.vms {
                                if let Some(s) = vm.labels.get("vyoma.stack") {
                                    if s == &stack_name {
                                        let service_name = vm
                                            .labels
                                            .get("vyoma.service")
                                            .cloned()
                                            .unwrap_or(vm.id.clone());
                                        service_ids.insert(service_name, vm.id);
                                    }
                                }
                            }
                        }
                    }

                    for (name, service) in service_order {
                        if service_ids.contains_key(&name) {
                            info!("Service '{}' is already running.", name);
                            continue;
                        }
                        info!("Starting service: {}", name);

                        let image_target = if let Some(ref build) = service.build {
                            let context = match build {
                                vyoma_compose::BuildSource::Path(p) => p.clone(),
                                vyoma_compose::BuildSource::Config(c) => c.context.clone(),
                            };
                            info!("Building service '{}' from {}", name, context);
                            build_image_with_client(&context, &client, &daemon_url, false).await?
                        } else if let Some(ref img) = service.image {
                            img.clone()
                        } else {
                            error!("Service '{}' has no image or build context", name);
                            continue;
                        };

                        // Prepare Ports
                        let mut port_mappings = Vec::new();
                        if let Some(ports) = service.ports {
                            for p in ports {
                                let parts: Vec<&str> = p.split(':').collect();
                                if parts.len() == 2 {
                                    let h = parts[0].parse().unwrap_or(0);
                                    let v = parts[1].parse().unwrap_or(0);
                                    port_mappings.push(PortMapping {
                                        host_port: h,
                                        vm_port: v,
                                    });
                                }
                            }
                        }

                        // Prepare Volumes
                        let mut volume_mounts = Vec::new();
                        if let Some(vols) = service.volumes {
                            for v in vols {
                                let parts: Vec<&str> = v.split(':').collect();
                                if parts.len() == 2 {
                                    volume_mounts.push(VolumeMount {
                                        host_path: parts[0].to_string(),
                                        vm_path: parts[1].to_string(),
                                        read_only: false,
                                    });
                                }
                            }
                        }

                        let payload = RunRequest {
                            image: image_target,
                            vcpu: service.cpus.unwrap_or(1),
                            mem_size_mib: service.memory.unwrap_or(512),
                            ports: port_mappings,
                            volumes: volume_mounts,
                            hostname: Some(name.clone()),
                            labels: {
                                let mut l = HashMap::new();
                                l.insert("vyoma.stack".to_string(), stack_name.clone());
                                l.insert("vyoma.service".to_string(), name.clone());
                                l
                            },
                        };

                        let resp = client
                            .post(format!("{}/run", daemon_url))
                            .json(&payload)
                            .send()
                            .await;

                        match resp {
                            Ok(r) => {
                                if r.status().is_success() {
                                    let body: RunResponse = r.json().await?;
                                    info!("Service '{}' started as VM {}", name, body.vm_id);
                                    service_ids.insert(name.clone(), body.vm_id);
                                } else {
                                    error!("Failed to start service '{}': {}", name, r.status());
                                }
                            }
                            Err(e) => error!("Failed to request start for '{}': {}", name, e),
                        }
                    }
                    if detach {
                        println!("(Detached mode selected)");
                    }
                }
                Err(e) => error!("Failed to parse compose file '{}': {}", file, e),
            }
        }
        Commands::Down { file } => {
            // 1. Identify Stack Name
            let stack_name = std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "default".to_string());

            info!("Stopping stack: {}", stack_name);

            // 2. Fetch all VMs to find stack members
            let mut vms_to_stop = HashMap::new(); // name -> id
            let resp = client.get(format!("{}/ps", daemon_url)).send().await;
            if let Ok(r) = resp {
                if let Ok(list) = r.json::<ListResponse>().await {
                    for vm in list.vms {
                        if let Some(s) = vm.labels.get("vyoma.stack") {
                            if s == &stack_name {
                                // Found a VM belonging to this stack
                                let service_name = vm
                                    .labels
                                    .get("vyoma.service")
                                    .cloned()
                                    .unwrap_or(vm.id.clone());
                                vms_to_stop.insert(service_name, vm.id);
                            }
                        }
                    }
                }
            }

            if vms_to_stop.is_empty() {
                println!("No running services found for stack '{}'.", stack_name);
                let _ = std::fs::remove_file("vyoma-compose.state");
                return Ok(());
            }

            // 3. Determine Order
            let mut stop_order = Vec::new();
            if let Ok(compose) = VyomaCompose::from_file(&file) {
                if let Ok(order) = compose.start_order() {
                    stop_order = order.into_iter().rev().map(|(n, _)| n).collect();
                }
            }

            // Add any remaining services from running list not in stop_order
            for name in vms_to_stop.keys() {
                if !stop_order.contains(name) {
                    stop_order.push(name.clone());
                }
            }

            for name in stop_order {
                if let Some(id) = vms_to_stop.get(&name) {
                    info!("Stopping service '{}' (VM {})", name, id);
                    let resp = client
                        .post(format!("{}/stop/{}", daemon_url, id))
                        .send()
                        .await;
                    match resp {
                        Ok(r) => {
                            if !r.status().is_success() {
                                error!("Failed to stop VM {}: {}", id, r.status());
                            }
                        }
                        Err(e) => error!("Failed to stop VM {}: {}", id, e),
                    }
                }
            }
            let _ = std::fs::remove_file("vyoma-compose.state");
            println!("Stack stopped and removed.");
        }
        Commands::Scale { replicas, file } => {
            // 1. Load Compose File
            let compose = match VyomaCompose::from_file(&file) {
                Ok(c) => c,
                Err(e) => {
                    error!(
                        "Validation Error: Cannot scale without valid {}: {}",
                        file, e
                    );
                    return Ok(());
                }
            };

            // 2. Identify Stack Name
            let stack_name = std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "default".to_string());

            // 3. Process scale args
            let mut scale_map = HashMap::new();
            for r in replicas {
                if let Some((svc, count_str)) = r.split_once('=') {
                    if let Ok(count) = count_str.parse::<usize>() {
                        if compose.services.contains_key(svc) {
                            scale_map.insert(svc.to_string(), count);
                        } else {
                            error!("Service '{}' not defined in compose file.", svc);
                        }
                    } else {
                        error!("Invalid count format for '{}'. Expected integer.", svc);
                    }
                } else {
                    error!("Invalid format: {}. Expected service=count", r);
                }
            }

            if scale_map.is_empty() {
                println!("No valid scaling instructions provided.");
                return Ok(());
            }

            // 4. Get Current State
            let mut current_state: HashMap<String, Vec<String>> = HashMap::new();
            let resp = client.get(format!("{}/ps", daemon_url)).send().await;
            if let Ok(r) = resp {
                if let Ok(list) = r.json::<ListResponse>().await {
                    for vm in list.vms {
                        if let Some(s) = vm.labels.get("vyoma.stack") {
                            if s == &stack_name {
                                if let Some(svc) = vm.labels.get("vyoma.service") {
                                    current_state.entry(svc.clone()).or_default().push(vm.id);
                                }
                            }
                        }
                    }
                }
            }

            // 5. Reconcile
            for (svc_name, target_count) in scale_map {
                let running_list = current_state.get(&svc_name).cloned().unwrap_or_default();
                let running_count = running_list.len();
                info!(
                    "Scaling {} from {} to {}",
                    svc_name, running_count, target_count
                );

                if target_count > running_count {
                    let needed = target_count - running_count;
                    let service = compose.services.get(&svc_name).unwrap();

                    for i in 0..needed {
                        info!("Starting replica {}/{}", i + 1, needed);
                        start_service_helper(&client, &daemon_url, &stack_name, &svc_name, service)
                            .await?;
                    }
                } else if running_count > target_count {
                    let remove_count = running_count - target_count;

                    // Stop the LAST N instances
                    for i in 0..remove_count {
                        if let Some(id) = running_list.get(running_count - 1 - i) {
                            info!("Stopping replica {} (VM {})", running_count - i, id);
                            let resp = client
                                .post(format!("{}/stop/{}", daemon_url, id))
                                .send()
                                .await;
                            handle_simple_response(resp, &daemon_url).await?;
                        }
                    }
                } else {
                    println!(
                        "Service {} is already at target scale ({}).",
                        svc_name, target_count
                    );
                }
            }
        }
        Commands::Attest { id, manifest, trust_keys, no_verify_signature, format, nonce: _ } => {
            let json_output = format == "json";

            match run_attest(&client, &daemon_url, &id, manifest.as_deref(), trust_keys.as_deref(), no_verify_signature, json_output).await {
                Ok(verified) => {
                    if !verified {
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    error!("{}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Stats { id, interval, no_stream } => {
            let mut actual_id = id.clone();
            let resp = client.get(format!("{}/ps", daemon_url)).send().await;
            if let Ok(r) = resp {
                if let Ok(list) = r.json::<ListResponse>().await {
                    for vm in list.vms {
                        if vm.id == id || vm.hostname.as_deref() == Some(id.as_str()) || vm.labels.get("vyoma.service").map(|s| s.as_str()) == Some(id.as_str()) {
                            actual_id = vm.id;
                            break;
                        }
                    }
                }
            }
            
            let url = format!("{}/vms/{}/stats", daemon_url, actual_id);

            if no_stream {
                let resp = client.get(&url).send().await;
                if let Ok(r) = resp {
                    if r.status().is_success() {
                        if let Ok(stats) = r.json::<vyoma_core::api::VmStatsResponse>().await {
                            println!("{:<15} {:<10} {:<25} {:<10}", "CONTAINER", "CPU %", "MEM USAGE / LIMIT", "MEM %");
                            
                            let limit_str = if stats.memory_limit_bytes > 0 {
                                format!("{:.2}MiB", stats.memory_limit_bytes as f64 / 1048576.0)
                            } else {
                                "unlimited".to_string()
                            };
                            let usage_str = format!("{:.2}MiB", stats.memory_usage_bytes as f64 / 1048576.0);
                            
                            println!("{:<15} {:<9.2}% {:<25} {:<9.2}%", 
                                &stats.vm_id[..12.min(stats.vm_id.len())],
                                stats.cpu_percent,
                                format!("{} / {}", usage_str, limit_str),
                                stats.memory_percent
                            );
                        }
                    } else {
                        handle_response(Ok(r), &daemon_url).await?;
                    }
                } else {
                    handle_response(resp, &daemon_url).await?;
                }
            } else {
                loop {
                    let resp = client.get(&url).send().await;
                    if let Ok(r) = resp {
                        if r.status().is_success() {
                            if let Ok(stats) = r.json::<vyoma_core::api::VmStatsResponse>().await {
                                // Clear screen and move to top
                                print!("\x1B[2J\x1B[1;1H");
                                println!("{:<15} {:<10} {:<25} {:<10}", "CONTAINER", "CPU %", "MEM USAGE / LIMIT", "MEM %");
                                
                                let limit_str = if stats.memory_limit_bytes > 0 {
                                    format!("{:.2}MiB", stats.memory_limit_bytes as f64 / 1048576.0)
                                } else {
                                    "unlimited".to_string()
                                };
                                let usage_str = format!("{:.2}MiB", stats.memory_usage_bytes as f64 / 1048576.0);
                                
                                println!("{:<15} {:<9.2}% {:<25} {:<9.2}%", 
                                    &stats.vm_id[..12.min(stats.vm_id.len())],
                                    stats.cpu_percent,
                                    format!("{} / {}", usage_str, limit_str),
                                    stats.memory_percent
                                );
                            }
                        } else if r.status() == reqwest::StatusCode::NOT_FOUND {
                            error!("VM {} not found", actual_id);
                            std::process::exit(1);
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                }
            }
        }
    }

    Ok(())
}

async fn run_doctor() -> Result<()> {
    println!(
        "{}",
        "Vyoma Doctor - System Health Check".bold().underline()
    );
    println!();

    let mut all_passed = true;

    // Helper to print status
    let check = |name: &str, result: Result<bool>, required: bool| -> bool {
        match result {
            Ok(true) => {
                println!("{} {}", "[OK]".green().bold(), name);
                true
            }
            Ok(false) => {
                if required {
                    println!("{} {}", "[FAIL]".red().bold(), name);
                    false
                } else {
                    println!("{} {}", "[WARN]".yellow().bold(), name);
                    true // Warn doesn't fail overall
                }
            }
            Err(e) => {
                if required {
                    println!("{} {} ({})", "[FAIL]".red().bold(), name, e);
                    false
                } else {
                    println!("{} {} ({})", "[WARN]".yellow().bold(), name, e);
                    true
                }
            }
        }
    };

    // 1. KVM Access
    if !check("KVM Device Access (/dev/kvm)", check_kvm(), true) {
        all_passed = false;
    }

    // 2. Cgroups
    if !check("Cgroups v2 (/sys/fs/cgroup)", check_cgroups(), true) {
        all_passed = false;
    }

    // 3. Binaries
    if !check("Cloud Hypervisor Binary", check_binary("cloud-hypervisor"), true) {
        all_passed = false;
    }
    if !check("Virtiofsd Binary", check_binary("virtiofsd"), true) {
        all_passed = false;
    }

    // 4. Networking
    if !check("Vyoma Bridge (vyoma0)", check_bridge("vyoma0"), false) {} // Warn only

    // 5. Rootless Tools
    if !check("debugfs (e2fsprogs)", check_binary("debugfs"), false) {} // Needed for rootless build

    println!();
    if all_passed {
        println!("{}", "Your system is ready for Vyoma!".green().bold());
    } else {
        println!(
            "{}",
            "Some checks failed. Please review the errors above."
                .red()
                .bold()
        );
    }

    Ok(())
}

fn check_kvm() -> Result<bool> {
    use std::fs::OpenOptions;
    let path = Path::new("/dev/kvm");
    if !path.exists() {
        return Ok(false);
    }
    // Try to open R/W
    OpenOptions::new().read(true).write(true).open(path)?;
    Ok(true)
}

fn check_cgroups() -> Result<bool> {
    // Just check if mount point exists and is cgroup2
    // Simple check: /sys/fs/cgroup/cgroup.controllers should exist
    let path = Path::new("/sys/fs/cgroup/cgroup.controllers");
    Ok(path.exists())
}

fn check_binary(name: &str) -> Result<bool> {
    use std::path::Path;

    // Check standard packaging paths (ADR 021)
    if Path::new(&format!("/opt/vyoma/bin/{}", name)).exists() {
        return Ok(true);
    }
    if Path::new(&format!("/usr/libexec/vyoma/{}", name)).exists() {
        return Ok(true);
    }
    // Check bundled path from package
    if Path::new(&format!("/usr/lib/vyoma/{}", name)).exists() {
        return Ok(true);
    }

    // Check local development path
    if Path::new(&format!("bin/{}", name)).exists() {
        return Ok(true);
    }

    // Check system PATH
    let status = std::process::Command::new("which")
        .arg(name)
        .output()?
        .status;

    Ok(status.success())
}

fn command_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn check_bridge(bridge_name: &str) -> Result<bool> {
    let output = std::process::Command::new("ip")
        .arg("link")
        .arg("show")
        .arg(bridge_name)
        .output()?;
    Ok(output.status.success())
}

async fn build_image_with_client(context_path: &str, client: &Client, daemon_url: &str, measured: bool) -> Result<String> {
    let context_path = Path::new(context_path);
    if !context_path.exists() {
        return Err(anyhow::anyhow!(
            "Context path does not exist: {:?}",
            context_path
        ));
    }

    // Create tarball to temp file
    let temp_dir = tempfile::tempdir()?;
    let tar_path = temp_dir.path().join("context.tar.gz");

    // Create tar.gz using subprocess for reliability
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&tar_path)
        .arg("-C")
        .arg(context_path)
        .arg(".")
        .status()?;

    if !status.success() {
        return Err(anyhow::anyhow!("Failed to create tarball"));
    }

    // Send to daemon
    let file = tokio::fs::File::open(&tar_path).await?;
    let stream = tokio_util::codec::FramedRead::new(file, tokio_util::codec::BytesCodec::new());
    let body = reqwest::Body::wrap_stream(stream);

    let measured_query = if measured { "?measured=true" } else { "" };
    let resp = client
        .post(format!("{}/build{}", daemon_url, measured_query))
        .body(body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Build failed ({}): {}", status, text));
    }

    let image_id = resp.text().await?;
    info!("Build complete. Image ID: {}", image_id);
    Ok(image_id)
}

async fn build_image(context_path: &str, daemon_url: &str) -> Result<String> {
    let client = Client::new();
    build_image_with_client(context_path, &client, daemon_url, false).await
}

async fn run_attest(
    client: &Client,
    daemon_url: &str,
    vm_id: &str,
    manifest_path: Option<&str>,
    trust_keys_path: Option<&str>,
    no_verify_signature: bool,
    json_output: bool,
) -> Result<bool> {
    info!("Attesting VM: {}", vm_id);

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    let (signed_manifest, manifest_source) = if let Some(ref mpath) = manifest_path {
        let path = PathBuf::from(mpath);
        let signed = SignedManifest::load_from_file(&path)
            .map_err(|e| anyhow::anyhow!("Failed to load manifest from {}: {}", mpath, e))?;
        (signed, format!("explicit: {}", mpath))
    } else {
        let inspect_url = format!("{}/vm/{}", daemon_url, vm_id);
        let resp = client.get(&inspect_url).send().await
            .map_err(|e| anyhow::anyhow!("Failed to inspect VM {}: {}", vm_id, e))?;

        if resp.status() != StatusCode::OK {
            return Err(anyhow::anyhow!("VM {} not found (daemon returned {})", vm_id, resp.status()));
        }

        #[derive(Deserialize)]
        struct VmInspect { base_image_path: String }

        let vm_info: VmInspect = resp.json().await
            .map_err(|e| anyhow::anyhow!("Failed to parse VM info: {}", e))?;

        let image_name = std::path::Path::new(&vm_info.base_image_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| vm_info.base_image_path.clone());

        let candidates = [
            home.join(".vyoma").join("images").join(&image_name),
            home.join(".vyoma").join("images").join(&image_name),
        ];

        let mut loaded = None;
        let mut source = String::new();
        for dir in &candidates {
            let sig_path = dir.join("vyoma.toml.sig");
            if sig_path.exists() {
                match SignedManifest::load_from_file(&sig_path) {
                    Ok(sm) => {
                        loaded = Some(sm);
                        source = format!("auto-discovered signed: {}", sig_path.display());
                        break;
                    }
                    Err(e) => warn!("Failed to load {}: {}", sig_path.display(), e),
                }
            }
            let plain_path = dir.join("vyoma.toml");
            if plain_path.exists() {
                match vyoma_image::VmifConverter::load_signed_manifest(&plain_path) {
                    Ok(sm) => {
                        loaded = Some(sm);
                        source = format!("auto-discovered: {}", plain_path.display());
                        break;
                    }
                    Err(e) => warn!("Failed to load {}: {}", plain_path.display(), e),
                }
            }
        }

        match loaded {
            Some(sm) => (sm, source),
            None => return Err(anyhow::anyhow!(
                "No manifest found for image '{}'. Searched: {:?}. Build image first.",
                image_name, candidates
            )),
        }
    };

    if !no_verify_signature {
        let mut policy = TrustPolicy::new(true);
        let keys_dir = if let Some(ref kp) = trust_keys_path {
            PathBuf::from(kp)
        } else {
            home.join(".vyoma").join("keys").join("trusted")
        };
        policy.load_trusted_keys_from_dir(keys_dir.clone())
            .map_err(|e| anyhow::anyhow!("Failed to load trusted keys from {:?}: {}", keys_dir, e))?;

        match policy.verify(&signed_manifest) {
            Ok(()) => {
                let key_hex: String = signed_manifest.public_key.iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>();
                info!("Manifest signature verified. Signing key: {}...", &key_hex[..16]);
                if !json_output {
                    println!("  {}", format!("Manifest signed by: {}...", &key_hex[..16]).green());
                }
            }
            Err(e) => {
                error!("Manifest signature verification FAILED: {}", e);
                return Err(anyhow::anyhow!("Signature verification failed: {}. Use --no-verify-signature to skip.", e));
            }
        }
    } else if !json_output {
        println!("  Note: Signature verification skipped (--no-verify-signature)");
    }

    if !json_output {
        println!("  Manifest: {}", manifest_source);
    }

    let url = format!("{}/attest/{}", daemon_url, vm_id);
    let resp = client.post(&url).send().await
        .map_err(|e| anyhow::anyhow!("Failed to contact daemon at {}: {}", daemon_url, e))?;

    let status = resp.status();
    if status != StatusCode::OK {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Attestation request failed ({}): {}", status, body));
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct AttestResponse {
        vm_id: String,
        verified: bool,
        pcr_results: Vec<PcrResult>,
        error: Option<String>,
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct PcrResult {
        pcr_index: u32,
        expected: String,
        actual: String,
        verified: bool,
    }

    let attest_resp: AttestResponse = resp.json().await
        .map_err(|e| anyhow::anyhow!("Failed to parse attest response: {}", e))?;

    if json_output {
        #[derive(serde::Serialize)]
        struct JsonOutput<'a> {
            vm_id: &'a str,
            verified: bool,
            pcr_results: &'a Vec<PcrResult>,
            error: &'a Option<String>,
            manifest: String,
        }
        let out = JsonOutput {
            vm_id: &attest_resp.vm_id,
            verified: attest_resp.verified,
            pcr_results: &attest_resp.pcr_results,
            error: &attest_resp.error,
            manifest: manifest_source.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return Ok(attest_resp.verified);
    }

    // Human-readable output
    println!("\n{}", "╔═══════════════════════════════════════════════════════════╗".cyan());
    println!("{}", "║              VM Attestation Report                      ║".cyan());
    println!("{}", "╚═══════════════════════════════════════════════════════════╝".cyan());
    println!("VM ID: {}", attest_resp.vm_id);
    println!();

    let pcr_names = [
        (0u32, "PCR-0 (Firmware)"),
        (1, "PCR-1 (Firmware Config)"),
        (4, "PCR-4 (Boot Manager)"),
        (5, "PCR-5 (Boot Manager Config)"),
        (7, "PCR-7 (Secure Boot State)"),
        (9, "PCR-9 (Kernel)"),
        (10, "PCR-10 (Initrd)"),
        (14, "PCR-14 (Rootfs)"),
    ];

    let mut passed = 0;
    let mut failed = 0;

    for result in &attest_resp.pcr_results {
        let pcr_name = if let Some((_, name)) = pcr_names.iter().find(|(idx, _)| *idx == result.pcr_index) {
        *name
    } else {
        return Err(anyhow::anyhow!("Unknown PCR index: {}", result.pcr_index));
    };

        if result.verified {
            println!("  [{}] {}: {}", "OK".green(), pcr_name, result.actual.yellow());
            passed += 1;
        } else {
            println!("  [{}] {}: {}", "FAIL".red(), pcr_name, result.actual.red());
            println!("         Expected: {}", result.expected.yellow());
            failed += 1;
        }
    }

    println!();
    println!("PCRs verified: {}/{}", passed, passed + failed);
    println!();

    if attest_resp.verified {
        println!("{}", "  Attestation: VERIFIED".green().bold());
        println!("  VM {} boot integrity is confirmed.", vm_id);
    } else {
        println!("{}", "  Attestation: FAILED".red().bold());
        if let Some(ref err) = attest_resp.error {
            println!("  Error: {}", err);
        }
        if failed > 0 {
            println!("  {} PCR(s) did not match expected values.", failed);
        }
    }

    println!("  Manifest: {}", manifest_source);

    Ok(attest_resp.verified)
}

fn export_vm(vm_id: &str, output_path: &str) -> Result<()> {
    // 1. Locate VM dir
    // HACK: Accessing .vyoma directly. CLI normally shouldn't allow this if daemon is remote.
    // But for MVP, we assume local access.
    let home = dirs::home_dir().ok_or(anyhow::anyhow!("No home dir"))?;
    let vm_dir = home.join(".vyoma").join("vms").join(vm_id);

    if !vm_dir.exists() {
        return Err(anyhow::anyhow!("VM directory not found: {:?}", vm_dir));
    }

    // Check files
    let snap_path = vm_dir.join("snapshot.snap");
    let mem_path = vm_dir.join("memory.mem");
    let cow_path = vm_dir.join("diff.cow");

    if !snap_path.exists() || !mem_path.exists() {
        return Err(anyhow::anyhow!(
            "Snapshot files missing. Did you run 'vyoma snapshot <id>' first?"
        ));
    }

    // 2. Create Tarball
    let tar_file = File::create(output_path)?;
    let enc = GzEncoder::new(tar_file, Compression::default());
    let mut tar = tar::Builder::new(enc);

    // Add files with specific names for portability
    tar.append_path_with_name(&snap_path, "snapshot.snap")?;
    tar.append_path_with_name(&mem_path, "memory.mem")?;
    tar.append_path_with_name(&cow_path, "diff.cow")?;

    tar.finish()?;

    info!("Export complete: {}", output_path);
    Ok(())
}

async fn import_vm(input_path: &str, daemon_url: &str) -> Result<()> {
    // 1. Unpack to temp dir
    let file = File::open(input_path)?;
    let tar = GzDecoder::new(file);
    let mut archive = Archive::new(tar);

    let temp_dir = tempfile::tempdir()?;
    info!("Unpacking to temporary location: {:?}", temp_dir.path());

    archive.unpack(temp_dir.path())?;

    // 2. Verify files
    let snap_path = temp_dir.path().join("snapshot.snap");
    let mem_path = temp_dir.path().join("memory.mem");
    let cow_path = temp_dir.path().join("diff.cow");

    if !snap_path.exists() || !mem_path.exists() || !cow_path.exists() {
        return Err(anyhow::anyhow!(
            "Invalid export archive. Missing core files."
        ));
    }

    // 3. Call Restore API
    // Need absolute paths for Daemon to access them (Daemon must be on same FS)
    let client = Client::new();
    let payload = RestoreRequest {
        snapshot_path: snap_path.to_string_lossy().to_string(),
        mem_path: mem_path.to_string_lossy().to_string(),
        cow_path: cow_path.to_string_lossy().to_string(),
        original_vm_id: "imported".to_string(),
    };

    let resp = client
        .post(format!("{}/restore", daemon_url))
        .json(&payload)
        .send()
        .await;

    handle_response(resp, &daemon_url).await?;

    // NOTE: TempDir will be deleted when it goes out of scope!
    // But Restore API copies the COW file, so that's fine for COW.
    // However, Snapshot and Mem files are LOADED by Firecracker.
    // Firecracker keeps them open? Or does it read them?
    // Firecracker `load_snapshot` usually loads them.
    // If we delete them too early, restoration might fail if it's async?
    // Actually, `restore_vm` awaits `load_snapshot`.
    // But does `load_snapshot` keep file handle or read into RAM?
    // Firecracker loads into RAM.

    // To be safe, we should probably PERSIST these unpacked files into a new VM directory BEFORE calling restore.
    // But the Daemon creates the new VM directory.
    // Ideally, Daemon should handle import upload.
    // For MVP, we will rely on TempDir not being deleted until function returns,
    // and `restore_vm` waits for success.

    // But wait, `Cow` is copied. `Snapshot/Mem` are READ.
    // If `restore_vm` finishes, the VM is running.
    // Does Firecracker need `memory.mem` to stay on disk after load?
    // Docs say: "The file is mmap()-ed". So YES, it must exist while VM is running?
    // Actually, if it's MAP_PRIVATE, maybe not.
    // If it's backing memory, deleting it is BAD.

    // Safer Approach:
    // Move these files to a permanent location?
    // Since Daemon doesn't know about "Import", we are stuck.
    // Let's rely on standard practice: The User (CLI) is responsible for data validity.
    // We should unpack to a PERMANENT "imports" folder in .vyoma/imports/<uuid>/ ?

    Ok(()) // temp_dir drop here.
}

#[derive(Deserialize, Debug)]
struct VmSummary {
    id: String,
    ip_address: String,
    hostname: Option<String>,
    #[serde(default)]
    labels: HashMap<String, String>,
    attestation_status: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ListResponse {
    vms: Vec<VmSummary>,
}

#[derive(Deserialize, Debug)]
struct VmState {
    #[allow(dead_code)]
    id: String,
    ports: Vec<PortMapping>,
    volumes: Vec<VolumeMount>,
    hostname: Option<String>,
    #[serde(default)]
    labels: HashMap<String, String>,
    #[serde(default)]
    base_image_path: String,
    #[serde(default)]
    vcpu: u32,
    #[serde(default)]
    mem_size_mib: u32,
}

async fn handle_response(resp: Result<reqwest::Response, reqwest::Error>, url: &str) -> Result<()> {
    match resp {
        Ok(response) => {
            if response.status().is_success() {
                let body: RunResponse = response.json().await?;
                info!("Success! VM ID: {}, Status: {}", body.vm_id, body.status);
            } else {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                error!("Daemon returned error: {} - {}", status, text);
            }
        }
        Err(e) => {
            error!("Failed to connect to daemon at {}: {}", url, e);
            info!("Is 'vyomad' running?");
        }
    }
    Ok(())
}

async fn handle_simple_response(
    resp: Result<reqwest::Response, reqwest::Error>,
    url: &str,
) -> Result<()> {
    match resp {
        Ok(response) => {
            if response.status().is_success() {
                let text = response.text().await?;
                info!("Success: {}", text);
            } else {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                error!("Daemon returned error: {} - {}", status, text);
            }
        }
        Err(e) => {
            error!("Failed to connect to daemon at {}: {}", url, e);
            info!("Is 'vyomad' running?");
        }
    }
    Ok(())
}

async fn start_service_helper(
    client: &Client,
    daemon_url: &str,
    stack_name: &str,
    name: &str,
    service: &vyoma_compose::Service,
) -> Result<()> {
    let image_target = if let Some(ref build) = service.build {
        let context = match build {
            vyoma_compose::BuildSource::Path(p) => p.clone(),
            vyoma_compose::BuildSource::Config(c) => c.context.clone(),
        };
        info!("Building service '{}' from {}", name, context);
        build_image_with_client(&context, client, daemon_url, false).await?
    } else if let Some(ref img) = service.image {
        img.clone()
    } else {
        error!("Service '{}' has no image or build context", name);
        return Ok(());
    };

    // Prepare Ports
    let mut port_mappings = Vec::new();
    if let Some(ref ports) = service.ports {
        for p in ports {
            let parts: Vec<&str> = p.split(':').collect();
            if parts.len() == 2 {
                let h = parts[0].parse().unwrap_or(0);
                let v = parts[1].parse().unwrap_or(0);
                port_mappings.push(PortMapping {
                    host_port: h,
                    vm_port: v,
                });
            }
        }
    }

    // Prepare Volumes
    let mut volume_mounts = Vec::new();
    if let Some(ref vols) = service.volumes {
        for v in vols {
            let parts: Vec<&str> = v.split(':').collect();
            if parts.len() == 2 {
                volume_mounts.push(VolumeMount {
                    host_path: parts[0].to_string(),
                    vm_path: parts[1].to_string(),
                    read_only: false,
                });
            }
        }
    }

    let payload = RunRequest {
        image: image_target,
        vcpu: service.cpus.unwrap_or(1),
        mem_size_mib: service.memory.unwrap_or(512),
        ports: port_mappings,
        volumes: volume_mounts,
        hostname: Some(name.to_string()),
        labels: {
            let mut l = HashMap::new();
            l.insert("vyoma.stack".to_string(), stack_name.to_string());
            l.insert("vyoma.service".to_string(), name.to_string());
            l
        },
    };

    let resp = client
        .post(format!("{}/run", daemon_url))
        .json(&payload)
        .send()
        .await;

    match resp {
        Ok(r) => {
            if r.status().is_success() {
                let body: RunResponse = r.json().await?;
                info!("Service '{}' started as VM {}", name, body.vm_id);
            } else {
                error!("Failed to start service '{}': {}", name, r.status());
            }
        }
        Err(e) => error!("Failed to request start for '{}': {}", name, e),
    }
    Ok(())
}

async fn show_migration_progress(client: &reqwest::Client, daemon_url: &str, session_id: &str) {
    use std::io::{self, Write};
    
    let status_url = format!("{}/teleport/status/{}", daemon_url, session_id);
    
    println!("\nMigration progress:");
    println!("{}", "=".repeat(50));
    
    loop {
        let resp = client.get(&status_url).send().await;
        
        match resp {
            Ok(r) if r.status().is_success() => {
                if let Ok(data) = r.json::<serde_json::Value>().await {
                    let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let vm_id = data.get("vm_id").and_then(|v| v.as_str()).unwrap_or("");
                    
                    println!("\rVM: {:20} Status: {:20}", vm_id, status);
                    
                    if let Some(progress) = data.get("progress") {
                        let total = progress.get("total_pages").and_then(|v| v.as_u64()).unwrap_or(0);
                        let transferred = progress.get("transferred_pages").and_then(|v| v.as_u64()).unwrap_or(0);
                        let dirty = progress.get("dirty_pages").and_then(|v| v.as_u64()).unwrap_or(0);
                        let round = progress.get("round").and_then(|v| v.as_u64()).unwrap_or(0);
                        
                        if total > 0 {
                            let pct = (transferred as f64 / total as f64) * 100.0;
                            let bar_len = 30;
                            let filled = ((pct / 100.0) * bar_len as f64) as usize;
                            let bar: String = format!("{}{}", "█".repeat(filled), "░".repeat(bar_len - filled));
                            
                            io::stdout().write_all(format!("\r[{}] {:5.1}% (round {}, dirty: {} pages)\r", bar, pct, round, dirty).as_bytes()).ok();
                            io::stdout().flush().ok();
                        }
                    }
                    
                    if status == "completed" || status == "source_cleaned" {
                        println!("\n\nMigration completed successfully!");
                        break;
                    }
                    
                    if status == "failed" {
                        println!("\n\nMigration failed!");
                        break;
                    }
                }
            }
            _ => break,
        }
        
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
}
