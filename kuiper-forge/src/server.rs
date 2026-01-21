//! gRPC server implementation with optional mTLS support.
//!
//! Provides:
//! - RegistrationService: Token-based agent registration (no client cert required)
//! - AgentService: Bidirectional streaming for agent communication (client cert required)
//!
//! Security model:
//! - Single port with optional client certificate verification
//! - Registration service works without client cert (uses registration token)
//! - Agent service requires client cert - identity extracted from certificate CN


use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentService, AgentServiceServer,
    CoordinatorMessage, CoordinatorPayload, Ping, RegisterRequest, RegisterResponse,
    RegistrationService, RegistrationServiceServer,
};
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

use crate::agent_registry::{AgentRegistry, AgentType};
use crate::auth::AuthManager;
use crate::config::TlsConfig;
use crate::fleet::FleetNotifier;
use crate::runner_state::RunnerStateStore;

/// Registration service implementation
pub struct RegistrationServiceImpl {
    auth_manager: Arc<AuthManager>,
}

impl RegistrationServiceImpl {
    pub fn new(auth_manager: Arc<AuthManager>) -> Self {
        Self { auth_manager }
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
                Status::unauthenticated(format!("Registration failed: {}", e))
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
            ca_cert_pem: self.auth_manager.ca_cert_pem().to_string(),
        }))
    }
}

/// Agent service implementation for bidirectional streaming
pub struct AgentServiceImpl {
    auth_manager: Arc<AuthManager>,
    agent_registry: Arc<AgentRegistry>,
    fleet_notifier: Option<FleetNotifier>,
    runner_state: Option<Arc<RunnerStateStore>>,
}

impl AgentServiceImpl {
    pub fn new(
        auth_manager: Arc<AuthManager>,
        agent_registry: Arc<AgentRegistry>,
        fleet_notifier: Option<FleetNotifier>,
        runner_state: Option<Arc<RunnerStateStore>>,
    ) -> Self {
        Self {
            auth_manager,
            agent_registry,
            fleet_notifier,
            runner_state,
        }
    }

    /// Extract agent ID from mTLS client certificate CN.
    ///
    /// When optional mTLS is enabled, tonic populates TlsConnectInfo with peer certificates.
    /// We extract the Common Name (CN) from the client certificate, which contains
    /// the agent_id that was set during registration.
    fn extract_agent_id_from_cert<T>(&self, request: &Request<T>) -> Result<String, Status> {
        use tonic::transport::server::TlsConnectInfo;

        // Get TLS connection info from request extensions
        let tls_info = request
            .extensions()
            .get::<TlsConnectInfo<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>>()
            .ok_or_else(|| {
                Status::unauthenticated("No TLS connection info - TLS required")
            })?;

        // Get peer certificates (None if client didn't present a cert)
        let certs = tls_info
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("No client certificate presented - mTLS required for agent service"))?;

        let cert_der = certs
            .first()
            .ok_or_else(|| Status::unauthenticated("Empty client certificate chain"))?;

        // Parse the certificate to extract CN
        let (_, cert) = x509_parser::parse_x509_certificate(cert_der.as_ref())
            .map_err(|e| Status::unauthenticated(format!("Invalid client certificate: {}", e)))?;

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
    type AgentStreamStream =
        Pin<Box<dyn Stream<Item = Result<CoordinatorMessage, Status>> + Send>>;

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
        let first_msg = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("No initial message received"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        // Extract metadata from status message (identity comes from cert, not message)
        let (hostname, agent_type, labels, max_vms, vm_names) = match &first_msg.payload {
            Some(AgentPayload::Status(status)) => {
                let agent_type = match status.agent_type.to_lowercase().as_str() {
                    "tart" => AgentType::Tart,
                    "proxmox" => AgentType::Proxmox,
                    _ => {
                        return Err(Status::invalid_argument(
                            format!("Unknown agent type: {}", status.agent_type),
                        ));
                    }
                };

                // Extract VM names for recovery
                let vm_names: Vec<String> = status.vms.iter().map(|vm| vm.name.clone()).collect();

                (
                    status.hostname.clone(),
                    agent_type,
                    status.labels.clone(),
                    status.max_vms as usize,
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
            max_vms = max_vms,
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
                labels,
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
                notifier.notify_with_recovery(agent_id.clone(), vm_names).await;
            }
        }

        // Clone what we need for the task
        let agent_registry = Arc::clone(&self.agent_registry);
        let runner_state = self.runner_state.clone();
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
                                handle_agent_message(&agent_registry, &agent_id, agent_msg, runner_state.as_ref()).await;
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
            registry.update_status(agent_id, status.active_vms as usize).await;

            // Reconcile persisted runner state against the agent's current VM list
            // This allows the recovery watcher to detect when VMs have completed
            if let Some(rs) = runner_state {
                let vm_names: Vec<String> = status.vms.iter().map(|vm| vm.name.clone()).collect();
                let removed = rs.reconcile_agent_vms(agent_id, &vm_names).await;
                if !removed.is_empty() {
                    info!(
                        agent_id = %agent_id,
                        removed_count = removed.len(),
                        "Reconciled runner state - {} runner(s) marked as completed",
                        removed.len()
                    );
                }
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
}

/// Start the gRPC server with optional mTLS.
///
/// Both registration and agent services run on the same port:
/// - Registration service: No client cert required (uses registration token)
/// - Agent service: Client cert required (identity extracted from cert CN)
///
/// Uses `client_auth_optional(true)` so client certs are verified if presented
/// but not required at the TLS layer. The agent service enforces the requirement.
pub async fn run_server(
    config: ServerConfig,
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

    let identity = Identity::from_pem(&server_cert, &server_key);

    // Configure TLS with optional client auth
    // - client_ca_root: CA to verify client certs (if presented)
    // - client_auth_optional: don't reject connections without client certs
    let tls_config = ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(Certificate::from_pem(&ca_cert))
        .client_auth_optional(true);

    // Create services
    let registration_service = RegistrationServiceImpl::new(Arc::clone(&auth_manager));
    let agent_service = AgentServiceImpl::new(auth_manager, agent_registry, fleet_notifier, runner_state);

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

    Ok(())
}

#[cfg(test)]
mod tests {
    // Integration tests would go here
    // These would require setting up a test CA and certificates
}
