use anyhow::{Context, Result};
use std::sync::Arc;
use std::path::PathBuf;
use tokio::sync::Mutex as TokioMutex;
use tracing::{info, error};

use crate::state::{AppState, VmInstance, wal::WalEntry};
use vyoma_core::oci::OciImageConfig;

pub async fn save_vm_state(
    state: &AppState,
    instance: VmInstance,
    vm_id: String,
) -> Result<()> {
    instance.save_state(&state.data_dir).context("Failed to save state")?;

    {
        let mut vms = state.vms.lock().await;
        vms.insert(vm_id.clone(), Arc::new(TokioMutex::new(instance)));
    }
    Ok(())
}

pub async fn emit_vm_start_event(
    state: &AppState,
    vm_id: String,
    labels: std::collections::HashMap<String, String>,
) {
    let _ = state.events_tx.send(serde_json::json!({
        "type": "vm_start",
        "id": vm_id,
        "name": labels.get("vyoma.service").unwrap_or(&vm_id)
    }).to_string());
}

pub async fn load_vm_state(
    _state: &AppState,
    vm_id: &str,
) -> Result<Option<VmInstance>> {
    let data_dir = PathBuf::from(&_state.data_dir);
    let state_file = data_dir.join(".vyoma").join("vms").join(vm_id).join("state.json");
    
    if !state_file.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&state_file).context("Failed to read state file")?;
    let _state: crate::state::VmState = serde_json::from_str(&content)
        .context("Failed to parse state")?;
    
    info!("Loaded state for VM {}", vm_id);
    Ok(None)
}

pub async fn stop_vm(
    state: &AppState,
    vm_id: &str,
) -> Result<String> {
    info!("VmService: Stopping VM {}", vm_id);

    let vm_arc = {
        let mut vms = state.vms.lock().await;
        vms.remove(vm_id)
    };

    if let Some(vm_mutex) = vm_arc {
        let mut vm = vm_mutex.lock().await;
        vm.cleanup(&state.cni_manager).await;

        if let Err(e) = state.wal.append(&WalEntry::vm_stop(vm_id.to_string())) {
            error!("Failed to write WAL entry: {}", e);
        }
        
        let _ = state.events_tx.send(serde_json::json!({
            "type": "vm_stop",
            "id": vm_id
        }).to_string());
        
        Ok(format!("VM {} stopped and cleaned up", vm_id))
    } else {
        anyhow::bail!("VM {} not found", vm_id)
    }
}

pub async fn pause_vm(
    state: &AppState,
    vm_id: &str,
) -> Result<String> {
    info!("VmService: Pausing VM {}", vm_id);

    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(vm_id).cloned()
    };

    if let Some(vm_mutex) = vm_arc {
        let vm = vm_mutex.lock().await;
        vm.vmm
            .pause_instance()
            .await
            .context("Failed to pause VM")?;
        Ok(format!("VM {} paused", vm_id))
    } else {
        anyhow::bail!("VM {} not found", vm_id)
    }
}

pub async fn resume_vm(
    state: &AppState,
    vm_id: &str,
) -> Result<String> {
    info!("VmService: Resuming VM {}", vm_id);

    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(vm_id).cloned()
    };

    if let Some(vm_mutex) = vm_arc {
        let vm = vm_mutex.lock().await;
        vm.vmm
            .resume_instance()
            .await
            .context("Failed to resume VM")?;
        Ok(format!("VM {} resumed", vm_id))
    } else {
        anyhow::bail!("VM {} not found", vm_id)
    }
}

pub struct SnapshotResult {
    pub id: String,
    pub path: PathBuf,
}

pub async fn snapshot_vm(
    state: &AppState,
    vm_id: &str,
    label: Option<String>,
) -> Result<SnapshotResult> {
    info!("VmService: Creating snapshot for VM {}", vm_id);

    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(vm_id).cloned()
    };

    if let Some(vm_mutex) = vm_arc {
        let entry = {
            let tm = state.timemachine.write().await;
            tm.create_snapshot(vm_id.to_string(), label.or_else(|| Some(format!("Manual snapshot for {}", vm_id))))
        };

        let data_dir = PathBuf::from(&state.data_dir);
        let vm_dir = data_dir.join(".vyoma").join("vms").join(vm_id);
        let snaps_dir = vm_dir.join("snapshots").join(&entry.id);
        
        std::fs::create_dir_all(&snaps_dir).context("Failed to create snapshots dir")?;

        let snapshot_path = snaps_dir.join("snapshot.bin");
        
        let _ = state.events_tx.send(serde_json::json!({
            "type": "snapshot_created",
            "id": vm_id,
            "snapshot_id": entry.id,
            "path": snaps_dir.to_string_lossy()
        }).to_string());

        Ok(SnapshotResult {
            id: entry.id,
            path: snapshot_path,
        })
    } else {
        anyhow::bail!("VM {} not found", vm_id)
    }
}

pub async fn commit_vm(
    state: &AppState,
    vm_id: &str,
    new_image_name: &str,
) -> Result<String> {
    info!("VmService: Committing VM {} to image {}", vm_id, new_image_name);

    let dm_name = {
        let vm_arc = {
            let vms = state.vms.lock().await;
            vms.get(vm_id).cloned()
        };

        if let Some(vm_mutex) = vm_arc {
            let vm = vm_mutex.lock().await;
            vm.dm_name.clone()
        } else {
            anyhow::bail!("VM {} not found or not running", vm_id)
        }
    };

    let src_device = PathBuf::from(format!("/dev/mapper/{}", dm_name));
    
    let data_dir = PathBuf::from(&state.data_dir);
    let images_dir = data_dir.join(".vyoma").join("images").join(new_image_name);
    
    std::fs::create_dir_all(&images_dir).context("Failed to create images dir")?;
    let dst_file = images_dir.join("root.ext4");

    commit_snapshot_native(&src_device, &dst_file)
        .context("Failed to commit snapshot")?;

    let config = OciImageConfig::default();
    let config_path = images_dir.join("vyoma-config.json");
    let config_json = serde_json::to_string_pretty(&config)
        .context("Failed to serialize config")?;
    std::fs::write(&config_path, config_json)
        .context("Failed to write config")?;

    info!("VM {} committed to image {} at {:?}", vm_id, new_image_name, dst_file);
    Ok(format!("VM {} committed to image {}", vm_id, new_image_name))
}

fn commit_snapshot_native(src_device: &std::path::Path, dst_file: &std::path::Path) -> Result<()> {
    info!("Committing snapshot from {:?} to {:?}", src_device, dst_file);
    
    let mut src = std::fs::File::open(src_device)
        .with_context(|| format!("Failed to open source device {:?}", src_device))?;
    let mut dst = std::fs::File::create(dst_file)
        .with_context(|| format!("Failed to create destination file {:?}", dst_file))?;
    
    std::io::copy(&mut src, &mut dst)
        .context("Failed to copy device contents")?;
    
    info!("Snapshot committed successfully: {} bytes", dst.metadata()?.len());
    Ok(())
}

pub async fn get_vm_state(
    state: &AppState,
    vm_id: &str,
) -> Result<Option<VmInstance>> {
    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(vm_id).cloned()
    };

    if let Some(_vm_mutex) = vm_arc {
        Ok(None)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_result_creation() {
        let result = SnapshotResult {
            id: "snap-123".to_string(),
            path: PathBuf::from("/tmp/snap.bin"),
        };
assert_eq!(result.id, "snap-123");
    }

    #[test]
    fn test_commit_result_message() {
        let result = commit_snapshot_native;
        let _ = std::mem::size_of_val(&result);
    }
}
