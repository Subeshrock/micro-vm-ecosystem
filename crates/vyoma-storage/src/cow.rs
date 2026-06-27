use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use tracing::{info, warn};
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
        info!("Attaching loop device to {:?}", file);
        
        if !file.exists() {
            return Err(StorageError::NotFound(format!("File not found: {:?}", file)));
        }
        
        let ld = self.control.next_free().map_err(|e| StorageError::Io(e))?;
        
        // Ensure read/write with retry for udev race condition
        let mut retries = 0;
        loop {
            match ld.with().read_only(false).attach(file) {
                Ok(_) => break,
                Err(e) if e.raw_os_error() == Some(13) && retries < 50 => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    retries += 1;
                }
                Err(e) => return Err(StorageError::Io(e)),
            }
        }
        
        let loop_path = ld.path().unwrap_or_else(|| PathBuf::from(""));
        Ok(LoopDevice::new(loop_path, Some(ld)))
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
        let mut devices = Vec::new();
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
