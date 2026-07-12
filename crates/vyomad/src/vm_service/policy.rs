use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{timeout, Duration, Instant};
use tracing::{info, warn, error};

use crate::state::{AppState, VmStatus};
use vyoma_core::unified_attest::{UnifiedAttestationManager, UnifiedAttestationRequest};
use vyoma_core::attest::{TpmQuote, AttestationResponse};
use vyoma_core::vtpm::VtpmManager;
use vyoma_core::vmm::VmmManager;
use vyoma_image::{VmifConverter, SignedManifest};

/// PCR comparison result for a single PCR index.
#[derive(Debug, Clone)]
pub struct PcrResult {
    pub pcr_index: u32,
    pub expected: String,
    pub actual: String,
    pub verified: bool,
}

/// Result of an attestation verification.
pub struct AttestationResult {
    pub verified: bool,
    pub pcr_results: Vec<PcrResult>,
    pub signed_manifest_verified: bool,
    pub error: Option<String>,
}

/// Configuration for measured boot verification behavior.
#[derive(Debug, Clone)]
pub struct MeasuredBootConfig {
    /// PCR indices to verify during attestation.
    pub pcr_selection: Vec<u32>,
    /// Timeout in seconds for the attestation check.
    pub verification_timeout_secs: u64,
    /// If true, block the VM (kill/pause) when attestation fails.
    pub block_on_failure: bool,
    /// If true, require manifest signature verification before trusting PCR values.
    pub require_signed_manifest: bool,
    /// Directory containing trusted public keys for manifest verification.
    pub trusted_keys_dir: Option<PathBuf>,
}

impl Default for MeasuredBootConfig {
    fn default() -> Self {
        Self {
            pcr_selection: vec![0, 1, 4, 5, 7, 9, 10, 14],
            verification_timeout_secs: 30,
            block_on_failure: true,
            require_signed_manifest: true,
            trusted_keys_dir: None,
        }
    }
}

/// Perform the full attestation check for a VM:
/// 1. Get PCR quote from the vTPM
/// 2. Load and verify the signed manifest
/// 3. Compare PCR values against expected values
/// 4. Update VM status based on result
pub async fn check_policy_and_perform_attestation(
    state: Arc<AppState>,
    vm_id: String,
    vm_dir: PathBuf,
    image_name: String,
    tpm_socket_path: Option<String>,
) -> Result<(), String> {
    let needs_verification = {
        let policy = state.policy_manager.lock().unwrap();
        policy.must_verify_on_boot()
    };

    if !needs_verification {
        info!("Policy verification not required for VM {}", vm_id);
        update_vm_status(&state, &vm_id, VmStatus::Running).await?;
        return Ok(());
    }

    info!("Measured boot policy requires attestation for VM {}", vm_id);

    // Check if vTPM socket is available
    let tpm_socket = match tpm_socket_path {
        Some(path) => path,
        None => {
            let msg = format!("vTPM socket not available for VM {} - cannot perform attestation", vm_id);
            error!("{}", msg);
            handle_attestation_failure(&state, &vm_id, &msg).await;
            return Err(msg);
        }
    };

    // Build attestation config from policy
    let attestation_config = build_attestation_config(&state);
    let verification_timeout = Duration::from_secs(attestation_config.verification_timeout_secs);

    info!("Starting attestation check for VM {} with timeout {}s", vm_id, verification_timeout.as_secs());
    let start = Instant::now();

    // Perform the attestation check with timeout
    let result = timeout(verification_timeout, verify_attestation_from_manifest(
        &image_name,
        &tpm_socket,
        &attestation_config,
    )).await;

    match result {
        Ok(Ok(attestation_result)) => {
            if attestation_result.verified {
                let elapsed = start.elapsed();
                info!("Attestation verification succeeded for VM {} in {:.2?}", vm_id, elapsed);
                update_vm_status(&state, &vm_id, VmStatus::Running).await
                    .map_err(|e| format!("Failed to update VM status: {}", e))?;

                // Update attestation_status to verified
                {
                    let vm_arc = {
                        let mut vms = state.vms.lock().await;
                        vms.get(&vm_id).cloned()
                    };
                    if let Some(vm_arc) = vm_arc {
                        let mut vm = vm_arc.lock().await;
                        vm.attestation_status = Some("VERIFIED".to_string());
                    }
                }

                // Emit SSE event for attestation success
                let _ = state.events_tx.send(serde_json::json!({
                    "type": "vm_attestation",
                    "vm_id": vm_id,
                    "verified": true,
                    "error": null
                }).to_string());

                Ok(())
            } else {
                let elapsed = start.elapsed();
                let error_msg = attestation_result.error
                    .unwrap_or_else(|| "Attestation failed".to_string());
                error!("Attestation verification failed for VM {} after {:.2?}: {}", vm_id, elapsed, error_msg);
                handle_attestation_failure(&state, &vm_id, &format!("Attestation failed: {}", error_msg)).await;
                Err(error_msg)
            }
        }
        Ok(Err(e)) => {
            let elapsed = start.elapsed();
            error!("Attestation verification failed for VM {} after {:.2?}: {}", vm_id, elapsed, e);
            handle_attestation_failure(&state, &vm_id, &format!("Attestation failed: {}", e)).await;
            Err(e)
        }
        Err(_) => {
            // Timeout elapsed
            let elapsed = start.elapsed();
            let msg = format!("Attestation timed out after {:.2?} for VM {}", elapsed, vm_id);
            error!("{}", msg);
            handle_attestation_failure(&state, &vm_id, &msg).await;
            Err(msg)
        }
    }
}

/// The core attestation check logic - gets PCR quote, loads manifest, verifies.
/// Returns an AttestationResult with detailed verification info.
pub async fn verify_attestation_from_manifest(
    image_name: &str,
    tpm_socket: &str,
    config: &MeasuredBootConfig,
) -> Result<AttestationResult, String> {
    // Step 1: Get PCR quote from the vTPM
    info!("Getting PCR quote from vTPM for image {}", image_name);
    let quote_data = get_pcr_quote("attest", tpm_socket)
        .map_err(|e| format!("Failed to get PCR quote: {}", e))?;
    info!("Retrieved PCR quote ({} bytes) for image {}", quote_data.len(), image_name);

    // Step 2: Parse the PCR quote to extract PCR values
    let live_pcrs = vyoma_core::attest::parse_pcr_values(&quote_data)
        .map_err(|e| format!("Failed to parse PCR values from quote: {}", e))?;
    info!("Parsed {} PCR values from quote for image {}", live_pcrs.len(), image_name);

    // Step 3: Load and verify the signed manifest
    info!("Loading signed manifest for image {}", image_name);
    let signed_manifest = load_and_verify_manifest(image_name, config).await?;
    info!("Successfully loaded and verified signed manifest for image {}", image_name);

    // Step 4: Extract expected PCR values from the manifest
    let expected_pcrs = signed_manifest.manifest.measured_boot.pcr_policy
        .ok_or_else(|| format!("No PCR policy found in manifest for image {}", image_name))?;
    info!("Found {} expected PCR values in manifest for image {}", expected_pcrs.len(), image_name);

    // Step 5: Build an AttestationResponse from the parsed quote
    let attestation_response = AttestationResponse {
        vm_id: image_name.to_string(),
        verified: true,
        quote: Some(TpmQuote {
            quote: quote_data,
            signature: Vec::new(),
            pcr_values: live_pcrs.clone(),
            timestamp: String::new(),
        }),
        pcr_results: HashMap::new(),
        error: None,
    };

    // Step 6: Verify PCR values using the unified attestation manager
    let verifier = UnifiedAttestationManager::new();
    let result = verifier.verify_tpm_attestation(&attestation_response, &expected_pcrs)
        .map_err(|e| format!("Attestation verification error: {}", e))?;

    // Step 7: Build detailed PCR results
    let mut pcr_results = Vec::new();
    for (pcr_index, expected_hash) in &expected_pcrs {
        let actual_hash = live_pcrs.get(pcr_index).cloned().unwrap_or_default();
        let verified = actual_hash == *expected_hash;
        pcr_results.push(PcrResult {
            pcr_index: *pcr_index,
            expected: expected_hash.clone(),
            actual: actual_hash,
            verified,
        });
    }

    let signed_manifest_verified = result.verified;

    if !result.verified {
        let error_msg = result.error
            .unwrap_or_else(|| "PCR verification failed".to_string());
        // Log which PCRs failed
        for pcr_result in &pcr_results {
            if !pcr_result.verified {
                warn!("PCR {} verification FAILED - expected value did not match live value: {}",
                    pcr_result.pcr_index, pcr_result.actual);
            }
        }
        return Ok(AttestationResult {
            verified: false,
            pcr_results,
            signed_manifest_verified,
            error: Some(error_msg),
        });
    }

    // Additional direct check: verify all expected PCRs are present and match
    for (pcr_index, expected_hash) in &expected_pcrs {
        if let Some(actual_hash) = live_pcrs.get(pcr_index) {
            if actual_hash != expected_hash {
                return Ok(AttestationResult {
                    verified: false,
                    pcr_results,
                    signed_manifest_verified,
                    error: Some(format!(
                        "PCR {} mismatch after unified verification: expected {}, got {}",
                        pcr_index, expected_hash, actual_hash
                    )),
                });
            }
        } else {
            return Ok(AttestationResult {
                verified: false,
                pcr_results,
                signed_manifest_verified,
                error: Some(format!(
                    "PCR {} not found in live quote but expected in manifest",
                    pcr_index
                )),
            });
        }
    }

    info!("All PCR values verified successfully for image {}", image_name);
    Ok(AttestationResult {
        verified: true,
        pcr_results,
        signed_manifest_verified,
        error: None,
    })
}

/// Get a PCR quote from the vTPM using the tpm2-tools.
///
/// This connects to the vTPM socket and retrieves a quote containing
/// the specified PCR values.
pub fn get_pcr_quote(vm_id: &str, tpm_socket: &str) -> anyhow::Result<Vec<u8>> {
    use std::process::Command;

    let pcr_list = TPM_QUOTE_PCRS.iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let output = Command::new("tpm2_quote")
        .args(&[
            "-T", &format!("socket:path={}", tpm_socket),
            "-c", "/etc/tpm2/tpm2-quote.ak",
            "-g", "sha256",
            "-p", &pcr_list,
            "-o", &format!("/tmp/{}_quote.bin", vm_id),
            "-s", &format!("/tmp/{}_sig.bin", vm_id),
            "-m", &format!("/tmp/{}_msg.bin", vm_id),
            "-f", "sha256",
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to get TPM quote: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("tpm2_quote failed: {}", stderr);
    }

    let quote_path = format!("/tmp/{}_quote.bin", vm_id);
    std::fs::read(&quote_path)
        .map_err(|e| anyhow::anyhow!("Failed to read quote: {}", e))
}

/// Find the image directory on disk, trying multiple known locations.
fn find_image_dir(image_name: &str) -> Result<std::path::PathBuf, String> {
    let home = dirs::home_dir()
        .ok_or_else(|| "No home directory".to_string())?;

    // Try ~/.vyoma/images/ first (used by the build pipeline)
    let vyoma_images = home.join(".vyoma").join("images");
    let image_dir = vyoma_images.join(image_name.replace('/', "_").replace(':', "_"));
    if image_dir.exists() {
        return Ok(image_dir);
    }

    // Fallback to ~/.vyoma/images/
    let vyoma_images = home.join(".vyoma").join("images");
    let image_dir = vyoma_images.join(image_name.replace('/', "_").replace(':', "_"));
    if image_dir.exists() {
        return Ok(image_dir);
    }

    Err(format!("Image directory not found for image: {}", image_name))
}

/// Load and verify the signed manifest from disk.
///
/// If `require_signed_manifest` is true in the config, this will:
/// 1. Load the signed manifest (.sig file)
/// 2. Verify the manifest signature against trusted keys
/// 3. Return the verified manifest
///
/// If the image is unsigned and `require_signed_manifest` is true, this fails.
/// If `require_signed_manifest` is false, unsigned manifests are accepted
/// but PCR verification is skipped (returns None for pcr_policy).
async fn load_and_verify_manifest(
    image_name: &str,
    config: &MeasuredBootConfig,
) -> Result<SignedManifest, String> {
    // Find the image directory
    let image_dir = find_image_dir(image_name)?;

    // Try to load signed manifest first (.sig file)
    let sig_path = image_dir.join("vyoma.toml.sig");
    if sig_path.exists() {
        info!("Loading signed manifest from {:?}", sig_path);
        let signed_manifest = VmifConverter::load_signed_manifest(&sig_path)
            .map_err(|e| format!("Failed to load signed manifest: {}", e))?;

        // If signature verification is required, verify against trusted keys
        if config.require_signed_manifest {
            info!("Verifying manifest signature against trusted keys");
            let trusted_keys_dir = config.trusted_keys_dir
                .as_ref()
                .cloned()
                .unwrap_or_else(|| {
                    let home = dirs::home_dir()
                        .expect("No home directory");
                    home.join(".vyoma").join("keys").join("trusted")
                });

            let mut trust_policy = vyoma_image::signing::TrustPolicy::new(true);
            trust_policy.load_trusted_keys_from_dir(trusted_keys_dir)
                .map_err(|e| format!("Failed to load trusted keys: {}", e))?;

            trust_policy.verify(&signed_manifest)
                .map_err(|e| format!("Manifest signature verification failed: {}", e))?;

            info!("Manifest signature verified successfully");
        } else {
            // Even without requiring signed manifests, verify the signature is valid
            // if keys are available (best effort)
            let trusted_keys_dir = config.trusted_keys_dir
                .as_ref()
                .cloned()
                .unwrap_or_else(|| {
                    let home = dirs::home_dir()
                        .expect("No home directory");
                    home.join(".vyoma").join("keys").join("trusted")
                });

            if trusted_keys_dir.exists() {
                let mut trust_policy = vyoma_image::signing::TrustPolicy::new(false);
                if let Ok(()) = trust_policy.load_trusted_keys_from_dir(trusted_keys_dir) {
                    if let Err(e) = trust_policy.verify(&signed_manifest) {
                        warn!("Manifest signature verification failed (non-fatal): {}", e);
                    } else {
                        info!("Manifest signature verified successfully (non-fatal)");
                    }
                }
            }
        }

        Ok(signed_manifest)
    } else {
        // No signed manifest - try loading unsigned manifest
        let manifest_path = image_dir.join("vyoma.toml");
        if !manifest_path.exists() {
            return Err(format!(
                "Neither signed manifest (vyoma.toml.sig) nor unsigned manifest (vyoma.toml) found in {:?}",
                image_dir
            ));
        }

        if config.require_signed_manifest {
            return Err(format!(
                "Image {} has no signed manifest (vyoma.toml.sig) - signed manifest required by policy. \
                 Build the image with --measured flag to generate a signed manifest.",
                image_name
            ));
        }

        // Accept unsigned manifest but skip PCR verification - log warning
        warn!(
            "Image {} has no signed manifest - PCR verification will be SKIPPED (not recommended for production)",
            image_name
        );

        let manifest = VmifConverter::load_manifest(&manifest_path)
            .map_err(|e| format!("Failed to load unsigned manifest: {}", e))?;

        // Wrap in a SignedManifest for compatibility with verification code
        Ok(SignedManifest {
            manifest,
            signature: Vec::new(),
            public_key: Vec::new(),
        })
    }
}

/// Handle attestation failure based on policy configuration.
///
/// If `block_on_failure` is true, the VM is killed immediately.
/// The VM status is set to AttestationFailed with the failure reason.
async fn handle_attestation_failure(state: &Arc<AppState>, vm_id: &str, reason: &str) {
    error!("Attestation failure for VM {}: {}", vm_id, reason);

    // Set VM status to AttestationFailed
    if let Err(e) = update_vm_status(state, vm_id, VmStatus::AttestationFailed {
        reason: reason.to_string(),
    }).await {
        error!("Failed to update VM status after attestation failure: {}", e);
    }

    // Update attestation_status field
    {
        let vm_arc = {
            let mut vms = state.vms.lock().await;
            vms.get(vm_id).cloned()
        };
        if let Some(vm_arc) = vm_arc {
            let mut vm = vm_arc.lock().await;
            vm.attestation_status = Some("FAILED".to_string());
        }
    }

    // Emit SSE event for attestation failure
    let _ = state.events_tx.send(serde_json::json!({
        "type": "vm_attestation",
        "vm_id": vm_id,
        "verified": false,
        "error": reason
    }).to_string());

    // Check if we should kill the VM
    let should_block = {
        let policy = state.policy_manager.lock().unwrap();
        policy.get_config().measured_boot.block_on_failure
    };

    if should_block {
        info!("Block-on-failure enabled - killing VM {}", vm_id);
        if let Err(e) = kill_vm(state, vm_id).await {
            error!("Failed to kill VM {} after attestation failure: {}", vm_id, e);
        }
    }
}

/// Force-stop a VM by removing it from the active VM map and cleaning up resources.
async fn kill_vm(state: &Arc<AppState>, vm_id: &str) -> Result<(), String> {
    let vm_arc = {
        let mut vms = state.vms.lock().await;
        vms.remove(vm_id)
    };

    if let Some(vm_mutex) = vm_arc {
        let mut vm = vm_mutex.lock().await;
        // Kill the VMM process
        let _ = vm.vmm.kill();
        info!("VM {} killed due to attestation failure", vm_id);
    }

    Ok(())
}

/// Build a MeasuredBootConfig from the current policy manager state.
pub fn build_attestation_config(state: &Arc<AppState>) -> MeasuredBootConfig {
    let policy = state.policy_manager.lock().unwrap();
    let config = policy.get_config();

    MeasuredBootConfig {
        pcr_selection: config.measured_boot.pcr_selection.clone(),
        verification_timeout_secs: config.measured_boot.verification_timeout_secs,
        block_on_failure: config.measured_boot.block_on_failure,
        require_signed_manifest: true,
        trusted_keys_dir: config.measured_boot.trusted_keys_dir
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| {
                config.measured_boot.build_signing_key_path
                    .as_ref()
                    .map(PathBuf::from)
            })
            .or_else(|| {
                let home = dirs::home_dir()?;
                Some(home.join(".vyoma").join("keys").join("trusted"))
            }),
    }
}

/// The standard PCR indices measured during UEFI Secure Boot.
///
/// These correspond to:
/// - PCR 0:  Firmware (BIOS/UEFI)
/// - PCR 1:  Firmware configuration
/// - PCR 4:  Boot manager
/// - PCR 5:  Boot manager configuration
/// - PCR 7:  Secure Boot state
/// - PCR 9:  Kernel image
/// - PCR 10: Initrd/initramfs
/// - PCR 14: Rootfs
const TPM_QUOTE_PCRS: &[u32] = &[0, 1, 4, 5, 7, 9, 10, 14];

/// Update the status of a VM in the shared state.
pub async fn update_vm_status(state: &AppState, vm_id: &str, new_status: VmStatus) -> Result<(), String> {
    let vm_arc = {
        let vms = state.vms.lock().await;
        vms.get(vm_id).cloned()
    };

    let vm_arc = match vm_arc {
        Some(arc) => arc,
        None => return Err(format!("VM {} not found in state", vm_id)),
    };

    {
        let mut vm = vm_arc.lock().await;
        vm.status = new_status.clone();
    }

    // Save updated state to disk
    {
        let vm = vm_arc.lock().await;
        vm.save_state(&state.data_dir)
            .map_err(|e| format!("Failed to save VM state: {}", e))?;
    }

    info!("Updated VM {} status to {:?}", vm_id, new_status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_pcr_quote_pcr_list_format() {
        let pcr_list = TPM_QUOTE_PCRS.iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(pcr_list, "0,1,4,5,7,9,10,14");
    }

    #[test]
    fn test_measured_boot_config_default() {
        let config = MeasuredBootConfig::default();
        assert_eq!(config.pcr_selection, vec![0, 1, 4, 5, 7, 9, 10, 14]);
        assert_eq!(config.verification_timeout_secs, 30);
        assert!(config.block_on_failure);
        assert!(config.require_signed_manifest);
    }

    #[test]
    fn test_build_attestation_config() {
        let state = AppState::new_test();
        {
            let mut policy = state.policy_manager.lock().unwrap();
            policy.set_require_measured_boot(true);
        }
        let config = build_attestation_config(&Arc::new(state));
        assert_eq!(config.verification_timeout_secs, 30);
        assert!(config.block_on_failure);
    }

    #[test]
    fn test_pcr_idx_conversion() {
        assert_eq!(pcr_idx_to_usize(&0), 0);
        assert_eq!(pcr_idx_to_usize(&14), 14);
        assert_eq!(pcr_idx_to_usize(&9), 9);
    }

    #[test]
    fn test_measured_boot_config_custom_timeout() {
        let config = MeasuredBootConfig {
            verification_timeout_secs: 60,
            ..Default::default()
        };
        assert_eq!(config.verification_timeout_secs, 60);
    }

    #[test]
    fn test_find_image_dir_not_found() {
        let result = find_image_dir("nonexistent-image-xyz");
        assert!(result.is_err());
    }
}

fn pcr_idx_to_usize(pcr_idx: &u32) -> usize {
    *pcr_idx as usize
}