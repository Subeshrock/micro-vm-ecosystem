mod types;
mod health;

pub use types::{CompatReport, CompatSummary, ImageConfig, ImageList, TestPhase, TestResult};
pub use health::{HealthResult, Healthchecker};

use anyhow::Result;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{error, info, warn};

pub struct CompatMatrix {
    vyomad_url: String,
    data_dir: String,
    pull_timeout: Duration,
    boot_timeout: Duration,
    health_timeout: Duration,
}

impl CompatMatrix {
    pub fn new(vyomad_url: impl Into<String>) -> Self {
        Self {
            vyomad_url: vyomad_url.into(),
            data_dir: "~/.vyoma".to_string(),
            pull_timeout: Duration::from_secs(300),
            boot_timeout: Duration::from_secs(120),
            health_timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeouts(mut self, pull: Duration, boot: Duration, health: Duration) -> Self {
        self.pull_timeout = pull;
        self.boot_timeout = boot;
        self.health_timeout = health;
        self
    }

    pub async fn run_image(&self, config: &ImageConfig) -> Vec<TestResult> {
        let mut results = Vec::new();
        let image = &config.name;

        let pull_result = self.pull_image(image).await;
        results.push(pull_result);

        if !results.last().map(|r| r.success).unwrap_or(false) {
            results.push(self.failed_result(image, TestPhase::Build, "Skipped due to pull failure"));
            results.push(self.failed_result(image, TestPhase::Boot, "Skipped due to pull failure"));
            results.push(self.failed_result(image, TestPhase::Healthcheck, "Skipped due to pull failure"));
            return results;
        }

        let run_result = self.run_vm(image, config).await;
        results.push(run_result);

        if !results.last().map(|r| r.success).unwrap_or(false) {
            results.push(self.failed_result(image, TestPhase::Healthcheck, "Skipped due to boot failure"));
            return results;
        }

        let health_result = self.healthcheck(image, config).await;
        results.push(health_result);

        let teardown_result = self.teardown_vm(image).await;
        results.push(teardown_result);

        results
    }

    async fn pull_image(&self, image: &str) -> TestResult {
        let start = std::time::Instant::now();
        let url = format!("{}/pull", self.vyomad_url);

        info!("Pulling image: {}", image);

        let client = reqwest::Client::builder()
            .timeout(self.pull_timeout)
            .build()
            .unwrap_or_default();

        match client
            .post(&url)
            .json(&serde_json::json!({ "image": image }))
            .send()
            .await
        {
            Ok(response) => {
                let duration = start.elapsed().as_millis() as u64;
                if response.status().is_success() {
                    info!("Successfully pulled: {}", image);
                    TestResult {
                        image: image.to_string(),
                        phase: TestPhase::Pull,
                        success: true,
                        message: format!("Pulled successfully in {}ms", duration),
                        duration_ms: duration,
                        details: None,
                    }
                } else {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                    let error_msg = format!("Pull failed with status: {} - {}", status, body);
                    error!("{}", error_msg);
                    TestResult {
                        image: image.to_string(),
                        phase: TestPhase::Pull,
                        success: false,
                        message: error_msg,
                        duration_ms: duration,
                        details: None,
                    }
                }
            }
            Err(e) => {
                let duration = start.elapsed().as_millis() as u64;
                let error_msg = format!("Pull request failed: {}", e);
                error!("{}", error_msg);
                TestResult {
                    image: image.to_string(),
                    phase: TestPhase::Pull,
                    success: false,
                    message: error_msg,
                    duration_ms: duration,
                    details: None,
                }
            }
        }
    }

    async fn run_vm(&self, image: &str, config: &ImageConfig) -> TestResult {
        let start = std::time::Instant::now();
        let url = format!("{}/run", self.vyomad_url);

        info!("Starting VM for: {}", image);

        let port = config.check_port();
        let ports = if let Some(p) = port {
            vec![serde_json::json!({
                "host_port": 0,
                "vm_port": p
            })]
        } else {
            vec![]
        };

        let request = serde_json::json!({
            "image": image,
            "vcpu": 1,
            "mem_size_mib": 512,
            "ports": ports,
        });

        let client = reqwest::Client::builder()
            .timeout(self.boot_timeout)
            .build()
            .unwrap_or_default();

        match timeout(self.boot_timeout, client.post(&url).json(&request).send()).await {
            Ok(Ok(response)) => {
                let duration = start.elapsed().as_millis() as u64;
                if response.status().is_success() {
                    match response.json::<serde_json::Value>().await {
                        Ok(body) => {
                            let vm_id = body.get("vm_id").and_then(|v| v.as_str()).unwrap_or("unknown");
                            info!("VM {} started for {}", vm_id, image);
                            TestResult {
                                image: image.to_string(),
                                phase: TestPhase::Boot,
                                success: true,
                                message: format!("VM {} started successfully in {}ms", vm_id, duration),
                                duration_ms: duration,
                                details: serde_json::json!({ "vm_id": vm_id }).into(),
                            }
                        }
                        Err(_) => TestResult {
                            image: image.to_string(),
                            phase: TestPhase::Boot,
                            success: true,
                            message: format!("VM started in {}ms", duration),
                            duration_ms: duration,
                            details: None,
                        },
                    }
                } else {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                    let error_msg = format!("VM start failed with status: {} - {}", status, body);
                    error!("{}", error_msg);
                    TestResult {
                        image: image.to_string(),
                        phase: TestPhase::Boot,
                        success: false,
                        message: error_msg,
                        duration_ms: duration,
                        details: None,
                    }
                }
            }
            Ok(Err(e)) => {
                let duration = start.elapsed().as_millis() as u64;
                let error_msg = format!("Run request failed: {}", e);
                error!("{}", error_msg);
                TestResult {
                    image: image.to_string(),
                    phase: TestPhase::Boot,
                    success: false,
                    message: error_msg,
                    duration_ms: duration,
                    details: None,
                }
            }
            Err(_) => {
                let duration = self.boot_timeout.as_millis() as u64;
                TestResult {
                    image: image.to_string(),
                    phase: TestPhase::Boot,
                    success: false,
                    message: format!("Boot timeout after {}ms", duration),
                    duration_ms: duration,
                    details: None,
                }
            }
        }
    }

    async fn healthcheck(&self, image: &str, config: &ImageConfig) -> TestResult {
        let start = std::time::Instant::now();
        let port = match config.check_port() {
            Some(p) => p,
            None => {
                return TestResult {
                    image: image.to_string(),
                    phase: TestPhase::Healthcheck,
                    success: true,
                    message: "No port to healthcheck".to_string(),
                    duration_ms: 0,
                    details: None,
                };
            }
        };

        info!("Running healthcheck for {} on port {}", image, port);

        let healthchecker = Healthchecker::new(port, self.health_timeout);
        match timeout(self.health_timeout, healthchecker.check(config)).await {
            Ok(Ok(result)) => {
                let duration = start.elapsed().as_millis() as u64;
                if result.skipped {
                    TestResult {
                        image: image.to_string(),
                        phase: TestPhase::Healthcheck,
                        success: true,
                        message: format!("Healthcheck skipped: {}", result.message),
                        duration_ms: duration,
                        details: None,
                    }
                } else if result.healthy {
                    TestResult {
                        image: image.to_string(),
                        phase: TestPhase::Healthcheck,
                        success: true,
                        message: format!("Healthy: {}", result.message),
                        duration_ms: duration,
                        details: None,
                    }
                } else {
                    TestResult {
                        image: image.to_string(),
                        phase: TestPhase::Healthcheck,
                        success: false,
                        message: format!("Unhealthy: {}", result.message),
                        duration_ms: duration,
                        details: None,
                    }
                }
            }
            Ok(Err(e)) => {
                let duration = start.elapsed().as_millis() as u64;
                TestResult {
                    image: image.to_string(),
                    phase: TestPhase::Healthcheck,
                    success: false,
                    message: format!("Healthcheck error: {}", e),
                    duration_ms: duration,
                    details: None,
                }
            }
            Err(_) => {
                let duration = self.health_timeout.as_millis() as u64;
                TestResult {
                    image: image.to_string(),
                    phase: TestPhase::Healthcheck,
                    success: false,
                    message: format!("Healthcheck timeout after {}ms", duration),
                    duration_ms: duration,
                    details: None,
                }
            }
        }
    }

    async fn teardown_vm(&self, image: &str) -> TestResult {
        let start = std::time::Instant::now();
        let list_url = format!("{}/vms", self.vyomad_url);

        info!("Stopping VM for: {}", image);

        let client = reqwest::Client::new();

        let vms = match client.get(&list_url).send().await {
            Ok(response) => response.json::<serde_json::Value>().await.unwrap_or_default(),
            Err(_) => serde_json::json!({ "vms": [] }),
        };

        let vms_array = vms.get("vms").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let image_vms: Vec<_> = vms_array
            .iter()
            .filter(|vm| {
                vm.get("base_image_path")
                    .and_then(|p| p.as_str())
                    .map(|p| p.contains(image))
                    .unwrap_or(false)
            })
            .collect();

        let mut all_stopped = true;
        for vm in image_vms {
            if let Some(vm_id) = vm.get("id").and_then(|v| v.as_str()) {
                let stop_url = format!("{}/stop/{}", self.vyomad_url, vm_id);
                match client.post(&stop_url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        info!("Stopped VM: {}", vm_id);
                    }
                    _ => {
                        warn!("Failed to stop VM: {}", vm_id);
                        all_stopped = false;
                    }
                }
            }
        }

        let duration = start.elapsed().as_millis() as u64;
        TestResult {
            image: image.to_string(),
            phase: TestPhase::Teardown,
            success: all_stopped,
            message: if all_stopped {
                format!("Teardown complete in {}ms", duration)
            } else {
                format!("Teardown completed with errors in {}ms", duration)
            },
            duration_ms: duration,
            details: None,
        }
    }

    fn failed_result(&self, image: &str, phase: TestPhase, message: &str) -> TestResult {
        TestResult {
            image: image.to_string(),
            phase,
            success: false,
            message: message.to_string(),
            duration_ms: 0,
            details: None,
        }
    }
}

pub async fn run_compat_matrix(
    vyomad_url: &str,
    images: Vec<ImageConfig>,
) -> Result<CompatReport> {
    let matrix = CompatMatrix::new(vyomad_url);
    let mut all_results = Vec::new();

    for config in images {
        info!("Testing image: {}", config.name);
        let results = matrix.run_image(&config).await;
        all_results.extend(results);
    }

    Ok(CompatReport::new(all_results))
}

pub async fn run_compat_matrix_parallel(
    vyomad_url: &str,
    images: Vec<ImageConfig>,
    parallel: usize,
) -> Result<CompatReport> {
    use tokio::sync::Semaphore;

    let semaphore = std::sync::Arc::new(Semaphore::new(parallel));
    let mut all_results = Vec::new();

    let handles: Vec<_> = images
        .into_iter()
        .map(|config| {
            let matrix = CompatMatrix::new(vyomad_url);
            let sem = semaphore.clone();
            async move {
                let _permit = sem.acquire().await.unwrap();
                matrix.run_image(&config).await
            }
        })
        .collect();

    let results_group = futures::future::join_all(handles).await;
    for results in results_group {
        all_results.extend(results);
    }

    Ok(CompatReport::new(all_results))
}
