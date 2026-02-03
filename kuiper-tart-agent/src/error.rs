//! Error types for kuiper-tart-agent.

use thiserror::Error;

/// Tart agent error type.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Capacity exceeded - cannot create more VMs.
    #[error("Capacity exceeded: max {0} VMs")]
    CapacityExceeded(u32),

    /// VM clone failed.
    #[error("Clone failed: {0}")]
    CloneFailed(String),

    /// VM not found.
    #[error("VM not found: {0}")]
    VmNotFound(String),

    /// Operation timed out.
    #[error("Timeout: {0}")]
    Timeout(&'static str),

    /// Tart CLI error.
    #[error("Tart CLI error: {0}")]
    Tart(String),

    /// SSH connection error.
    #[error("SSH error: {0}")]
    Ssh(String),

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

    /// Send error on channel.
    #[error("Channel send error")]
    ChannelSend,

    /// VM is already running.
    #[error("VM already running: {0}")]
    VmAlreadyRunning(String),

    /// Installation error.
    #[error("Install error: {0}")]
    Install(String),
}

/// Result type alias for kuiper-tart-agent.
pub type Result<T, E = Error> = std::result::Result<T, E>;
