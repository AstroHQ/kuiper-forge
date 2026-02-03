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
    pub labels: Vec<String>,
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

/// Runner summary
pub struct RunnerSummary {
    pub runner_name: String,
    pub vm_name: String,
    pub created_at: DateTime<Utc>,
}
