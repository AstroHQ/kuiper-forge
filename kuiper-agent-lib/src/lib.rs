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
//! - GitHub Actions runner version fetching and download URL construction

pub mod bundle;
mod certs;
mod connector;
mod error;
pub mod github_runner;
pub mod labels;
pub mod runtime;
pub mod shell;

pub use bundle::RegistrationBundle;
pub use certs::AgentCertStore;
pub use connector::{AgentConfig, AgentConnector};
pub use error::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;
