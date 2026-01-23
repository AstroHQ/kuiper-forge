//! Shared library for CI runner agents.
//!
//! This crate provides common functionality for both kuiper-tart-agent and kuiper-proxmox-agent:
//! - Certificate storage and management
//! - gRPC connection handling with mTLS
//! - Registration token exchange
//! - Automatic reconnection with backoff
//! - GitHub Actions runner version fetching and download URL construction

mod certs;
mod connector;
mod error;
pub mod github_runner;

pub use certs::AgentCertStore;
pub use connector::{AgentConfig, AgentConnector, RegistrationTlsMode};
pub use error::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;
