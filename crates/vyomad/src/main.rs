use axum::{
    routing::{get, post},
    Router,
};
use tracing::{info, error, warn};
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as TokioMutex;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use tower_http::cors::{CorsLayer, AllowOrigin};
use tower_service::Service;
use http::Method;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;

use vyoma_core::cgroups::CgroupManager;
use vyoma_core::policy::PolicyManager;
use vyoma_core::proxy::ProxyManager;

use clap::Parser;

fn check_dependencies(data_dir: &str) -> Result<(), String> {
    let mut missing = Vec::new();

    // Check for iptables
    if std::process::Command::new("which")
        .arg("iptables")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        // Try alternative paths
        if !std::path::Path::new("/sbin/iptables").exists()
            && !std::path::Path::new("/usr/sbin/iptables").exists()
        {
            missing.push("iptables (required for NAT networking)");
        }
    }

    // Check for KVM device
    if !std::path::Path::new("/dev/kvm").exists() {
        missing.push("/dev/kvm (KVM kernel module required for virtualization)");
    }

    // Check for bundled cloud-hypervisor
    let ch_path = std::path::Path::new(data_dir).join("bin/cloud-hypervisor");
    if !ch_path.exists() {
        // Check in common fallback locations
        let fallback_paths = vec![
            "/usr/bin/cloud-hypervisor",
            "/usr/local/bin/cloud-hypervisor",
        ];
        let found = fallback_paths.iter().any(|p| std::path::Path::new(p).exists());
        if !found {
            missing.push("cloud-hypervisor (not found in data_dir/bin or /usr/bin)");
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing.join("\n  - "))
    }
}

mod dns;
mod ui;
mod state;
mod api;
mod auth;
mod swarm;
mod metrics;
mod timemachine;
mod auto_snapshot;
mod hibernation;
mod grpc;
mod vm_service;
mod privdrop;
#[cfg(feature = "chaos")]
mod chaos;
#[cfg(feature = "chaos")]
mod chaos_tests;

use state::{AppState, wal::Wal, recovery::Recovery};
use api::handlers;

#[cfg(feature = "chaos")]
use chaos::enable_chaos_on_startup;

#[derive(Parser, Debug)]
#[command(name = "vyomad", about = "Vyoma MicroVM Daemon", version)]
struct Args {
    /// Path to listen on (Unix Socket)
    #[arg(short, long, default_value = "/run/vyoma/vyoma.sock")]
    socket_path: String,
    /// HTTP port for dashboard (default: 8080, use 0 to disable)
    #[arg(short = 'p', long, default_value_t = 8080)]
    http_port: u16,
    /// HTTP bind IP address (defaults to 127.0.0.1 for security)
    /// Use 0.0.0.0 for remote access (requires --api-token for authentication)
    #[arg(long, default_value = "127.0.0.1")]
    http_bind_ip: String,
    /// API authentication token (optional)
    /// When set, all API endpoints require Authorization: Bearer <token>
    /// Can also be set via VYOMA_API_TOKEN environment variable
    #[arg(long)]
    api_token: Option<String>,
    /// Data directory containing kernel and cloud-hypervisor binaries
    #[arg(short, long, default_value = "/var/lib/vyoma")]
    data_dir: String,
    /// Enable chaos mode for crash injection testing
    #[arg(long)]
    chaos_mode: bool,
    /// Do not drop privileges (keep running as root). For development only.
    #[arg(long)]
    keep_root: bool,
    /// Comma-separated list of allowed CORS origins (defaults to the daemon's own HTTP URL).
    /// Use "*" to allow all origins (not recommended for production).
    #[arg(long)]
    cors_origins: Option<String>,
}

fn main() {
    tracing_subscriber::fmt::init();

    // Parse Args (Handles --help / --version)
    let args = Args::parse();

    // Check critical dependencies before starting
    if let Err(e) = check_dependencies(&args.data_dir) {
        tracing::error!("Missing dependencies: {}", e);
        tracing::error!("Please install the required dependencies and try again.");
        std::process::exit(1);
    }

    // Drop privileges to vyoma user BEFORE starting the multithreaded runtime
    // This ensures that all subsequently created Tokio worker threads inherit the capabilities
    if !args.keep_root {
        match privdrop::drop_privileges() {
            Ok(()) => tracing::info!("Privileges dropped successfully, running as user 'vyoma'"),
            Err(e) => {
                tracing::error!("Failed to drop privileges: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        tracing::info!("Running as root (--keep-root specified)");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime");

    rt.block_on(async_main(args));
}

async fn async_main(args: Args) {
    info!("vyomad (Vyoma Daemon) starting up (Unix Socket: {})...", args.socket_path);

    #[cfg(feature = "chaos")]
    {
        if args.chaos_mode {
            chaos::enable_chaos_on_startup(std::path::Path::new(&args.data_dir));
            info!("Chaos mode enabled!");
        }
    }

    let cgroups = CgroupManager::new();
    if let Err(e) = cgroups.init() {
        error!("Failed to init cgroups: {}", e);
    }
    let cgroups = Arc::new(cgroups);

    // CNI Initialization
    let home = dirs::home_dir().expect("No home dir");
    let cni_config_dir = home.join(".vyoma").join("cni").join("net.d");
    let cni_plugin_dir = home.join(".vyoma").join("cni").join("bin");

    std::fs::create_dir_all(&cni_config_dir).unwrap();
    std::fs::create_dir_all(&cni_plugin_dir).unwrap();

    // Create default bridge
    let bridge_conf = cni_config_dir.join("10-vyoma-bridge.conf");
    if !bridge_conf.exists() {
        let conf = r#"{
    "cniVersion": "0.4.0",
    "name": "vyoma-net",
    "type": "bridge",
    "bridge": "vyoma0",
    "isGateway": true,
    "ipMasq": true,
    "ipam": {
        "type": "host-local",
        "subnet": "172.16.0.0/24",
        "routes": [ { "dst": "0.0.0.0/0" } ]
    }
}"#;
        std::fs::write(&bridge_conf, conf).unwrap();
    }

    let cni_manager = Arc::new(vyoma_core::cni::CniManager::new(
        cni_plugin_dir,
        cni_config_dir,
    ));

    // Initialize WAL (ADR-023)
    let state_dir = home.join(".vyoma").join("state");
    let (db, wal) = match Wal::open_or_create(&state_dir) {
        Ok((db, wal)) => {
            info!("WAL initialized successfully");
            (db, Arc::new(wal))
        }
        Err(e) => {
            error!("Failed to initialize WAL: {}", e);
            panic!("Cannot start without WAL");
        }
    };

    let (events_tx, _rx) = broadcast::channel(100);

    let timemachine = Arc::new(tokio::sync::RwLock::new(timemachine::TimeMachine::new(&db)));

    let data_dir_path = std::path::PathBuf::from(&args.data_dir);

    let node_id = 1u64;

    let network_integration = Arc::new(tokio::sync::Mutex::new(Some(
        crate::swarm::NetworkIntegration::new(data_dir_path.clone())
    )));

    let mut swarm_raft = crate::swarm::SwarmRaft::new(node_id);

    let net_integration = network_integration.lock().await.as_ref().unwrap().clone();
    let callback = crate::swarm::create_network_callback(net_integration);
    swarm_raft.set_side_effect_callback(callback);
    
    let swarm_raft = Arc::new(tokio::sync::Mutex::new(swarm_raft));
    
    // Get API token from CLI arg or environment variable
    let api_token = args.api_token.clone().or_else(|| std::env::var("VYOMA_API_TOKEN").ok());

     let state = AppState {
         vms: Arc::new(TokioMutex::new(HashMap::new())),
         cgroups,
         cni_manager,
         events_tx,
         wal,
         data_dir: args.data_dir.clone(),
         swarm_raft,
         network_integration,
         timemachine,
         policy_manager: Arc::new(StdMutex::new(PolicyManager::new())),
         api_token,
     };

    // Run WAL-based crash recovery and adopt surviving VMs
    let recovered_vms = match Recovery::recover_on_startup(&home, &state.wal, &state).await {
        Ok(vms) => vms,
        Err(e) => {
            warn!("Recovery scan failed: {}", e);
            vec![]
        }
    };

    // Adopt recovered VMs into state
    for rvm in recovered_vms {
        if matches!(rvm.status, crate::state::recovery::VmRecoveryStatus::Running) {
            info!("Adopting recovered VM: {}", rvm.vm_id);

            let vm_dir = home.join(".vyoma").join("vms").join(&rvm.vm_id);
            let ch_socket = vm_dir.join("ch.sock").to_string_lossy().to_string();

            let vmm = vyoma_core::vmm::VmmManager::new(&ch_socket);

            let mut proxy_tasks = Vec::new();
            for port in &rvm.state.ports {
                let task = ProxyManager::start_proxy(
                    port.host_port,
                    rvm.state.ip_address.clone(),
                    port.vm_port,
                );
                proxy_tasks.push(task);
            }

            if !rvm.state.volumes.is_empty() {
                warn!("VM {} has volumes but virtiofsd cannot be restarted after daemon restart - manual remount may be required", rvm.vm_id);
            }

            let vm_instance = crate::state::VmInstance {
                vmm,
                id: rvm.vm_id.clone(),
                status: crate::state::VmStatus::Running,
                attestation_status: None,
                ch_socket_path: ch_socket,
                tap_name: rvm.state.tap_name.clone(),
                dm_name: rvm.state.dm_name.clone(),
                loop_devices: rvm.state.loop_devices.clone(),
                cow_file_path: rvm.state.cow_file_path.clone(),
                ip_address: rvm.state.ip_address.clone(),
                proxy_tasks,
                fs_managers: vec![],
                vtpm_manager: None,
                cgroup_path: rvm.state.cgroup_path.clone(),
                netns_path: rvm.state.netns_path.clone(),
                config_ports: rvm.state.ports.clone(),
                config_volumes: rvm.state.volumes.clone(),
                hostname: rvm.state.hostname.clone(),
                labels: rvm.state.labels.clone(),
                networks: rvm.state.networks.clone(),
                base_image_path: rvm.state.base_image_path.clone(),
                vcpu: rvm.state.vcpu,
                mem_size_mib: rvm.state.mem_size_mib,
                attestation_task: None,
                last_cpu_sample: None,
            };
            
            let mut vms = state.vms.lock().await;
            vms.insert(rvm.vm_id, Arc::new(tokio::sync::Mutex::new(vm_instance)));
        }
    }

    api::handlers::start_process_monitor(state.clone()).await;

    // Start DNS
    dns::start_dns_server(state.clone()).await;

    let shutdown_state = state.clone();

    // Create API routes with auth middleware
    let api_routes = Router::new()
        .route("/run", post(handlers::run_vm))
        .route("/pull", post(handlers::pull_image_handler))
        .route("/stop/:id", post(handlers::stop_vm))
        .route("/pause/:id", post(handlers::pause_vm))
        .route("/resume/:id", post(handlers::resume_vm))
        .route("/ps", get(handlers::list_vms))
        .route("/exec/:id", post(handlers::exec_in_vm))
        .route("/commit/:id", post(handlers::commit_vm))
        .route("/snapshot/:id", post(handlers::snapshot_vm))
        .route("/history/:id", get(handlers::history_vm))
        .route("/time-travel", post(handlers::time_travel_vm))
        .route("/restore", post(handlers::restore_vm))
        .route("/logs/:id", get(handlers::stream_logs))
        .route("/build", post(handlers::build_image))
        .route("/images", get(handlers::list_images_handler))
        .route("/volumes", get(handlers::list_volumes_handler))
        .route(
            "/networks",
            get(handlers::list_networks_handler).post(handlers::create_network_handler),
        )
        .route(
            "/networks/:name",
            axum::routing::delete(handlers::delete_network_handler),
        )
        .route("/events", get(handlers::events_handler))
        .route("/vms/:id", get(handlers::inspect_vm_handler))
        .route("/swarm/init", post(handlers::swarm_init_handler))
        .route("/swarm/join", post(handlers::swarm_join_handler))
        .route("/swarm/nodes", get(handlers::swarm_nodes_handler))
        .route("/teleport", post(handlers::teleport_handler))
        .route("/teleport/status/:session_id", get(handlers::teleport_status_handler))
        .route("/receive-teleport", post(handlers::receive_teleport_handler))
        .route("/adopt-teleported-vm", post(handlers::adopt_teleported_vm))
        .route("/policy", get(handlers::get_policy_handler).post(handlers::set_policy_handler))
        .route("/attest/:id", post(handlers::attest_vm_handler))
        .route("/vms/:id/stats", get(handlers::get_vm_stats))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth::auth_middleware));

    // Health check and UI are public (no auth required)
    let public_routes = Router::new()
        .route("/health", get(handlers::health_check));

    // Build allowed CORS origins from configuration
    let mut allowed_origins: Vec<String> = Vec::new();
    let http_port = args.http_port;

    // Default origin from the daemon's HTTP address
    let default_origin = format!("http://{}:{}", args.http_bind_ip, http_port);
    allowed_origins.push(default_origin);

    // Always add localhost origins for convenience
    if http_port > 0 {
        allowed_origins.push(format!("http://localhost:{}", http_port));
        allowed_origins.push(format!("http://127.0.0.1:{}", http_port));
    }

    // Add user-specified origins
    if let Some(ref custom_origins) = args.cors_origins {
        for origin in custom_origins.split(',') {
            let origin = origin.trim().to_string();
            if !origin.is_empty() {
                allowed_origins.push(origin);
            }
        }
    }

    // Build CORS layer
    let cors_layer = if allowed_origins.iter().any(|o| o == "*") {
        warn!("CORS is configured to allow any origin. This is insecure and should only be used in development.");
        CorsLayer::permissive()
    } else {
        let header_values: Vec<http::HeaderValue> = allowed_origins
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        info!("CORS allowed origins: {:?}", allowed_origins);
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(header_values))
            .allow_methods([Method::GET, Method::POST, Method::DELETE])
            .allow_headers(vec![
                http::header::AUTHORIZATION,
                http::header::CONTENT_TYPE,
            ])
    };

    // Combine: public routes + API routes (with auth) + UI fallback (public)
    let app = public_routes
        .merge(api_routes)
        .fallback(ui::ui_handler)
        .layer(cors_layer)
        .with_state(state.clone());

    let socket_path = args.socket_path;
    let path = std::path::Path::new(&socket_path);
    if let Some(parent) = path.parent() {
        // Try to create directory
        match std::fs::create_dir_all(parent) {
            Ok(_) => {}
            Err(e) => {
                warn!("Cannot create directory {}: {}", parent.display(), e);
            }
        }
    }
    
    // Try to bind, if fails try alternative locations
    let actual_socket_path: String;
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => {
            actual_socket_path = socket_path.clone();
            l
        }
        Err(_) => {
            // Try alternative: XDG_RUNTIME_DIR or /tmp
            let alt_dir = std::env::var("XDG_RUNTIME_DIR")
                .map(|d| std::path::PathBuf::from(d))
                .unwrap_or_else(|_| std::env::temp_dir());
            
            let alt_socket_path = alt_dir.join("vyoma.sock");
            warn!("Cannot bind to {}. Trying alternative: {}", socket_path, alt_socket_path.display());
            
            let _ = std::fs::remove_file(&alt_socket_path);
            match UnixListener::bind(&alt_socket_path) {
                Ok(l) => {
                    actual_socket_path = alt_socket_path.to_string_lossy().to_string();
                    warn!("Using alternative socket at {}", actual_socket_path);
                    l
                }
                Err(e) => {
                    error!("Failed to bind socket at any location: {}", e);
                    std::process::exit(1);
                }
            }
        }
    };

    // Set socket permissions: root:vyoma (0660) - users in vyoma group can access
    let permissions = std::fs::Permissions::from_mode(0o660);
    if let Err(e) = std::fs::set_permissions(&actual_socket_path, permissions) {
        warn!("Could not set 0660 permissions on socket: {}", e);
    }

    // Change socket ownership to vyoma:vyoma so CLI can access it
    let pw = unsafe { libc::getpwnam(b"vyoma\0".as_ptr() as *const _) };
    if !pw.is_null() {
        let uid = unsafe { (*pw).pw_uid };
        let gid = unsafe { (*pw).pw_gid };
        let path_c = std::ffi::CString::new(actual_socket_path.clone()).unwrap();
        unsafe { libc::chown(path_c.as_ptr(), uid, gid); }
        info!("Socket ownership changed to vyoma:vyoma");
    }

    info!("Daemon listening on Unix Socket {}", actual_socket_path);

    // Privileges are now dropped before Tokio runtime initialization

    // Start HTTP server for dashboard if port is not 0
    if args.http_port > 0 {
        let http_app = app.clone();
        let api_token = args.api_token.clone();
        tokio::spawn(async move {
            let addr = format!("{}:{}", args.http_bind_ip, args.http_port);
            // Warn if binding to non-localhost address
            if !args.http_bind_ip.starts_with("127.") && args.http_bind_ip != "::1" && args.http_bind_ip != "localhost" {
                if api_token.is_none() {
                    warn!("HTTP server bound on non-localhost address {} WITHOUT authentication! Use --api-token for security", addr);
                } else {
                    warn!("HTTP server bound on non-localhost address {} with authentication enabled", addr);
                }
            }
            info!("Dashboard available at http://{}", addr);
            let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let io = TokioIo::new(stream);
                        let tower_service = http_app.clone();
                        tokio::spawn(async move {
                            let hyper_service = hyper::service::service_fn(move |request| {
                                tower_service.clone().call(request)
                            });
                            let _ = auto::Builder::new(TokioExecutor::new())
                                .serve_connection(io, hyper_service)
                                .await;
                        });
                    }
                    Err(e) => error!("HTTP accept error: {}", e),
                }
            }
        });
    }

    let grpc_state = Arc::new(state);
    tokio::spawn(async move {
        let addr = "[::1]:7071".parse().unwrap();
        info!("gRPC interface available at {}", addr);
        use vyoma_proto::v1::vm_service_server::VmServiceServer;
        
        let svc = VmServiceServer::new(grpc::GrpcVmService::new(grpc_state.clone()));
        
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(svc)
            .serve(addr)
            .await
        {
            error!("gRPC server error: {}", e);
        }
    });

    let shutdown_rx = handlers::shutdown_signal(shutdown_state);
    let mut shutdown_flag = Box::pin(shutdown_rx);

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let io = TokioIo::new(stream);
                        let tower_service = app.clone();
                        
                        tokio::spawn(async move {
                            let hyper_service = hyper::service::service_fn(move |request: axum::extract::Request<hyper::body::Incoming>| {
                                tower_service.clone().call(request)
                            });

                            if let Err(err) = auto::Builder::new(TokioExecutor::new())
                                .serve_connection(io, hyper_service)
                                .await
                            {
                                error!("Error parsing connection: {:?}", err);
                            }
                        });
                    }
                    Err(e) => error!("Failed to accept connection: {}", e),
                }
            }
            _ = &mut shutdown_flag => {
                info!("Daemon shutting down gracefully...");
                break;
            }
        }
    }
    
    let _ = std::fs::remove_file(&socket_path);
}
