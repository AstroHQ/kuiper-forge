use crate::{AgentCertStore, Error, Result};
use kuiper_agent_proto::{
    AgentServiceClient, RegisterRequest, RegistrationServiceClient,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use std::sync::Arc;
use tokio_rustls::TlsConnector;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Uri};
use tracing::{debug, error, info, warn};

/// How to handle TLS verification during initial registration.
#[derive(Debug, Clone, Default)]
pub enum RegistrationTlsMode {
    /// Skip TLS certificate verification entirely for registration (TOFU).
    /// This is the default for ease of bootstrapping with self-signed CAs.
    /// The registration token provides authentication, and the returned
    /// CA certificate will be used for all subsequent mTLS connections.
    ///
    /// Security note: This trusts the first server you connect to. Ensure
    /// you're connecting to the correct coordinator URL.
    #[default]
    Insecure,

    /// Use the system/native certificate roots for TLS verification.
    /// Works when:
    /// - The coordinator uses a publicly-signed certificate (Let's Encrypt, etc.)
    /// - The internal CA has been installed in the OS trust store
    SystemRoots,

    /// Use a pre-configured CA certificate for registration.
    /// Most secure option - requires distributing the CA cert beforehand.
    ProvidedCa,
}

/// A certificate verifier that accepts any certificate (for TOFU registration).
#[derive(Debug)]
struct InsecureServerCertVerifier;

impl ServerCertVerifier for InsecureServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        // Accept any certificate - this is intentionally insecure for TOFU
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // Support common signature schemes
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

/// Create a rustls ClientConfig that accepts any certificate (insecure).
fn make_insecure_rustls_config() -> ClientConfig {
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("Failed to set protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureServerCertVerifier))
        .with_no_client_auth()
}

/// Create a gRPC channel with insecure TLS (accepts any certificate).
async fn make_insecure_channel(url: &str) -> Result<Channel> {
    use hyper_util::rt::TokioIo;
    use std::task::{Context, Poll};
    use tower::Service;

    // Parse URL to get host and port
    let uri: Uri = url.parse().map_err(|e| Error::Certificate(format!("Invalid URL: {}", e)))?;
    let host = uri.host().ok_or_else(|| Error::Certificate("No host in URL".to_string()))?.to_string();
    let port = uri.port_u16().unwrap_or(443);

    debug!("Connecting to {}:{} (insecure TLS)", host, port);

    // Create insecure TLS connector
    let tls_config = make_insecure_rustls_config();
    let tls_connector = TlsConnector::from(Arc::new(tls_config));

    // Create HTTPS connector wrapper
    #[derive(Clone)]
    struct InsecureHttpsConnector {
        tls: TlsConnector,
        host: String,
        port: u16,
    }

    impl Service<Uri> for InsecureHttpsConnector {
        type Response = TokioIo<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;
        type Error = Box<dyn std::error::Error + Send + Sync>;
        type Future = std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _uri: Uri) -> Self::Future {
            let tls = self.tls.clone();
            let host = self.host.clone();
            let port = self.port;

            Box::pin(async move {
                let addr = format!("{}:{}", host, port);
                tracing::debug!("TCP connecting to {}", addr);

                let tcp = match tokio::net::TcpStream::connect(&addr).await {
                    Ok(stream) => {
                        tracing::debug!("TCP connected successfully");
                        stream
                    }
                    Err(e) => {
                        tracing::error!("TCP connection failed: {} (kind: {:?})", e, e.kind());
                        return Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
                    }
                };

                let server_name = match ServerName::try_from(host.clone()) {
                    Ok(name) => name,
                    Err(e) => {
                        tracing::error!("Invalid server name '{}': {:?}", host, e);
                        return Err(format!("Invalid server name: {}", host).into());
                    }
                };

                tracing::debug!("Starting TLS handshake with server_name: {:?}", server_name);

                let tls_stream = match tls.connect(server_name, tcp).await {
                    Ok(stream) => {
                        tracing::debug!("TLS handshake completed successfully");
                        stream
                    }
                    Err(e) => {
                        tracing::error!("TLS handshake failed: {}", e);
                        return Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
                    }
                };

                Ok(TokioIo::new(tls_stream))
            })
        }
    }

    let connector = InsecureHttpsConnector {
        tls: tls_connector,
        host: host.clone(),
        port,
    };

    // Use http:// scheme for the endpoint since we handle TLS in our connector.
    // Tonic checks the scheme and complains about "HTTPS without TLS enabled" otherwise.
    let endpoint_url = format!("http://{}:{}", host, port);
    let endpoint = Endpoint::from_shared(endpoint_url.clone())
        .map_err(|e| Error::Certificate(format!("Invalid endpoint: {}", e)))?;

    debug!("Creating gRPC channel to {} (via custom TLS connector)", endpoint_url);

    let channel = endpoint
        .connect_with_connector(connector)
        .await
        .map_err(|e| {
            // Try to get more details about the error
            let mut err_chain = String::new();
            let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
            while let Some(err) = source {
                if !err_chain.is_empty() {
                    err_chain.push_str(" -> ");
                }
                err_chain.push_str(&err.to_string());
                source = err.source();
            }
            error!("Channel connection failed: {}", err_chain);
            Error::Connection(err_chain)
        })?;

    debug!("gRPC channel established");
    Ok(channel)
}

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
    /// How to handle TLS during registration (default: TOFU)
    pub registration_tls_mode: RegistrationTlsMode,
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
                    warn!("Certificate rejected: {}, clearing and re-registering...", e);
                    self.cert_store.clear()?;
                }
                Err(e) => return Err(e),
            }
        }

        // No valid certificate, need registration token
        let reg_token = self
            .config
            .registration_token
            .clone()
            .ok_or_else(|| {
                Error::NoCredentials(
                    "No stored certificate and no registration token provided. \
                     Run: coordinator token create --labels ..."
                        .to_string(),
                )
            })?;

        self.register_and_connect(&reg_token).await
    }

    /// Register with coordinator using registration token.
    async fn register_and_connect(
        &mut self,
        token: &str,
    ) -> Result<AgentServiceClient<Channel>> {
        info!("Registering with coordinator using token...");

        // Configure TLS for registration based on mode
        let channel = match self.config.registration_tls_mode {
            RegistrationTlsMode::Insecure => {
                warn!("Using INSECURE TLS for registration (accepting any certificate)");
                warn!("This is safe if you trust the coordinator URL: {}", self.config.coordinator_url);
                // Use custom connector with insecure rustls config
                make_insecure_channel(&self.config.coordinator_url).await?
            }
            RegistrationTlsMode::SystemRoots => {
                info!("Using system/native root certificates for registration TLS");
                let tls = ClientTlsConfig::new()
                    .domain_name(&self.config.coordinator_hostname);
                Channel::from_shared(self.config.coordinator_url.clone())
                    .map_err(|e| Error::Certificate(format!("Invalid URL: {}", e)))?
                    .tls_config(tls)?
                    .connect()
                    .await?
            }
            RegistrationTlsMode::ProvidedCa => {
                let ca_cert = self.cert_store.load_ca()?;
                debug!("Using provided CA certificate for registration");
                let tls = ClientTlsConfig::new()
                    .ca_certificate(ca_cert)
                    .domain_name(&self.config.coordinator_hostname);
                Channel::from_shared(self.config.coordinator_url.clone())
                    .map_err(|e| Error::Certificate(format!("Invalid URL: {}", e)))?
                    .tls_config(tls)?
                    .connect()
                    .await?
            }
        };

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
        info!(
            "Certificates saved to {:?}",
            self.cert_store.base_dir()
        );
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
            .map_err(|e| Error::Certificate(format!("Invalid URL: {}", e)))?
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
