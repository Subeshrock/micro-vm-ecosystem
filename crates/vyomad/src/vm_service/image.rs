use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use async_trait::async_trait;
use vyoma_core::layers::LayerManager;
use vyoma_core::oci::OciManager;
use vyoma_image::{VmifConverter, VmifManifest, OciImageConfig as VyomaOciConfig, SquashfsCompression};

use super::types::PreparedImage;

pub async fn ensure_image_locally(image_name: &str, data_dir: &str) -> Result<PathBuf> {
    let images_root = PathBuf::from(data_dir).join(".vyoma").join("images");
    std::fs::create_dir_all(&images_root)?;

    let safe_image_name = image_name.replace('/', "_").replace(':', "_");
    let image_store_path = images_root.join(&safe_image_name);
    let manifest_path = image_store_path.join("vyoma.toml");
    let rootfs_sqfs_path = image_store_path.join("rootfs.sqfs");

    if !rootfs_sqfs_path.exists() {
        info!("Image {} not found locally. Pulling...", image_name);
        std::fs::create_dir_all(&image_store_path)?;

        let mut oci = OciManager::new();
        let manifest_json = oci
            .pull_manifest(image_name)
            .await
            .context("Pull manifest failed")?;

        let layers = oci
            .parse_layers(&manifest_json)
            .context("Parse layers failed")?;

        let mut oci_config: Option<VyomaOciConfig> = None;
        if let Ok(config_digest) = oci.parse_config_digest(&manifest_json) {
            info!("Fetching OCI config blob: {}", config_digest);
            if let Ok(config) = oci.pull_config_blob(image_name, &config_digest).await {
                let config_path = image_store_path.join("vyoma-config.json");
                if let Ok(json_str) = serde_json::to_string_pretty(&config) {
                    if let Err(e) = std::fs::write(&config_path, json_str) {
                        warn!("Failed to write vyoma-config.json: {}", e);
                    } else {
                        info!("Saved OCI configuration to {:?}", config_path);
                    }
                }
                oci_config = Some(VyomaOciConfig {
                    entrypoint: config.entrypoint,
                    cmd: config.cmd,
                    env: config.env,
                    working_dir: config.working_dir,
                    exposed_ports: config.exposed_ports,
                    user: config.user,
                });
            }
        }

        let temp_unpack_dir = tempfile::tempdir().context("Failed to create temp dir")?;

        for digest in layers {
            let layer_data = oci.pull_layer(image_name, &digest)
                .await
                .context(format!("Failed layer {}", digest))?;
            LayerManager::unpack_layer(&layer_data, temp_unpack_dir.path())
                .context("Unpack failed")?;
        }

        let converter = VmifConverter::new();
        let config = oci_config.unwrap_or_else(|| VyomaOciConfig::default());
        
        let _vmif_image = converter.convert_directory_to_vmif(
            temp_unpack_dir.path(),
            &image_store_path,
            image_name,
            "amd64",
            config,
            None,
            None,
            SquashfsCompression::default(),
        ).context("VMIF conversion failed")?;

        info!("Image {} converted to VMIF successfully", image_name);
    } else {
        info!("VMIF image found locally at {:?}", rootfs_sqfs_path);
    }

    Ok(rootfs_sqfs_path)
}

pub async fn load_vmif_manifest(image_name: &str, data_dir: &str) -> Result<VmifManifest> {
    let images_root = PathBuf::from(data_dir).join(".vyoma").join("images");
    
    let safe_image_name = image_name.replace('/', "_").replace(':', "_");
    let image_store_path = images_root.join(&safe_image_name);
    let manifest_path = image_store_path.join("vyoma.toml");
    
    VmifConverter::load_manifest(&manifest_path)
        .context("Failed to load VMIF manifest")
}

#[async_trait]
pub trait ImageProvider: Send + Sync {
    async fn fetch_image(&self, image_name: &str) -> Result<PathBuf>;
    async fn get_config(&self, image_path: &PathBuf) -> Result<vyoma_core::oci::OciImageConfig>;
    async fn get_vmif_manifest(&self, image_name: &str) -> Result<Option<vyoma_image::VmifManifest>>;
}

pub struct OciImageProvider {
    pub data_dir: String,
}

#[async_trait]
impl ImageProvider for OciImageProvider {
    async fn fetch_image(&self, image_name: &str) -> Result<PathBuf> {
        ensure_image_locally(image_name, &self.data_dir).await
    }

    async fn get_config(&self, image_path: &PathBuf) -> Result<vyoma_core::oci::OciImageConfig> {
        extract_oci_config(image_path)
    }

    async fn get_vmif_manifest(&self, image_name: &str) -> Result<Option<vyoma_image::VmifManifest>> {
        match load_vmif_manifest(image_name, &self.data_dir).await {
            Ok(m) => Ok(Some(m)),
            Err(_) => Ok(None),
        }
    }
}

pub struct CachedImageProvider {
    cache_dir: PathBuf,
    data_dir: String,
}

impl CachedImageProvider {
    pub fn new(data_dir: &str) -> Result<Self> {
        let cache_dir = PathBuf::from(data_dir).join(".vyoma").join("images");
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self { cache_dir, data_dir: data_dir.to_string() })
    }

    pub fn get_cached_path(&self, image_name: &str) -> Option<PathBuf> {
        let sanitized = image_name.replace(':', "_").replace('/', "_");
        let image_dir = self.cache_dir.join(&sanitized);
        let sqfs_path = image_dir.join("rootfs.sqfs");
        let manifest_path = image_dir.join("vyoma.toml");
        if sqfs_path.exists() && manifest_path.exists() {
            Some(sqfs_path)
        } else {
            None
        }
    }
}

#[async_trait]
impl ImageProvider for CachedImageProvider {
    async fn fetch_image(&self, image_name: &str) -> Result<PathBuf> {
        if let Some(cached) = self.get_cached_path(image_name) {
            info!("Using cached VMIF image for {}", image_name);
            return Ok(cached);
        }
        ensure_image_locally(image_name, &self.data_dir).await
    }

    async fn get_config(&self, image_path: &PathBuf) -> Result<vyoma_core::oci::OciImageConfig> {
        extract_oci_config(image_path)
    }

    async fn get_vmif_manifest(&self, image_name: &str) -> Result<Option<vyoma_image::VmifManifest>> {
        match load_vmif_manifest(image_name, &self.data_dir).await {
            Ok(m) => Ok(Some(m)),
            Err(_) => Ok(None),
        }
    }
}

pub async fn prepare_image(image_name: &str, data_dir: &str) -> Result<PreparedImage> {
    prepare_image_with_provider(image_name, &OciImageProvider { data_dir: data_dir.to_string() }).await
}

pub fn resolve_kernel_from_manifest(manifest: &Option<vyoma_image::VmifManifest>, data_dir: &str) -> Option<PathBuf> {
    let kernel_ref = manifest.as_ref()?.kernel.as_ref()?;
    
    if kernel_ref.starts_with("sha256:") {
        let hash = kernel_ref.trim_start_matches("sha256:");
        resolve_kernel_by_hash(hash, data_dir)
    } else if kernel_ref.starts_with("kernels/") {
        resolve_kernel_by_tag(kernel_ref, data_dir)
    } else {
        resolve_kernel_by_tag(kernel_ref, data_dir)
    }
}

fn resolve_kernel_by_hash(hash: &str, data_dir: &str) -> Option<PathBuf> {
    let kernel_store = std::path::Path::new(data_dir).join("kernels");
    let kernel_path = kernel_store.join(hash);
    if kernel_path.exists() {
        Some(kernel_path)
    } else {
        warn!("Kernel with hash {} not found in kernel store", hash);
        None
    }
}

fn resolve_kernel_by_tag(tag: &str, data_dir: &str) -> Option<PathBuf> {
    let kernel_store = std::path::Path::new(data_dir).join("kernels");
    
    if let Ok(entries) = std::fs::read_dir(&kernel_store) {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name == tag || file_name == format!("{}.vmlinuz", tag) {
                return Some(entry.path());
            }
        }
    }
    
    warn!("Kernel with tag {} not found in kernel store", tag);
    None
}

pub fn get_default_kernel_path(data_dir: &str) -> PathBuf {
    std::path::Path::new(data_dir).join("bin/vmlinux")
}

pub async fn prepare_image_with_provider<P: ImageProvider>(
    image_name: &str,
    provider: &P,
) -> Result<PreparedImage> {
    info!("Preparing VMIF image: {}", image_name);
    let image_path = provider.fetch_image(image_name).await?;
    let config = provider.get_config(&image_path).await?;
    let manifest = provider.get_vmif_manifest(image_name).await?;

    Ok(PreparedImage {
        rootfs_sqfs_path: image_path,
        manifest,
        config,
        kernel_path: None,
    })
}

pub fn extract_oci_config(image_path: &std::path::Path) -> Result<vyoma_core::oci::OciImageConfig> {
    let config_path = image_path.parent().unwrap().join("vyoma-config.json");

    if config_path.exists() {
        let config_str = std::fs::read_to_string(&config_path).context("Failed to read config")?;
        let config: vyoma_core::oci::OciImageConfig = serde_json::from_str(&config_str)
            .context("Failed to parse OCI config")?;
        Ok(config)
    } else {
        warn!("No OCI config found at {:?}, using defaults", config_path);
        Ok(vyoma_core::oci::OciImageConfig::default())
    }
}

pub async fn ensure_image_locally_handler(
    image_name: &str,
    data_dir: &str,
) -> Result<std::path::PathBuf, (axum::http::StatusCode, String)> {
    ensure_image_locally(image_name, data_dir)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}