use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use tracing::{info, warn, error};
use loopdev::LoopControl;

use crate::error::{StorageError, Result};

pub struct LoopDevice {
    pub path: PathBuf,
    // Store the underlying loopdev device so we can detach it natively
    device: Option<loopdev::LoopDevice>,
}

impl LoopDevice {
    pub fn new(path: PathBuf, device: Option<loopdev::LoopDevice>) -> Self {
        Self { path, device }
    }
    
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub struct LoopManager {
    control: LoopControl,
}

impl LoopManager {
    pub fn new() -> Result<Self> {
        info!("Initializing Loop manager");
        let control = LoopControl::open().map_err(|e| StorageError::Io(e))?;
        Ok(Self { control })
    }
    
    pub fn attach(&self, file: &Path) -> Result<LoopDevice> {
        self.attach_internal(file, false)
    }

    pub fn attach_readonly(&self, file: &Path) -> Result<LoopDevice> {
        self.attach_internal(file, true)
    }

    fn attach_internal(&self, file: &Path, readonly: bool) -> Result<LoopDevice> {
        info!("Attaching loop device to {:?} (readonly: {})", file, readonly);
        
        if !file.exists() {
            return Err(StorageError::NotFound(format!("File not found: {:?}", file)));
        }

        match std::fs::OpenOptions::new().read(true).write(!readonly).open(file) {
            Ok(_) => info!("Successfully opened backing file {:?}", file),
            Err(e) => error!("Failed to open backing file {:?} with write={}: {:?}", file, !readonly, e),
        }
        
        let mut retries = 0;
        let _ld = loop {
            let next_free_result = self.control.next_free();
            let attach_result: std::io::Result<LoopDevice> = match next_free_result {
                Ok(ld) => {
                    let res = ld.with().read_only(readonly).attach(file);
                    match res {
                        Ok(()) => return Ok(LoopDevice::new(ld.path().unwrap(), Some(ld))),
                        Err(e) => Err(e),
                    }
                },
                Err(e) => Err(e),
            };

            if let Err(e) = attach_result {
                if e.raw_os_error() == Some(13) {
                    warn!("loopdev failed with EACCES, falling back to losetup...");
                    let mut cmd = std::process::Command::new("losetup");
                    cmd.arg("-f").arg("--show");
                    if readonly {
                        cmd.arg("-r");
                    }
                    cmd.arg(file);
                    match cmd.output() {
                        Ok(output) if output.status.success() => {
                            let stdout = String::from_utf8_lossy(&output.stdout);
                            let dev_path = stdout.trim();
                            info!("losetup fallback succeeded: {}", dev_path);
                            return Ok(LoopDevice::new(std::path::PathBuf::from(dev_path), None));
                        }
                        Ok(output) => {
                            error!("losetup fallback failed: {}", String::from_utf8_lossy(&output.stderr));
                            if retries < 50 {
                                std::thread::sleep(std::time::Duration::from_millis(20));
                                retries += 1;
                                continue;
                            }
                            return Err(StorageError::Io(e));
                        }
                        Err(err) => {
                            error!("Failed to execute losetup: {}", err);
                            return Err(StorageError::Io(e));
                        }
                    }
                } else {
                    error!("loopdev operation failed: {:?}", e);
                    return Err(StorageError::Io(e));
                }
            }
        };
    }
    
    pub fn detach(&self, device: &LoopDevice) -> Result<()> {
        info!("Detaching loop device {:?}", device.path);
        
        if let Some(ld) = &device.device {
            ld.detach().map_err(|e| StorageError::Io(e))?;
        } else {
            let path_str = match device.path.to_str() {
                Some(s) => s,
                None => {
                    warn!("Loop device path is not valid UTF-8; skipping detach");
                    return Ok(());
                }
            };
            #[cfg(unix)]
            {
                let path = std::path::Path::new(path_str);
                let metadata = match std::fs::metadata(path) {
                    Ok(m) => m,
                    Err(_) => {
                        info!("Loop device {} already removed; nothing to detach", path_str);
                        return Ok(());
                    }
                };
                if !metadata.file_type().is_block_device() {
                    warn!("Path {} is not a block device; skipping detach", path_str);
                    return Ok(());
                }
                match loopdev::LoopDevice::open(path_str) {
                    Ok(ld) => {
                        if let Err(e) = ld.detach() {
                            warn!("Failed to detach loop device {} via fallback: {}; assuming it is already freed", path_str, e);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to open loop device {} via fallback: {}; assuming already detached", path_str, e);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                warn!("Loop device detachment not supported on non-Unix platforms");
            }
        }
        
        Ok(())
    }
    
    pub fn create_cow_file(path: &Path, size_mb: u64) -> Result<()> {
        info!("Creating COW file: {:?} ({} MB)", path, size_mb);
        
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        let file = File::create(path)?;
        file.set_len(size_mb * 1024 * 1024)?;
        
        info!("COW file created successfully");
        Ok(())
    }
    
    pub fn get_size(path: &Path) -> Result<u64> {
        let metadata = std::fs::metadata(path)?;
        Ok(metadata.len())
    }
    
    pub fn is_attached(&self, device: &LoopDevice) -> Result<bool> {
        Ok(device.path.exists()) // Proper check might involve interrogating LoopControl natively
    }
    
    pub fn list_devices(&self) -> Result<Vec<LoopDevice>> {
        info!("Listing loop devices");
        let devices = Vec::new();
        // Since list_devices isn't trivially exposed or needed safely right now, we keep it minimum
        Ok(devices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    #[test]
    fn test_create_cow_file() {
        let temp_dir = TempDir::new().unwrap();
        let cow_path = temp_dir.path().join("test.cow");

        LoopManager::create_cow_file(&cow_path, 100).unwrap();

        assert!(cow_path.exists());
        assert!(LoopManager::get_size(&cow_path).unwrap() > 0);
    }

    #[test]
    #[ignore = "requires root privileges for loop device control"]
    fn test_detach_non_existent_path_returns_ok() {
        let manager = LoopManager::new().expect("Need root to create LoopManager");
        let non_existent_path = PathBuf::from("/dev/loop99999");
        let device = LoopDevice::new(non_existent_path, None);

        let result = manager.detach(&device);
        assert!(result.is_ok());
    }

    #[test]
    #[ignore = "requires root privileges for loop device control"]
    fn test_detach_non_block_device_returns_ok() {
        let manager = LoopManager::new().expect("Need root to create LoopManager");
        let temp_dir = TempDir::new().unwrap();
        let regular_file = temp_dir.path().join("not_a_device");
        std::fs::write(&regular_file, b"test").unwrap();

        let device = LoopDevice::new(regular_file, None);
        let result = manager.detach(&device);
        assert!(result.is_ok());
    }
}
