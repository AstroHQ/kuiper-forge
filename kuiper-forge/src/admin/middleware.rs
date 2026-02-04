//! Admin state and constants.

use crate::admin::auth::AdminAuthStore;
use crate::agent_registry::AgentRegistry;
use crate::auth::AuthManager;
use crate::pending_jobs::PendingJobStore;
use crate::runner_state::RunnerStateStore;
use crate::tls::ServerTrust;
use std::sync::Arc;

/// Cookie name for the session ID
pub const SESSION_COOKIE: &str = "kuiper_admin_session";

/// State shared by admin routes
pub struct AdminState {
    /// Admin user/session authentication
    pub auth_store: AdminAuthStore,
    /// Session timeout in seconds
    pub session_timeout_secs: u64,
    /// Agent certificate/registration management
    pub auth_manager: Arc<AuthManager>,
    /// In-memory registry of connected agents
    pub agent_registry: Arc<AgentRegistry>,
    /// Persistent runner state
    pub runner_state: Arc<RunnerStateStore>,
    /// Pending webhook jobs
    pub pending_jobs: Arc<PendingJobStore>,
    /// Server trust info for registration bundles
    pub server_trust: ServerTrust,
    /// Coordinator URL for registration bundles
    pub coordinator_url: String,
}
