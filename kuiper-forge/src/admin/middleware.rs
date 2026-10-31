//! Admin state and constants.

use crate::admin::api_tokens::ApiTokenStore;
use crate::admin::auth::AdminAuthStore;
use crate::agent_failures::AgentFailureStore;
use crate::agent_logs::AgentLogStore;
use crate::agent_registry::AgentRegistry;
use crate::agent_updates::AgentUpdates;
use crate::auth::AuthManager;
use crate::fleet::FleetNotifier;
use crate::metrics::MetricsStore;
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
    /// API tokens for `/api/v1`
    pub api_tokens: ApiTokenStore,
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
    /// Recent per-agent failures
    pub agent_failures: Arc<AgentFailureStore>,
    /// Newest agent releases, for flagging out-of-date agents
    pub agent_updates: Arc<AgentUpdates>,
    pub metrics: Arc<MetricsStore>,
    /// Logs uploaded by agents
    pub agent_logs: Arc<AgentLogStore>,
    /// Server trust info for registration bundles
    pub server_trust: ServerTrust,
    /// Coordinator URL for registration bundles
    pub coordinator_url: String,
    /// Wakes the fleet manager, e.g. when an agent takes work again
    pub fleet_notifier: Option<FleetNotifier>,
}
