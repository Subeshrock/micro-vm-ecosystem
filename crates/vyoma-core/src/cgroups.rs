use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::info;

pub struct CgroupManager {
    root_path: String,
}

impl CgroupManager {
    pub fn new() -> Self {
        // Cgroup v2 mount point
        Self {
            root_path: "/sys/fs/cgroup/vyoma.slice".to_string(),
        }
    }

    /// Initializes the root vyoma slice.
    pub fn init(&self) -> Result<()> {
        let path = Path::new(&self.root_path);
        if !path.exists() {
            info!("Creating root cgroup slice: {}", self.root_path);
            fs::create_dir_all(path)?;
        }
            
        // Enable controllers in subtree
        // We usually want cpu, memory, io
        // Check what is available in root cgroup
        let controllers_path = Path::new("/sys/fs/cgroup/cgroup.controllers");
        let available = fs::read_to_string(controllers_path).unwrap_or_default();
        
        let mut subtree_control = String::new();
        // Split available to match exactly "cpu" and not "cpuset"
        let avail_list: Vec<&str> = available.split_whitespace().collect();
        if avail_list.contains(&"cpu") { subtree_control.push_str("+cpu "); }
        if avail_list.contains(&"memory") { subtree_control.push_str("+memory "); }
        if avail_list.contains(&"io") { subtree_control.push_str("+io "); }
        
        if !subtree_control.is_empty() {
            let control_path = path.join("cgroup.subtree_control");
            if control_path.exists() {
                 if let Err(e) = fs::write(&control_path, subtree_control.trim()) {
                     tracing::warn!("Failed to enable cgroup controllers: {}", e);
                 }
            }
        }
        Ok(())
    }

    /// Creates a cgroup for a specific VM.
    /// Returns the absolute path to the created cgroup directory.
    pub fn create_vm_cgroup(&self, vm_id: &str) -> Result<String> {
        let vm_cgroup_path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id));
        if !vm_cgroup_path.exists() {
            fs::create_dir_all(&vm_cgroup_path)?;
        }
        Ok(vm_cgroup_path.to_string_lossy().to_string())
    }

    /// Sets CPU limit (quota/period).
    /// vcpu_percentage: 100 = 1 core, 50 = 0.5 core.
    pub fn set_cpu_limit(&self, vm_id: &str, vcpu_percentage: u32) -> Result<()> {
        let path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id));
        
        // cpu.max: "quota period"
        // period usually 100000 (100ms)
        // quota = vcpu_percentage * 1000
        let period = 100000;
        let quota = vcpu_percentage * 1000;
        
        let file_path = path.join("cpu.max");
        fs::write(file_path, format!("{} {}", quota, period))?;
        Ok(())
    }

    /// Sets Memory limit in bytes.
    pub fn set_memory_limit(&self, vm_id: &str, bytes: u64) -> Result<()> {
        let path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id));
        let file_path = path.join("memory.max");
        fs::write(file_path, bytes.to_string())?;
        Ok(())
    }

    /// Adds a process ID to the cgroup.
    pub fn add_process(&self, vm_id: &str, pid: u32) -> Result<()> {
        let path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id));
        let file_path = path.join("cgroup.procs");
        fs::write(file_path, pid.to_string())?;
        Ok(())
    }
    
    pub fn remove_vm_cgroup(&self, vm_id: &str) -> Result<()> {
         let path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id));
         if path.exists() {
             fs::remove_dir(&path)?;
         }
         Ok(())
    }

    pub fn get_oom_kill_count(&self, vm_id: &str) -> Result<u64> {
        let path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id)).join("memory.events");
        if !path.exists() {
             return Ok(0);
        }
        let content = fs::read_to_string(path)?;
        for line in content.lines() {
            if line.starts_with("oom_kill ") {
                 if let Some(val_str) = line.split_whitespace().nth(1) {
                     return Ok(val_str.parse().unwrap_or(0));
                 }
            }
        }
        Ok(0)
    }

    pub fn get_cpu_usage_usec(&self, vm_id: &str) -> Result<u64> {
        let path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id)).join("cpu.stat");
        let content = fs::read_to_string(&path)?;
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("usage_usec ") {
                return Ok(val.trim().parse::<u64>()?);
            }
        }
        anyhow::bail!("usage_usec not found in cpu.stat")
    }

    pub fn get_memory_current(&self, vm_id: &str) -> Result<u64> {
        let path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id)).join("memory.current");
        Ok(fs::read_to_string(&path)?.trim().parse::<u64>()?)
    }

    pub fn get_memory_max(&self, vm_id: &str) -> Result<u64> {
        let path = Path::new(&self.root_path).join(format!("vyoma-{}", vm_id)).join("memory.max");
        let content = fs::read_to_string(&path)?.trim().to_string();
        if content == "max" {
            Ok(0) // unlimited indicator
        } else {
            Ok(content.parse::<u64>()?)
        }
    }
}
