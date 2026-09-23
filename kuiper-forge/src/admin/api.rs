//! Read-only HTTP API, authenticated with API tokens from the admin UI.
//!
//! - `GET /api/v1/status` - fleet-wide counts
//! - `GET /api/v1/agents` - every registered agent with its live status

use crate::admin::middleware::AdminState;
use crate::agent_registry::AgentInfo;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

/// Build the API router, to be nested under `/api/v1`.
pub fn api_router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/agents", get(agents))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn require_token(
    State(state): State<Arc<AdminState>>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);

    let Some(token) = token else {
        let mut response = api_error(StatusCode::UNAUTHORIZED, "missing bearer token");
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
        return response;
    };

    match state.api_tokens.validate(token).await {
        Ok(Some(_)) => next.run(request).await,
        Ok(None) => api_error(StatusCode::UNAUTHORIZED, "invalid token"),
        Err(e) => {
            error!("Failed to validate API token: {:#}", e);
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

#[derive(Serialize)]
struct StatusResponse {
    registered_agents: usize,
    connected_agents: usize,
    revoked_agents: usize,
    /// Sum of `max_vms` across connected agents
    total_capacity: usize,
    active_runners: usize,
    pending_jobs: usize,
}

async fn status(State(state): State<Arc<AdminState>>) -> Response {
    let registered = state.auth_manager.list_agents().await;
    let connected = state.agent_registry.list_all().await;
    let runners = match state.runner_state.get_all_runners().await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to load runners: {}", e);
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let pending_jobs = match state.pending_jobs.get_all_pending_jobs().await {
        Ok(j) => j,
        Err(e) => {
            error!("Failed to load pending jobs: {}", e);
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };

    Json(StatusResponse {
        registered_agents: registered.len(),
        connected_agents: connected.len(),
        revoked_agents: registered.iter().filter(|a| a.revoked).count(),
        total_capacity: connected.iter().map(|a| a.max_vms).sum(),
        active_runners: runners.len(),
        pending_jobs: pending_jobs.len(),
    })
    .into_response()
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum AgentStatus {
    Online,
    Offline,
    Revoked,
}

#[derive(Serialize)]
struct AgentResponse {
    agent_id: String,
    hostname: String,
    agent_type: String,
    status: AgentStatus,
    labels: Vec<String>,
    /// Only known while the agent is connected
    label_sets: Vec<Vec<String>>,
    max_vms: u32,
    active_runners: usize,
    /// Only set while the agent is connected
    last_seen_secs: Option<u64>,
    created_at: DateTime<Utc>,
    cert_expires_at: DateTime<Utc>,
}

async fn agents(State(state): State<Arc<AdminState>>) -> Response {
    let registered = state.auth_manager.list_agents().await;
    let connected: HashMap<String, AgentInfo> = state
        .agent_registry
        .list_all()
        .await
        .into_iter()
        .map(|a| (a.agent_id.clone(), a))
        .collect();
    let runners = match state.runner_state.get_all_runners().await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to load runners: {}", e);
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };

    let agents: Vec<AgentResponse> = registered
        .into_iter()
        .map(|a| {
            let live = connected.get(&a.agent_id);
            let status = if a.revoked {
                AgentStatus::Revoked
            } else if live.is_some() {
                AgentStatus::Online
            } else {
                AgentStatus::Offline
            };
            AgentResponse {
                active_runners: runners
                    .iter()
                    .filter(|(_, r)| r.agent_id == a.agent_id)
                    .count(),
                label_sets: live.map(|c| c.label_sets.clone()).unwrap_or_default(),
                last_seen_secs: live.map(|c| c.last_seen_secs),
                status,
                agent_id: a.agent_id,
                hostname: a.hostname,
                agent_type: a.agent_type,
                labels: a.labels,
                max_vms: a.max_vms,
                created_at: a.created_at,
                cert_expires_at: a.expires_at,
            }
        })
        .collect();

    Json(agents).into_response()
}
