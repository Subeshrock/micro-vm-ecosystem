use crate::types::{HealthcheckType, ImageConfig};
use anyhow::{Context, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream as TokioTcp;

const DEFAULT_HEALTHCHECK_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 500;

pub struct Healthchecker {
    host_port: u16,
    timeout: Duration,
    retries: u32,
}

impl Healthchecker {
    pub fn new(host_port: u16, timeout: Duration) -> Self {
        Self {
            host_port,
            timeout,
            retries: DEFAULT_HEALTHCHECK_RETRIES,
        }
    }

    pub fn with_retries(host_port: u16, timeout: Duration, retries: u32) -> Self {
        Self {
            host_port,
            timeout,
            retries,
        }
    }

    pub async fn check(&self, config: &ImageConfig) -> Result<HealthResult> {
        match config.healthcheck_type {
            HealthcheckType::Tcp => self.check_with_retry(|| self.check_tcp()).await,
            HealthcheckType::Http => {
                let path = config.healthcheck_path.as_deref().unwrap_or("/");
                let expected = config.expected_status.clone().unwrap_or(vec![200, 204]);
                self.check_with_retry(|| self.check_http(path, &expected)).await
            }
            HealthcheckType::Redis => self.check_with_retry(|| self.check_redis()).await,
            HealthcheckType::Postgres => self.check_with_retry(|| self.check_postgres(config)).await,
            HealthcheckType::Exec => Ok(HealthResult::skipped("Exec healthcheck requires agent access")),
            HealthcheckType::Generic => Ok(HealthResult::skipped("Generic healthcheck requires agent access")),
        }
    }

    async fn check_with_retry<F, Fut>(&self, mut check_fn: F) -> Result<HealthResult>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<HealthResult>>,
    {
        let mut last_result: Option<HealthResult> = None;

        for attempt in 1..=self.retries {
            match check_fn().await {
                Ok(result) if result.healthy || result.skipped => {
                    return Ok(result);
                }
                Ok(result) => {
                    last_result = Some(result);
                    if attempt < self.retries {
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                    }
                }
                Err(e) => {
                    last_result = Some(HealthResult::unhealthy(
                        0,
                        format!("Healthcheck error: {}", e),
                    ));
                    if attempt < self.retries {
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                    }
                }
            }
        }

        Ok(last_result.unwrap_or_else(|| {
            HealthResult::unhealthy(0, "All healthcheck retries exhausted".to_string())
        }))
    }

    async fn check_tcp(&self) -> Result<HealthResult> {
        let start = std::time::Instant::now();
        let addr = format!("127.0.0.1:{}", self.host_port);

        let result = tokio::time::timeout(self.timeout, TokioTcp::connect(&addr)).await;

        match result {
            Ok(Ok(_stream)) => {
                let duration = start.elapsed().as_millis() as u64;
                Ok(HealthResult::healthy(duration, format!("TCP connected to port {}", self.host_port)))
            }
            Ok(Err(e)) => {
                let duration = start.elapsed().as_millis() as u64;
                Ok(HealthResult::unhealthy(duration, format!("Connection failed: {}", e)))
            }
            Err(_) => {
                let duration = self.timeout.as_millis() as u64;
                Ok(HealthResult::timeout(duration))
            }
        }
    }

    async fn check_http(&self, path: &str, expected_status: &[u16]) -> Result<HealthResult> {
        let start = std::time::Instant::now();
        let url = format!("http://127.0.0.1:{}{}", self.host_port, path);

        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.timeout)
            .build()
            .context("Failed to create HTTP client")?;

        match client.get(&url).send().await {
            Ok(response) => {
                let duration = start.elapsed().as_millis() as u64;
                let status = response.status().as_u16();
                let msg = format!("HTTP {} from {}", status, url);

                if expected_status.contains(&status) {
                    Ok(HealthResult::healthy(duration, msg))
                } else {
                    let detail = format!("Unexpected status (expected {:?})", expected_status);
                    Ok(HealthResult::unhealthy(duration, detail))
                }
            }
            Err(e) => {
                let duration = start.elapsed().as_millis() as u64;
                if e.is_timeout() {
                    Ok(HealthResult::timeout(duration))
                } else if e.is_connect() {
                    Ok(HealthResult::unhealthy(duration, format!("Connection refused: {}", e)))
                } else {
                    Ok(HealthResult::unhealthy(duration, format!("HTTP request failed: {}", e)))
                }
            }
        }
    }

    async fn check_redis(&self) -> Result<HealthResult> {
        let start = std::time::Instant::now();
        let addr = format!("127.0.0.1:{}", self.host_port);

        let result = tokio::time::timeout(self.timeout, async {
            let mut stream = TokioTcp::connect(&addr).await?;
            stream.write_all(b"PING\r\n").await?;
            let mut buf = [0u8; 128];
            let n = stream.read(&mut buf).await?;
            Ok::<_, std::io::Error>(buf[..n].to_vec())
        }).await;

        match result {
            Ok(Ok(response)) => {
                let response_str = String::from_utf8_lossy(&response);
                if response_str.contains("PONG") || response_str.starts_with("+") {
                    let duration = start.elapsed().as_millis() as u64;
                    Ok(HealthResult::healthy(duration, "Redis PONG received".to_string()))
                } else {
                    let duration = start.elapsed().as_millis() as u64;
                    let msg = format!("Unexpected Redis response: {}", response_str);
                    Ok(HealthResult::unhealthy(duration, msg))
                }
            }
            Ok(Err(e)) => {
                let duration = start.elapsed().as_millis() as u64;
                Ok(HealthResult::unhealthy(duration, format!("Redis connection failed: {}", e)))
            }
            Err(_) => {
                let duration = self.timeout.as_millis() as u64;
                Ok(HealthResult::timeout(duration))
            }
        }
    }

    async fn check_postgres(&self, config: &ImageConfig) -> Result<HealthResult> {
        let cmd = config
            .healthcheck_cmd
            .as_ref()
            .map(|c| c.join(" "))
            .unwrap_or_else(|| "pg_isready -U postgres".to_string());

        let start = std::time::Instant::now();

        let tcp_check = tokio::time::timeout(
            Duration::from_secs(5),
            TokioTcp::connect(format!("127.0.0.1:{}", self.host_port)),
        ).await;

        if tcp_check.is_err() {
            let duration = start.elapsed().as_millis() as u64;
            return Ok(HealthResult::unhealthy(
                duration,
                format!("PostgreSQL port {} not reachable", self.host_port),
            ));
        }

        if tcp_check.ok().map(|r| r.is_err()).unwrap_or(true) {
            let duration = start.elapsed().as_millis() as u64;
            return Ok(HealthResult::unhealthy(
                duration,
                "PostgreSQL not accepting connections".to_string(),
            ));
        }

        let check_cmd = format!("{} || true", cmd);
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&check_cmd)
            .output()
            .await
            .context("Failed to run postgres healthcheck")?;

        let duration = start.elapsed().as_millis() as u64;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let output_combined = if stdout.is_empty() { stderr } else { stdout };

        if output.status.success()
            || output_combined.contains("accepting")
            || output_combined.contains("up")
            || output_combined.trim() == "ok"
        {
            Ok(HealthResult::healthy(duration, format!("PostgreSQL ready: {}", output_combined.trim())))
        } else {
            Ok(HealthResult::unhealthy(duration, format!("PostgreSQL not ready: {}", output_combined.trim())))
        }
    }
}

#[derive(Debug, Clone)]
pub struct HealthResult {
    pub healthy: bool,
    pub skipped: bool,
    pub duration_ms: u64,
    pub message: String,
}

impl HealthResult {
    pub fn healthy(duration_ms: u64, message: String) -> Self {
        Self {
            healthy: true,
            skipped: false,
            duration_ms,
            message,
        }
    }

    pub fn unhealthy(duration_ms: u64, message: String) -> Self {
        Self {
            healthy: false,
            skipped: false,
            duration_ms,
            message,
        }
    }

    pub fn timeout(duration_ms: u64) -> Self {
        Self {
            healthy: false,
            skipped: false,
            duration_ms,
            message: format!("Healthcheck timed out after {}ms", duration_ms),
        }
    }

    pub fn skipped(message: impl Into<String>) -> Self {
        Self {
            healthy: true,
            skipped: true,
            duration_ms: 0,
            message: message.into(),
        }
    }
}
