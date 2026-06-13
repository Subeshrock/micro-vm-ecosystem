use anyhow::{Context, Result};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use super::types::AgentConfig;
use crate::state::AppState;

pub async fn prepare_agent(
    _state: &AppState,
    _dm_path: &str,
    vm_dir: &Path,
    _config: &vyoma_core::oci::OciImageConfig,
) -> Result<AgentConfig> {
    let initramfs_path = vm_dir.join("initramfs.cpio.gz");
    let init_script = generate_init_script(_config);

    let agent_binary_usr = PathBuf::from("/usr/bin/vyoma-agent-vm");
    let agent_binary_lib = PathBuf::from("/usr/lib/vyoma/vyoma-agent-vm");
    let agent_binary_data = PathBuf::from(&_state.data_dir).join("bin").join("vyoma-agent-vm");

    let agent_path = if agent_binary_usr.exists() {
        Some(&agent_binary_usr as &Path)
    } else if agent_binary_lib.exists() {
        Some(&agent_binary_lib as &Path)
    } else if agent_binary_data.exists() {
        Some(&agent_binary_data as &Path)
    } else if let Ok(path) = which::which("vyoma-agent-vm") {
        // Fallback to searching PATH (requires which crate or just a simple check)
        // Since we don't want to add a new dependency if we can avoid it, let's just log a warning.
        None
    } else {
        None
    };

    if agent_path.is_none() {
        tracing::warn!("vyoma-agent-vm binary not found in expected locations. VM will boot without agent capabilities.");
    }

    // Try to find it in PATH using a simple which-like implementation if we really need it,
    // but typically standard paths are enough. Let's add a basic PATH search.
    let mut resolved_agent_path = agent_path;
    let mut env_path_buf = PathBuf::new();
    if resolved_agent_path.is_none() {
        if let Ok(paths) = std::env::var("PATH") {
            for p in std::env::split_paths(&paths) {
                let candidate = p.join("vyoma-agent-vm");
                if candidate.exists() {
                    env_path_buf = candidate;
                    resolved_agent_path = Some(&env_path_buf as &Path);
                    break;
                }
            }
        }
    }

    vyoma_core::initramfs::create_initramfs(&init_script, resolved_agent_path, &initramfs_path)
        .context("Failed to create initramfs")?;

    info!("Agent prepared with initramfs at {:?}", initramfs_path);

    Ok(AgentConfig {
        initramfs_path: Some(initramfs_path),
        cmd: vec!["/sbin/init".to_string()],
        workdir: "/".to_string(),
        envs: vec![],
    })
}

/// Escapes a string for safe use in shell scripts.
///
/// This function ensures that strings can be safely used in:
/// - `export KEY=value` statements
/// - Command arguments after `exec`
///
/// - If the string contains only "safe" characters (alphanumeric, _, -, ., /, :),
///   it is returned unquoted.
/// - Otherwise, it is wrapped in single quotes with embedded single quotes
///   escaped as '\'' (standard POSIX shell escaping).
///
/// The safe character set is conservative and excludes shell metacharacters
/// like `$`, `` ` ``, `;`, `|`, `&`, etc.
fn shell_escape(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/' || c == ':') {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Generates the init script that runs as PID 1 inside the VM.
///
/// # Security Considerations
///
/// This script is part of the initramfs and is measured into PCR 10 during
/// measured boot. If a signed manifest is required by policy, the entire OCI
/// config (including these values) is signed, so tampering is detected.
///
/// If unsigned images are allowed, the attacker already has arbitrary code
/// execution inside the VM, so there is no additional security boundary being
/// breached.
///
/// The script uses defensive shell options:
/// - `set -e`: Exit on any error
/// - `set -u`: Treat unset variables as errors
/// - `trap ERR`: Power off on any error to prevent continuing with broken state
fn generate_init_script(config: &vyoma_core::oci::OciImageConfig) -> String {
    let mut script = String::new();

    script.push_str("#!/bin/sh\n");
    // Defensive shell options: fail fast on errors and unset variables
    script.push_str("set -e\n");
    script.push_str("set -u\n");
    // Power off on any error to prevent continuing with broken state
    script.push_str("trap 'echo Init error at line $LINENO; poweroff -f' ERR\n");
    script.push_str("\n");
    script.push_str("mount -t proc proc /proc 2>/dev/null || true\n");
    script.push_str("mount -t sysfs sys /sys 2>/dev/null || true\n");
    script.push_str("mount -t devtmpfs dev /dev 2>/dev/null || true\n");
    script.push_str("ip link set lo up 2>/dev/null || true\n");

    if let Some(envs) = &config.env {
        for env in envs {
            if let Some((key, value)) = env.split_once('=') {
                script.push_str(&format!("export {}={}\n", shell_escape(key), shell_escape(value)));
            }
        }
    }

    if let Some(workdir) = &config.working_dir {
        script.push_str(&format!("cd {}\n", shell_escape(workdir)));
    }

    let full_cmd = config.full_command();

    // Start the agent in the background before executing the workload.
    // The agent is forked (&) so it continues running after exec replaces
    // this shell. The orphaned agent process gets reparented to the VM's init.
    script.push_str("/sbin/vyoma-agent-vm &\n");

    if !full_cmd.is_empty() {
        let cmd_args: Vec<String> = full_cmd.iter().map(|s| shell_escape(s)).collect();
        script.push_str(&format!("exec {}\n", cmd_args.join(" ")));
    } else {
        script.push_str("exec /bin/sh\n");
    }

    script
}

pub async fn cleanup_agent(_agent_config: &AgentConfig) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_generate_init_script_default() {
        let config = vyoma_core::oci::OciImageConfig::default();
        let script = generate_init_script(&config);
        assert!(script.contains("#!/bin/sh"));
        assert!(script.contains("vyoma-agent-vm"));
        assert!(script.contains("mount"));
    }

    #[test]
    fn test_generate_init_script_with_config() {
        let mut config = vyoma_core::oci::OciImageConfig::default();
        config.entrypoint = Some(vec!["/bin/nginx".to_string()]);
        config.cmd = Some(vec!["-g".to_string(), "daemon off;".to_string()]);
        config.env = Some(vec!["NGINX_HOST=localhost".to_string(), "NGINX_PORT=80".to_string()]);
        config.working_dir = Some("/usr/share/nginx/html".to_string());

        let script = generate_init_script(&config);
        assert!(script.contains("export NGINX_HOST=localhost"));
        assert!(script.contains("export NGINX_PORT=80"));
        assert!(script.contains("cd /usr/share/nginx/html"));
        assert!(script.contains("exec /bin/nginx -g 'daemon off;'"));
    }

    #[test]
    fn test_shell_escape() {
        assert_eq!(shell_escape("simple"), "simple");
        assert_eq!(shell_escape("with-dash"), "with-dash");
        assert_eq!(shell_escape("with_underscore"), "with_underscore");
        assert_eq!(shell_escape("with'quote"), "'with'\\''quote'");
    }

    #[test]
    fn test_shell_escape_fuzz() {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for _ in 0..10000 {
            let len = rng.gen_range(0..50);
            let mut s = String::new();
            for _ in 0..len {
                s.push(rng.gen_range(0u8..=127) as char);
            }
            let escaped = shell_escape(&s);
            let line = format!("export X={}", escaped);

            // Check for dangerous characters outside of quoted context
            let mut in_single = false;
            for (i, c) in line.char_indices() {
                if c == '\'' && (i == 0 || line.as_bytes()[i-1] != b'\\') {
                    in_single = !in_single;
                    continue;
                }
                if !in_single {
                    if matches!(c, '`' | '$' | ';' | '|' | '&' | '(' | ')' | '{' | '}' | '#' | '!' | '~' | '\n' | '\r') {
                        panic!("Unsafe character '{}' found outside single quotes in: {}", c, line);
                    }
                }
            }

            // Verify round-trip: unescape should yield original string
            let unescaped = unescape(&escaped);
            assert_eq!(unescaped, s, "Round-trip failed for {:?} -> {:?}", s, escaped);
        }
    }

    fn unescape(escaped: &str) -> String {
        if escaped.starts_with('\'') && escaped.ends_with('\'') {
            let inner = &escaped[1..escaped.len()-1];
            inner.replace("'\\''", "'")
        } else {
            escaped.to_string()
        }
    }

    #[test]
    fn test_generate_init_script_has_defensive_options() {
        let config = vyoma_core::oci::OciImageConfig::default();
        let script = generate_init_script(&config);
        assert!(script.contains("set -e"), "Script should contain 'set -e'");
        assert!(script.contains("set -u"), "Script should contain 'set -u'");
        assert!(script.contains("trap"), "Script should contain 'trap ERR'");
    }

    #[test]
    fn test_shell_escape_injection_attempts() {
        // Test various injection attempts are properly escaped
        let injection_attempts = vec![
            "$(whoami)",
            "`whoami`",
            "${whoami}",
            "; rm -rf /",
            "| cat /etc/passwd",
            "& sleep 10",
            "$(echo pwned)",
            "`echo pwned`",
            "newline\ncommand",
            "newline\rcommand",
            "dollar$HOME",
            "backtick`id`",
        ];

        for attempt in injection_attempts {
            let escaped = shell_escape(attempt);
            // Escaped string should either be quoted or not contain dangerous chars
            if !escaped.starts_with('\'') {
                // Unquoted - check no dangerous chars
                assert!(!escaped.contains('$'), "Unquoted string contains $: {} -> {}", attempt, escaped);
                assert!(!escaped.contains('`'), "Unquoted string contains backtick: {} -> {}", attempt, escaped);
                assert!(!escaped.contains(';'), "Unquoted string contains ;: {} -> {}", attempt, escaped);
            }
        }
    }

    #[tokio::test]
    async fn test_prepare_agent_without_agent() {
        let temp_dir = TempDir::new().unwrap();
        let config = vyoma_core::oci::OciImageConfig::default();

        let state = Arc::new(crate::state::AppState::new_test());
        let result = prepare_agent(
            &crate::state::AppState::with_vm_service(state),
            "/dev/null",
            temp_dir.path(),
            &config,
        ).await;

        assert!(result.is_ok());
        let agent_config = result.unwrap();
        assert!(agent_config.initramfs_path.is_some());
        assert!(agent_config.initramfs_path.unwrap().exists());
    }
}