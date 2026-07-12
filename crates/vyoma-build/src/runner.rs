use std::collections::HashMap;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use tracing::{info, warn, error};
use vyoma_core::oci::OciImageConfig;
use vyoma_core::vtpm::VtpmManager;
use vyoma_core::cgroups::CgroupManager;
use vyoma_image::{VmifConverter, SquashfsCompression, SignedManifest, SigningKeyPair};
use chrono;
use std::process::Command;
use tokio::time::{timeout, Duration};
use std::sync::Arc;
use tempfile::TempDir;

use crate::Instruction;

use crate::{BuildResult, BuildError, Vyomafile};

struct BuildResourceGuard {
    loop_devices: Vec<vyoma_storage::cow::LoopDevice>,
    dm_name: Option<String>,
    cgroup_vm_id: Option<String>,
    temp_dir: Option<TempDir>,
}

impl BuildResourceGuard {
    fn new(
        loop_devices: Vec<vyoma_storage::cow::LoopDevice>,
        dm_name: Option<String>,
        cgroup_vm_id: Option<String>,
        temp_dir: Option<TempDir>,
    ) -> Self {
        Self {
            loop_devices,
            dm_name,
            cgroup_vm_id,
            temp_dir,
        }
    }
}

impl Drop for BuildResourceGuard {
    fn drop(&mut self) {
        info!("BuildResourceGuard: cleaning up build resources");

        let loop_mgr = match vyoma_storage::cow::LoopManager::new() {
            Ok(mgr) => mgr,
            Err(e) => {
                warn!("BuildResourceGuard: failed to create loop manager: {}", e);
                return;
            }
        };

        for device in self.loop_devices.drain(..) {
            if let Err(e) = loop_mgr.detach(&device) {
                warn!("BuildResourceGuard: failed to detach loop device {}: {}", device.path.display(), e);
            } else {
                info!("BuildResourceGuard: detached loop device {}", device.path.display());
            }
        }

        if let Some(ref dm_name) = self.dm_name {
            let dm_mgr = match vyoma_storage::dm::DmManager::new() {
                Ok(mgr) => mgr,
                Err(e) => {
                    warn!("BuildResourceGuard: failed to create DM manager: {}", e);
                    return;
                }
            };
            if let Err(e) = dm_mgr.remove_snapshot(dm_name) {
                warn!("BuildResourceGuard: failed to remove DM snapshot {}: {}", dm_name, e);
            } else {
                info!("BuildResourceGuard: removed DM snapshot {}", dm_name);
            }
        }

        if let Some(ref cgroup_vm_id) = self.cgroup_vm_id {
            let cgroup_mgr = CgroupManager::new();
            if let Err(e) = cgroup_mgr.remove_vm_cgroup(cgroup_vm_id) {
                warn!("BuildResourceGuard: failed to remove cgroup {}: {}", cgroup_vm_id, e);
            } else {
                info!("BuildResourceGuard: removed cgroup {}", cgroup_vm_id);
            }
        }

        info!("BuildResourceGuard: cleanup complete");
    }
}

/// Core build engine that executes Vyomafile instructions in isolated VMs
pub struct BuildRunner {
    pub work_dir: PathBuf,
    temp_dir: PathBuf,
    /// If true, perform measured build: launch ephemeral VM, capture PCRs, sign manifest.
    pub measured: bool,
    /// Optional path to a signing key for manifest signing.
    pub signing_key_path: Option<String>,
    /// Cache of converted ext4 base images (image name -> ext4 path)
    ext4_cache: std::collections::HashMap<String, PathBuf>,
    /// Optional cgroup manager for resource limits
    cgroups: Option<Arc<CgroupManager>>,
}

impl BuildRunner {
    pub fn new(work_dir: PathBuf) -> Self {
        let temp_dir = work_dir.join("temp");
        Self {
            work_dir,
            temp_dir,
            measured: false,
            signing_key_path: None,
            ext4_cache: std::collections::HashMap::new(),
            cgroups: None,
        }
    }

    pub fn with_measured(mut self, measured: bool, signing_key_path: Option<String>) -> Self {
        self.measured = measured;
        self.signing_key_path = signing_key_path;
        self
    }

    pub fn with_cgroups(mut self, cgroups: Arc<CgroupManager>) -> Self {
        self.cgroups = Some(cgroups);
        self
    }

    /// Execute a complete build from Vyomafile
    pub async fn build(
        &mut self,
        vyomafile_path: &Path,
        context_dir: &Path,
        image_name: &str,
    ) -> Result<BuildResult, BuildError> {
        info!("Starting VM-isolated build for {} (measured={})", image_name, self.measured);

        // Parse Vyomafile
        let vyomafile = Vyomafile::parse(vyomafile_path)
            .map_err(|e| BuildError::ParseError(e.to_string()))?;

        // Initialize build state
        let mut current_rootfs: Option<PathBuf> = None;
        let mut current_config = OciImageConfig {
            entrypoint: None,
            cmd: None,
            env: Some(Vec::new()),
            working_dir: None,
            exposed_ports: None,
            user: None,
        };

        // Process each instruction
        for instruction in &vyomafile.instructions {
            match instruction {
                Instruction::From { image } => {
                    info!("Processing FROM {}", image);
                    current_rootfs = Some(self.handle_from(&image).await?);
                }
                Instruction::Run { command } => {
                    info!("Processing RUN {}", command);
                    if let Some(ref rootfs) = current_rootfs {
                        let new_rootfs = self.handle_run(rootfs, &command).await?;
                        current_rootfs = Some(new_rootfs);
                    } else {
                        return Err(BuildError::ExecutionError(
                            "RUN instruction without FROM".to_string()
                        ));
                    }
                }
                Instruction::Copy { src, dst } => {
                    info!("Processing COPY {} -> {}", src, dst);
                    if let Some(ref rootfs) = current_rootfs {
                        self.handle_copy(rootfs, context_dir, &src, &dst).await?;
                    } else {
                        return Err(BuildError::ExecutionError(
                            "COPY instruction without FROM".to_string()
                        ));
                    }
                }
                Instruction::Cmd { args } => {
                    info!("Processing CMD {:?}", args);
                    current_config.cmd = Some(args.clone());
                }
                Instruction::Entrypoint { args } => {
                    info!("Processing ENTRYPOINT {:?}", args);
                    current_config.entrypoint = Some(args.clone());
                }
                Instruction::Env { key, value } => {
                    info!("Processing ENV {}={}", key, value);
                    if let Some(ref mut env_vars) = current_config.env {
                        env_vars.push(format!("{}={}", key, value));
                    } else {
                        current_config.env = Some(vec![format!("{}={}", key, value)]);
                    }
                }
                Instruction::Workdir { path } => {
                    info!("Processing WORKDIR {}", path);
                    current_config.working_dir = Some(path.clone());
                }
                Instruction::VmMeasuredBoot => {
                    info!("Processing VM_MEASURED_BOOT directive - measured boot enabled");
                    // The measured flag is already set at the build runner level,
                    // so we just log here for clarity
                }
            }
        }

        // Finalize the image
        if let Some(final_rootfs) = current_rootfs {
            self.finalize_image(&final_rootfs, image_name, &current_config).await
        } else {
            Err(BuildError::ExecutionError(
                "No FROM instruction found".to_string()
            ))
        }
    }

    /// Launch an ephemeral VM to pre-compute expected PCR values.
    /// The VM boots the final rootfs with OVMF firmware and a vTPM.
    /// After boot, PCR values are read and the VM is destroyed.
    async fn measure_boot_pcr(&self, rootfs_path: &Path) -> Result<HashMap<u32, String>, BuildError> {
        info!("Starting ephemeral measurement VM for PCR pre-computation");

        let measure_vm_dir = self.temp_dir.join("measure-vm");
        std::fs::create_dir_all(&measure_vm_dir)
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create measure VM dir: {}", e)))?;

        // Find kernel and initrd from the built rootfs or use defaults
        let kernel_path = self.find_kernel_path()
            .map_err(|e| BuildError::ExecutionError(format!("Kernel not found: {}", e)))?;

        // 1. Start vTPM
        let mut vtpm = VtpmManager::new("measure-vm", &self.temp_dir)
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create vTPM: {}", e)))?;
        vtpm.start()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to start vTPM: {}", e)))?;
        info!("vTPM started at {}", vtpm.socket_path());

        let tpm_socket = vtpm.socket_path().to_string();

        // 2. Build Cloud Hypervisor config for the measurement VM
        let ch_socket_path = measure_vm_dir.join("ch.sock");

        // We'll build the CH args manually for the measurement VM
        let mut ch_args = vec![
            "--kernel".to_string(),
            kernel_path.to_string_lossy().to_string(),
            "--memory".to_string(),
            "size=512M".to_string(),
            "--cpus".to_string(),
            "boot=1".to_string(),
            "--console".to_string(),
            "off".to_string(),
            "--serial".to_string(),
            "tty".to_string(),
            "--api-socket".to_string(),
            ch_socket_path.to_string_lossy().to_string(),
            "--rng".to_string(),
            "src=/dev/urandom".to_string(),
            "--tpm".to_string(),
            format!("socket={}", tpm_socket),
        ];

        // Add rootfs drive
        let rootfs_str = rootfs_path.to_string_lossy().to_string();
        ch_args.extend_from_slice(&[
            "--disk".to_string(),
            format!("path={},readonly=on", rootfs_str),
        ]);

        // Check if OVMF firmware exists
        let ovmf_paths = [
            Path::new("/usr/share/OVMF/OVMF_CODE.fd"),
            Path::new("/usr/share/qemu/ovmf-x64/OVMF_CODE.fd"),
            Path::new("/usr/share/edk2/ovmf/x64/OVMF_CODE.fd"),
        ];

        if let Some(fw_path) = ovmf_paths.iter().find(|p| p.exists()) {
            ch_args.extend_from_slice(&[
                "--firmware".to_string(),
                fw_path.to_string_lossy().to_string(),
            ]);
            info!("Using OVMF firmware: {:?}", fw_path);
        } else {
            warn!("OVMF firmware not found in standard locations, measurement VM will use direct boot");
        }

        info!("Launching measurement VM with args: {:?}", ch_args);

        // 3. Launch Cloud Hypervisor
        let mut child = Command::new(self.find_ch_path()?)
            .args(&ch_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to start cloud-hypervisor: {}", e)))?;

        // Wait for socket
        let timeout_duration = Duration::from_secs(10);
        let start = std::time::Instant::now();
        while !ch_socket_path.exists() {
            if start.elapsed() > timeout_duration {
                let _ = child.kill();
                return Err(BuildError::ExecutionError(
                    "Timed out waiting for Cloud Hypervisor socket".to_string()
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // 4. Boot the VM via API
        let client = reqwest::Client::builder()
            .unix_socket(ch_socket_path)
            .build()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to build HTTP client: {}", e)))?;

        // Create VM
        let vm_config = serde_json::json!({
            "vcpu": { "boot_vcpus": 1, "max_vcpus": 1 },
            "memory": { "size": 512 * 1024 * 1024, "shared": true },
            "payload": {
                "kernel": kernel_path.to_string_lossy().to_string(),
                "cmdline": "console=ttyS0 reboot=k panic=1 root=/dev/vda rw init=/bin/sh"
            },
            "disks": [{
                "path": rootfs_path.to_string_lossy().to_string(),
                "readonly": true
            }],
            "tpm": {
                "socket": tpm_socket
            }
        });

        // Allow time for firmware measurement during boot
        // Use a generous boot timeout since firmware + kernel + initrd need to be measured
        let boot_timeout = Duration::from_secs(30);

        let _ = timeout(boot_timeout, async {
            // Try to create and boot the VM
            let _ = client
                .put("http://localhost/api/v1/vm.create")
                .json(&vm_config)
                .send()
                .await;
            let _ = client
                .put("http://localhost/api/v1/vm.boot")
                .json(&serde_json::json!({}))
                .send()
                .await;
        }).await;

        // 5. Wait a bit more for measurements to settle
        tokio::time::sleep(Duration::from_secs(5)).await;

        // 6. Read PCR values from vTPM
        let pcrs = vtpm.read_pcrs(&[0, 1, 4, 5, 7, 9, 10, 14])
            .map_err(|e| BuildError::ExecutionError(format!("Failed to read PCRs: {}", e)))?;

        info!("Captured PCR values: {:?}", pcrs);

        // 7. Cleanup: kill the measurement VM and vTPM
        let _ = child.kill();
        let _ = child.wait();
        drop(vtpm);

        // Clean up measurement VM directory
        let _ = std::fs::remove_dir_all(&measure_vm_dir);

        Ok(pcrs)
    }

    async fn handle_from(&self, image: &str) -> Result<PathBuf, BuildError> {
        // For now, we'll assume the image is already available locally
        // In a real implementation, this would call ensure_image_locally
        let image_path = self.work_dir.join(".vyoma").join("images").join(image.replace('/', "_").replace(':', "_"));
        let rootfs_path = image_path.join("rootfs.sqfs");

        if !rootfs_path.exists() {
            return Err(BuildError::ExecutionError(
                format!("Base image {} not found at {:?}", image, rootfs_path)
            ));
        }

        Ok(rootfs_path)
    }

    async fn handle_run(&mut self, rootfs_path: &Path, command: &str) -> Result<PathBuf, BuildError> {
        info!("Executing RUN command in real VM: {}", command);
        
        let build_id = format!("build-{}", chrono::Utc::now().timestamp_millis());
        let build_dir = self.temp_dir.join(&build_id);
        std::fs::create_dir_all(&build_dir)
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create build dir: {}", e)))?;
        
        let result = self.execute_build_in_vm(rootfs_path, command, &build_dir).await;
        
        if let Ok(layer_path) = result {
            let final_layer = self.temp_dir.join(format!("layer_{}.sqfs", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
            if let Err(e) = std::fs::rename(&layer_path, &final_layer) {
                let _ = std::fs::remove_dir_all(&build_dir);
                return Err(BuildError::ExecutionError(format!("Failed to preserve layer: {}", e)));
            }
            let _ = std::fs::remove_dir_all(&build_dir);
            Ok(final_layer)
        } else {
            let _ = std::fs::remove_dir_all(&build_dir);
            result
        }
    }
    
    async fn execute_build_in_vm(
        &mut self,
        base_squashfs: &Path,
        command: &str,
        build_dir: &Path,
    ) -> Result<PathBuf, BuildError> {
        let cache_key = base_squashfs.to_string_lossy().to_string();

        let ext4_path = if let Some(cached_ext4) = self.ext4_cache.get(&cache_key) {
            if cached_ext4.exists() {
                info!("Using cached ext4 base image: {:?}", cached_ext4);
                cached_ext4.clone()
            } else {
                self.create_cached_ext4(base_squashfs, &cache_key).await?
            }
        } else {
            self.create_cached_ext4(base_squashfs, &cache_key).await?
        };

        let cow_path = build_dir.join("cow.img");
        let dm_name = format!("vyoma-build-{}", std::process::id());
        let new_layer_path = build_dir.join("layer.sqfs");
        let cgroup_vm_id = format!("vyoma-build-{}", std::process::id());

        info!("Using ext4 base: {:?}", ext4_path);

        vyoma_storage::cow::LoopManager::create_cow_file(&cow_path, 1024)
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create COW file: {}", e)))?;

        let loop_mgr = vyoma_storage::cow::LoopManager::new()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create loop manager: {}", e)))?;
        let base_loop = loop_mgr.attach(&ext4_path)
            .map_err(|e| BuildError::ExecutionError(format!("Failed to attach base ext4: {}", e)))?;
        let cow_loop = loop_mgr.attach(&cow_path)
            .map_err(|e| BuildError::ExecutionError(format!("Failed to attach COW: {}", e)))?;

        info!("Creating device mapper snapshot");
        let dm_mgr = vyoma_storage::dm::DmManager::new()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create DM manager: {}", e)))?;
        let dm_dev = dm_mgr.create_snapshot(
            &dm_name,
            base_loop.path(),
            cow_loop.path(),
        ).map_err(|e| BuildError::ExecutionError(format!("Failed to create DM snapshot: {}", e)))?;

        let loop_devices = vec![base_loop, cow_loop];

        let cgroup_id = if self.cgroups.is_some() {
            Some(cgroup_vm_id.as_str())
        } else {
            None
        };

        let vm_result = self.run_command_in_ch(&dm_dev.path().to_path_buf(), command, build_dir, cgroup_id).await;

        let cgroup_id_to_store = if self.cgroups.is_some() {
            Some(cgroup_vm_id.clone())
        } else {
            None
        };

        let guard = BuildResourceGuard::new(
            loop_devices,
            Some(dm_name.clone()),
            cgroup_id_to_store,
            None,
        );

        let res = match vm_result {
            Ok((0, _)) => {
                info!("Creating new squashfs layer from DM device");
                let dm_device_path = PathBuf::from(format!("/dev/mapper/{}", dm_name));
                if dm_device_path.exists() {
                    self.ext4_to_squashfs(&dm_device_path, &new_layer_path).await?;
                    Ok(new_layer_path)
                } else {
                    Err(BuildError::ExecutionError(
                        "DM device not found for layer creation".to_string()
                    ))
                }
            }
            Ok((code, _)) => Err(BuildError::ExecutionError(
                format!("Build command failed with exit code {}", code)
            )),
            Err(e) => Err(e),
        };

        // Explicitly drop guard here to clean up loops/dm before returning
        drop(guard);
        
        let _ = std::fs::remove_file(&cow_path);
        
        res
    }
    
    async fn squashfs_to_ext4(&self, squashfs: &Path, ext4: &Path) -> Result<(), BuildError> {
        let temp_dir = tempfile::tempdir()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create temp dir: {}", e)))?;
        
        self.extract_squashfs(squashfs, temp_dir.path()).await?;
        
        let ext4_str = ext4.to_string_lossy();
        
        // Create a 2G sparse file
        let trunc_out = Command::new("truncate")
            .args(["-s", "2G", &ext4_str])
            .output()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to run truncate: {}", e)))?;
            
        if !trunc_out.status.success() {
            return Err(BuildError::ExecutionError(
                format!("truncate failed: {}", String::from_utf8_lossy(&trunc_out.stderr))
            ));
        }
            
        let output = Command::new("mkfs.ext4")
            .args([
                "-F",
                "-d",
                &temp_dir.path().to_string_lossy(),
                &ext4_str
            ])
            .output()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to run mkfs.ext4: {}", e)))?;
        
        if !output.status.success() {
            return Err(BuildError::ExecutionError(
                format!("mkfs.ext4 failed: {}", String::from_utf8_lossy(&output.stderr))
            ));
        }
        
        Ok(())
    }
    
    async fn create_cached_ext4(&mut self, squashfs: &Path, cache_key: &str) -> Result<PathBuf, BuildError> {
        let cache_dir = self.work_dir.join("ext4-cache");
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create cache dir: {}", e)))?;
        
        let safe_key = cache_key.replace('/', "_").replace(':', "_").replace('\\', "_");
        let ext4_path = cache_dir.join(format!("{}.ext4", safe_key));
        
        if !ext4_path.exists() {
            info!("Creating cached ext4 from {:?} -> {:?}", squashfs, ext4_path);
            self.squashfs_to_ext4(squashfs, &ext4_path).await?;
            self.ext4_cache.insert(cache_key.to_string(), ext4_path.clone());
            info!("Cached ext4 created and stored in cache");
        }
        
        Ok(ext4_path)
    }
    
    async fn ext4_to_squashfs(&self, ext4: &Path, squashfs: &Path) -> Result<(), BuildError> {
        let mount_dir = tempfile::tempdir()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to create mount dir: {}", e)))?;
        
        let ext4_c = std::ffi::CString::new(ext4.to_string_lossy().as_bytes())
            .map_err(|e| BuildError::ExecutionError(format!("CString error: {}", e)))?;
        let mount_dir_c = std::ffi::CString::new(mount_dir.path().to_string_lossy().as_bytes())
            .map_err(|e| BuildError::ExecutionError(format!("CString error: {}", e)))?;
        let fstype_c = std::ffi::CString::new("ext4").unwrap();
        
        let res = unsafe {
            libc::mount(
                ext4_c.as_ptr(),
                mount_dir_c.as_ptr(),
                fstype_c.as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        if res != 0 {
            return Err(BuildError::ExecutionError(format!("mount syscall failed: {}", std::io::Error::last_os_error())));
        }
        
        let output = Command::new("mksquashfs")
            .arg(mount_dir.path())
            .arg(squashfs)
            .arg("-comp")
            .arg("zstd")
            .arg("-quiet")
            .output()
            .map_err(|e| BuildError::ExecutionError(format!("Failed to execute mksquashfs: {}", e)))?;
        
        let res_umount = unsafe {
            libc::umount(mount_dir_c.as_ptr())
        };
        if res_umount != 0 {
            tracing::warn!("umount syscall failed: {}", std::io::Error::last_os_error());
        }
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BuildError::ExecutionError(format!("mksquashfs failed: {}", stderr)));
        }
        
        Ok(())
    }
    
    async fn run_command_in_ch(
        &self,
        rootdisk: &Path,
        command: &str,
        vm_dir: &Path,
        cgroup_vm_id: Option<&str>,
    ) -> Result<(i32, Option<u32>), BuildError> {
        info!("Launching Cloud Hypervisor for build command: {}", command);

        let kernel_path = self.find_kernel_path()?;
        let initramfs_path = self.create_build_initramfs(command).await?;
        let socket_path = vm_dir.join("ch.sock");

        let ch_args = vec![
            "--kernel".to_string(),
            kernel_path.to_string_lossy().to_string(),
            "--initramfs".to_string(),
            initramfs_path.to_string_lossy().to_string(),
            "--disk".to_string(),
            format!("path={}", rootdisk.to_string_lossy()),
            "--console".to_string(),
            "off".to_string(),
            "--serial".to_string(),
            "tty".to_string(),
            "--api-socket".to_string(),
            socket_path.to_string_lossy().to_string(),
            "--cpus".to_string(),
            "boot=1".to_string(),
            "--memory".to_string(),
            "size=512M".to_string(),
            "--cmdline".to_string(),
            "console=ttyS0 reboot=k panic=1 quiet".to_string(),
        ];

        let mut child = Command::new(self.find_ch_path()?)
            .args(&ch_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| BuildError::VmError(format!("Failed to start cloud-hypervisor: {}", e)))?;

        let pid = child.id();

        if let Some(cgroup_id) = cgroup_vm_id {
            if let Some(ref cgroups) = self.cgroups {
                let cgroup_id_only = cgroup_id.trim_start_matches("vyoma-build-");
                if let Err(e) = cgroups.create_vm_cgroup(cgroup_id_only) {
                    warn!("Failed to create cgroup {}: {}", cgroup_id, e);
                } else {
                    if let Err(e) = cgroups.set_cpu_limit(cgroup_id_only, 100) {
                        warn!("Failed to set CPU limit: {}", e);
                    }
                    if let Err(e) = cgroups.set_memory_limit(cgroup_id_only, 512 * 1024 * 1024) {
                        warn!("Failed to set memory limit: {}", e);
                    }
                    if let Err(e) = cgroups.add_process(cgroup_id_only, pid) {
                        warn!("Failed to add process to cgroup: {}", e);
                    }
                    info!("Added build VM {} to cgroup with PID {}", cgroup_id, pid);
                }
            }
        }

        info!("Starting Cloud Hypervisor with rootdisk: {:?}", rootdisk);

        let timeout_duration = Duration::from_secs(300);
        let exit_status = timeout(timeout_duration, async {
            loop {
                if let Ok(Some(status)) = child.try_wait() {
                    return Ok::<_, std::io::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await
        .map_err(|_| BuildError::VmError("VM execution timed out".to_string()))?
        .map_err(|e| BuildError::VmError(format!("VM process error: {}", e)))?;

        let code = exit_status.code().unwrap_or(1);
        if code != 0 {
            if let Some(mut stderr) = child.stderr.take() {
                use std::io::Read;
                let mut buf = String::new();
                if stderr.read_to_string(&mut buf).is_ok() {
                    error!("Cloud Hypervisor failed. stderr: {}", buf);
                }
            }
            if let Some(mut stdout) = child.stdout.take() {
                use std::io::Read;
                let mut buf = String::new();
                if stdout.read_to_string(&mut buf).is_ok() {
                    error!("Cloud Hypervisor failed. stdout: {}", buf);
                }
            }
        }
        info!("Build VM exited with code: {}", code);
        Ok((code, Some(pid)))
    }

    async fn execute_in_vm(&self, command: &str) -> Result<i32, BuildError> {
        info!("Launching Cloud Hypervisor VM to execute: {}", command);

        // Create build-specific initramfs
        let initramfs_path = self.create_build_initramfs(command).await?;

        // Find kernel path (assume default for now)
        let kernel_path = self.find_kernel_path()?;

        // Create temporary VM directory
        let vm_id = format!("build-{}", std::process::id());
        let vm_dir = self.temp_dir.join(&vm_id);
        std::fs::create_dir_all(&vm_dir)?;

        // Build Cloud Hypervisor configuration
        let socket_path = vm_dir.join("ch.sock");
        let rootfs_path = self.temp_dir.join("temp_root.sqfs"); // Placeholder rootfs
        let ch_args = self.build_ch_args(&rootfs_path, &kernel_path, &initramfs_path, &socket_path);

        // Launch Cloud Hypervisor
        info!("Starting Cloud Hypervisor with args: {:?}", ch_args);
        let mut child = Command::new(self.find_ch_path()?)
            .args(&ch_args)
            .spawn()
            .map_err(|e| BuildError::VmError(format!("Failed to start Cloud Hypervisor: {}", e)))?;

        // Wait for VM to complete with timeout (using tokio::time::timeout with async block)
        let timeout_duration = Duration::from_secs(300); // 5 minute timeout for builds

        let exit_status_result = timeout(timeout_duration, async {
            child.wait()
        }).await;

        let exit_status = match exit_status_result {
            Ok(result) => result.map_err(|e| BuildError::VmError(format!("VM process error: {}", e)))?,
            Err(_) => {
                // Timeout - kill the process
                let _ = child.kill();
                return Err(BuildError::VmError("VM execution timed out".to_string()));
            }
        };

        // Clean up
        let _ = std::fs::remove_dir_all(&vm_dir);

        let exit_code = exit_status.code().unwrap_or(1);
        info!("VM execution completed with exit code: {}", exit_code);

        Ok(exit_code)
    }

    async fn create_build_initramfs(&self, command: &str) -> Result<PathBuf, BuildError> {
        let initramfs_path = self.temp_dir.join("build-initramfs.cpio.gz");

        // Generate build-specific init script
        let init_script = format!(r#"#!/bin/busybox sh

/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sys /sys
/bin/busybox mount -t devtmpfs dev /dev

# Wait for the target root filesystem to appear (max 50 tries)
tries=0
while [ ! -b /dev/vda ]; do
    /bin/busybox sleep 0.1
    tries=$((tries + 1))
    if [ $tries -ge 50 ]; then
        echo "Build VM: Timed out waiting for /dev/vda"
        echo "Build VM: Contents of /dev:"
        /bin/busybox ls -la /dev
        /bin/busybox poweroff -f
        exit 1
    fi
done

/bin/busybox mkdir -p /mnt/root
/bin/busybox mount /dev/vda /mnt/root

# Mount essential filesystems for chroot
/bin/busybox mount -t proc proc /mnt/root/proc
/bin/busybox mount -t sysfs sys /mnt/root/sys
/bin/busybox mount --bind /dev /mnt/root/dev
/bin/busybox mount -t devpts pts /mnt/root/dev/pts 2>/dev/null || true

# Execute the build script inside the root filesystem
/bin/busybox chroot /mnt/root /bin/sh /vyoma-build.sh

# Power off the VM when done
/bin/busybox poweroff -f
echo "Build VM: Executing command..."

cat > /mnt/root/vyoma-build.sh << 'VYOMAEOF'
#!/bin/sh
set -e
{}
VYOMAEOF
chmod +x /mnt/root/vyoma-build.sh

chroot /mnt/root /vyoma-build.sh

# Capture exit code
exit_code=$?
echo "Build VM: Command completed with exit code: $exit_code"

# Unmount filesystems safely
umount /mnt/root/proc 2>/dev/null || true
umount /mnt/root/sys 2>/dev/null || true
umount /mnt/root/dev 2>/dev/null || true
umount /mnt/root 2>/dev/null || true
sync

# Power off (this will cause Cloud Hypervisor to exit)
poweroff -f
"#, command);

        let busybox_binary_usr = PathBuf::from("/usr/bin/busybox");
        let busybox_binary_lib = PathBuf::from("/var/lib/vyoma/bin/busybox");
        let busybox_path = if busybox_binary_lib.exists() {
            Some(&busybox_binary_lib as &Path)
        } else if busybox_binary_usr.exists() {
            Some(&busybox_binary_usr as &Path)
        } else {
            None
        };

        if busybox_path.is_none() {
            warn!("Busybox not found at expected locations. Build VM may fail to boot.");
        }

        vyoma_core::initramfs::create_initramfs(&init_script, None, busybox_path, &initramfs_path)
            .map_err(|e| BuildError::VmError(format!("Failed to create build initramfs: {}", e)))?;

        info!("Created build initramfs at: {:?}", initramfs_path);
        Ok(initramfs_path)
    }

    fn find_kernel_path(&self) -> Result<PathBuf, BuildError> {
        let local_kernel = self.work_dir.join("bin").join("vmlinux");
        if local_kernel.exists() {
            return Ok(local_kernel);
        }
        
        let kernel_path = PathBuf::from("/usr/lib/vyoma/vmlinux");

        if kernel_path.exists() {
            Ok(kernel_path)
        } else {
            Err(BuildError::VmError("Kernel not found at /usr/lib/vyoma/vmlinux or local bin".to_string()))
        }
    }

    fn find_ch_path(&self) -> Result<PathBuf, BuildError> {
        let local_ch = self.work_dir.join("bin").join("cloud-hypervisor");
        if local_ch.exists() {
            return Ok(local_ch);
        }
        
        let ch_path = PathBuf::from("/usr/bin/cloud-hypervisor");

        if ch_path.exists() {
            Ok(ch_path)
        } else {
            Ok(PathBuf::from("cloud-hypervisor"))
        }
    }

    fn build_ch_args(
        &self,
        rootfs_path: &Path,
        kernel_path: &Path,
        initramfs_path: &Path,
        socket_path: &Path,
    ) -> Vec<String> {
        vec![
            "--kernel".to_string(),
            kernel_path.to_string_lossy().to_string(),
            "--initramfs".to_string(),
            initramfs_path.to_string_lossy().to_string(),
            "--disk".to_string(),
            format!("path={},readonly=on", rootfs_path.display()),
            "--console".to_string(),
            "off".to_string(), // Disable console to avoid hanging
            "--serial".to_string(),
            "tty".to_string(),
            "--api-socket".to_string(),
            socket_path.to_string_lossy().to_string(),
            "--cpus".to_string(),
            "boot=1".to_string(), // Single CPU for builds
            "--memory".to_string(),
            "size=512M".to_string(), // 512MB RAM for builds
            "--rng".to_string(),
            "src=/dev/urandom".to_string(),
        ]
    }



    async fn handle_copy(&mut self, rootfs_path: &Path, context_dir: &Path, src: &str, dst: &str) -> Result<(), BuildError> {
        info!("Injecting file {} -> {} using debugfs", src, dst);

        // For squashfs, we can't modify it directly. Instead, we need to:
        // 1. Extract the current squashfs to a temporary directory
        // 2. Copy the file to the appropriate location
        // 3. Create a new squashfs with the updated contents

        let temp_extract_dir = tempfile::tempdir()
            .map_err(|e| BuildError::InjectionError(format!("Failed to create temp dir: {}", e)))?;
        let extract_path = temp_extract_dir.path();

        // Extract the current squashfs
        self.extract_squashfs(rootfs_path, extract_path).await?;

        // Copy the source file to destination
        let src_path = context_dir.join(src);
        if !src_path.exists() {
            return Err(BuildError::InjectionError(
                format!("Source path {} does not exist", src)
            ));
        }

        let dst_path = extract_path.join(dst.trim_start_matches('/'));
        if let Some(parent) = dst_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| BuildError::InjectionError(format!("Failed to create dest dir: {}", e)))?;
        }

        std::fs::copy(&src_path, &dst_path)
            .map_err(|e| BuildError::InjectionError(format!("Failed to copy file: {}", e)))?;

        // Create new squashfs with the injected file
        let new_squashfs_name = format!("layer_{}_injected.sqfs", chrono::Utc::now().timestamp());
        let new_squashfs_path = self.temp_dir.join(&new_squashfs_name);

        VmifConverter::create_squashfs(
            extract_path,
            &new_squashfs_path,
            vyoma_image::SquashfsCompression::default(),
        ).map_err(|e| BuildError::InjectionError(format!("Failed to create new squashfs: {}", e)))?;

        // Replace the original rootfs with the new one
        std::fs::copy(&new_squashfs_path, rootfs_path)
            .map_err(|e| BuildError::InjectionError(format!("Failed to update rootfs: {}", e)))?;

        info!("Successfully injected {} -> {}", src, dst);
        Ok(())
    }

    async fn extract_squashfs(&self, squashfs_path: &Path, dest_dir: &Path) -> Result<(), BuildError> {
        info!("Extracting squashfs: {:?} -> {:?}", squashfs_path, dest_dir);

        // Create destination directory
        std::fs::create_dir_all(dest_dir)
            .map_err(|e| BuildError::InjectionError(format!("Failed to create extract dir: {}", e)))?;

        // Use unsquashfs to extract the squashfs file
        let output = Command::new("unsquashfs")
            .args(&[
                "-f", // force overwrite
                "-d", // destination directory
                &dest_dir.to_string_lossy(),
                &squashfs_path.to_string_lossy(),
            ])
            .output()
            .map_err(|e| BuildError::InjectionError(format!("Failed to run unsquashfs: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BuildError::InjectionError(format!("unsquashfs failed: {}", stderr)));
        }

        info!("Successfully extracted squashfs to: {:?}", dest_dir);
        Ok(())
    }

    async fn finalize_image(
        &self,
        rootfs_path: &Path,
        image_name: &str,
        config: &OciImageConfig,
    ) -> Result<BuildResult, BuildError> {
        info!("Finalizing image {}", image_name);

        // Create output directory
        let output_dir = self.work_dir.join(".vyoma").join("images").join(image_name.replace('/', "_").replace(':', "_"));
        std::fs::create_dir_all(&output_dir)?;

        // Copy the final rootfs
        let final_rootfs = output_dir.join("rootfs.sqfs");
        std::fs::copy(rootfs_path, &final_rootfs)?;

        // Create manifest
        let converter = VmifConverter::new();
        let manifest_path = output_dir.join("vyoma.toml");

        // Convert config types
        let image_config = vyoma_image::OciImageConfig {
            entrypoint: config.entrypoint.clone(),
            cmd: config.cmd.clone(),
            env: config.env.clone(),
            working_dir: config.working_dir.clone(),
            exposed_ports: config.exposed_ports.clone(),
            user: config.user.clone(),
        };

        // Compute actual hash of the rootfs
        let hash = VmifConverter::compute_squashfs_hash(&final_rootfs)
            .map_err(|e| BuildError::ExecutionError(format!("Failed to compute hash: {}", e)))?;

        // If measured build, pre-compute PCRs via ephemeral VM
        let mut pcr_policy: Option<HashMap<u32, String>> = None;
        if self.measured {
            info!("Measured build requested - pre-computing PCR values");
            let pcrs = self.measure_boot_pcr(&final_rootfs).await?;
            pcr_policy = Some(pcrs);
            info!("PCR pre-computation complete: {:?}", pcr_policy);
        }

        let mut manifest = vyoma_image::VmifManifest::new(
            "amd64".to_string(),
            None,
            None,
            format!("sha256:{}", hash),
            image_config,
            std::fs::metadata(&final_rootfs)?.len(),
        );

        // Set measured boot PCR policy if measured build
        if let Some(ref pcrs) = pcr_policy {
            manifest.measured_boot.pcr_policy = Some(pcrs.clone());
        }

        let content = toml::to_string_pretty(&manifest)
            .map_err(|e| BuildError::ExecutionError(e.to_string()))?;
        std::fs::write(&manifest_path, content)?;

        // Sign the manifest if a signing key is available
        let signing_key = self.resolve_signing_key().await?;
        let manifest_signed = if let Some(ref keypair) = signing_key {
            info!("Signing manifest with build key");
            let signed = keypair.sign_manifest(&manifest)
                .map_err(|e| BuildError::ExecutionError(format!("Failed to sign manifest: {}", e)))?;

            let sig_path = output_dir.join("vyoma.toml.sig");
            signed.save_to_file(&sig_path)
                .map_err(|e| BuildError::ExecutionError(format!("Failed to save signed manifest: {}", e)))?;
            info!("Signed manifest saved to {:?}", sig_path);
            true
        } else {
            if self.measured {
                warn!("Measured build requested but no signing key available - manifest will be unsigned");
            }
            false
        };

        Ok(BuildResult {
            image_name: image_name.to_string(),
            rootfs_path: final_rootfs,
            manifest_path,
            config: config.clone(),
            pcr_policy,
            manifest_signed,
        })
    }

    /// Resolve the signing key from the configured path or generate a new one.
    async fn resolve_signing_key(&self) -> Result<Option<SigningKeyPair>, BuildError> {
        // 1. Check if a signing key path was explicitly provided
        if let Some(ref key_path) = self.signing_key_path {
            let path = Path::new(key_path);
            if path.exists() {
                info!("Loading signing key from: {:?}", path);
                let secret_path = path.join("build_signing_key");
                let public_path = path.join("build_signing_key.pub");

                if secret_path.exists() && public_path.exists() {
                    let seed = std::fs::read(&secret_path)
                        .map_err(|e| BuildError::ExecutionError(format!("Failed to read signing key: {}", e)))?;
                    let public = std::fs::read(&public_path)
                        .map_err(|e| BuildError::ExecutionError(format!("Failed to read public key: {}", e)))?;

                    let keypair = SigningKeyPair::from_seed_and_public(&seed, &public)
                        .map_err(|e| BuildError::ExecutionError(format!("Failed to load signing key: {}", e)))?;
                    return Ok(Some(keypair));
                }
            } else {
                // Create directory and generate new key pair
                std::fs::create_dir_all(path)
                    .map_err(|e| BuildError::ExecutionError(format!("Failed to create key dir: {}", e)))?;

                let keypair = SigningKeyPair::generate();
                let (seed, public) = keypair.to_seed_and_public();

                // Save public key
                let public_path = path.join("build_signing_key.pub");
                std::fs::write(&public_path, &public)
                    .map_err(|e| BuildError::ExecutionError(format!("Failed to save public key: {}", e)))?;

                // Save seed (private key material)
                let secret_path = path.join("build_signing_key");
                std::fs::write(&secret_path, &seed)
                    .map_err(|e| BuildError::ExecutionError(format!("Failed to save signing key: {}", e)))?;

                // Set restrictive permissions
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))
                        .ok();
                }

                info!("Generated new build signing key at {:?}", path);
                return Ok(Some(keypair));
            }
        }

        // 2. Check standard location in work_dir
        let standard_path = self.work_dir.join("build_signing_key");
        if standard_path.exists() {
            let public_path = self.work_dir.join("build_signing_key.pub");
            if public_path.exists() {
                let seed = std::fs::read(&standard_path)
                    .map_err(|e| BuildError::ExecutionError(format!("Failed to read signing key: {}", e)))?;
                let public = std::fs::read(&public_path)
                    .map_err(|e| BuildError::ExecutionError(format!("Failed to read public key: {}", e)))?;

                let keypair = SigningKeyPair::from_seed_and_public(&seed, &public)
                    .map_err(|e| BuildError::ExecutionError(format!("Failed to load signing key: {}", e)))?;
                return Ok(Some(keypair));
            }
        }

        Ok(None)
    }
}

impl Default for BuildRunner {
    fn default() -> Self {
        Self::new(PathBuf::from("/tmp/vyoma-build"))
    }
}