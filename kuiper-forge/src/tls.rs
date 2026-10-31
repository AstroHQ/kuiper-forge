//! TLS utilities for coordinator server trust.

use crate::config::{ServerTrustMode, TlsConfig};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::rustls::RootCertStore;
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::ServerCertVerifier;
use tokio_rustls::rustls::crypto::aws_lc_rs;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::sign::CertifiedKey;

#[derive(Clone, Debug)]
pub struct ServerTrust {
    /// Optional CA bundle for server certificate validation.
    pub server_ca_pem: Option<String>,
    /// Server trust mode for agents.
    pub server_trust_mode: ServerTrustMode,
}

pub fn build_server_trust(tls: &TlsConfig) -> Result<ServerTrust> {
    let server_ca_pem =
        match tls.server_trust_mode {
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

/// Why `tls` couldn't serve agents connecting to `hostname`, None if it can. Asks rustls instead of looking at the
/// certificate's fields: the server has to be able to load the certificate with its key, and an agent has to accept
/// it, so it's checked against the CA agents get, for that name, today.
///
/// With native roots there's no bundle of ours to check against, so that part is left out.
pub fn server_identity_problem(tls: &TlsConfig, hostname: &str) -> Option<String> {
    check_server_identity(tls, hostname)
        .err()
        .map(|e| format!("{e:#}"))
}

fn check_server_identity(tls: &TlsConfig, hostname: &str) -> Result<()> {
    let provider = Arc::new(aws_lc_rs::default_provider());

    let chain = CertificateDer::pem_file_iter(&tls.server_cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .with_context(|| format!("can't read {}", tls.server_cert.display()))?;
    let Some((end_entity, intermediates)) = chain.split_first() else {
        anyhow::bail!("no certificate in {}", tls.server_cert.display());
    };
    let key = PrivateKeyDer::from_pem_file(&tls.server_key)
        .with_context(|| format!("can't read {}", tls.server_key.display()))?;
    CertifiedKey::from_der(chain.clone(), key, &provider).with_context(|| {
        format!(
            "{} isn't the key of {}",
            tls.server_key.display(),
            tls.server_cert.display()
        )
    })?;
    if tls.server_use_native_roots {
        return Ok(());
    }

    let ca_path = tls.server_ca_cert.as_ref().unwrap_or(&tls.ca_cert);
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca_path)
        .with_context(|| format!("can't read {}", ca_path.display()))?
    {
        roots.add(cert.with_context(|| format!("can't read {}", ca_path.display()))?)?;
    }
    let verifier = WebPkiServerVerifier::builder_with_provider(roots.into(), provider).build()?;
    let name = ServerName::try_from(hostname.to_string())
        .with_context(|| format!("'{hostname}' isn't a server name"))?;
    verifier.verify_server_cert(end_entity, intermediates, &name, &[], UnixTime::now())?;
    Ok(())
}

/// The name the server certificate was issued for: the first of its subject alternative names, expired or not.
pub fn server_cert_name(path: &Path) -> Option<String> {
    use x509_parser::extensions::GeneralName;
    use x509_parser::pem::parse_x509_pem;

    let pem = fs::read(path).ok()?;
    let (_, pem) = parse_x509_pem(&pem).ok()?;
    let cert = pem.parse_x509().ok()?;
    let names = cert.subject_alternative_name().ok()??;
    match names.value.general_names.first()? {
        GeneralName::DNSName(name) => Some(name.to_string()),
        GeneralName::IPAddress(bytes) => match <[u8; 4]>::try_from(*bytes) {
            Ok(ip) => Some(std::net::Ipv4Addr::from(ip).to_string()),
            Err(_) => <[u8; 16]>::try_from(*bytes)
                .ok()
                .map(|ip| std::net::Ipv6Addr::from(ip).to_string()),
        },
        _ => None,
    }
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
