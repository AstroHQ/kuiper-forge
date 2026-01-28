use crate::{AgentCertStore, Error, Result};
use kuiper_agent_proto::{AgentServiceClient, RegisterRequest, RegistrationServiceClient};
use tonic::transport::{Channel, ClientTlsConfig};
use tracing::{debug, info, warn};

/// Configuration for connecting to the coordinator.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// gRPC URL of the coordinator (e.g., "https://coordinator.example.com:9443")
    pub coordinator_url: String,
    /// Hostname for TLS verification
    pub coordinator_hostname: String,
    /// Registration token (for first-time registration)
    pub registration_token: Option<String>,
    /// Agent type identifier ("tart" or "proxmox")
    pub agent_type: String,
    /// Labels this agent advertises
    pub labels: Vec<String>,
    /// Maximum concurrent VMs this agent can handle
    pub max_vms: u32,
}

/// Handles connection to the coordinator with automatic registration and mTLS.
pub struct AgentConnector {
    config: AgentConfig,
    cert_store: AgentCertStore,
}

impl AgentConnector {
    /// Create a new connector with the given config and certificate store.
    pub fn new(config: AgentConfig, cert_store: AgentCertStore) -> Self {
        Self { config, cert_store }
    }

    /// Connect to the coordinator, registering if needed.
    ///
    /// This method:
    /// 1. Checks for stored certificates
    /// 2. If found, attempts mTLS connection
    /// 3. If connection fails with auth error, clears certs and re-registers
    /// 4. If no certs, uses registration token to get new certificate
    pub async fn connect(&mut self) -> Result<AgentServiceClient<Channel>> {
        // Check for stored certificates
        if self.cert_store.has_certificates() {
            debug!("Found stored certificates, attempting mTLS connection");
            match self.connect_with_mtls().await {
                Ok(client) => {
                    info!("Connected to coordinator via mTLS");
                    return Ok(client);
                }
                Err(e) if e.is_auth_error() => {
                    // Certificate revoked or expired, need to re-register
                    warn!(
                        "Certificate rejected: {}, clearing and re-registering...",
                        e
                    );
                    self.cert_store.clear()?;
                }
                Err(e) => return Err(e),
            }
        }

        // No valid certificate, need registration token
        let reg_token = self.config.registration_token.clone().ok_or_else(|| {
            Error::NoCredentials(
                "No stored certificate and no registration token provided. \
                     Run: coordinator token create --url https://..."
                    .to_string(),
            )
        })?;

        self.register_and_connect(&reg_token).await
    }

    /// Register with coordinator using registration token.
    ///
    /// Uses the CA certificate from the cert store (provided via the registration bundle)
    /// to verify the coordinator's TLS certificate during registration.
    async fn register_and_connect(&mut self, token: &str) -> Result<AgentServiceClient<Channel>> {
        info!("Registering with coordinator using token...");

        let ca_cert = self.cert_store.load_ca()?;
        debug!("Using CA certificate for registration TLS");
        let tls = ClientTlsConfig::new()
            .ca_certificate(ca_cert)
            .domain_name(&self.config.coordinator_hostname);
        let channel = Channel::from_shared(self.config.coordinator_url.clone())
            .map_err(|e| Error::Certificate(format!("Invalid URL: {e}")))?
            .tls_config(tls)?
            .connect()
            .await?;

        let mut reg_client = RegistrationServiceClient::new(channel);

        // Get hostname
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        // Call Register RPC
        let request = tonic::Request::new(RegisterRequest {
            registration_token: token.to_string(),
            hostname,
            agent_type: self.config.agent_type.clone(),
            labels: self.config.labels.clone(),
            max_vms: self.config.max_vms,
        });

        let response = reg_client
            .register(request)
            .await
            .map_err(|e| Error::RegistrationFailed(e.to_string()))?
            .into_inner();

        // Save the CA certificate received from coordinator
        // This will be used for all future mTLS connections
        if !response.ca_cert_pem.is_empty() {
            self.cert_store.save_ca(&response.ca_cert_pem)?;
            info!("Saved CA certificate from coordinator");
        } else {
            warn!("Coordinator did not provide CA certificate in registration response");
        }

        // Save the issued client certificate
        self.cert_store.save_with_expiry(
            &response.client_cert_pem,
            &response.client_key_pem,
            &response.agent_id,
            &response.expires_at,
        )?;

        info!("Successfully registered as {}", response.agent_id);
        info!("Certificates saved to {:?}", self.cert_store.base_dir());
        info!("Certificate expires: {}", response.expires_at);

        // Explicitly drop the registration client/channel before creating mTLS connection.
        // This prevents HTTP/2 connection reuse from the non-mTLS registration channel.
        drop(reg_client);

        // Brief delay to ensure the old connection is fully closed before creating new one.
        // This works around potential HTTP/2 connection pooling issues.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Now connect with the new certificate
        self.connect_with_mtls().await
    }

    /// Connect using stored mTLS certificate.
    async fn connect_with_mtls(&self) -> Result<AgentServiceClient<Channel>> {
        let identity = self.cert_store.load_identity()?;
        let ca_cert = self.cert_store.load_ca()?;

        let tls = ClientTlsConfig::new()
            .ca_certificate(ca_cert)
            .identity(identity) // Client certificate for mTLS
            .domain_name(&self.config.coordinator_hostname);

        let channel = Channel::from_shared(self.config.coordinator_url.clone())
            .map_err(|e| Error::Certificate(format!("Invalid URL: {e}")))?
            .tls_config(tls)?
            .connect()
            .await?;

        Ok(AgentServiceClient::new(channel))
    }

    /// Get the agent ID if registered.
    pub fn agent_id(&self) -> Option<String> {
        self.cert_store.get_agent_id()
    }
}

/// Hostname detection module (simple implementation).
mod hostname {
    use std::ffi::OsString;

    #[cfg(unix)]
    pub fn get() -> std::io::Result<OsString> {
        use std::ffi::CStr;
        let mut buf = vec![0i8; 256];
        let result = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
        if result == 0 {
            let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
            Ok(OsString::from(cstr.to_string_lossy().into_owned()))
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(windows)]
    pub fn get() -> std::io::Result<OsString> {
        std::env::var_os("COMPUTERNAME").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "COMPUTERNAME not set")
        })
    }

    #[cfg(not(any(unix, windows)))]
    pub fn get() -> std::io::Result<OsString> {
        Ok(OsString::from("unknown"))
    }
}
