//! Error types for the libvirt agent.

use thiserror::Error;

/// Main error type for the libvirt agent.
#[derive(Debug, Error)]
pub enum Error {
    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    /// virsh or qemu-img failed
    #[error("libvirt error: {0}")]
    Libvirt(String),

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

    /// Timeout error
    #[error("Timeout: {0}")]
    Timeout(String),

    /// Runner error
    #[error("Runner error: {0}")]
    Runner(String),
}

impl From<kuiper_agent_lib::ssh::SshError> for Error {
    fn from(e: kuiper_agent_lib::ssh::SshError) -> Self {
        match e {
            kuiper_agent_lib::ssh::SshError::Timeout(_) => Error::Timeout(e.to_string()),
            e => Error::Ssh(e.to_string()),
        }
    }
}

impl From<kuiper_agent_lib::guest::RunnerError> for Error {
    fn from(e: kuiper_agent_lib::guest::RunnerError) -> Self {
        match e {
            kuiper_agent_lib::guest::RunnerError::Ssh(e) => e.into(),
            kuiper_agent_lib::guest::RunnerError::Runner(msg) => Error::Runner(msg),
        }
    }
}

/// Result type alias for the libvirt agent.
pub type Result<T, E = Error> = std::result::Result<T, E>;
