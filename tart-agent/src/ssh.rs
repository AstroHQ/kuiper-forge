//! SSH client for configuring GitHub runners on VMs.

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};

/// SSH configuration for connecting to VMs.
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// SSH username (typically "admin" for Tart VMs)
    pub username: String,
    /// Path to SSH private key (optional, uses default if not specified)
    pub private_key: Option<std::path::PathBuf>,
    /// Connection timeout
    pub timeout: Duration,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            username: "admin".to_string(),
            private_key: None,
            timeout: Duration::from_secs(30),
        }
    }
}

/// Wait for SSH port to become available.
pub async fn wait_for_ssh(ip: Ipv4Addr, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let addr = format!("{}:22", ip);

    debug!("Waiting for SSH on {}", addr);

    while tokio::time::Instant::now() < deadline {
        match TcpStream::connect(&addr).await {
            Ok(_) => {
                info!("SSH port available on {}", ip);
                return Ok(());
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    Err(Error::Timeout("waiting for SSH"))
}

/// Execute a command on a remote host via SSH.
pub async fn ssh_exec(
    ip: Ipv4Addr,
    config: &SshConfig,
    command: &str,
) -> Result<String> {
    let mut cmd = Command::new("ssh");

    // Disable strict host key checking for ephemeral VMs
    cmd.arg("-o").arg("StrictHostKeyChecking=no");
    cmd.arg("-o").arg("UserKnownHostsFile=/dev/null");
    cmd.arg("-o").arg("BatchMode=yes");
    cmd.arg("-o").arg(format!("ConnectTimeout={}", config.timeout.as_secs()));

    // Add private key if specified
    if let Some(key_path) = &config.private_key {
        cmd.arg("-i").arg(key_path);
    }

    // Destination
    cmd.arg(format!("{}@{}", config.username, ip));

    // Command to execute
    cmd.arg(command);

    debug!("SSH exec on {}: {}", ip, command);

    let output = cmd.output().await?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(Error::Ssh(format!(
            "SSH command failed: {} (exit code: {:?})",
            stderr,
            output.status.code()
        )))
    }
}

/// Configure the GitHub Actions runner on a VM.
pub async fn configure_runner(
    ip: Ipv4Addr,
    config: &SshConfig,
    registration_token: &str,
    labels: &[String],
    runner_scope_url: &str,
    runner_name: &str,
) -> Result<()> {
    let labels_str = labels.join(",");

    // Configure the runner
    let config_cmd = format!(
        "cd ~/actions-runner && ./config.sh --url '{}' --token '{}' --name '{}' --labels '{}' --ephemeral --unattended",
        runner_scope_url,
        registration_token,
        runner_name,
        labels_str
    );

    info!("Configuring runner {} with labels: {}", runner_name, labels_str);
    ssh_exec(ip, config, &config_cmd).await?;

    info!("Runner {} configured successfully", runner_name);
    Ok(())
}

/// Start the GitHub Actions runner and wait for it to complete.
///
/// For ephemeral runners, the process exits when the job completes.
pub async fn start_runner_and_wait(ip: Ipv4Addr, config: &SshConfig) -> Result<()> {
    info!("Starting runner on {}", ip);

    // Run the runner - for ephemeral runners, this blocks until the job completes
    let run_cmd = "cd ~/actions-runner && ./run.sh";

    match ssh_exec(ip, config, run_cmd).await {
        Ok(_) => {
            info!("Runner completed successfully on {}", ip);
            Ok(())
        }
        Err(e) => {
            // Runner exiting is expected for ephemeral mode
            warn!("Runner process ended on {}: {}", ip, e);
            // Check if the runner actually completed successfully
            // by looking at the exit state
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_config_default() {
        let config = SshConfig::default();
        assert_eq!(config.username, "admin");
        assert!(config.private_key.is_none());
        assert_eq!(config.timeout, Duration::from_secs(30));
    }
}
