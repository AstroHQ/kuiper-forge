//! Error types for the Proxmox agent.

use thiserror::Error;

/// Main error type for the Proxmox agent.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    /// Proxmox API error
    #[error("Proxmox API error: {0}")]
    ProxmoxApi(#[from] kuiper_proxmox_api::Error),

    /// Agent library error (connection, registration)
    #[error("Agent error: {0}")]
    AgentLib(#[from] kuiper_agent_lib::Error),

    /// SSH connection error
    #[error("SSH error: {0}")]
    Ssh(String),

    /// VM lifecycle error
    #[error("VM error: {0}")]
    Vm(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// gRPC error
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    /// Timeout error
    #[error("Timeout: {0}")]
    Timeout(String),

    /// Runner error
    #[error("Runner error: {0}")]
    Runner(String),

    /// Channel send error
    #[error("Channel send error")]
    ChannelSend,
}

impl Error {
    /// Create a new SSH error.
    pub fn ssh(msg: impl Into<String>) -> Self {
        Error::Ssh(msg.into())
    }

    /// Create a new VM error.
    pub fn vm(msg: impl Into<String>) -> Self {
        Error::Vm(msg.into())
    }

    /// Create a new timeout error.
    pub fn timeout(msg: impl Into<String>) -> Self {
        Error::Timeout(msg.into())
    }

    /// Create a new runner error.
    pub fn runner(msg: impl Into<String>) -> Self {
        Error::Runner(msg.into())
    }
}

/// Result type alias for the Proxmox agent.
pub type Result<T, E = Error> = std::result::Result<T, E>;
