use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use std::io::Write;
use flate2::write::GzEncoder;
use flate2::Compression;

pub fn create_initramfs(
    init_script: &str,
    agent_path: Option<&Path>,
    busybox_path: Option<&Path>,
    output_path: &Path,
) -> Result<PathBuf> {
    let file = std::fs::File::create(output_path)
        .with_context(|| format!("Failed to create initramfs at {:?}", output_path))?;
    let gz = GzEncoder::new(file, Compression::default());
    
    let mut output = gz;
    
    write_cpio_dir(&mut output, "bin", 0o755)?;
    write_cpio_dir(&mut output, "sbin", 0o755)?;
    write_cpio_dir(&mut output, "dev", 0o755)?;
    write_cpio_dir(&mut output, "proc", 0o755)?;
    write_cpio_dir(&mut output, "sys", 0o755)?;
    write_cpio_dir(&mut output, "mnt", 0o755)?;

    
    write_cpio_entry(&mut output, "sbin/vyoma-init", init_script.as_bytes(), 0o755)?;
    
    if let Some(path) = agent_path {
        if path.exists() {
            let agent_bytes = std::fs::read(path)?;
            write_cpio_entry(&mut output, "sbin/vyoma-agent-vm", &agent_bytes, 0o755)?;
        }
    }
    
    if let Some(path) = busybox_path {
        if path.exists() {
            let busybox_bytes = std::fs::read(path)?;
            write_cpio_entry(&mut output, "bin/busybox", &busybox_bytes, 0o755)?;
        }
    }
    
    let init_wrapper = "#!/bin/busybox sh\nexec /bin/busybox sh /sbin/vyoma-init\n";
    write_cpio_entry(&mut output, "init", init_wrapper.as_bytes(), 0o755)?;
    
    cpio::newc::trailer(&mut output)?;
    
    let gz = output;
    gz.finish()?;
    
    Ok(output_path.to_path_buf())
}

fn write_cpio_entry<W: Write>(output: &mut W, name: &str, content: &[u8], mode: u32) -> Result<()> {
    let builder = cpio::newc::Builder::new(name)
        .mode(mode | 0o100000)
        .nlink(1)
        .uid(0)
        .gid(0)
        .mtime(0);
    
    let file_size = content.len() as u32;
    let mut writer = builder.write(output, file_size);
    writer.write_all(content)?;
    writer.finish()?;
    
    Ok(())
}

fn write_cpio_dir<W: Write>(output: &mut W, name: &str, mode: u32) -> Result<()> {
    let builder = cpio::newc::Builder::new(name)
        .mode(mode | 0o040000)
        .nlink(2)
        .uid(0)
        .gid(0)
        .mtime(0);
    
    let mut writer = builder.write(output, 0);
    writer.finish()?;
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_create_initramfs() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("initramfs.cpio.gz");

        let init_script = "#!/bin/sh\necho test";
        let result = create_initramfs(init_script, None, None, &output_path);

        assert!(result.is_ok());
        assert!(output_path.exists());
        assert!(output_path.metadata().unwrap().len() > 0);
    }

    #[test]
    fn test_create_initramfs_with_agent() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("initramfs.cpio.gz");

        let fake_agent = temp_dir.path().join("fake_agent");
        std::fs::write(&fake_agent, b"fake binary").unwrap();

        let init_script = "#!/bin/sh\necho test";
        let result = create_initramfs(init_script, Some(&fake_agent), None, &output_path);

        assert!(result.is_ok());
        assert!(output_path.exists());
    }
}
