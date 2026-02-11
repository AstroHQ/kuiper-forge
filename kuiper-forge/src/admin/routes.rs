//! Admin route handlers.
//!
//! Provides HTTP handlers for the admin UI: login, logout, dashboard, agents.

use crate::admin::auth::AdminSession;
use crate::admin::middleware::{AdminState, SESSION_COOKIE};
use crate::admin::templates::{
    AgentDetailTemplate, AgentSummary, BaseContext, DashboardTemplate, LoginTemplate,
    RunnerSummary, TokenSummary,
};
use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::CookieJar;
use chrono::Duration;
use serde::Deserialize;
use std::collections::HashSet;
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
        .with_state(state)
}

/// Check session and return user if authenticated.
async fn check_auth(state: &AdminState, jar: &CookieJar) -> Option<AdminSession> {
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
    let cookie = format!(
        "{SESSION_COOKIE}={session_id}; Path=/admin; HttpOnly; SameSite=Strict"
    );

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
        && let Err(e) = state.auth_store.delete_session(&session.session_id).await {
            error!("Failed to delete session: {}", e);
        }

    // Clear cookie by setting it to expire in the past
    let cookie = format!(
        "{SESSION_COOKIE}=; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=0"
    );

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
    let active_runners = state
        .runner_state
        .get_all_runners()
        .await
        .map(|r| r.len())
        .unwrap_or(0);
    let pending_jobs = state
        .pending_jobs
        .get_all_pending_jobs()
        .await
        .map(|j| j.len())
        .unwrap_or(0);

    // Get agents
    let registered = state.auth_manager.list_agents().await;
    let connected_ids: HashSet<String> = state
        .agent_registry
        .list_all()
        .await
        .into_iter()
        .map(|a| a.agent_id)
        .collect();
    let runners = state
        .runner_state
        .get_all_runners()
        .await
        .unwrap_or_default();

    let agents: Vec<AgentSummary> = registered
        .into_iter()
        .map(|a| {
            let active_vms = runners
                .iter()
                .filter(|(_, r)| r.agent_id == a.agent_id)
                .count();
            AgentSummary {
                agent_id: a.agent_id.clone(),
                hostname: a.hostname,
                agent_type: a.agent_type,
                labels: a.labels,
                max_vms: a.max_vms,
                is_online: connected_ids.contains(&a.agent_id),
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
        active_runners,
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

    Redirect::to("/admin/dashboard").into_response()
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

    let connected_ids: HashSet<String> = state
        .agent_registry
        .list_all()
        .await
        .into_iter()
        .map(|a| a.agent_id)
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

    let agent_summary = AgentSummary {
        agent_id: agent.agent_id.clone(),
        hostname: agent.hostname,
        agent_type: agent.agent_type,
        labels: agent.labels,
        max_vms: agent.max_vms,
        is_online: connected_ids.contains(&agent.agent_id),
        active_vms: runners.len(),
        created_at: agent.created_at,
        revoked: agent.revoked,
    };

    let template = AgentDetailTemplate {
        base,
        agent: agent_summary,
        runners,
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
