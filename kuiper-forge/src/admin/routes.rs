//! Admin route handlers.
//!
//! Provides HTTP handlers for the admin UI: login, logout, dashboard, agents.

use crate::admin::auth::AdminSession;
use crate::admin::middleware::{AdminState, SESSION_COOKIE};
use crate::admin::templates::{
    AgentDetailTemplate, AgentSummary, BaseContext, DashboardTemplate, FailureSummary,
    LoginTemplate, PendingJobSummary, RunnerSummary, TokenSummary,
};
use crate::admin::{api_token_routes, user_routes};
use crate::agent_failures::FailureKind;
use crate::agent_registry::AgentInfo;
use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::CookieJar;
use chrono::{Duration, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

/// Build the admin router.
pub fn admin_router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/login", get(login_page))
        .route("/login", post(login_submit))
        .route("/dashboard", get(dashboard))
        .route("/logout", post(logout))
        .route("/tokens/create", post(token_create))
        .route("/tokens/{token}/delete", post(token_delete))
        .route("/agents/{agent_id}", get(agent_detail))
        .route("/agents/{agent_id}/revoke", post(agent_revoke))
        .route("/users", get(user_routes::users_page))
        .route("/users/create", post(user_routes::user_create))
        .route("/users/password", post(user_routes::user_set_password))
        .route("/users/delete", post(user_routes::user_delete))
        .route("/api-tokens", get(api_token_routes::api_tokens_page))
        .route(
            "/api-tokens/create",
            post(api_token_routes::api_token_create),
        )
        .route(
            "/api-tokens/delete",
            post(api_token_routes::api_token_delete),
        )
        .with_state(state)
}

/// Check session and return user if authenticated.
pub(crate) async fn check_auth(state: &AdminState, jar: &CookieJar) -> Option<AdminSession> {
    let session_id = jar.get(SESSION_COOKIE)?.value().to_string();
    state.auth_store.validate_session(&session_id).await.ok()?
}

/// Login page handler.
async fn login_page(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    // If already logged in, redirect to dashboard
    if check_auth(&state, &jar).await.is_some() {
        return Redirect::to("/admin/dashboard").into_response();
    }

    let template = LoginTemplate { error: None };
    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// Login form data.
#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

/// Login form submission handler.
async fn login_submit(
    State(state): State<Arc<AdminState>>,
    _jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    // Attempt authentication
    let session_id = match state
        .auth_store
        .authenticate(
            &form.username,
            &form.password,
            state.session_timeout_secs,
            None,
            None,
        )
        .await
    {
        Ok(Some(session_id)) => session_id,
        Ok(None) => {
            let template = LoginTemplate {
                error: Some("Invalid username or password".to_string()),
            };
            return Html(
                template
                    .render()
                    .unwrap_or_else(|e| format!("Template error: {e}")),
            )
            .into_response();
        }
        Err(e) => {
            error!("Login error: {}", e);
            let template = LoginTemplate {
                error: Some("An error occurred. Please try again.".to_string()),
            };
            return Html(
                template
                    .render()
                    .unwrap_or_else(|e| format!("Template error: {e}")),
            )
            .into_response();
        }
    };

    // Set session cookie
    let cookie = format!("{SESSION_COOKIE}={session_id}; Path=/admin; HttpOnly; SameSite=Strict");

    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/admin/dashboard")
        .header(header::SET_COOKIE, cookie)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// Logout handler.
async fn logout(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    // Delete session from database
    if let Some(session) = check_auth(&state, &jar).await
        && let Err(e) = state.auth_store.delete_session(&session.session_id).await
    {
        error!("Failed to delete session: {}", e);
    }

    // Clear cookie by setting it to expire in the past
    let cookie = format!("{SESSION_COOKIE}=; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=0");

    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/admin/login")
        .header(header::SET_COOKIE, cookie)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// Dashboard handler.
async fn dashboard(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    render_dashboard(&state, &jar, None).await
}

/// Render dashboard with optional new token to display.
async fn render_dashboard(
    state: &AdminState,
    jar: &CookieJar,
    new_token: Option<String>,
) -> Response {
    let session = match check_auth(state, jar).await {
        Some(s) => s,
        None => return Redirect::to("/admin/login").into_response(),
    };

    let base = BaseContext {
        username: session.username.clone(),
    };

    let connected_agents = state.agent_registry.count().await;
    let runners = state
        .runner_state
        .get_all_runners()
        .await
        .unwrap_or_default();

    // jobs stay pending until their runner finishes, so tell apart the ones still waiting for an agent
    let job_agents: HashMap<u64, String> = runners
        .iter()
        .filter_map(|(_, r)| Some((r.job_id?, r.agent_id.clone())))
        .collect();
    let mut pending = state
        .pending_jobs
        .get_all_pending_jobs()
        .await
        .unwrap_or_default();
    pending.sort_by_key(|(id, j)| (job_agents.contains_key(id), j.created_at));
    let now = Utc::now();
    let mut pending_jobs = Vec::with_capacity(pending.len());
    for (job_id, job) in pending {
        pending_jobs.push(PendingJobSummary {
            job_id,
            assigned_agent: job_agents.get(&job_id).cloned(),
            waiting: format_age(now - job.created_at),
            matching_agents: state
                .agent_registry
                .find_agents_by_labels(&job.agent_labels)
                .await
                .len(),
            free_capacity: state
                .agent_registry
                .available_capacity(&job.agent_labels)
                .await,
            repository: job.repository,
            workflow_name: job.workflow_name,
            job_name: job.job_name,
            labels: job.job_labels,
            retry_count: job.retry_count,
            failed_agents: job.failed_agents.len(),
        });
    }

    // Get agents
    let registered = state.auth_manager.list_agents().await;
    let connected: HashMap<String, AgentInfo> = state
        .agent_registry
        .list_all()
        .await
        .into_iter()
        .map(|a| (a.agent_id.clone(), a))
        .collect();
    let agents: Vec<AgentSummary> = registered
        .into_iter()
        .map(|a| {
            let active_vms = runners
                .iter()
                .filter(|(_, r)| r.agent_id == a.agent_id)
                .count();
            let label_sets = connected
                .get(&a.agent_id)
                .map(|c| c.label_sets.clone())
                .unwrap_or_default();
            AgentSummary {
                agent_id: a.agent_id.clone(),
                hostname: a.hostname,
                agent_type: a.agent_type,
                label_sets,
                max_vms: a.max_vms,
                is_online: connected.contains_key(&a.agent_id),
                active_vms,
                created_at: a.created_at,
                revoked: a.revoked,
            }
        })
        .collect();

    // Get pending tokens
    let tokens: Vec<TokenSummary> = state
        .auth_manager
        .list_tokens()
        .await
        .into_iter()
        .map(|t| TokenSummary {
            token: t.token,
            expires_at: t.expires_at,
            created_by: t.created_by,
            created_at: t.created_at,
        })
        .collect();

    let template = DashboardTemplate {
        base,
        connected_agents,
        active_runners: runners.len(),
        pending_jobs,
        agents,
        tokens,
        new_token,
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// Create a new registration token.
async fn token_create(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    let session = match check_auth(&state, &jar).await {
        Some(s) => s,
        None => return Redirect::to("/admin/login").into_response(),
    };

    // Create token valid for 1 hour
    match state
        .auth_manager
        .create_registration_token(Duration::hours(1), &session.username)
        .await
    {
        Ok(token) => {
            // Build registration bundle like the CLI does
            let bundle = build_registration_bundle(
                &token.token,
                &state.server_trust,
                &state.coordinator_url,
            );
            render_dashboard(&state, &jar, Some(bundle)).await
        }
        Err(e) => {
            error!("Failed to create registration token: {}", e);
            Redirect::to("/admin/dashboard").into_response()
        }
    }
}

/// Build a registration bundle from token and server trust info.
fn build_registration_bundle(
    token: &str,
    server_trust: &crate::tls::ServerTrust,
    coordinator_url: &str,
) -> String {
    use base64::Engine;

    let mut bundle_map = serde_json::Map::new();
    bundle_map.insert(
        "t".to_string(),
        serde_json::Value::String(token.to_string()),
    );

    if let Some(ref ca_pem) = server_trust.server_ca_pem {
        bundle_map.insert("ca".to_string(), serde_json::Value::String(ca_pem.clone()));
    }

    let trust_mode = match server_trust.server_trust_mode {
        crate::config::ServerTrustMode::Ca => "ca",
        crate::config::ServerTrustMode::Chain => "chain",
    };
    bundle_map.insert(
        "m".to_string(),
        serde_json::Value::String(trust_mode.to_string()),
    );
    bundle_map.insert(
        "u".to_string(),
        serde_json::Value::String(coordinator_url.to_string()),
    );

    let bundle_json = serde_json::Value::Object(bundle_map);
    let encoded =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bundle_json.to_string().as_bytes());
    format!("kfr1_{encoded}")
}

/// Delete a registration token.
async fn token_delete(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Path(token): Path<String>,
) -> Response {
    if check_auth(&state, &jar).await.is_none() {
        return Redirect::to("/admin/login").into_response();
    }

    if let Err(e) = state.auth_manager.delete_token(&token).await {
        error!("Failed to delete token: {}", e);
    }

    // the fragment tells the dashboard to reopen the register dialog
    Redirect::to("/admin/dashboard#register").into_response()
}

/// Agent detail handler.
async fn agent_detail(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Path(agent_id): Path<String>,
) -> Response {
    let session = match check_auth(&state, &jar).await {
        Some(s) => s,
        None => return Redirect::to("/admin/login").into_response(),
    };

    let base = BaseContext {
        username: session.username.clone(),
    };

    let registered = state.auth_manager.list_agents().await;
    let agent = match registered.into_iter().find(|a| a.agent_id == agent_id) {
        Some(a) => a,
        None => {
            return (StatusCode::NOT_FOUND, "Agent not found").into_response();
        }
    };

    let connected: HashMap<String, AgentInfo> = state
        .agent_registry
        .list_all()
        .await
        .into_iter()
        .map(|a| (a.agent_id.clone(), a))
        .collect();

    let all_runners = state
        .runner_state
        .get_all_runners()
        .await
        .unwrap_or_default();
    let runners: Vec<RunnerSummary> = all_runners
        .into_iter()
        .filter(|(_, r)| r.agent_id == agent_id)
        .map(|(name, r)| RunnerSummary {
            runner_name: name,
            vm_name: r.vm_name,
            created_at: r.created_at,
            job_id: r.job_id,
            job_name: r.job_name,
            repository: r.repository,
            workflow_name: r.workflow_name,
        })
        .collect();

    let label_sets = connected
        .get(&agent.agent_id)
        .map(|c| c.label_sets.clone())
        .unwrap_or_default();
    let agent_summary = AgentSummary {
        agent_id: agent.agent_id.clone(),
        hostname: agent.hostname,
        agent_type: agent.agent_type,
        label_sets,
        max_vms: agent.max_vms,
        is_online: connected.contains_key(&agent.agent_id),
        active_vms: runners.len(),
        created_at: agent.created_at,
        revoked: agent.revoked,
    };

    let now = Utc::now();
    let failures = state
        .agent_failures
        .recent(&agent_id, 25)
        .await
        .unwrap_or_else(|e| {
            error!("Failed to load failures for agent {}: {}", agent_id, e);
            Vec::new()
        })
        .into_iter()
        .map(|f| FailureSummary {
            ago: format_age(now - f.occurred_at),
            occurred_at: f.occurred_at,
            kind: FailureKind::label(&f.kind),
            runner_name: f.runner_name,
            job_id: f.job_id,
            message: f.message,
        })
        .collect();

    let template = AgentDetailTemplate {
        base,
        agent: agent_summary,
        runners,
        failures,
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// Agent revoke handler.
async fn agent_revoke(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Path(agent_id): Path<String>,
) -> Response {
    if check_auth(&state, &jar).await.is_none() {
        return Redirect::to("/admin/login").into_response();
    }

    match state.auth_manager.revoke_agent(&agent_id).await {
        Ok(true) => Redirect::to("/admin/dashboard").into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "Agent not found").into_response(),
        Err(e) => {
            error!("Failed to revoke agent {}: {}", agent_id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to revoke agent").into_response()
        }
    }
}

/// Short human age like `45s`, `4m 12s`, `3h 5m` or `2d 4h`.
fn format_age(age: Duration) -> String {
    let secs = age.num_seconds().max(0);
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m {s}s"),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_age() {
        assert_eq!(format_age(Duration::seconds(-5)), "0s");
        assert_eq!(format_age(Duration::seconds(45)), "45s");
        assert_eq!(format_age(Duration::seconds(252)), "4m 12s");
        assert_eq!(
            format_age(Duration::seconds(3 * 3600 + 5 * 60 + 9)),
            "3h 5m"
        );
        assert_eq!(format_age(Duration::seconds(2 * 86400 + 4 * 3600)), "2d 4h");
    }
}
