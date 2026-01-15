//! Certificate Authority and authentication management.
//!
//! Handles:
//! - CA initialization and certificate generation
//! - Server certificate generation
//! - Agent certificate signing
//! - Registration token management (create, validate, consume)


use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use rand::distributions::Alphanumeric;
use rand::Rng;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Registration token for agent bootstrap
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegistrationToken {
    pub token: String,
    pub labels: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// Certificate issued to an agent
#[derive(Debug, Clone)]
pub struct AgentCertificate {
    pub agent_id: String,
    #[allow(dead_code)] // Metadata - also stored in RegisteredAgent
    pub labels: Vec<String>,
    pub cert_pem: String,
    pub key_pem: String,
    pub expires_at: DateTime<Utc>,
    #[allow(dead_code)] // Metadata - also stored in RegisteredAgent
    pub serial_number: String,
}

/// Registered agent record (stored after certificate issuance)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegisteredAgent {
    pub agent_id: String,
    pub hostname: String,
    pub agent_type: String,
    pub labels: Vec<String>,
    pub max_vms: u32,
    #[allow(dead_code)] // Certificate serial for revocation tracking
    pub serial_number: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked: bool,
}

/// Persistent data structure for auth store
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct AuthStoreData {
    tokens: HashMap<String, RegistrationToken>,
    agents: HashMap<String, RegisteredAgent>,
}

/// File-backed storage for tokens and agents.
/// Data is persisted to a JSON file on each write operation.
#[derive(Debug)]
pub struct AuthStore {
    data: RwLock<AuthStoreData>,
    file_path: std::path::PathBuf,
}

impl AuthStore {
    /// Create a new AuthStore backed by the given file.
    /// If the file exists, data is loaded from it.
    pub fn new(file_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let file_path = file_path.into();
        let data = if file_path.exists() {
            let content = fs::read_to_string(&file_path)
                .with_context(|| format!("Failed to read auth store: {}", file_path.display()))?;
            serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse auth store: {}", file_path.display()))?
        } else {
            AuthStoreData::default()
        };

        Ok(Self {
            data: RwLock::new(data),
            file_path,
        })
    }

    /// Save current state to file
    async fn save(&self) -> Result<()> {
        let data = self.data.read().await;
        let content = serde_json::to_string_pretty(&*data)
            .context("Failed to serialize auth store")?;
        fs::write(&self.file_path, content)
            .with_context(|| format!("Failed to write auth store: {}", self.file_path.display()))?;
        Ok(())
    }

    /// Store a registration token
    pub async fn store_token(&self, token: RegistrationToken) -> Result<()> {
        {
            let mut data = self.data.write().await;
            data.tokens.insert(token.token.clone(), token);
        }
        self.save().await
    }

    /// Get and remove a registration token (single-use)
    pub async fn consume_token(&self, token_str: &str) -> Result<Option<RegistrationToken>> {
        let token = {
            let mut data = self.data.write().await;
            data.tokens.remove(token_str)
        };
        if token.is_some() {
            self.save().await?;
        }
        Ok(token)
    }

    /// Get a token without consuming it (for inspection)
    #[allow(dead_code)]
    pub async fn get_token(&self, token_str: &str) -> Option<RegistrationToken> {
        let data = self.data.read().await;
        data.tokens.get(token_str).cloned()
    }

    /// List all pending registration tokens
    pub async fn list_tokens(&self) -> Vec<RegistrationToken> {
        let data = self.data.read().await;
        data.tokens.values().cloned().collect()
    }

    /// Delete a specific token
    pub async fn delete_token(&self, token_str: &str) -> Result<bool> {
        let removed = {
            let mut data = self.data.write().await;
            data.tokens.remove(token_str).is_some()
        };
        if removed {
            self.save().await?;
        }
        Ok(removed)
    }

    /// Store a registered agent
    pub async fn store_agent(&self, agent: RegisteredAgent) -> Result<()> {
        {
            let mut data = self.data.write().await;
            data.agents.insert(agent.agent_id.clone(), agent);
        }
        self.save().await
    }

    /// Get a registered agent by ID
    pub async fn get_agent(&self, agent_id: &str) -> Option<RegisteredAgent> {
        let data = self.data.read().await;
        data.agents.get(agent_id).cloned()
    }

    /// List all registered agents
    pub async fn list_agents(&self) -> Vec<RegisteredAgent> {
        let data = self.data.read().await;
        data.agents.values().cloned().collect()
    }

    /// Mark an agent as revoked
    pub async fn revoke_agent(&self, agent_id: &str) -> Result<bool> {
        let revoked = {
            let mut data = self.data.write().await;
            if let Some(agent) = data.agents.get_mut(agent_id) {
                agent.revoked = true;
                true
            } else {
                false
            }
        };
        if revoked {
            self.save().await?;
        }
        Ok(revoked)
    }

    /// Check if an agent is valid (exists and not revoked)
    pub async fn is_agent_valid(&self, agent_id: &str) -> bool {
        let data = self.data.read().await;
        data.agents
            .get(agent_id)
            .map(|a| !a.revoked && a.expires_at > Utc::now())
            .unwrap_or(false)
    }
}

/// Stored CA data for signing operations
struct CaData {
    cert: Certificate,
    key: KeyPair,
}

/// Authentication manager that handles CA operations and token/certificate management
pub struct AuthManager {
    store: Arc<AuthStore>,
    ca_data: CaData,
    ca_cert_pem: String,
}

impl AuthManager {
    /// Create a new AuthManager by loading CA cert and key from files
    pub fn new(store: Arc<AuthStore>, ca_cert_path: &Path, ca_key_path: &Path) -> Result<Self> {
        let ca_cert_pem = fs::read_to_string(ca_cert_path)
            .with_context(|| format!("Failed to read CA cert: {}", ca_cert_path.display()))?;

        let ca_key_pem = fs::read_to_string(ca_key_path)
            .with_context(|| format!("Failed to read CA key: {}", ca_key_path.display()))?;

        // Parse the CA key
        let ca_key = KeyPair::from_pem(&ca_key_pem)
            .map_err(|e| anyhow!("Failed to parse CA key: {}", e))?;

        // Extract subject from the actual CA certificate on disk
        let ca_subject = extract_ca_subject(&ca_cert_pem)?;

        // We need to recreate the CA certificate from the key
        // Since rcgen doesn't support loading existing certs for signing,
        // we recreate CA params using the ACTUAL subject from the cert on disk
        let ca_params = create_ca_params(&ca_subject.org_name)?;
        let ca_cert = ca_params
            .self_signed(&ca_key)
            .map_err(|e| anyhow!("Failed to create CA cert: {}", e))?;

        Ok(Self {
            store,
            ca_data: CaData {
                cert: ca_cert,
                key: ca_key,
            },
            ca_cert_pem,
        })
    }

    /// Get the CA certificate PEM (the original one from disk)
    #[allow(dead_code)]
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// Generate a new registration token
    pub async fn create_registration_token(
        &self,
        labels: Vec<String>,
        ttl: Duration,
        created_by: &str,
    ) -> Result<RegistrationToken> {
        let token = format!("reg_{}", generate_random_string(32));
        let now = Utc::now();
        let expires_at = now + ttl;

        let reg_token = RegistrationToken {
            token,
            labels,
            expires_at,
            created_by: created_by.to_string(),
            created_at: now,
        };

        self.store.store_token(reg_token.clone()).await?;
        Ok(reg_token)
    }

    /// Exchange registration token for client certificate
    pub async fn exchange_token_for_certificate(
        &self,
        token: &str,
        hostname: &str,
        agent_type: &str,
        labels: Vec<String>,
        max_vms: u32,
    ) -> Result<AgentCertificate> {
        // 1. Consume the token (single-use)
        let reg_token = self
            .store
            .consume_token(token)
            .await?
            .ok_or_else(|| anyhow!("Invalid or already used registration token"))?;

        // 2. Check expiry
        if reg_token.expires_at < Utc::now() {
            return Err(anyhow!("Registration token has expired"));
        }

        // 3. Generate agent ID
        let agent_id = format!(
            "agent_{}_{}_{}",
            agent_type,
            sanitize_hostname(hostname),
            generate_random_string(8)
        );

        // 4. Generate client certificate
        let validity_days = 365i64;
        let expires_at = Utc::now() + Duration::days(validity_days);

        // Create agent certificate params
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, &agent_id);
        dn.push(DnType::OrganizationName, "CI Runner Agent");
        params.distinguished_name = dn;

        // Set validity using time crate
        let now = ::time::OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + ::time::Duration::days(validity_days);

        // Add SAN for the agent ID
        params.subject_alt_names = vec![SanType::DnsName(agent_id.clone().try_into().unwrap())];

        // Key usage for client authentication
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];

        // Generate key pair for agent
        let agent_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| anyhow!("Failed to generate key pair: {}", e))?;

        // Sign the certificate with our CA
        let agent_cert = params
            .signed_by(&agent_key, &self.ca_data.cert, &self.ca_data.key)
            .map_err(|e| anyhow!("Failed to sign certificate: {}", e))?;

        let cert_pem = agent_cert.pem();
        let key_pem = agent_key.serialize_pem();
        let serial = hex::encode(agent_cert.der());

        // 5. Record agent in store
        // Use the labels from the request, merging with token labels
        let mut all_labels = reg_token.labels.clone();
        for label in labels {
            if !all_labels.contains(&label) {
                all_labels.push(label);
            }
        }

        let registered = RegisteredAgent {
            agent_id: agent_id.clone(),
            hostname: hostname.to_string(),
            agent_type: agent_type.to_string(),
            labels: all_labels.clone(),
            max_vms,
            serial_number: serial.clone(),
            created_at: Utc::now(),
            expires_at,
            revoked: false,
        };
        self.store.store_agent(registered).await?;

        Ok(AgentCertificate {
            agent_id,
            labels: all_labels,
            cert_pem,
            key_pem,
            expires_at,
            serial_number: serial,
        })
    }

    /// List all pending registration tokens
    pub async fn list_tokens(&self) -> Vec<RegistrationToken> {
        self.store.list_tokens().await
    }

    /// Delete a registration token
    pub async fn delete_token(&self, token: &str) -> Result<bool> {
        self.store.delete_token(token).await
    }

    /// List all registered agents
    pub async fn list_agents(&self) -> Vec<RegisteredAgent> {
        self.store.list_agents().await
    }

    /// Revoke an agent's certificate
    pub async fn revoke_agent(&self, agent_id: &str) -> Result<bool> {
        self.store.revoke_agent(agent_id).await
    }

    /// Check if an agent is valid
    pub async fn is_agent_valid(&self, agent_id: &str) -> bool {
        self.store.is_agent_valid(agent_id).await
    }

    /// Get agent by ID
    #[allow(dead_code)]
    pub async fn get_agent(&self, agent_id: &str) -> Option<RegisteredAgent> {
        self.store.get_agent(agent_id).await
    }
}

/// Create CA certificate params
fn create_ca_params(org_name: &str) -> Result<CertificateParams> {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, &format!("{} CA", org_name));
    dn.push(DnType::OrganizationName, org_name);
    params.distinguished_name = dn;

    // CA-specific settings
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    // Valid for 10 years
    let now = ::time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + ::time::Duration::days(3650);

    Ok(params)
}

/// Initialize a new Certificate Authority
pub fn init_ca(ca_cert_path: &Path, ca_key_path: &Path, org_name: &str) -> Result<()> {
    // Check if files already exist
    if ca_cert_path.exists() || ca_key_path.exists() {
        return Err(anyhow!(
            "CA files already exist. Remove them first if you want to reinitialize."
        ));
    }

    // Generate CA key pair
    let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| anyhow!("Failed to generate CA key: {}", e))?;

    // Create CA certificate params
    let params = create_ca_params(org_name)?;

    // Self-sign the CA certificate
    let ca_cert = params
        .self_signed(&ca_key)
        .map_err(|e| anyhow!("Failed to create CA certificate: {}", e))?;

    // Create parent directories
    if let Some(parent) = ca_cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = ca_key_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write CA certificate
    fs::write(ca_cert_path, ca_cert.pem())?;

    // Write CA key with restricted permissions
    write_private_key(ca_key_path, &ca_key.serialize_pem())?;

    Ok(())
}

/// Generate a server certificate signed by the CA
pub fn generate_server_cert(
    ca_cert_path: &Path,
    ca_key_path: &Path,
    server_cert_path: &Path,
    server_key_path: &Path,
    hostname: &str,
) -> Result<()> {
    // Load CA cert and key
    let ca_cert_pem = fs::read_to_string(ca_cert_path)
        .with_context(|| format!("Failed to read CA cert: {}", ca_cert_path.display()))?;

    let ca_key_pem = fs::read_to_string(ca_key_path)
        .with_context(|| format!("Failed to read CA key: {}", ca_key_path.display()))?;

    let ca_key = KeyPair::from_pem(&ca_key_pem)
        .map_err(|e| anyhow!("Failed to parse CA key: {}", e))?;

    // Extract subject from the actual CA certificate on disk
    let ca_subject = extract_ca_subject(&ca_cert_pem)?;

    // Recreate CA cert for signing using ACTUAL subject from disk
    let ca_params = create_ca_params(&ca_subject.org_name)?;
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .map_err(|e| anyhow!("Failed to reconstruct CA cert: {}", e))?;

    // Generate server key
    let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| anyhow!("Failed to generate server key: {}", e))?;

    // Create server certificate params
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, hostname);
    dn.push(DnType::OrganizationName, "CI Runner Coordinator");
    params.distinguished_name = dn;

    // Set validity (1 year)
    let now = ::time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + ::time::Duration::days(365);

    // Add SAN for hostname
    params.subject_alt_names = vec![
        SanType::DnsName(hostname.to_string().try_into().unwrap()),
        SanType::DnsName("localhost".to_string().try_into().unwrap()),
    ];

    // Key usage for server authentication
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    // Sign with CA
    let server_cert = params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .map_err(|e| anyhow!("Failed to sign server certificate: {}", e))?;

    // Create parent directories
    if let Some(parent) = server_cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = server_key_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write server certificate
    fs::write(server_cert_path, server_cert.pem())?;

    // Write server key with restricted permissions
    write_private_key(server_key_path, &server_key.serialize_pem())?;

    // Also update the CA cert file if it doesn't exist (for self-contained setup)
    if !ca_cert_path.exists() {
        fs::write(ca_cert_path, ca_cert.pem())?;
    }

    Ok(())
}

/// Export CA certificate to stdout
pub fn export_ca_cert(ca_cert_path: &Path) -> Result<String> {
    fs::read_to_string(ca_cert_path)
        .with_context(|| format!("Failed to read CA cert: {}", ca_cert_path.display()))
}

/// Write a private key with restricted permissions (0600 on Unix)
fn write_private_key(path: &Path, content: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        std::io::Write::write_all(&mut file, content.as_bytes())?;
    }

    #[cfg(not(unix))]
    {
        fs::write(path, content)?;
    }

    Ok(())
}

/// Generate a random alphanumeric string
fn generate_random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

/// Sanitize hostname for use in agent ID
fn sanitize_hostname(hostname: &str) -> String {
    hostname
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .to_lowercase()
}

/// CA subject info extracted from certificate
struct CaSubject {
    org_name: String,
}

/// Extract subject information from a CA certificate PEM
fn extract_ca_subject(ca_cert_pem: &str) -> Result<CaSubject> {
    use x509_parser::pem::parse_x509_pem;

    // Parse PEM and certificate
    let (_, pem) = parse_x509_pem(ca_cert_pem.as_bytes())
        .map_err(|e| anyhow!("Failed to parse CA cert PEM: {}", e))?;

    let cert = pem.parse_x509()
        .map_err(|e| anyhow!("Failed to parse CA certificate: {}", e))?;

    // Extract organization name from subject DN
    let subject = cert.subject();
    let org_name = subject
        .iter_organization()
        .next()
        .and_then(|o| o.as_str().ok())
        .unwrap_or("CI Runner")
        .to_string();

    Ok(CaSubject { org_name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_generate_random_string() {
        let s = generate_random_string(32);
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_alphanumeric()));
    }

    #[test]
    fn test_sanitize_hostname() {
        assert_eq!(sanitize_hostname("Mac-Mini-1"), "mac-mini-1");
        assert_eq!(sanitize_hostname("host.example.com"), "host-example-com");
        assert_eq!(sanitize_hostname("host_name"), "host-name");
    }

    #[test]
    fn test_init_ca() {
        let temp = TempDir::new().unwrap();
        let ca_cert = temp.path().join("ca.crt");
        let ca_key = temp.path().join("ca.key");

        init_ca(&ca_cert, &ca_key, "Test Org").unwrap();

        assert!(ca_cert.exists());
        assert!(ca_key.exists());

        let cert_content = fs::read_to_string(&ca_cert).unwrap();
        assert!(cert_content.contains("BEGIN CERTIFICATE"));

        let key_content = fs::read_to_string(&ca_key).unwrap();
        assert!(key_content.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn test_generate_server_cert() {
        let temp = TempDir::new().unwrap();
        let ca_cert = temp.path().join("ca.crt");
        let ca_key = temp.path().join("ca.key");
        let server_cert = temp.path().join("server.crt");
        let server_key = temp.path().join("server.key");

        init_ca(&ca_cert, &ca_key, "Test Org").unwrap();
        generate_server_cert(&ca_cert, &ca_key, &server_cert, &server_key, "localhost").unwrap();

        assert!(server_cert.exists());
        assert!(server_key.exists());
    }

    #[tokio::test]
    async fn test_token_lifecycle() {
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("auth_store.json");
        let store = Arc::new(AuthStore::new(&store_path).unwrap());

        // Create a token
        let token = RegistrationToken {
            token: "reg_test123".to_string(),
            labels: vec!["macos".to_string()],
            expires_at: Utc::now() + Duration::hours(1),
            created_by: "admin".to_string(),
            created_at: Utc::now(),
        };

        store.store_token(token.clone()).await.unwrap();

        // Get token should work
        let retrieved = store.get_token("reg_test123").await;
        assert!(retrieved.is_some());

        // Consume token should work once
        let consumed = store.consume_token("reg_test123").await.unwrap();
        assert!(consumed.is_some());

        // Token should be gone now
        let gone = store.consume_token("reg_test123").await.unwrap();
        assert!(gone.is_none());
    }
}
