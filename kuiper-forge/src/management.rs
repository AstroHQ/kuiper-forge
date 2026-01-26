//! Unix socket management interface for CLI commands.
//!
//! Provides a localhost-only gRPC interface over Unix socket for CLI commands
//! to communicate with the running coordinator. This prevents multi-process
//! database access issues and centralizes all state management.

use crate::auth::AuthManager;
use anyhow::{Context, Result};
use kuiper_agent_proto::{
    AgentInfo, CreateTokenRequest, CreateTokenResponse, DeleteTokenRequest, DeleteTokenResponse,
    ListAgentsRequest, ListAgentsResponse, ListTokensRequest, ListTokensResponse,
    ManagementService, ManagementServiceServer, RevokeAgentRequest, RevokeAgentResponse, TokenInfo,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use tracing::info;

/// gRPC service implementation for management operations.
pub struct ManagementServiceImpl {
    auth_manager: Arc<AuthManager>,
}

impl ManagementServiceImpl {
    pub fn new(auth_manager: Arc<AuthManager>) -> Self {
        Self { auth_manager }
    }
}

#[tonic::async_trait]
impl ManagementService for ManagementServiceImpl {
    async fn create_token(
        &self,
        request: Request<CreateTokenRequest>,
    ) -> Result<Response<CreateTokenResponse>, Status> {
        let req = request.into_inner();
        let ttl = chrono::Duration::seconds(req.expires_secs as i64);

        let token = self
            .auth_manager
            .create_registration_token(ttl, &req.created_by)
            .await
            .map_err(|e| Status::internal(format!("Failed to create token: {e}")))?;

        Ok(Response::new(CreateTokenResponse {
            token: token.token,
            expires_at: token.expires_at.to_rfc3339(),
            created_at: token.created_at.to_rfc3339(),
        }))
    }

    async fn list_tokens(
        &self,
        _request: Request<ListTokensRequest>,
    ) -> Result<Response<ListTokensResponse>, Status> {
        let tokens = self.auth_manager.list_tokens().await;

        let token_infos: Vec<TokenInfo> = tokens
            .into_iter()
            .map(|t| TokenInfo {
                token: t.token,
                expires_at: t.expires_at.to_rfc3339(),
                created_by: t.created_by,
                created_at: t.created_at.to_rfc3339(),
            })
            .collect();

        Ok(Response::new(ListTokensResponse {
            tokens: token_infos,
        }))
    }

    async fn delete_token(
        &self,
        request: Request<DeleteTokenRequest>,
    ) -> Result<Response<DeleteTokenResponse>, Status> {
        let req = request.into_inner();

        let deleted = self
            .auth_manager
            .delete_token(&req.token)
            .await
            .map_err(|e| Status::internal(format!("Failed to delete token: {e}")))?;

        Ok(Response::new(DeleteTokenResponse { deleted }))
    }

    async fn list_agents(
        &self,
        _request: Request<ListAgentsRequest>,
    ) -> Result<Response<ListAgentsResponse>, Status> {
        let agents = self.auth_manager.list_agents().await;

        let agent_infos: Vec<AgentInfo> = agents
            .into_iter()
            .map(|a| AgentInfo {
                agent_id: a.agent_id,
                hostname: a.hostname,
                agent_type: a.agent_type,
                labels: a.labels,
                max_vms: a.max_vms,
                created_at: a.created_at.to_rfc3339(),
                expires_at: a.expires_at.to_rfc3339(),
                revoked: a.revoked,
            })
            .collect();

        Ok(Response::new(ListAgentsResponse {
            agents: agent_infos,
        }))
    }

    async fn revoke_agent(
        &self,
        request: Request<RevokeAgentRequest>,
    ) -> Result<Response<RevokeAgentResponse>, Status> {
        let req = request.into_inner();

        let revoked = self
            .auth_manager
            .revoke_agent(&req.agent_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to revoke agent: {e}")))?;

        Ok(Response::new(RevokeAgentResponse { revoked }))
    }
}

/// Run the management gRPC server on a Unix socket.
///
/// This should be spawned as a background task when the coordinator starts.
pub async fn run_management_server(
    auth_manager: Arc<AuthManager>,
    socket_path: PathBuf,
) -> Result<()> {
    // Remove stale socket file if it exists (from previous crash)
    let _ = std::fs::remove_file(&socket_path);

    // Create parent directory if needed
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let uds = UnixListener::bind(&socket_path)
        .with_context(|| format!("Failed to bind Unix socket: {}", socket_path.display()))?;

    let uds_stream = UnixListenerStream::new(uds);

    let svc = ManagementServiceImpl::new(auth_manager);

    info!(path = %socket_path.display(), "Management socket listening");

    Server::builder()
        .add_service(ManagementServiceServer::new(svc))
        .serve_with_incoming(uds_stream)
        .await
        .context("Management server error")?;

    Ok(())
}

// ============================================================================
// Client for CLI to connect to management socket
// ============================================================================

use hyper_util::rt::TokioIo;
use kuiper_agent_proto::ManagementServiceClient;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

/// Client for connecting to the management Unix socket.
pub struct ManagementClient {
    client: ManagementServiceClient<Channel>,
}

impl ManagementClient {
    /// Connect to the management socket.
    ///
    /// Returns an error if the coordinator is not running (socket doesn't exist).
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        // Check if socket exists first for better error messages
        if !socket_path.exists() {
            anyhow::bail!(
                "Coordinator is not running.\n\
                 Start it with: coordinator serve"
            );
        }

        let socket_path = socket_path.to_owned();

        // tonic requires a URI even for Unix sockets, but it's not used
        // We wrap UnixStream in TokioIo for hyper compatibility
        let channel = Endpoint::try_from("http://[::]:50051")
            .context("Failed to create endpoint")?
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = socket_path.clone();
                async move {
                    let stream = UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await
            .context("Failed to connect to coordinator socket")?;

        Ok(Self {
            client: ManagementServiceClient::new(channel),
        })
    }

    /// Create a new registration token.
    pub async fn create_token(
        &mut self,
        expires_secs: u64,
        created_by: &str,
    ) -> Result<CreateTokenResponse> {
        let resp = self
            .client
            .create_token(CreateTokenRequest {
                expires_secs,
                created_by: created_by.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("gRPC error: {}", e))?;

        Ok(resp.into_inner())
    }

    /// List all registration tokens.
    pub async fn list_tokens(&mut self) -> Result<ListTokensResponse> {
        let resp = self
            .client
            .list_tokens(ListTokensRequest {})
            .await
            .map_err(|e| anyhow::anyhow!("gRPC error: {}", e))?;

        Ok(resp.into_inner())
    }

    /// Delete a registration token.
    pub async fn delete_token(&mut self, token: &str) -> Result<DeleteTokenResponse> {
        let resp = self
            .client
            .delete_token(DeleteTokenRequest {
                token: token.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("gRPC error: {}", e))?;

        Ok(resp.into_inner())
    }

    /// List all registered agents.
    pub async fn list_agents(&mut self) -> Result<ListAgentsResponse> {
        let resp = self
            .client
            .list_agents(ListAgentsRequest {})
            .await
            .map_err(|e| anyhow::anyhow!("gRPC error: {}", e))?;

        Ok(resp.into_inner())
    }

    /// Revoke an agent.
    pub async fn revoke_agent(&mut self, agent_id: &str) -> Result<RevokeAgentResponse> {
        let resp = self
            .client
            .revoke_agent(RevokeAgentRequest {
                agent_id: agent_id.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("gRPC error: {}", e))?;

        Ok(resp.into_inner())
    }
}

/// Get the default socket path for the management interface.
pub fn default_socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("coordinator.sock")
}
