//! gRPC server implementation with mTLS support.
//!
//! Provides:
//! - RegistrationService: Token-based agent registration (server TLS only)
//! - AgentService: Bidirectional streaming for agent communication (mTLS)


use agent_proto::{
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
}

impl AgentServiceImpl {
    pub fn new(auth_manager: Arc<AuthManager>, agent_registry: Arc<AgentRegistry>) -> Self {
        Self {
            auth_manager,
            agent_registry,
        }
    }

    /// Extract agent ID from mTLS client certificate.
    /// Used in production when proper mTLS is configured.
    #[allow(dead_code)] // Used with full mTLS implementation
    fn extract_agent_id(&self, request: &Request<Streaming<AgentMessage>>) -> Result<String, Status> {
        // In a full mTLS setup, we'd extract the CN from the client certificate
        // For now, we'll use a header or metadata approach that the TLS layer can set

        // Try to get agent_id from request metadata (set by TLS layer or interceptor)
        if let Some(agent_id) = request.metadata().get("x-agent-id") {
            return agent_id
                .to_str()
                .map(String::from)
                .map_err(|_| Status::unauthenticated("Invalid agent ID header"));
        }

        // In production with proper mTLS, this would come from the TLS peer certificate
        // For now, agents must send their ID in the first message
        Err(Status::unauthenticated(
            "Agent identity not found in request",
        ))
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
        // Note: In a full implementation, agent_id would come from mTLS cert
        // For now, we'll get it from the first status message

        let mut inbound = request.into_inner();

        // Wait for first message to get agent identity
        let first_msg = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("No initial message received"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        // The first message should be a status message containing agent identity
        // When mTLS is enabled, agent_id would come from certificate CN
        // When mTLS is disabled (MVP), agent must self-identify in the status message

        let (agent_id, hostname, agent_type, labels, max_vms) = match &first_msg.payload {
            Some(AgentPayload::Status(status)) => {
                // Extract identity from status message
                if status.agent_id.is_empty() {
                    return Err(Status::invalid_argument(
                        "First status message must contain agent_id",
                    ));
                }

                let agent_type = match status.agent_type.to_lowercase().as_str() {
                    "tart" => AgentType::Tart,
                    "proxmox" => AgentType::Proxmox,
                    _ => {
                        return Err(Status::invalid_argument(
                            format!("Unknown agent type: {}", status.agent_type),
                        ));
                    }
                };

                (
                    status.agent_id.clone(),
                    status.hostname.clone(),
                    agent_type,
                    status.labels.clone(),
                    status.max_vms as usize,
                )
            }
            _ => {
                return Err(Status::invalid_argument(
                    "First message must be a status message",
                ));
            }
        };

        // Verify agent is registered and valid
        if !self.auth_manager.is_agent_valid(&agent_id).await {
            warn!(
                agent_id = %agent_id,
                "Agent not registered or certificate revoked/expired"
            );
            return Err(Status::unauthenticated(
                "Agent not registered or certificate revoked/expired",
            ));
        }

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

        // Clone what we need for the task
        let agent_registry = Arc::clone(&self.agent_registry);
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
                                handle_agent_message(&agent_registry, &agent_id, agent_msg).await;
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
async fn handle_agent_message(registry: &AgentRegistry, agent_id: &str, msg: AgentMessage) {
    match msg.payload {
        Some(AgentPayload::Status(status)) => {
            debug!(
                agent_id = %agent_id,
                active_vms = status.active_vms,
                available_slots = status.available_slots,
                "Agent status update"
            );
            registry.update_status(agent_id, status.active_vms as usize).await;
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
            // Just update last_seen
            registry.update_status(agent_id, 0).await;
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

    /// Whether to require mTLS for agent service (true in production)
    pub require_mtls: bool,
}

/// Start the gRPC server
pub async fn run_server(
    config: ServerConfig,
    auth_manager: Arc<AuthManager>,
    agent_registry: Arc<AgentRegistry>,
) -> Result<()> {
    // Load TLS credentials
    let server_cert = std::fs::read_to_string(&config.tls.server_cert)
        .with_context(|| format!("Failed to read server cert: {:?}", config.tls.server_cert))?;
    let server_key = std::fs::read_to_string(&config.tls.server_key)
        .with_context(|| format!("Failed to read server key: {:?}", config.tls.server_key))?;
    let ca_cert = std::fs::read_to_string(&config.tls.ca_cert)
        .with_context(|| format!("Failed to read CA cert: {:?}", config.tls.ca_cert))?;

    let identity = Identity::from_pem(&server_cert, &server_key);

    // Configure TLS
    let tls_config = if config.require_mtls {
        // mTLS: require client certificate
        ServerTlsConfig::new()
            .identity(identity)
            .client_ca_root(Certificate::from_pem(&ca_cert))
    } else {
        // Server TLS only (for registration service)
        ServerTlsConfig::new().identity(identity)
    };

    // Create services
    let registration_service = RegistrationServiceImpl::new(Arc::clone(&auth_manager));
    let agent_service = AgentServiceImpl::new(auth_manager, agent_registry);

    info!(addr = %config.listen_addr, "Starting gRPC server");

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

/// Run server with separate TLS for registration (no mTLS) and agent service (mTLS)
/// This is the production setup where registration doesn't require client certs
/// but the agent stream does.
#[allow(dead_code)] // Production deployment option
pub async fn run_dual_server(
    registration_addr: SocketAddr,
    agent_addr: SocketAddr,
    tls_config: TlsConfig,
    auth_manager: Arc<AuthManager>,
    agent_registry: Arc<AgentRegistry>,
) -> Result<()> {
    // Load TLS credentials
    let server_cert = std::fs::read_to_string(&tls_config.server_cert)?;
    let server_key = std::fs::read_to_string(&tls_config.server_key)?;
    let ca_cert = std::fs::read_to_string(&tls_config.ca_cert)?;

    let identity = Identity::from_pem(&server_cert, &server_key);

    // Registration server (server TLS only, no client cert required)
    let reg_tls = ServerTlsConfig::new().identity(identity.clone());
    let registration_service = RegistrationServiceImpl::new(Arc::clone(&auth_manager));

    let reg_server = Server::builder()
        .tls_config(reg_tls)?
        .add_service(RegistrationServiceServer::new(registration_service))
        .serve(registration_addr);

    // Agent server (mTLS, requires client cert)
    let agent_tls = ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(Certificate::from_pem(&ca_cert));

    let agent_service = AgentServiceImpl::new(auth_manager, agent_registry);

    let agent_server = Server::builder()
        .tls_config(agent_tls)?
        .add_service(AgentServiceServer::new(agent_service))
        .serve(agent_addr);

    info!(
        registration = %registration_addr,
        agent = %agent_addr,
        "Starting dual gRPC servers"
    );

    // Run both servers
    tokio::try_join!(reg_server, agent_server)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    // Integration tests would go here
    // These would require setting up a test CA and certificates
}
