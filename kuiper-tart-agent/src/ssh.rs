//! SSH client for configuring GitHub runners on VMs.
//!
//! Supports both password and key-based authentication using the russh library.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use russh::client::{self, Config, Handle, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::ssh_key::PublicKey;
use russh::keys::PrivateKey;
use russh::ChannelMsg;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, error, info};

use crate::error::{Error, Result};

/// SSH authentication method.
#[derive(Debug, Clone)]
pub enum SshAuth {
    /// Password-based authentication
    Password(String),
    /// Key-based authentication with path to private key file
    KeyFile(PathBuf),
    /// No explicit auth - will try agent or default keys
    None,
}

impl Default for SshAuth {
    fn default() -> Self {
        Self::None
    }
}

/// SSH configuration for connecting to VMs.
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// SSH username (typically "admin" for Tart VMs)
    pub username: String,
    /// Authentication method
    pub auth: SshAuth,
    /// Connection timeout
    pub timeout: Duration,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            username: "admin".to_string(),
            auth: SshAuth::None,
            timeout: Duration::from_secs(30),
        }
    }
}

impl SshConfig {
    /// Create a new SSH config with password authentication.
    #[cfg(test)]
    pub fn with_password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            auth: SshAuth::Password(password.into()),
            timeout: Duration::from_secs(30),
        }
    }

    /// Create a new SSH config with key file authentication.
    #[cfg(test)]
    pub fn with_key_file(username: impl Into<String>, key_path: impl Into<PathBuf>) -> Self {
        Self {
            username: username.into(),
            auth: SshAuth::KeyFile(key_path.into()),
            timeout: Duration::from_secs(30),
        }
    }

    /// Create from a configuration struct.
    pub fn from_config(config: &crate::config::SshAuthConfig) -> Self {
        let auth = match config.auth_method.as_str() {
            "password" => config
                .password
                .as_ref()
                .map(|p| SshAuth::Password(p.clone()))
                .unwrap_or(SshAuth::None),
            "key" => config
                .private_key
                .as_ref()
                .map(|p| SshAuth::KeyFile(p.clone()))
                .unwrap_or(SshAuth::None),
            _ => SshAuth::None, // "default" or any other value - try default keys
        };

        Self {
            username: config.username.clone(),
            auth,
            timeout: Duration::from_secs(config.timeout_secs),
        }
    }
}

/// Client handler for russh.
struct SshHandler;

impl Handler for SshHandler {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> impl std::future::Future<Output = std::result::Result<bool, Self::Error>> + Send {
        // Accept all host keys for ephemeral VMs
        // In production, you might want to verify against known hosts
        async { Ok(true) }
    }
}

/// Wait for SSH port to become available.
pub async fn wait_for_ssh(ip: Ipv4Addr, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let addr = format!("{}:22", ip);

    debug!("Waiting for SSH on {}", addr);

    while tokio::time::Instant::now() < deadline {
        let connect_result = TcpStream::connect(&addr).await;
        match connect_result {
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

/// Load a private key from a file.
fn load_key(path: &PathBuf) -> Result<PrivateKey> {
    let key_data =
        std::fs::read_to_string(path).map_err(|e| Error::Ssh(format!("Failed to read key: {}", e)))?;
    PrivateKey::from_openssh(&key_data).map_err(|e| Error::Ssh(format!("Failed to parse key: {}", e)))
}

/// Establish an SSH connection to a remote host.
async fn connect(ip: Ipv4Addr, config: &SshConfig) -> Result<Handle<SshHandler>> {
    let ssh_config = Config::default();

    let addr = format!("{}:22", ip);
    debug!("Connecting to SSH at {}", addr);

    let timeout_result =
        tokio::time::timeout(config.timeout, client::connect(Arc::new(ssh_config), &addr, SshHandler))
            .await;

    let mut session = match timeout_result {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(Error::Ssh(format!("Failed to connect: {}", e))),
        Err(_) => return Err(Error::Timeout("SSH connection")),
    };

    // Authenticate based on config
    let auth_result = match &config.auth {
        SshAuth::Password(password) => {
            debug!("Authenticating with password for user {}", config.username);
            session
                .authenticate_password(&config.username, password)
                .await
                .map_err(|e| Error::Ssh(format!("Password auth failed: {}", e)))?
        }
        SshAuth::KeyFile(key_path) => {
            debug!(
                "Authenticating with key file {:?} for user {}",
                key_path, config.username
            );
            let key = load_key(key_path)?;
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), None);
            session
                .authenticate_publickey(&config.username, key_with_hash)
                .await
                .map_err(|e| Error::Ssh(format!("Key auth failed: {}", e)))?
        }
        SshAuth::None => {
            // Try to find default SSH key
            let default_key_path = dirs::home_dir()
                .map(|h| h.join(".ssh").join("id_ed25519"))
                .filter(|p| p.exists())
                .or_else(|| {
                    dirs::home_dir()
                        .map(|h| h.join(".ssh").join("id_rsa"))
                        .filter(|p| p.exists())
                });

            if let Some(key_path) = default_key_path {
                debug!(
                    "Authenticating with default key {:?} for user {}",
                    key_path, config.username
                );
                let key = load_key(&key_path)?;
                let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), None);
                session
                    .authenticate_publickey(&config.username, key_with_hash)
                    .await
                    .map_err(|e| Error::Ssh(format!("Key auth failed: {}", e)))?
            } else {
                return Err(Error::Ssh(
                    "No authentication method specified and no default SSH key found".to_string(),
                ));
            }
        }
    };

    if auth_result.success() {
        debug!("SSH authentication successful");
        Ok(session)
    } else {
        Err(Error::Ssh("Authentication failed".to_string()))
    }
}

/// Execute a command on a remote host via SSH.
pub async fn ssh_exec(ip: Ipv4Addr, config: &SshConfig, command: &str) -> Result<String> {
    let session = connect(ip, config).await?;

    debug!("SSH exec on {}: {}", ip, command);

    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("Failed to open channel: {}", e)))?;

    channel
        .exec(true, command)
        .await
        .map_err(|e| Error::Ssh(format!("Failed to exec command: {}", e)))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_status = None;

    loop {
        let msg = channel.wait().await;
        match msg {
            Some(ChannelMsg::Data { data }) => {
                stdout.extend_from_slice(&data);
            }
            Some(ChannelMsg::ExtendedData { data, ext }) => {
                if ext == 1 {
                    // stderr
                    stderr.extend_from_slice(&data);
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                exit_status = Some(status);
            }
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            Some(_) => {}
        }
    }

    let exit_code = exit_status.unwrap_or(0);

    if exit_code == 0 {
        Ok(String::from_utf8_lossy(&stdout).to_string())
    } else {
        let stderr_str = String::from_utf8_lossy(&stderr).to_string();
        Err(Error::Ssh(format!(
            "SSH command failed: {} (exit code: {})",
            stderr_str, exit_code
        )))
    }
}

/// Execute a command on a remote host via SSH, streaming output to a log file.
///
/// This is designed for long-running commands like the GitHub runner where we want
/// to capture all output for debugging purposes.
pub async fn ssh_exec_with_logging(
    ip: Ipv4Addr,
    config: &SshConfig,
    command: &str,
    log_path: &Path,
) -> Result<()> {
    let session = connect(ip, config).await?;

    info!("SSH exec (logged to {}): {}", log_path.display(), command);

    // Open log file for appending
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await
        .map_err(|e| Error::Ssh(format!("Failed to open log file: {}", e)))?;

    // Write header with timestamp
    let header = format!(
        "\n=== Runner started at {} ===\n=== Command: {} ===\n\n",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        command
    );
    log_file
        .write_all(header.as_bytes())
        .await
        .map_err(|e| Error::Ssh(format!("Failed to write to log file: {}", e)))?;

    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| Error::Ssh(format!("Failed to open channel: {}", e)))?;

    channel
        .exec(true, command)
        .await
        .map_err(|e| Error::Ssh(format!("Failed to exec command: {}", e)))?;

    let mut exit_status = None;

    loop {
        let msg = channel.wait().await;
        match msg {
            Some(ChannelMsg::Data { data }) => {
                // Write stdout to log file
                log_file
                    .write_all(&data)
                    .await
                    .map_err(|e| Error::Ssh(format!("Failed to write stdout to log: {}", e)))?;
                // Also log to tracing at debug level for real-time visibility
                if let Ok(text) = std::str::from_utf8(&data) {
                    for line in text.lines() {
                        debug!("[runner stdout] {}", line);
                    }
                }
            }
            Some(ChannelMsg::ExtendedData { data, ext }) => {
                if ext == 1 {
                    // stderr - prefix with [stderr] in log
                    let prefixed: Vec<u8> = data
                        .iter()
                        .copied()
                        .collect();
                    log_file
                        .write_all(&prefixed)
                        .await
                        .map_err(|e| Error::Ssh(format!("Failed to write stderr to log: {}", e)))?;
                    // Log stderr at info level since it's often important
                    if let Ok(text) = std::str::from_utf8(&data) {
                        for line in text.lines() {
                            info!("[runner stderr] {}", line);
                        }
                    }
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                exit_status = Some(status);
            }
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            Some(_) => {}
        }
    }

    // Write footer with exit status
    let exit_code = exit_status.unwrap_or(0);
    let footer = format!(
        "\n=== Runner exited at {} with code {} ===\n",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        exit_code
    );
    log_file
        .write_all(footer.as_bytes())
        .await
        .map_err(|e| Error::Ssh(format!("Failed to write to log file: {}", e)))?;

    // Ensure everything is flushed
    log_file
        .flush()
        .await
        .map_err(|e| Error::Ssh(format!("Failed to flush log file: {}", e)))?;

    if exit_code == 0 {
        Ok(())
    } else {
        Err(Error::Ssh(format!(
            "SSH command failed (exit code: {}). See log: {}",
            exit_code,
            log_path.display()
        )))
    }
}

/// Fallback GitHub Actions runner version if API fetch fails.
const FALLBACK_RUNNER_VERSION: &str = "2.321.0";

/// Fetch the latest GitHub Actions runner version from the GitHub API.
/// Returns None if the fetch fails (will fall back to configured/default version).
async fn fetch_latest_runner_version() -> Option<String> {
    // Use curl on the VM or locally - we'll do it via the VM since we're already SSHing
    // Actually, let's do it locally since we have network access
    let url = "https://api.github.com/repos/actions/runner/releases/latest";

    debug!("Fetching latest runner version from GitHub API");

    // Simple HTTP GET using the VM's curl (we could also use reqwest but this avoids adding deps)
    let output = tokio::process::Command::new("curl")
        .args(["-sL", "-H", "Accept: application/vnd.github.v3+json", url])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        debug!("Failed to fetch latest runner version from GitHub API");
        return None;
    }

    let body = String::from_utf8_lossy(&output.stdout);

    // Parse JSON to extract tag_name (e.g., "v2.321.0")
    // Simple parsing without adding serde_json dependency to this module
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("\"tag_name\"") {
            // Extract version from: "tag_name": "v2.321.0",
            if let Some(start) = line.find(": \"v") {
                let version_start = start + 4; // skip `: "v`
                if let Some(end) = line[version_start..].find('"') {
                    let version = &line[version_start..version_start + end];
                    info!("Latest GitHub Actions runner version: v{}", version);
                    return Some(version.to_string());
                }
            }
        }
    }

    debug!("Could not parse runner version from GitHub API response");
    None
}

/// Ensure the GitHub Actions runner is installed on the VM.
/// Downloads and extracts the runner if not present.
/// If `runner_version` is empty, fetches the latest version from GitHub.
pub async fn ensure_runner_installed(
    ip: Ipv4Addr,
    config: &SshConfig,
    runner_version: &str,
) -> Result<()> {
    // Check if runner is already installed
    let check_cmd = "test -d ~/actions-runner && test -f ~/actions-runner/run.sh && echo 'installed'";
    match ssh_exec(ip, config, check_cmd).await {
        Ok(output) if output.trim() == "installed" => {
            debug!("GitHub Actions runner already installed on {}", ip);
            return Ok(());
        }
        _ => {
            info!("GitHub Actions runner not found on {}, installing...", ip);
        }
    }

    // Determine which version to install
    let version = if runner_version.is_empty() || runner_version == "latest" {
        // Fetch latest version from GitHub API
        match fetch_latest_runner_version().await {
            Some(v) => v,
            None => {
                info!(
                    "Could not fetch latest runner version, using fallback v{}",
                    FALLBACK_RUNNER_VERSION
                );
                FALLBACK_RUNNER_VERSION.to_string()
            }
        }
    } else {
        runner_version.to_string()
    };

    // Detect architecture
    let arch_output = ssh_exec(ip, config, "uname -m").await?;
    let arch = arch_output.trim();
    let runner_arch = match arch {
        "arm64" | "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => {
            return Err(Error::Ssh(format!(
                "Unsupported architecture: {}. Expected arm64 or x86_64.",
                arch
            )));
        }
    };

    info!(
        "Installing GitHub Actions runner v{} for osx-{} on {}",
        version, runner_arch, ip
    );

    // Download URL for macOS runner
    let download_url = format!(
        "https://github.com/actions/runner/releases/download/v{}/actions-runner-osx-{}-{}.tar.gz",
        version, runner_arch, version
    );

    // Create directory and download/extract runner
    let install_cmd = format!(
        r#"
        set -e
        mkdir -p ~/actions-runner
        cd ~/actions-runner
        echo "Downloading runner from {url}..."
        curl -sL -o actions-runner.tar.gz "{url}"
        echo "Extracting runner..."
        tar xzf actions-runner.tar.gz
        rm actions-runner.tar.gz
        echo "Runner installed successfully"
        "#,
        url = download_url
    );

    match ssh_exec(ip, config, &install_cmd).await {
        Ok(output) => {
            info!("GitHub Actions runner installed on {}", ip);
            debug!("Install output: {}", output);
            Ok(())
        }
        Err(e) => {
            error!("Failed to install GitHub Actions runner on {}: {}", ip, e);
            Err(e)
        }
    }
}

/// Configure the GitHub Actions runner on a VM.
/// Automatically installs the runner if not present.
pub async fn configure_runner(
    ip: Ipv4Addr,
    config: &SshConfig,
    registration_token: &str,
    labels: &[String],
    runner_scope_url: &str,
    runner_name: &str,
    runner_version: &str,
) -> Result<()> {
    let labels_str = labels.join(",");

    // Validate inputs
    if registration_token.is_empty() {
        return Err(Error::Ssh(
            "GitHub runner registration token is empty - cannot configure runner".to_string(),
        ));
    }

    if runner_scope_url.is_empty() {
        return Err(Error::Ssh(
            "Runner scope URL is empty - cannot configure runner".to_string(),
        ));
    }

    // Ensure runner is installed first
    ensure_runner_installed(ip, config, runner_version).await?;

    info!(
        "Configuring runner {} with labels: {} (scope: {})",
        runner_name, labels_str, runner_scope_url
    );

    // Configure the runner
    let config_cmd = format!(
        "cd ~/actions-runner && ./config.sh --url '{}' --token '{}' --name '{}' --labels '{}' --ephemeral --unattended",
        runner_scope_url,
        registration_token,
        runner_name,
        labels_str
    );

    match ssh_exec(ip, config, &config_cmd).await {
        Ok(output) => {
            debug!("Runner config output: {}", output);
            info!("Runner {} configured successfully", runner_name);
            Ok(())
        }
        Err(e) => {
            error!(
                "Failed to configure runner {} on {}: {}",
                runner_name, ip, e
            );
            error!(
                "This usually means the GitHub registration token is invalid or expired. \
                 Check that the coordinator has valid GitHub App credentials."
            );
            Err(e)
        }
    }
}

/// Start the GitHub Actions runner and wait for it to complete.
///
/// For ephemeral runners, the process exits when the job completes.
/// Returns Ok if the runner exited normally (exit code 0), Err otherwise.
///
/// All runner output (stdout/stderr) is captured to the specified log file.
pub async fn start_runner_and_wait(ip: Ipv4Addr, config: &SshConfig, log_path: &Path) -> Result<()> {
    info!("Starting runner on {} (logging to {})", ip, log_path.display());

    // Run the runner - for ephemeral runners, this blocks until the job completes
    let run_cmd = "cd ~/actions-runner && ./run.sh";

    match ssh_exec_with_logging(ip, config, run_cmd, log_path).await {
        Ok(()) => {
            info!("Runner completed successfully on {}", ip);
            Ok(())
        }
        Err(e) => {
            // For ephemeral runners, exit code 0 means success, anything else is a problem
            error!("Runner failed on {}: {}", ip, e);
            Err(e)
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
        assert!(matches!(config.auth, SshAuth::None));
        assert_eq!(config.timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_ssh_config_with_password() {
        let config = SshConfig::with_password("user", "pass123");
        assert_eq!(config.username, "user");
        assert!(matches!(config.auth, SshAuth::Password(ref p) if p == "pass123"));
    }

    #[test]
    fn test_ssh_config_with_key_file() {
        let config = SshConfig::with_key_file("admin", "/path/to/key");
        assert_eq!(config.username, "admin");
        assert!(
            matches!(config.auth, SshAuth::KeyFile(ref p) if p == &PathBuf::from("/path/to/key"))
        );
    }
}
