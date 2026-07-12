use std::path::{Path, PathBuf};
use tracing::{info, debug, warn, error};

// Add devicemapper imports
use devicemapper::{DM, DmName, DmOptions, DmUuid, DevId};
use crate::error::{StorageError, Result};

#[derive(Debug, Clone)]
pub struct DmDevice {
    pub name: String,
    pub path: PathBuf,
}

impl DmDevice {
    pub fn new(name: String, path: PathBuf) -> Self {
        Self { name, path }
    }
    
    pub fn path(&self) -> &Path {
        &self.path
    }
    
    pub fn name(&self) -> &str {
        &self.name
    }
}

pub struct DmManager {
    dm: DM,
}

impl DmManager {
    pub fn new() -> Result<Self> {
        info!("Initializing Device Mapper manager");
        
        let groups = unsafe {
            let mut g = vec![0; 32];
            let n = libc::getgroups(32, g.as_mut_ptr());
            if n >= 0 {
                g.truncate(n as usize);
                format!("{:?}", g)
            } else {
                "failed to get groups".to_string()
            }
        };
        info!("Current process groups: {}", groups);
        
        if let Ok(meta) = std::fs::metadata("/dev/mapper/control") {
            use std::os::unix::fs::PermissionsExt;
            info!("/dev/mapper/control exists, mode: {:o}", meta.permissions().mode());
        } else {
            info!("/dev/mapper/control DOES NOT EXIST!");
        }

        let dm = DM::new().map_err(|e| {
            error!("DM::new() failed with: {}", e);
            StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e))
        })?;
        Ok(Self { dm })
    }
    
    // fn device_size_sectors(path: &Path) -> Result<u64> {
    //     use std::os::unix::fs::MetadataExt;
    //     let meta = std::fs::metadata(path).map_err(StorageError::Io)?;
    //     let bytes = meta.size();
    //     Ok(bytes / 512)
    // }

    fn device_size_sectors(path: &Path) -> Result<u64> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::File::open(path)
            .map_err(StorageError::Io)?;
        let mut size: u64 = 0;
        unsafe {
            if libc::ioctl(file.as_raw_fd(), 0x80081272 /* BLKGETSIZE64 */, &mut size as *mut u64) != 0 {
                return Err(StorageError::Io(std::io::Error::last_os_error()));
            }
        }
        Ok(size / 512)
    }

    pub fn create_snapshot(
        &self,
        name: &str,
        base_dev: &Path,
        cow_dev: &Path,
    ) -> Result<DmDevice> {
        info!("Creating snapshot {}: base={:?}, cow={:?}", name, base_dev, cow_dev);

        if !base_dev.exists() {
            return Err(StorageError::NotFound(format!("Base device not found: {:?}", base_dev)));
        }
        if !cow_dev.exists() {
            return Err(StorageError::NotFound(format!("COW device not found: {:?}", cow_dev)));
        }

        let origin_name_str = format!("{}-origin", name);
        let sectors = Self::device_size_sectors(base_dev)?;

        // 1. Create origin linear device via devicemapper API
        let origin_name = DmName::new(&origin_name_str)
            .map_err(|e| StorageError::Other(format!("Invalid DM name {}: {}", origin_name_str, e)))?;
        let origin_uuid_str = uuid::Uuid::new_v4().to_string();
        let origin_uuid = DmUuid::new(&origin_uuid_str)
            .map_err(|e| StorageError::Other(format!("Invalid UUID for origin: {}", e)))?;

        let origin_table = vec![(
            0u64,
            sectors,
            "linear".to_string(),
            format!("{} 0", base_dev.display()),
        )];

        self.dm
            .device_create(&origin_name, Some(&origin_uuid), DmOptions::default())
            .map_err(|e| StorageError::Other(format!("Failed to create origin device via dm API: {}", e)))?;
        self.dm
            .table_load(&DevId::Name(&origin_name), &origin_table, DmOptions::default())
            .map_err(|e| StorageError::Other(format!("Failed to load origin table via dm API: {}", e)))?;

        // Ensure the origin device node appears (dmsetup resume triggers udev)
        let _ = std::process::Command::new("dmsetup")
            .args(&["resume", &origin_name_str])
            .output();

        // Wait for the origin device node to appear
        let origin_dev_path = format!("/dev/mapper/{}", origin_name_str);
        for _ in 0..50 {
            if std::path::Path::new(&origin_dev_path).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !std::path::Path::new(&origin_dev_path).exists() {
            return Err(StorageError::Other(format!(
                "Origin device {} did not appear after waiting",
                origin_dev_path
            )));
        }

        // 2. Create snapshot device via devicemapper API
        let snap_name = DmName::new(name)
            .map_err(|e| StorageError::Other(format!("Invalid DM name {}: {}", name, e)))?;
        let snap_uuid_str = uuid::Uuid::new_v4().to_string();
        let snap_uuid = DmUuid::new(&snap_uuid_str)
            .map_err(|e| StorageError::Other(format!("Invalid UUID for snapshot: {}", e)))?;

        let snap_table = vec![(
            0u64,
            sectors,
            "snapshot".to_string(),
            format!("{} {} P 8", origin_dev_path, cow_dev.display()),
        )];

        self.dm
            .device_create(&snap_name, Some(&snap_uuid), DmOptions::default())
            .map_err(|e| StorageError::Other(format!("Failed to create snapshot device via dm API: {}", e)))?;
        self.dm
            .table_load(&DevId::Name(&snap_name), &snap_table, DmOptions::default())
            .map_err(|e| StorageError::Other(format!("Failed to load snapshot table via dm API: {}", e)))?;

        // Ensure the snapshot device node appears
        let _ = std::process::Command::new("dmsetup")
            .args(&["resume", name])
            .output();

        let path = PathBuf::from(format!("/dev/mapper/{}", name));
        
        // Wait for the snapshot device node to appear
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !path.exists() {
            return Err(StorageError::Other(format!(
                "Snapshot device {} did not appear after waiting",
                path.display()
            )));
        }

        Ok(DmDevice::new(name.to_string(), path))
    }
    
    pub fn remove_snapshot(&self, name: &str) -> Result<()> {
        info!("Removing snapshot {}", name);
        let origin_name_str = format!("{}-origin", name);
        
        // Remove snap first
        let dm_name = DmName::new(name).map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        self.dm.device_remove(&DevId::Name(dm_name), DmOptions::default())
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
            
        // Remove origin
        if let Ok(origin_dm_name) = DmName::new(&origin_name_str) {
            let _ = self.dm.device_remove(&DevId::Name(origin_dm_name), DmOptions::default());
        }

        Ok(())
    }
    
    pub fn list_devices(&self) -> Result<Vec<DmDevice>> {
        info!("Listing device mapper devices");
        
        let devs = self.dm.list_devices().map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let mut dm_devices = Vec::new();
        // Since iterator output in older devicemapper is 2-tuple instead of 3
        for item in devs {
            let dev_name = item.0;
            let path = PathBuf::from(format!("/dev/mapper/{}", dev_name.to_string()));
            dm_devices.push(DmDevice::new(dev_name.to_string(), path));
        }
        
        Ok(dm_devices)
    }
    
    pub fn device_exists(&self, name: &str) -> Result<bool> {
        let dm_name = DmName::new(name).map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        match self.dm.device_info(&DevId::Name(dm_name)) {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    fn activate_device(&self, name: &DmName) -> Result<()> {
        let _ = self
            .dm
            .device_suspend(&DevId::Name(name), DmOptions::default())
            .map_err(|e| StorageError::Other(format!("Failed to activate device {}: {}", name, e)))?;
        Ok(())
    }
}

