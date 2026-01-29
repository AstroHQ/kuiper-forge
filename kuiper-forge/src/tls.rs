//! TLS utilities for coordinator server trust.

use crate::config::{ServerTrustMode, TlsConfig};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct ServerTrust {
    /// Optional CA bundle for server certificate validation.
    pub server_ca_pem: Option<String>,
    /// Server trust mode for agents.
    pub server_trust_mode: ServerTrustMode,
}

pub fn build_server_trust(tls: &TlsConfig) -> Result<ServerTrust> {
    let server_ca_pem = match tls.server_trust_mode {
        ServerTrustMode::Ca => {
            let ca_path = tls.server_ca_cert.as_ref().unwrap_or(&tls.ca_cert);
            Some(read_pem(ca_path).with_context(|| {
                format!("Failed to read server CA cert: {}", ca_path.display())
            })?)
        }
        ServerTrustMode::Chain => {
            if tls.server_use_native_roots {
                None
            } else {
                let ca_path = tls.server_ca_cert.as_ref().unwrap_or(&tls.ca_cert);
                Some(read_pem(ca_path).with_context(|| {
                    format!("Failed to read server CA cert: {}", ca_path.display())
                })?)
            }
        }
    };

    Ok(ServerTrust {
        server_ca_pem,
        server_trust_mode: tls.server_trust_mode.clone(),
    })
}

fn read_pem(path: &Path) -> Result<String> {
    let pem = fs::read_to_string(path)?;
    let trimmed = pem.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Empty PEM file: {}", path.display());
    }
    Ok(trimmed.to_string())
}

// Chain validation is handled client-side based on trust mode.
