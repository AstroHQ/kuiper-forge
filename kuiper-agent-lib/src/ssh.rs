//! SSH transport to runner VMs: connect and authenticate, run commands, wait for the port.
//!
//! Host keys aren't checked, every VM is a fresh clone. What runs over the session (runner
//! install/config commands) is up to the agent, see [`crate::github_runner::RunnerConfigBuilder`].

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use russh::client::{self, Config, Handle, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::{HashAlg, PublicKeyOrCertificate};

pub use russh::keys::PrivateKey;
use russh_sftp::client::SftpSession;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// SSH failure. Agents fold it into their own error type, which says it was SSH.
#[derive(Debug, Error)]
pub enum SshError {
    #[error("{0} timed out")]
    Timeout(&'static str),

    #[error("key error: {0}")]
    Key(String),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("{0}")]
    Connection(String),

    /// Remote command exited non-zero. `stderr` is left out of the message, it can echo tokens or runner
    /// output and the message ends up in uploaded logs.
    #[error("command failed (exit code: {exit_code})")]
    Command { exit_code: u32, stderr: String },

    #[error("upload failed: {0}")]
    Upload(String),
}

pub type Result<T, E = SshError> = std::result::Result<T, E>;

/// How to authenticate to the VM.
#[derive(Debug, Clone)]
pub enum SshAuth {
    Key(Arc<PrivateKey>),
    Password(String),
    /// Try the key first, fall back to the password
    KeyThenPassword {
        key: Arc<PrivateKey>,
        password: String,
    },
}

impl SshAuth {
    /// Key auth with an OpenSSH private key file.
    pub fn key_file(path: &Path) -> Result<Self> {
        Ok(Self::Key(Arc::new(load_key(path)?)))
    }

    /// Key auth with the user's default key, `~/.ssh/id_ed25519` then `~/.ssh/id_rsa`.
    pub fn default_key() -> Result<Self> {
        let path = default_key_path().ok_or_else(|| {
            SshError::Key("no default SSH key found (~/.ssh/id_ed25519 or ~/.ssh/id_rsa)".into())
        })?;
        debug!("Using default SSH key {:?}", path);
        Self::key_file(&path)
    }
}

fn default_key_path() -> Option<PathBuf> {
    let ssh_dir = dirs::home_dir()?.join(".ssh");
    ["id_ed25519", "id_rsa"]
        .into_iter()
        .map(|name| ssh_dir.join(name))
        .find(|p| p.exists())
}

/// Load an OpenSSH private key file.
pub fn load_key(path: &Path) -> Result<PrivateKey> {
    let key = std::fs::read_to_string(path)
        .map_err(|e| SshError::Key(format!("failed to read {path:?}: {e}")))?;
    PrivateKey::from_openssh(&key)
        .map_err(|e| SshError::Key(format!("failed to parse {path:?}: {e}")))
}

/// Connects to VMs as one user.
#[derive(Debug, Clone)]
pub struct SshClient {
    username: String,
    auth: SshAuth,
    port: u16,
    /// Per step: TCP connect, handshake, authentication
    timeout: Duration,
}

impl SshClient {
    /// A client for port 22 with a 30s timeout per connection step.
    pub fn new(username: impl Into<String>, auth: SshAuth) -> Self {
        Self {
            username: username.into(),
            auth,
            port: 22,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// Connect and authenticate, one attempt.
    pub async fn connect(&self, ip: IpAddr) -> Result<SshSession> {
        let addr = SocketAddr::new(ip, self.port);

        // Keepalives make a dead VM (say one destroyed mid-job) fail the session after ~90s
        // instead of leaving a runner command waiting forever.
        let config = Arc::new(Config {
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            ..Config::default()
        });

        let stream = timeout(self.timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| SshError::Timeout("SSH connection"))?
            .map_err(|e| SshError::Connection(format!("TCP connection to {addr} failed: {e}")))?;

        let mut handle = timeout(
            self.timeout,
            client::connect_stream(config, stream, ClientHandler),
        )
        .await
        .map_err(|_| SshError::Timeout("SSH handshake"))?
        .map_err(|e| SshError::Connection(format!("handshake with {addr} failed: {e}")))?;

        let authenticated = match &self.auth {
            SshAuth::Key(key) => self.auth_key(&mut handle, key.clone()).await?,
            SshAuth::Password(password) => self.auth_password(&mut handle, password).await?,
            SshAuth::KeyThenPassword { key, password } => {
                match self.auth_key(&mut handle, key.clone()).await {
                    Ok(true) => true,
                    Ok(false) | Err(_) => {
                        debug!("Public key auth failed, trying password auth");
                        self.auth_password(&mut handle, password).await?
                    }
                }
            }
        };
        if !authenticated {
            return Err(SshError::Auth(format!(
                "{addr} rejected user {}",
                self.username
            )));
        }

        Ok(SshSession { handle })
    }

    /// Connect, retrying up to `attempts` times with exponential backoff (2s, 4s, 8s, ... 32s).
    pub async fn connect_with_retries(&self, ip: IpAddr, attempts: u32) -> Result<SshSession> {
        let mut last_error = None;
        for attempt in 1..=attempts {
            debug!("SSH connection attempt {}/{} to {}", attempt, attempts, ip);
            match self.connect(ip).await {
                Ok(session) => {
                    info!("SSH connection established to {}:{}", ip, self.port);
                    return Ok(session);
                }
                Err(e) => {
                    warn!("SSH connection attempt {} failed: {}", attempt, e);
                    last_error = Some(e);
                    if attempt < attempts {
                        tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(5)))).await;
                    }
                }
            }
        }
        Err(last_error
            .unwrap_or_else(|| SshError::Connection("no connection attempts made".into())))
    }

    /// Wait for the SSH port to accept TCP connections.
    pub async fn wait_for_port(&self, ip: IpAddr, wait: Duration) -> Result<()> {
        wait_for_port(SocketAddr::new(ip, self.port), wait).await
    }

    async fn auth_key(
        &self,
        handle: &mut Handle<ClientHandler>,
        key: Arc<PrivateKey>,
    ) -> Result<bool> {
        let hash = rsa_hash(handle, &key).await;
        let result = timeout(
            self.timeout,
            handle.authenticate_publickey(&self.username, PrivateKeyWithHashAlg::new(key, hash)),
        )
        .await
        .map_err(|_| SshError::Timeout("SSH public key authentication"))?
        .map_err(|e| SshError::Auth(format!("public key: {e}")))?;
        Ok(result.success())
    }

    async fn auth_password(
        &self,
        handle: &mut Handle<ClientHandler>,
        password: &str,
    ) -> Result<bool> {
        let result = timeout(
            self.timeout,
            handle.authenticate_password(&self.username, password),
        )
        .await
        .map_err(|_| SshError::Timeout("SSH password authentication"))?
        .map_err(|e| SshError::Auth(format!("password: {e}")))?;
        Ok(result.success())
    }
}

/// Wait for `addr` to accept TCP connections, e.g. a VM's SSH port while it boots.
pub async fn wait_for_port(addr: SocketAddr, wait: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + wait;
    debug!("Waiting for SSH on {}", addr);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(SshError::Timeout("waiting for SSH"));
        }
        // bounded, a connect to a host that drops SYNs can hang for minutes
        let attempt = remaining.min(Duration::from_secs(5));
        if let Ok(Ok(_)) = timeout(attempt, TcpStream::connect(addr)).await {
            info!("SSH port available on {}", addr);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2).min(remaining)).await;
    }
}

/// RSA keys need a SHA-2 signature: OpenSSH 8.8+ turned off `ssh-rsa` (SHA-1) by default, and russh uses SHA-1 when
/// the hash is `None`. Other key types ignore the hash.
async fn rsa_hash<H: Handler>(session: &Handle<H>, key: &PrivateKey) -> Option<HashAlg> {
    if !key.algorithm().is_rsa() {
        return None;
    }
    match session.best_supported_rsa_hash().await {
        Ok(Some(hash)) => hash,

        // server didn't list its algorithms, assume a modern one
        _ => Some(HashAlg::Sha256),
    }
}

/// Read size for [`SshSession::upload`].
const UPLOAD_BUFFER: usize = 256 * 1024;

/// An authenticated SSH session to a VM. Each command runs on its own channel.
pub struct SshSession {
    handle: Handle<ClientHandler>,
}

impl SshSession {
    /// Run a command and collect its output.
    pub async fn execute(&mut self, command: &str) -> Result<CommandOutput> {
        debug!(
            local_only = tracing::field::Empty,
            "Executing SSH command: {}", command
        );
        self.run(command).await
    }

    /// Run a command that carries a secret (a runner token, a JIT config), logging only
    /// `what` it is instead of the command itself.
    pub async fn execute_secret(&mut self, command: &str, what: &str) -> Result<CommandOutput> {
        debug!(
            "Executing SSH command: {} ({} bytes, redacted)",
            what,
            command.len()
        );
        self.run(command).await
    }

    async fn run(&mut self, command: &str) -> Result<CommandOutput> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Connection(format!("failed to open channel: {e}")))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| SshError::Connection(format!("failed to execute command: {e}")))?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;

        // read to Close, not Eof: sshd can send the exit status after EOF
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => exit_code = Some(exit_status),
                ChannelMsg::Close => break,
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

    /// Copy a local file to the VM over SFTP, replacing `remote`. A relative `remote` is under the user's home
    /// directory. Unlike a command's stdin this reads the same on Windows, where PowerShell can take stdin for itself.
    pub async fn upload(&mut self, local: &Path, remote: &str) -> Result<()> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Connection(format!("failed to open channel: {e}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| SshError::Upload(format!("no sftp subsystem: {e}")))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| SshError::Upload(format!("sftp handshake: {e}")))?;

        let mut source = tokio::fs::File::open(local)
            .await
            .map_err(|e| SshError::Upload(format!("failed to read {local:?}: {e}")))?;
        let mut target = sftp
            .create(remote)
            .await
            .map_err(|e| SshError::Upload(format!("failed to create {remote}: {e}")))?;

        // not `io::copy`: its 8K would mean an sftp packet for every 8K, and it flushes, which here
        // is an fsync that a slow VM disk can take longer on than sftp's 10s request timeout
        let mut buffer = vec![0; UPLOAD_BUFFER];
        loop {
            let read = source
                .read(&mut buffer)
                .await
                .map_err(|e| SshError::Upload(format!("failed to read {local:?}: {e}")))?;
            if read == 0 {
                break;
            }
            target
                .write_all(&buffer[..read])
                .await
                .map_err(|e| SshError::Upload(format!("failed to write {remote}: {e}")))?;
        }
        target
            .shutdown()
            .await
            .map_err(|e| SshError::Upload(format!("failed to close {remote}: {e}")))?;
        sftp.close()
            .await
            .map_err(|e| SshError::Upload(format!("failed to end sftp session: {e}")))
    }

    /// Disconnect.
    pub async fn close(self) -> Result<()> {
        self.handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await
            .map_err(|e| SshError::Connection(format!("failed to close session: {e}")))
    }
}

/// Output of a remote command.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u32,
}

impl CommandOutput {
    /// stdout on exit code 0, otherwise [`SshError::Command`].
    pub fn into_stdout(self) -> Result<String> {
        if self.exit_code == 0 {
            Ok(self.stdout)
        } else {
            Err(SshError::Command {
                exit_code: self.exit_code,
                stderr: self.stderr,
            })
        }
    }
}

struct ClientHandler;

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        // ephemeral clones, nothing to check against
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_into_stdout() {
        let ok = CommandOutput {
            stdout: "hi".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        assert_eq!(ok.into_stdout().unwrap(), "hi");

        let failed = CommandOutput {
            stdout: String::new(),
            stderr: "token abc".into(),
            exit_code: 3,
        };
        let err = failed.into_stdout().unwrap_err();
        assert!(matches!(err, SshError::Command { exit_code: 3, .. }));
        // stderr stays out of the message
        assert!(!err.to_string().contains("abc"));
    }

    #[test]
    fn test_key_file_missing() {
        let err = SshAuth::key_file(Path::new("/nonexistent/kuiper-key")).unwrap_err();
        assert!(matches!(err, SshError::Key(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_for_port_times_out() {
        // port 1 on loopback refuses right away
        let client = SshClient::new("ci", SshAuth::Password("x".into())).with_port(1);
        let err = client
            .wait_for_port(IpAddr::from([127, 0, 0, 1]), Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(matches!(err, SshError::Timeout(_)));
    }
}
