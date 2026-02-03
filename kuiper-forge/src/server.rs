//! gRPC server implementation with optional mTLS support and HTTP webhook endpoint.
//!
//! Provides:
//! - RegistrationService: Token-based agent registration (no client cert required)
//! - AgentService: Bidirectional streaming for agent communication (client cert required)
//! - Webhook endpoint: HTTP endpoint for GitHub webhooks (webhook mode only)
//!
//! Security model:
//! - Single port with optional client certificate verification
//! - Registration service works without client cert (uses registration token)
//! - Agent service requires client cert - identity extracted from certificate CN
//! - Webhook endpoint validates GitHub HMAC-SHA256 signatures

use anyhow::{Context, Result};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as HttpBuilder;
use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentService, AgentServiceServer, CoordinatorMessage,
    CoordinatorPayload, Ping, RegisterRequest, RegisterResponse, RegistrationService,
    RegistrationServiceServer,
};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::service::Routes;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};
use tower::Service;
use tracing::{debug, error, info, warn};

use crate::admin::AdminState;
use crate::agent_registry::{AgentRegistry, AgentType};
use crate::auth::AuthManager;
use crate::config::{TlsConfig, WebhookConfig};
use crate::fleet::FleetNotifier;
use crate::pending_jobs::PendingJobStore;
use crate::runner_state::RunnerStateStore;
use crate::tls::ServerTrust;
use crate::webhook::{self, WebhookNotifier, WebhookState};

/// Parse PROXY protocol header from an incoming TCP connection.
///
/// Supports both PROXY protocol v1 (text) and v2 (binary).
/// Returns the real client address extracted from the header.
///
/// The PROXY protocol header is sent by load balancers (HAProxy, AWS NLB, DO LB)
/// to pass the original client IP to the backend server.
async fn parse_proxy_protocol(
    stream: &mut TcpStream,
    fallback_addr: SocketAddr,
) -> Result<SocketAddr> {
    use ppp::v1::Addresses as V1Addr;
    use ppp::v2::Addresses as V2Addr;
    use ppp::HeaderResult;
    use tokio::io::AsyncReadExt;

    // PROXY protocol headers are at most 107 bytes (v1) or 232 bytes (v2)
    // Read incrementally to avoid over-reading into TLS data
    let mut buf = Vec::with_capacity(232);
    let mut tmp = [0u8; 1];

    // First, determine if this is v1 (text) or v2 (binary) by reading the first byte
    // v1 starts with "PROXY " (0x50 = 'P')
    // v2 starts with signature: 0x0D 0x0A 0x0D 0x0A 0x00 0x0D 0x0A 0x51 0x55 0x49 0x54 0x0A

    // Read first byte to determine protocol version
    let n = stream
        .read(&mut tmp)
        .await
        .context("Failed to read PROXY protocol header")?;
    if n == 0 {
        anyhow::bail!("Connection closed before PROXY protocol header received");
    }
    buf.push(tmp[0]);

    let is_v2 = tmp[0] == 0x0D; // v2 signature starts with 0x0D

    if is_v2 {
        // For v2, read byte-by-byte until ppp says complete
        loop {
            let result = HeaderResult::parse(&buf);

            if let HeaderResult::V2(Ok(header)) = result {
                let real_addr = match header.addresses {
                    V2Addr::IPv4(addr) => {
                        SocketAddr::from((addr.source_address, addr.source_port))
                    }
                    V2Addr::IPv6(addr) => {
                        SocketAddr::from((addr.source_address, addr.source_port))
                    }
                    V2Addr::Unix(_) | V2Addr::Unspecified => fallback_addr,
                };

                debug!(
                    proxy_version = 2,
                    real_addr = %real_addr,
                    header_len = buf.len(),
                    "Parsed PROXY protocol v2 header"
                );

                return Ok(real_addr);
            }

            // Need more data for v2
            if buf.len() >= 536 {
                anyhow::bail!("PROXY protocol v2 header too large or malformed");
            }

            let n = stream
                .read(&mut tmp)
                .await
                .context("Failed to read PROXY protocol header")?;
            if n == 0 {
                anyhow::bail!("Connection closed while reading PROXY protocol v2 header");
            }
            buf.push(tmp[0]);
        }
    } else {
        // For v1, read until \r\n (the v1 header is a single line)
        // v1 format: "PROXY TCP4|TCP6|UNKNOWN <src> <dst> <srcport> <dstport>\r\n"
        loop {
            if buf.len() >= 2 && buf[buf.len() - 2] == b'\r' && buf[buf.len() - 1] == b'\n' {
                // Found end of v1 header
                break;
            }
            if buf.len() > 107 {
                anyhow::bail!("PROXY protocol v1 header too long (max 107 bytes)");
            }

            let n = stream
                .read(&mut tmp)
                .await
                .context("Failed to read PROXY protocol header")?;
            if n == 0 {
                anyhow::bail!("Connection closed while reading PROXY protocol v1 header");
            }
            buf.push(tmp[0]);
        }

        // Now parse the complete v1 header
        let result = HeaderResult::parse(&buf);

        match result {
            HeaderResult::V1(Ok(header)) => {
                let real_addr = match header.addresses {
                    V1Addr::Tcp4(addr) => SocketAddr::from((addr.source_address, addr.source_port)),
                    V1Addr::Tcp6(addr) => SocketAddr::from((addr.source_address, addr.source_port)),
                    V1Addr::Unknown => fallback_addr,
                };

                debug!(
                    proxy_version = 1,
                    real_addr = %real_addr,
                    header_len = buf.len(),
                    "Parsed PROXY protocol v1 header"
                );

                return Ok(real_addr);
            }
            HeaderResult::V1(Err(e)) => {
                anyhow::bail!("Invalid PROXY protocol v1 header: {:?}", e);
            }
            HeaderResult::V2(_) => {
                // Should not happen since first byte wasn't 0x0D
                anyhow::bail!("Unexpected v2 result for v1 header");
            }
        }
    }
}

/// Custom extension for peer certificates when using manual TLS handling.
/// Used in webhook mode where we handle TLS ourselves instead of letting tonic do it.
#[derive(Clone)]
pub struct PeerCertificates(pub Vec<Vec<u8>>);

/// Registration service implementation
pub struct RegistrationServiceImpl {
    auth_manager: Arc<AuthManager>,
    server_trust: ServerTrust,
}

impl RegistrationServiceImpl {
    pub fn new(auth_manager: Arc<AuthManager>, server_trust: ServerTrust) -> Self {
        Self {
            auth_manager,
            server_trust,
        }
    }
}

#[tonic::async_trait]
impl RegistrationService for RegistrationServiceImpl {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let req = request.into_inner();

        info!(
            hostname = %req.hostname,
            agent_type = %req.agent_type,
            labels = ?req.labels,
            "Agent registration request"
        );

        // Exchange token for certificate
        let cert = self
            .auth_manager
            .exchange_token_for_certificate(
                &req.registration_token,
                &req.hostname,
                &req.agent_type,
                req.labels,
                req.max_vms,
            )
            .await
            .map_err(|e| {
                warn!(error = %e, "Registration failed");
                Status::unauthenticated(format!("Registration failed: {e}"))
            })?;

        info!(
            agent_id = %cert.agent_id,
            expires_at = %cert.expires_at,
            "Agent registered successfully"
        );

        Ok(Response::new(RegisterResponse {
            agent_id: cert.agent_id,
            client_cert_pem: cert.cert_pem,
            client_key_pem: cert.key_pem,
            expires_at: cert.expires_at.to_rfc3339(),
            server_ca_pem: self.server_trust.server_ca_pem.clone().unwrap_or_default(),
            server_trust_mode: match self.server_trust.server_trust_mode {
                crate::config::ServerTrustMode::Ca => "ca",
                crate::config::ServerTrustMode::Chain => "chain",
            }
            .to_string(),
        }))
    }
}

/// Agent service implementation for bidirectional streaming
pub struct AgentServiceImpl {
    auth_manager: Arc<AuthManager>,
    agent_registry: Arc<AgentRegistry>,
    fleet_notifier: Option<FleetNotifier>,
    runner_state: Option<Arc<RunnerStateStore>>,
    pending_job_store: Option<Arc<PendingJobStore>>,
}

impl AgentServiceImpl {
    pub fn new(
        auth_manager: Arc<AuthManager>,
        agent_registry: Arc<AgentRegistry>,
        fleet_notifier: Option<FleetNotifier>,
        runner_state: Option<Arc<RunnerStateStore>>,
        pending_job_store: Option<Arc<PendingJobStore>>,
    ) -> Self {
        Self {
            auth_manager,
            agent_registry,
            fleet_notifier,
            runner_state,
            pending_job_store,
        }
    }

    /// Extract agent ID from mTLS client certificate CN.
    ///
    /// When optional mTLS is enabled, tonic populates TlsConnectInfo with peer certificates.
    /// In webhook mode with manual TLS handling, we inject PeerCertificates instead.
    /// We extract the Common Name (CN) from the client certificate, which contains
    /// the agent_id that was set during registration.
    fn extract_agent_id_from_cert<T>(&self, request: &Request<T>) -> Result<String, Status> {
        use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};

        // Try to get peer cert DER bytes from either tonic's TlsConnectInfo or our custom PeerCertificates
        let cert_der: Vec<u8> =
            if let Some(tls_info) = request.extensions().get::<TlsConnectInfo<TcpConnectInfo>>() {
                // Standard tonic TLS path (gRPC-only mode)
                let certs = tls_info.peer_certs().ok_or_else(|| {
                    Status::unauthenticated(
                        "No client certificate presented - mTLS required for agent service",
                    )
                })?;
                certs
                    .first()
                    .ok_or_else(|| Status::unauthenticated("Empty client certificate chain"))?
                    .to_vec()
            } else if let Some(peer_certs) = request.extensions().get::<PeerCertificates>() {
                // Custom TLS path (webhook mode with manual TLS handling)
                peer_certs
                    .0
                    .first()
                    .ok_or_else(|| {
                        Status::unauthenticated(
                            "No client certificate presented - mTLS required for agent service",
                        )
                    })?
                    .clone()
            } else {
                return Err(Status::unauthenticated(
                    "No TLS connection info - TLS required",
                ));
            };

        // Parse the certificate to extract CN
        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der)
            .map_err(|e| Status::unauthenticated(format!("Invalid client certificate: {e}")))?;

        // Extract CN from subject
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok())
            .ok_or_else(|| Status::unauthenticated("No CN in client certificate"))?;

        Ok(cn.to_string())
    }
}

#[tonic::async_trait]
impl AgentService for AgentServiceImpl {
    type AgentStreamStream = Pin<Box<dyn Stream<Item = Result<CoordinatorMessage, Status>> + Send>>;

    async fn agent_stream(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::AgentStreamStream>, Status> {
        // Extract agent_id from mTLS client certificate - this is the authoritative identity
        let agent_id = self.extract_agent_id_from_cert(&request)?;

        // Verify agent is registered and not revoked
        if !self.auth_manager.is_agent_valid(&agent_id).await {
            warn!(
                agent_id = %agent_id,
                "Agent certificate valid but agent revoked or expired in registry"
            );
            return Err(Status::unauthenticated(
                "Agent not registered or has been revoked",
            ));
        }

        let mut inbound = request.into_inner();

        // Wait for first message to get agent metadata (hostname, labels, etc.)
        // Note: agent_id from the message is ignored - we use the cert CN
        let first_msg = timeout(Duration::from_secs(30), inbound.next())
            .await
            .map_err(|_| Status::deadline_exceeded("Timed out waiting for initial status message"))?
            .ok_or_else(|| Status::invalid_argument("No initial message received"))?
            .map_err(|e| Status::internal(format!("Stream error: {e}")))?;

        // Extract metadata from status message (identity comes from cert, not message)
        let (hostname, agent_type, labels, label_sets, max_vms, active_vms, vm_names) =
            match &first_msg.payload {
                Some(AgentPayload::Status(status)) => {
                    let agent_type = match status.agent_type.to_lowercase().as_str() {
                        "tart" => AgentType::Tart,
                        "proxmox" => AgentType::Proxmox,
                        _ => {
                            return Err(Status::invalid_argument(format!(
                                "Unknown agent type: {}",
                                status.agent_type
                            )));
                        }
                    };

                    // Extract VM names for recovery
                    let vm_names: Vec<String> =
                        status.vms.iter().map(|vm| vm.name.clone()).collect();

                    // Extract label_sets (capability sets) - each LabelSet becomes a Vec<String>
                    let label_sets: Vec<Vec<String>> = status
                        .label_sets
                        .iter()
                        .map(|ls| ls.labels.clone())
                        .collect();

                    (
                        status.hostname.clone(),
                        agent_type,
                        status.labels.clone(),
                        label_sets,
                        status.max_vms as usize,
                        status.active_vms as usize,
                        vm_names,
                    )
                }
                _ => {
                    return Err(Status::invalid_argument(
                        "First message must be a status message",
                    ));
                }
            };

        info!(
            agent_id = %agent_id,
            hostname = %hostname,
            agent_type = %agent_type,
            labels = ?labels,
            label_sets = ?label_sets,
            max_vms = max_vms,
            active_vms = active_vms,
            "Agent stream connected"
        );

        // Create channel for outbound messages
        let (outbound_tx, outbound_rx) = mpsc::channel::<Result<CoordinatorMessage, Status>>(32);

        // Also create a channel for the agent registry to send commands
        let (command_tx, mut command_rx) = mpsc::channel::<CoordinatorMessage>(32);

        // Register the agent with actual values from the status message
        let _agent = self
            .agent_registry
            .register(
                agent_id.clone(),
                agent_type,
                hostname,
                max_vms,
                active_vms,
                labels,
                label_sets,
                command_tx,
            )
            .await;

        // Notify fleet manager to check if runners need to be created
        // If agent has VMs, also send recovery info to match against persisted runners
        if let Some(ref notifier) = self.fleet_notifier {
            if vm_names.is_empty() {
                notifier.notify().await;
            } else {
                info!(
                    agent_id = %agent_id,
                    vm_count = vm_names.len(),
                    "Agent connected with existing VMs, triggering recovery check"
                );
                notifier
                    .notify_with_recovery(agent_id.clone(), vm_names)
                    .await;
            }
        }

        // Clone what we need for the task
        let agent_registry = Arc::clone(&self.agent_registry);
        let runner_state = self.runner_state.clone();
        let fleet_notifier = self.fleet_notifier.clone();
        let pending_job_store = self.pending_job_store.clone();
        let agent_id_clone = agent_id.clone();

        // Spawn task to handle inbound messages and forward commands
        tokio::spawn(async move {
            let agent_id = agent_id_clone;
            let mut ping_interval = tokio::time::interval(Duration::from_secs(30));

            loop {
                tokio::select! {
                    // Handle inbound messages from agent
                    msg = inbound.next() => {
                        match msg {
                            Some(Ok(agent_msg)) => {
                                handle_agent_message(
                                    &agent_registry,
                                    &agent_id,
                                    agent_msg,
                                    runner_state.as_ref(),
                                    fleet_notifier.as_ref(),
                                    pending_job_store.as_ref(),
                                )
                                .await;
                            }
                            Some(Err(e)) => {
                                error!(agent_id = %agent_id, error = %e, "Stream error");
                                break;
                            }
                            None => {
                                info!(agent_id = %agent_id, "Agent disconnected");
                                break;
                            }
                        }
                    }

                    // Handle commands to send to agent
                    cmd = command_rx.recv() => {
                        match cmd {
                            Some(command) => {
                                if outbound_tx.send(Ok(command)).await.is_err() {
                                    error!(agent_id = %agent_id, "Failed to send command to agent");
                                    break;
                                }
                            }
                            None => {
                                // Command channel closed
                                break;
                            }
                        }
                    }

                    // Send periodic pings
                    _ = ping_interval.tick() => {
                        let ping = CoordinatorMessage {
                            payload: Some(CoordinatorPayload::Ping(Ping {})),
                        };
                        if outbound_tx.send(Ok(ping)).await.is_err() {
                            error!(agent_id = %agent_id, "Failed to send ping");
                            break;
                        }
                    }
                }
            }

            // Unregister agent on disconnect
            agent_registry.unregister(&agent_id).await;
        });

        // Return the outbound stream
        let stream = ReceiverStream::new(outbound_rx);
        Ok(Response::new(Box::pin(stream)))
    }
}

/// Handle messages received from an agent
async fn handle_agent_message(
    registry: &AgentRegistry,
    agent_id: &str,
    msg: AgentMessage,
    runner_state: Option<&Arc<RunnerStateStore>>,
    fleet_notifier: Option<&FleetNotifier>,
    pending_job_store: Option<&Arc<PendingJobStore>>,
) {
    match msg.payload {
        Some(AgentPayload::Status(status)) => {
            debug!(
                agent_id = %agent_id,
                active_vms = status.active_vms,
                available_slots = status.available_slots,
                vm_count = status.vms.len(),
                "Agent status update"
            );
            registry
                .update_status(agent_id, status.active_vms as usize)
                .await;

            // Reconcile persisted runner state against the agent's current VM list
            // This allows the recovery watcher to detect when VMs have completed
            if let Some(rs) = runner_state {
                let vm_names: Vec<String> = status.vms.iter().map(|vm| vm.name.clone()).collect();
                let missing = rs.reconcile_agent_vms(agent_id, &vm_names).await;
                if !missing.is_empty() {
                    let mut deferred = 0usize;
                    let has_pending_store = pending_job_store.is_some();

                    for (runner_name, runner_info) in &missing {
                        if has_pending_store {
                            if let Some(job_id) = runner_info.job_id {
                                deferred += 1;
                                debug!(
                                    agent_id = %agent_id,
                                    runner_name = %runner_name,
                                    job_id = %job_id,
                                    "Runner VM missing - deferring cleanup until runner event"
                                );
                                continue;
                            }
                        }

                        rs.remove_runner(runner_name).await;
                        debug!(
                            agent_id = %agent_id,
                            runner_name = %runner_name,
                            "Removed runner from state during reconciliation"
                        );
                    }

                    info!(
                        agent_id = %agent_id,
                        missing_count = missing.len(),
                        deferred_count = deferred,
                        "Reconciled runner state - {} runner(s) missing ({} deferred)",
                        missing.len(),
                        deferred
                    );
                }
            }
        }
        Some(AgentPayload::Ack(ack)) => {
            let command_id = ack.command_id.clone();
            debug!(
                agent_id = %agent_id,
                command_id = %command_id,
                accepted = ack.accepted,
                "Command ack received"
            );
            registry
                .handle_response(
                    agent_id,
                    &command_id,
                    AgentMessage {
                        payload: Some(AgentPayload::Ack(ack)),
                    },
                )
                .await;
        }
        Some(AgentPayload::RunnerEvent(event)) => {
            if let Some(notifier) = fleet_notifier {
                notifier
                    .notify_runner_event(agent_id.to_string(), event)
                    .await;
            } else {
                warn!(
                    agent_id = %agent_id,
                    "Runner event received but no fleet notifier configured"
                );
            }
        }
        Some(AgentPayload::Result(result)) => {
            let command_id = result.command_id.clone();
            let success = result.success;
            debug!(
                agent_id = %agent_id,
                command_id = %command_id,
                success = success,
                "Command result received"
            );
            registry
                .handle_response(
                    agent_id,
                    &command_id,
                    AgentMessage {
                        payload: Some(AgentPayload::Result(result)),
                    },
                )
                .await;
        }
        Some(AgentPayload::Pong(_)) => {
            debug!(agent_id = %agent_id, "Pong received");
            // Just update last_seen, don't change VM counts
            registry.touch(agent_id).await;
        }
        None => {
            warn!(agent_id = %agent_id, "Empty message received");
        }
    }
}

/// Configuration for the gRPC server
pub struct ServerConfig {
    /// Address to listen on
    pub listen_addr: SocketAddr,

    /// TLS configuration paths
    pub tls: TlsConfig,

    /// Enable PROXY protocol support (v1 and v2)
    pub proxy_protocol: bool,
}

/// Start the gRPC server with optional mTLS.
///
/// Both registration and agent services run on the same port:
/// - Registration service: No client cert required (uses registration token)
/// - Agent service: Client cert required (identity extracted from cert CN)
///
/// Uses `client_auth_optional(true)` so client certs are verified if presented
/// but not required at the TLS layer. The agent service enforces the requirement.
///
/// If `webhook_config` is provided, also serves HTTP webhook endpoint on the same port.
/// If `admin_state` is provided, serves admin UI at /admin.
/// Traffic is routed based on content-type: `application/grpc` goes to gRPC services,
/// everything else goes to the HTTP handler.
pub async fn run_server(
    config: ServerConfig,
    server_trust: ServerTrust,
    auth_manager: Arc<AuthManager>,
    agent_registry: Arc<AgentRegistry>,
    fleet_notifier: Option<FleetNotifier>,
    runner_state: Option<Arc<RunnerStateStore>>,
    pending_job_store: Option<Arc<PendingJobStore>>,
    webhook_config: Option<(WebhookConfig, WebhookNotifier)>,
    admin_state: Option<Arc<AdminState>>,
) -> Result<()> {
    // If no webhook config, use the simple gRPC-only path
    // Note: Admin UI requires webhook mode (combined HTTP+gRPC server)
    if webhook_config.is_none() {
        return run_grpc_only_server(
            config,
            server_trust,
            auth_manager,
            agent_registry,
            fleet_notifier,
            runner_state,
        )
        .await;
    }

    let (wh_config, wh_notifier) = webhook_config.unwrap();
    let pending_job_store = pending_job_store.expect("pending_job_store required in webhook mode");

    // Load TLS credentials
    let server_cert = std::fs::read_to_string(&config.tls.server_cert)
        .with_context(|| format!("Failed to read server cert: {:?}", config.tls.server_cert))?;
    let server_key = std::fs::read_to_string(&config.tls.server_key)
        .with_context(|| format!("Failed to read server key: {:?}", config.tls.server_key))?;
    let ca_cert = std::fs::read_to_string(&config.tls.ca_cert)
        .with_context(|| format!("Failed to read CA cert: {:?}", config.tls.ca_cert))?;

    // Build TLS config using rustls
    let certs = rustls_pemfile::certs(&mut server_cert.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to parse server certificate")?;
    let key = rustls_pemfile::private_key(&mut server_key.as_bytes())
        .context("Failed to parse server key")?
        .context("No private key found")?;
    let ca_certs = rustls_pemfile::certs(&mut ca_cert.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to parse CA certificate")?;

    let mut root_store = tokio_rustls::rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store
            .add(cert)
            .context("Failed to add CA cert to root store")?;
    }

    // Configure TLS with optional client auth
    let client_verifier =
        tokio_rustls::rustls::server::WebPkiClientVerifier::builder(root_store.into())
            .allow_unauthenticated()
            .build()
            .context("Failed to create client verifier")?;

    let mut tls_config = tokio_rustls::rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .context("Failed to configure TLS")?;

    // Set ALPN protocols - h2 required for gRPC, http/1.1 for HTTP
    tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

    // Create gRPC services
    let registration_service =
        RegistrationServiceImpl::new(Arc::clone(&auth_manager), server_trust.clone());
    let agent_service = AgentServiceImpl::new(
        auth_manager,
        agent_registry,
        fleet_notifier,
        runner_state,
        Some(pending_job_store.clone()),
    );

    // Build gRPC service using tonic Routes
    let grpc_service = Routes::new(RegistrationServiceServer::new(registration_service))
        .add_service(AgentServiceServer::new(agent_service));

    // Build HTTP router (webhook + optional admin UI)
    let webhook_state = Arc::new(WebhookState {
        config: wh_config.clone(),
        notifier: wh_notifier,
        pending_job_store,
    });
    let admin_enabled = admin_state.is_some();
    let http_router = webhook::http_router(webhook_state, admin_state);

    // Log startup info
    if admin_enabled {
        info!(
            addr = %config.listen_addr,
            webhook_path = %wh_config.path,
            admin_ui = "/admin",
            "Starting server with gRPC + HTTP webhook + Admin UI (optional mTLS)"
        );
    } else {
        info!(
            addr = %config.listen_addr,
            webhook_path = %wh_config.path,
            "Starting server with gRPC + HTTP webhook (optional mTLS)"
        );
    }

    // Bind TCP listener
    let listener = TcpListener::bind(config.listen_addr)
        .await
        .context("Failed to bind to address")?;

    let proxy_protocol = config.proxy_protocol;

    // Accept connections and route them
    loop {
        let (tcp_stream, remote_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("Failed to accept connection: {}", e);
                continue;
            }
        };

        let tls_acceptor = tls_acceptor.clone();
        let grpc_service = grpc_service.clone();
        let http_router = http_router.clone();

        // Spawn per-connection task immediately to avoid blocking accept loop
        tokio::spawn(async move {
            // Parse PROXY protocol header if enabled (before TLS handshake)
            // Done inside spawned task to prevent slow clients from blocking new connections
            let (tcp_stream, real_addr) = if proxy_protocol {
                let mut stream = tcp_stream;
                // 5 second timeout for PROXY header to prevent slow-connection DoS
                match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    parse_proxy_protocol(&mut stream, remote_addr),
                )
                .await
                {
                    Ok(Ok(addr)) => (stream, addr),
                    Ok(Err(e)) => {
                        debug!("PROXY protocol parse failed from {}: {}", remote_addr, e);
                        return;
                    }
                    Err(_) => {
                        debug!(
                            "PROXY protocol timeout from {} (no header within 5s)",
                            remote_addr
                        );
                        return;
                    }
                }
            } else {
                (tcp_stream, remote_addr)
            };

            let _real_addr = real_addr; // Available for logging if needed

            // TLS handshake
            let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                Ok(stream) => stream,
                Err(e) => {
                    debug!("TLS handshake failed from {}: {}", remote_addr, e);
                    return;
                }
            };

            // Extract peer certificates from TLS connection (for mTLS auth)
            // Must be done before wrapping in TokioIo since we need access to the raw stream
            let peer_certs: Option<PeerCertificates> = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .map(|certs| PeerCertificates(certs.iter().map(|c| c.to_vec()).collect()));

            // Wrap in hyper IO adapter
            let io = TokioIo::new(tls_stream);

            // Create a combined service that routes based on content-type
            // Both branches return the same UnsyncBoxBody type
            type UnsyncBody =
                http_body_util::combinators::UnsyncBoxBody<hyper::body::Bytes, std::io::Error>;

            let service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                let mut grpc = grpc_service.clone();
                let http = http_router.clone();
                let peer_certs = peer_certs.clone();

                async move {
                    // Check if this is a gRPC request
                    let is_grpc = req
                        .headers()
                        .get("content-type")
                        .map(|ct| ct.as_bytes().starts_with(b"application/grpc"))
                        .unwrap_or(false);

                    if is_grpc {
                        // Inject peer certificates into request extensions for mTLS auth
                        let mut req = req;
                        if let Some(certs) = peer_certs {
                            req.extensions_mut().insert(certs);
                        }

                        // Route to gRPC using tonic's Routes service
                        match grpc.call(req).await {
                            Ok(resp) => {
                                let (parts, body) = resp.into_parts();
                                let body: UnsyncBody = body
                                    .map_err(|e| std::io::Error::other(e.to_string()))
                                    .boxed_unsync();
                                Ok::<_, std::io::Error>(hyper::Response::from_parts(parts, body))
                            }
                            Err(e) => Err(std::io::Error::other(e.to_string())),
                        }
                    } else {
                        // Route to HTTP (webhook) using axum
                        let (parts, body) = req.into_parts();
                        let body = axum::body::Body::new(body);
                        let req = hyper::Request::from_parts(parts, body);

                        match http.clone().call(req).await {
                            Ok(resp) => {
                                let (parts, body) = resp.into_parts();
                                let body: UnsyncBody = body
                                    .map_err(|e| std::io::Error::other(e.to_string()))
                                    .boxed_unsync();
                                Ok(hyper::Response::from_parts(parts, body))
                            }
                            Err(e) => Err(std::io::Error::other(e)),
                        }
                    }
                }
            });

            // Serve the connection with HTTP/2 support (required for gRPC)
            let builder = HttpBuilder::new(TokioExecutor::new());
            if let Err(e) = builder.serve_connection(io, service).await {
                debug!("Connection error from {}: {}", remote_addr, e);
            }
        });
    }
}

/// Run gRPC-only server (no webhook endpoint).
///
/// This is used when webhook mode is not enabled.
async fn run_grpc_only_server(
    config: ServerConfig,
    server_trust: ServerTrust,
    auth_manager: Arc<AuthManager>,
    agent_registry: Arc<AgentRegistry>,
    fleet_notifier: Option<FleetNotifier>,
    runner_state: Option<Arc<RunnerStateStore>>,
) -> Result<()> {
    // Load TLS credentials
    let server_cert = std::fs::read_to_string(&config.tls.server_cert)
        .with_context(|| format!("Failed to read server cert: {:?}", config.tls.server_cert))?;
    let server_key = std::fs::read_to_string(&config.tls.server_key)
        .with_context(|| format!("Failed to read server key: {:?}", config.tls.server_key))?;
    let ca_cert = std::fs::read_to_string(&config.tls.ca_cert)
        .with_context(|| format!("Failed to read CA cert: {:?}", config.tls.ca_cert))?;

    // Create services
    let registration_service =
        RegistrationServiceImpl::new(Arc::clone(&auth_manager), server_trust);
    let agent_service = AgentServiceImpl::new(
        auth_manager,
        agent_registry,
        fleet_notifier,
        runner_state,
        None, // No pending_job_store in gRPC-only mode
    );

    // If PROXY protocol is enabled, we need manual connection handling
    if config.proxy_protocol {
        info!(
            addr = %config.listen_addr,
            proxy_protocol = true,
            "Starting gRPC server with PROXY protocol support (optional mTLS)"
        );

        // Build TLS config using rustls (same as webhook mode)
        let certs = rustls_pemfile::certs(&mut server_cert.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("Failed to parse server certificate")?;
        let key = rustls_pemfile::private_key(&mut server_key.as_bytes())
            .context("Failed to parse server key")?
            .context("No private key found")?;
        let ca_certs = rustls_pemfile::certs(&mut ca_cert.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("Failed to parse CA certificate")?;

        let mut root_store = tokio_rustls::rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store
                .add(cert)
                .context("Failed to add CA cert to root store")?;
        }

        let client_verifier =
            tokio_rustls::rustls::server::WebPkiClientVerifier::builder(root_store.into())
                .allow_unauthenticated()
                .build()
                .context("Failed to create client verifier")?;

        let mut tls_config = tokio_rustls::rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certs, key)
            .context("Failed to configure TLS")?;

        tls_config.alpn_protocols = vec![b"h2".to_vec()];
        let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

        // Build gRPC service
        let grpc_service = Routes::new(RegistrationServiceServer::new(registration_service))
            .add_service(AgentServiceServer::new(agent_service));

        // Bind TCP listener
        let listener = TcpListener::bind(config.listen_addr)
            .await
            .context("Failed to bind to address")?;

        // Accept connections with PROXY protocol support
        loop {
            let (tcp_stream, remote_addr) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to accept connection: {}", e);
                    continue;
                }
            };

            let tls_acceptor = tls_acceptor.clone();
            let grpc_service = grpc_service.clone();

            // Spawn per-connection task immediately to avoid blocking accept loop
            tokio::spawn(async move {
                // Parse PROXY protocol header before TLS (with timeout to prevent DoS)
                let (tcp_stream, real_addr) = {
                    let mut stream = tcp_stream;
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        parse_proxy_protocol(&mut stream, remote_addr),
                    )
                    .await
                    {
                        Ok(Ok(addr)) => (stream, addr),
                        Ok(Err(e)) => {
                            debug!("PROXY protocol parse failed from {}: {}", remote_addr, e);
                            return;
                        }
                        Err(_) => {
                            debug!(
                                "PROXY protocol timeout from {} (no header within 5s)",
                                remote_addr
                            );
                            return;
                        }
                    }
                };

                let _real_addr = real_addr;

                // TLS handshake
                let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        debug!("TLS handshake failed from {}: {}", remote_addr, e);
                        return;
                    }
                };

                // Extract peer certificates for mTLS
                let peer_certs: Option<PeerCertificates> = tls_stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .map(|certs| PeerCertificates(certs.iter().map(|c| c.to_vec()).collect()));

                let io = TokioIo::new(tls_stream);

                type UnsyncBody =
                    http_body_util::combinators::UnsyncBoxBody<hyper::body::Bytes, std::io::Error>;

                let service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                    let mut grpc = grpc_service.clone();
                    let peer_certs = peer_certs.clone();

                    async move {
                        let mut req = req;
                        if let Some(certs) = peer_certs {
                            req.extensions_mut().insert(certs);
                        }

                        match grpc.call(req).await {
                            Ok(resp) => {
                                let (parts, body) = resp.into_parts();
                                let body: UnsyncBody = body
                                    .map_err(|e| std::io::Error::other(e.to_string()))
                                    .boxed_unsync();
                                Ok::<_, std::io::Error>(hyper::Response::from_parts(parts, body))
                            }
                            Err(e) => Err(std::io::Error::other(e.to_string())),
                        }
                    }
                });

                let builder = HttpBuilder::new(TokioExecutor::new());
                if let Err(e) = builder.serve_connection(io, service).await {
                    debug!("Connection error from {}: {}", remote_addr, e);
                }
            });
        }
    } else {
        // Standard tonic path (no PROXY protocol)
        let identity = Identity::from_pem(&server_cert, &server_key);

        let tls_config = ServerTlsConfig::new()
            .identity(identity)
            .client_ca_root(Certificate::from_pem(&ca_cert))
            .client_auth_optional(true);

        info!(
            addr = %config.listen_addr,
            "Starting gRPC server (optional mTLS - agent service requires client cert)"
        );

        Server::builder()
            .tls_config(tls_config)
            .context("Failed to configure TLS")?
            .add_service(RegistrationServiceServer::new(registration_service))
            .add_service(AgentServiceServer::new(agent_service))
            .serve(config.listen_addr)
            .await
            .context("gRPC server error")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Integration tests would go here
    // These would require setting up a test CA and certificates
}
