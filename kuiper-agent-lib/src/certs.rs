use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tonic::transport::{Certificate, Identity};
use tracing::debug;

/// Manages certificate storage for an agent.
///
/// Certificates are stored in the configured directory:
/// - `ca.crt` - Coordinator CA certificate (for verifying server)
/// - `client.crt` - Agent's client certificate (for mTLS)
/// - `client.key` - Agent's private key (mode 0600)
/// - `metadata.json` - Registration metadata
#[derive(Debug, Clone)]
pub struct AgentCertStore {
    base_dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct CertMetadata {
    agent_id: String,
    registered_at: String,
    expires_at: Option<String>,
}

impl AgentCertStore {
    /// Create a new cert store with the given base directory.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Default cert store location based on the agent name.
    ///
    /// - macOS: `~/Library/Application Support/{agent_name}/certs/`
    /// - Linux: `~/.config/{agent_name}/certs/`
    pub fn default_path(agent_name: &str) -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(agent_name)
            .join("certs")
    }

    fn cert_path(&self) -> PathBuf {
        self.base_dir.join("client.crt")
    }

    fn key_path(&self) -> PathBuf {
        self.base_dir.join("client.key")
    }

    fn ca_path(&self) -> PathBuf {
        self.base_dir.join("ca.crt")
    }

    fn metadata_path(&self) -> PathBuf {
        self.base_dir.join("metadata.json")
    }

    /// Check if we have valid stored certificates.
    pub fn has_certificates(&self) -> bool {
        self.cert_path().exists() && self.key_path().exists() && self.ca_path().exists()
    }

    /// Load certificate and key for mTLS connection.
    pub fn load_identity(&self) -> Result<Identity> {
        let cert = std::fs::read_to_string(self.cert_path())?;
        let key = std::fs::read_to_string(self.key_path())?;
        Ok(Identity::from_pem(cert, key))
    }

    /// Load CA certificate for server verification.
    pub fn load_ca(&self) -> Result<Certificate> {
        let ca = std::fs::read_to_string(self.ca_path())?;
        Ok(Certificate::from_pem(ca))
    }

    /// Save certificates received during registration.
    pub fn save(&self, cert_pem: &str, key_pem: &str, agent_id: &str) -> Result<()> {
        // Create directory with restricted permissions
        std::fs::create_dir_all(&self.base_dir)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.base_dir, std::fs::Permissions::from_mode(0o700))?;
        }

        // Write certificate (can be readable)
        std::fs::write(self.cert_path(), cert_pem)?;
        debug!("Wrote client certificate to {:?}", self.cert_path());

        // Write private key with restricted permissions
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(self.key_path())?;
            file.write_all(key_pem.as_bytes())?;
        }

        #[cfg(not(unix))]
        std::fs::write(self.key_path(), key_pem)?;

        debug!("Wrote client key to {:?}", self.key_path());

        // Write metadata
        let metadata = CertMetadata {
            agent_id: agent_id.to_string(),
            registered_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
        };
        let metadata_json = serde_json::to_string_pretty(&metadata)?;
        std::fs::write(self.metadata_path(), metadata_json)?;

        Ok(())
    }

    /// Save certificates with expiry information.
    pub fn save_with_expiry(
        &self,
        cert_pem: &str,
        key_pem: &str,
        agent_id: &str,
        expires_at: &str,
    ) -> Result<()> {
        self.save(cert_pem, key_pem, agent_id)?;

        // Update metadata with expiry
        let metadata = CertMetadata {
            agent_id: agent_id.to_string(),
            registered_at: chrono::Utc::now().to_rfc3339(),
            expires_at: Some(expires_at.to_string()),
        };
        let metadata_json = serde_json::to_string_pretty(&metadata)?;
        std::fs::write(self.metadata_path(), metadata_json)?;

        Ok(())
    }

    /// Save CA certificate (provided during initial setup).
    pub fn save_ca(&self, ca_pem: &str) -> Result<()> {
        std::fs::create_dir_all(&self.base_dir)?;
        std::fs::write(self.ca_path(), ca_pem)?;
        debug!("Wrote CA certificate to {:?}", self.ca_path());
        Ok(())
    }

    /// Clear stored certificates (on revocation or re-registration).
    pub fn clear(&self) -> Result<()> {
        if self.base_dir.exists() {
            // Only remove certificate files, keep CA
            let _ = std::fs::remove_file(self.cert_path());
            let _ = std::fs::remove_file(self.key_path());
            let _ = std::fs::remove_file(self.metadata_path());
            debug!("Cleared client certificates from {:?}", self.base_dir);
        }
        Ok(())
    }

    /// Get agent ID from stored metadata.
    pub fn get_agent_id(&self) -> Option<String> {
        let content = std::fs::read_to_string(self.metadata_path()).ok()?;
        let metadata: CertMetadata = serde_json::from_str(&content).ok()?;
        Some(metadata.agent_id)
    }

    /// Get the base directory path.
    pub fn base_dir(&self) -> &PathBuf {
        &self.base_dir
    }
}
