//! SSH client for connecting to and configuring VMs.
//!
//! This module provides SSH connectivity to VMs for:
//! - Configuring GitHub Actions runner
//! - Starting the runner process
//! - Monitoring runner completion

use crate::config::SshConfig;
use crate::error::{Error, Result};
use async_trait::async_trait;
use russh::client::{self, Config, Handle, Handler};
use russh::ChannelMsg;
use russh_keys::PrivateKey;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// SSH client for VM operations.
pub struct SshClient {
    config: SshConfig,
    private_key: Arc<PrivateKey>,
}

impl SshClient {
    /// Create a new SSH client with the given configuration.
    pub async fn new(config: SshConfig) -> Result<Self> {
        let key_path = &config.private_key_path;
        let private_key = Self::load_private_key(key_path).await?;

        Ok(Self {
            config,
            private_key: Arc::new(private_key),
        })
    }

    /// Load a private key from a file.
    async fn load_private_key(path: &Path) -> Result<PrivateKey> {
        let key_bytes = tokio::fs::read(path).await.map_err(|e| {
            Error::ssh(format!("Failed to read private key {:?}: {}", path, e))
        })?;

        let key_str = String::from_utf8(key_bytes).map_err(|e| {
            Error::ssh(format!("Invalid UTF-8 in key file: {}", e))
        })?;

        // Try to decode the key (supports both encrypted and unencrypted keys)
        russh_keys::PrivateKey::from_openssh(&key_str).map_err(|e| {
            Error::ssh(format!("Failed to decode private key: {}", e))
        })
    }

    /// Connect to a VM via SSH with retries.
    pub async fn connect(&self, ip: &str) -> Result<SshSession> {
        let addr: SocketAddr = format!("{}:{}", ip, self.config.port)
            .parse()
            .map_err(|e| Error::ssh(format!("Invalid address: {}", e)))?;

        let connect_timeout = Duration::from_secs(self.config.timeout_secs);
        let mut last_error = None;

        for attempt in 1..=self.config.retries {
            debug!("SSH connection attempt {}/{} to {}", attempt, self.config.retries, addr);

            match self.try_connect(&addr, connect_timeout).await {
                Ok(session) => {
                    info!("SSH connection established to {}", addr);
                    return Ok(session);
                }
                Err(e) => {
                    warn!(
                        "SSH connection attempt {} failed: {}",
                        attempt, e
                    );
                    last_error = Some(e);

                    if attempt < self.config.retries {
                        // Exponential backoff: 2s, 4s, 8s, etc.
                        let delay = Duration::from_secs(2u64.pow(attempt.min(5)));
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::ssh("No connection attempts made")))
    }

    /// Attempt a single SSH connection.
    async fn try_connect(&self, addr: &SocketAddr, connect_timeout: Duration) -> Result<SshSession> {
        // Configure SSH client
        let ssh_config = Arc::new(Config::default());
        let handler = ClientHandler;

        // Connect with timeout
        let stream = timeout(connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::timeout(format!("Connection to {} timed out", addr)))?
            .map_err(|e| Error::ssh(format!("TCP connection failed: {}", e)))?;

        // Create SSH connection
        let mut handle = timeout(
            connect_timeout,
            client::connect_stream(ssh_config.clone(), stream, handler),
        )
        .await
        .map_err(|_| Error::timeout("SSH handshake timed out"))?
        .map_err(|e| Error::ssh(format!("SSH handshake failed: {}", e)))?;

        // Authenticate with public key
        let authenticated = timeout(
            connect_timeout,
            handle.authenticate_publickey(
                &self.config.username,
                self.private_key.clone(),
            ),
        )
        .await
        .map_err(|_| Error::timeout("SSH authentication timed out"))?
        .map_err(|e| Error::ssh(format!("SSH authentication error: {}", e)))?;

        if !authenticated {
            return Err(Error::ssh("SSH authentication failed: not authenticated"));
        }

        Ok(SshSession {
            handle,
            _config: ssh_config,
        })
    }

    /// Wait for SSH to become available on a VM.
    pub async fn wait_for_ssh(&self, ip: &str, timeout_duration: Duration) -> Result<()> {
        let addr: SocketAddr = format!("{}:{}", ip, self.config.port)
            .parse()
            .map_err(|e| Error::ssh(format!("Invalid address: {}", e)))?;

        let start = std::time::Instant::now();
        let check_interval = Duration::from_secs(2);

        while start.elapsed() < timeout_duration {
            match TcpStream::connect(&addr).await {
                Ok(_) => {
                    debug!("SSH port is open on {}", addr);
                    return Ok(());
                }
                Err(_) => {
                    debug!("SSH not yet available on {}", addr);
                    tokio::time::sleep(check_interval).await;
                }
            }
        }

        Err(Error::timeout(format!(
            "SSH not available on {} within {:?}",
            addr, timeout_duration
        )))
    }
}

/// An established SSH session to a VM.
pub struct SshSession {
    handle: Handle<ClientHandler>,
    _config: Arc<Config>,
}

impl SshSession {
    /// Execute a command and return the output.
    pub async fn execute(&mut self, command: &str) -> Result<CommandOutput> {
        debug!("Executing SSH command: {}", command);

        let mut channel = self.handle.channel_open_session().await.map_err(|e| {
            Error::ssh(format!("Failed to open SSH channel: {}", e))
        })?;

        channel.exec(true, command).await.map_err(|e| {
            Error::ssh(format!("Failed to execute command: {}", e))
        })?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;

        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    stdout.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExtendedData { data, ext }) => {
                    if ext == 1 {
                        // stderr
                        stderr.extend_from_slice(&data);
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status);
                }
                Some(ChannelMsg::Eof) | None => {
                    break;
                }
                _ => {}
            }
        }

        let output = CommandOutput {
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
            exit_code: exit_code.unwrap_or(0),
        };

        debug!(
            "Command completed with exit code {}: stdout={} bytes, stderr={} bytes",
            output.exit_code,
            output.stdout.len(),
            output.stderr.len()
        );

        Ok(output)
    }

    /// Execute a command in the background (nohup-style).
    ///
    /// This is useful for starting the GitHub runner which should continue
    /// running after the SSH session ends.
    pub async fn execute_background(&mut self, command: &str) -> Result<()> {
        debug!("Executing background command: {}", command);

        let mut channel = self.handle.channel_open_session().await.map_err(|e| {
            Error::ssh(format!("Failed to open SSH channel: {}", e))
        })?;

        // Use nohup and redirect output to prevent hangups
        let bg_command = format!("nohup {} > /dev/null 2>&1 &", command);
        channel.exec(true, bg_command.as_str()).await.map_err(|e| {
            Error::ssh(format!("Failed to execute background command: {}", e))
        })?;

        // Wait for channel to close
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Eof) | None => break,
                _ => {}
            }
        }

        debug!("Background command started");
        Ok(())
    }

    /// Check if the GitHub runner process is still running.
    pub async fn is_runner_running(&mut self) -> Result<bool> {
        // Check for the runner process (Runner.Listener on Linux, Runner.Listener.exe on Windows)
        let output = self.execute("pgrep -f 'Runner.Listener' || true").await?;
        Ok(!output.stdout.trim().is_empty())
    }

    /// Close the SSH session.
    pub async fn close(self) -> Result<()> {
        self.handle.disconnect(russh::Disconnect::ByApplication, "", "en").await.map_err(|e| {
            Error::ssh(format!("Failed to close SSH session: {}", e))
        })?;
        Ok(())
    }
}

/// Output from an SSH command execution.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u32,
}


/// SSH client handler for connection events.
struct ClientHandler;

#[async_trait]
impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh_keys::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        // Accept all server keys for now
        // In production, you might want to implement known_hosts checking
        Ok(true)
    }
}

/// Builder for GitHub runner configuration commands.
pub struct RunnerConfigBuilder {
    runner_dir: String,
    is_windows: bool,
}

impl RunnerConfigBuilder {
    /// Create a new runner config builder for Linux.
    pub fn linux(runner_dir: &str) -> Self {
        Self {
            runner_dir: runner_dir.to_string(),
            is_windows: false,
        }
    }

    /// Create a new runner config builder for Windows.
    pub fn windows(runner_dir: &str) -> Self {
        Self {
            runner_dir: runner_dir.to_string(),
            is_windows: true,
        }
    }

    /// Build the runner configuration command.
    pub fn config_command(
        &self,
        url: &str,
        token: &str,
        labels: &[String],
        name: &str,
    ) -> String {
        let labels_str = labels.join(",");

        if self.is_windows {
            format!(
                r#"cd {}; .\config.cmd --url {} --token {} --labels {} --name {} --ephemeral --unattended"#,
                self.runner_dir, url, token, labels_str, name
            )
        } else {
            format!(
                r#"cd {} && ./config.sh --url {} --token {} --labels {} --name {} --ephemeral --unattended"#,
                self.runner_dir, url, token, labels_str, name
            )
        }
    }

    /// Build the runner start command for background execution.
    pub fn run_command_background(&self) -> String {
        if self.is_windows {
            // On Windows, use Start-Process for background execution
            format!(
                r#"Start-Process -FilePath "{}\run.cmd" -WindowStyle Hidden -WorkingDirectory "{}""#,
                self.runner_dir, self.runner_dir
            )
        } else {
            format!(r#"cd {} && nohup ./run.sh > runner.log 2>&1 &"#, self.runner_dir)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linux_config_command() {
        let builder = RunnerConfigBuilder::linux("~/actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN123",
            &["self-hosted".to_string(), "linux".to_string()],
            "runner-001",
        );

        assert!(cmd.contains("./config.sh"));
        assert!(cmd.contains("--ephemeral"));
        assert!(cmd.contains("--unattended"));
        assert!(cmd.contains("self-hosted,linux"));
    }

    #[test]
    fn test_windows_config_command() {
        let builder = RunnerConfigBuilder::windows(r"C:\actions-runner");
        let cmd = builder.config_command(
            "https://github.com/org/repo",
            "TOKEN123",
            &["self-hosted".to_string(), "windows".to_string()],
            "runner-001",
        );

        assert!(cmd.contains(".\\config.cmd"));
        assert!(cmd.contains("--ephemeral"));
        assert!(cmd.contains("--unattended"));
    }
}
