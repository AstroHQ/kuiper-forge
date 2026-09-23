//! Askama templates for the admin UI.

use askama::Template;
use chrono::{DateTime, Utc};

/// Base data available to all templates
pub struct BaseContext {
    pub username: String,
}

/// Login page template
#[derive(Template)]
#[template(path = "admin/login.html")]
pub struct LoginTemplate {
    pub error: Option<String>,
}

/// Registration token summary
pub struct TokenSummary {
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// Dashboard page template
#[derive(Template)]
#[template(path = "admin/dashboard.html")]
pub struct DashboardTemplate {
    pub base: BaseContext,
    pub connected_agents: usize,
    pub active_runners: usize,
    pub pending_jobs: usize,
    pub agents: Vec<AgentSummary>,
    pub tokens: Vec<TokenSummary>,
    pub new_token: Option<String>,
}

/// Agent summary for list view
pub struct AgentSummary {
    pub agent_id: String,
    pub hostname: String,
    pub agent_type: String,
    pub label_sets: Vec<Vec<String>>,
    pub max_vms: u32,
    pub is_online: bool,
    pub active_vms: usize,
    pub created_at: DateTime<Utc>,
    pub revoked: bool,
}

/// Agent detail page template
#[derive(Template)]
#[template(path = "admin/agent_detail.html")]
pub struct AgentDetailTemplate {
    pub base: BaseContext,
    pub agent: AgentSummary,
    pub runners: Vec<RunnerSummary>,
}

/// Admin user row for the users page
pub struct UserSummary {
    pub username: String,
    pub created_at: DateTime<Utc>,
    pub last_login: Option<DateTime<Utc>>,
    pub is_self: bool,
}

/// Admin users page template
#[derive(Template)]
#[template(path = "admin/users.html")]
pub struct UsersTemplate {
    pub base: BaseContext,
    pub users: Vec<UserSummary>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

/// API token row for the API tokens page
pub struct ApiTokenSummary {
    pub id: String,
    pub name: String,
    pub token_prefix: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// API tokens page template
#[derive(Template)]
#[template(path = "admin/api_tokens.html")]
pub struct ApiTokensTemplate {
    pub base: BaseContext,
    pub tokens: Vec<ApiTokenSummary>,
    /// Plaintext of a just-created token, shown once
    pub new_token: Option<String>,
    pub error: Option<String>,
}

/// Runner summary
pub struct RunnerSummary {
    pub runner_name: String,
    pub vm_name: String,
    pub created_at: DateTime<Utc>,
    pub job_id: Option<u64>,
    pub job_name: Option<String>,
    pub repository: Option<String>,
    pub workflow_name: Option<String>,
}
