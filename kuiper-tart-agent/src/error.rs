//! Error types for kuiper-tart-agent.

use thiserror::Error;

/// Tart agent error type.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// VM clone failed.
    #[error("Clone failed: {0}")]
    CloneFailed(String),

    /// Operation timed out.
    #[error("Timeout: {0}")]
    Timeout(&'static str),

    /// Tart CLI error.
    #[error("Tart CLI error: {0}")]
    Tart(String),

    /// SSH connection error.
    #[error("SSH error: {0}")]
    Ssh(String),

    /// Remote command exited non-zero. `stderr` is left out of the message, it can echo tokens or runner
    /// output and the message ends up in uploaded logs.
    #[error("SSH command failed (exit code: {exit_code})")]
    SshCommand { exit_code: u32, stderr: String },

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// gRPC transport error.
    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    /// gRPC status error.
    #[error("gRPC status error: {0}")]
    Status(#[from] tonic::Status),

    /// Agent library error.
    #[error("Agent error: {0}")]
    AgentLib(#[from] kuiper_agent_lib::Error),

    /// Installation error.
    #[error("Install error: {0}")]
    Install(String),
}

impl From<kuiper_agent_lib::ssh::SshError> for Error {
    fn from(e: kuiper_agent_lib::ssh::SshError) -> Self {
        use kuiper_agent_lib::ssh::SshError;
        match e {
            SshError::Timeout(what) => Error::Timeout(what),
            SshError::Command { exit_code, stderr } => Error::SshCommand { exit_code, stderr },
            e => Error::Ssh(e.to_string()),
        }
    }
}

/// Result type alias for kuiper-tart-agent.
pub type Result<T, E = Error> = std::result::Result<T, E>;
