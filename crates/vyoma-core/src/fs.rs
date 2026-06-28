use anyhow::{anyhow, Result};
use std::process::{Command, Child};
use std::path::Path;
use tracing::info;

#[derive(Debug)]
pub struct VirtioFsManager {
    process: Option<Child>,
    socket_path: String,
    tag: String,
}

impl VirtioFsManager {
    pub fn new(tag: &str, socket_path: &str) -> Self {
        Self {
            tag: tag.to_string(),
            socket_path: socket_path.to_string(),
            process: None,
        }
    }

    pub fn start(&mut self, source_path: &str) -> Result<()> {
        // Try to find virtiofsd in priority order (ADR 021)
        let binary = if Path::new("/opt/vyoma/bin/virtiofsd").exists() {
            "/opt/vyoma/bin/virtiofsd"
        } else if Path::new("/usr/lib/vyoma/virtiofsd").exists() {
            "/usr/lib/vyoma/virtiofsd"
        } else if Path::new("/usr/libexec/vyoma/virtiofsd").exists() {
            "/usr/libexec/vyoma/virtiofsd"
        } else if Path::new("bin/virtiofsd").exists() {
            "bin/virtiofsd"
        } else {
            "virtiofsd"
        };
        info!("Starting virtiofsd for tag {} on socket {} using binary: {}", self.tag, self.socket_path, binary);

        // Ensure socket doesn't exist
        if Path::new(&self.socket_path).exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        let child = Command::new(binary)
            .arg(format!("--socket-path={}", self.socket_path))
            .arg(format!("--shared-dir={}", source_path))
            .arg("--sandbox=none") // Required for unprivileged execution (if rootless) or simple setup
            .arg("--seccomp=none") // Relax security for MVP
             // .arg("--log-level=debug")
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn virtiofsd (is it installed?): {}", e))?;

        self.process = Some(child);
        
        // Wait for socket to appear (up to 1s)
        let loop_delay = std::time::Duration::from_millis(50);
        for _ in 0..20 {
            if Path::new(&self.socket_path).exists() {
                return Ok(());
            }
            std::thread::sleep(loop_delay);
        }
        
        // If we timeout, we return error but importantly, we should check if process died.
        // But for MVP, we assume timeout.
        Err(anyhow!("Timed out waiting for virtiofsd socket"))
    }

    pub fn kill(&mut self) -> Result<()> {
        if let Some(mut child) = self.process.take() {
            info!("Killing virtiofsd for tag {}", self.tag);
            child.kill()?;
            child.wait()?;
        }
        if Path::new(&self.socket_path).exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
        Ok(())
    }

    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if let Some(child) = self.process.as_mut() {
             child.try_wait().map_err(|e| anyhow!("Failed to wait on child: {}", e))
        } else {
             Ok(None)
        }
    }
}

impl Drop for VirtioFsManager {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}
