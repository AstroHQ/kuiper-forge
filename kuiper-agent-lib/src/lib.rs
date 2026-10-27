//! Shared library for building coordinator-managed CI runner agents.
//!
//! This crate provides the common building blocks an agent needs, so a new agent
//! (for any VM/provider backend, in-tree or third-party) only has to implement
//! its provider-specific VM lifecycle:
//! - The agent runtime / gRPC stream driver ([`runtime`])
//! - Label matching and capability advertisement ([`labels`])
//! - Certificate storage and management
//! - gRPC connection handling with mTLS
//! - Registration token exchange
//! - Automatic reconnection with backoff
//! - Batched upload of the agent's own logs to the coordinator ([`log_upload`])
//! - GitHub Actions runner version fetching and download URL construction
//! - SSH to runner VMs and the runner install/config commands run over it ([`ssh`], [`github_runner`])
//! - Setting up and running one job's runner on a VM over SSH ([`guest`])

pub mod bundle;
mod certs;
mod connector;
mod error;
pub mod forgejo_runner;
pub mod github_runner;
pub mod guest;
pub mod labels;
pub mod log_upload;
pub mod runtime;
pub mod shell;
pub mod ssh;

pub use bundle::RegistrationBundle;
pub use certs::AgentCertStore;
pub use connector::{AgentConfig, AgentConnector};
pub use error::Error;
pub use log_upload::LogCapture;

pub type Result<T, E = Error> = std::result::Result<T, E>;
